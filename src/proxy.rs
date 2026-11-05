use std::error::Error as StdError;
use std::fmt;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use bytes::Bytes;
use http_body_util::combinators::BoxBody;
use http_body_util::{BodyExt, Full};
use hyper::body::{Body, Frame, Incoming};
use hyper::header::{
    ACCEPT_ENCODING, AUTHORIZATION, CONTENT_ENCODING, CONTENT_LENGTH, CONTENT_TYPE, HOST,
    HeaderMap, HeaderName, HeaderValue,
};
use hyper::{Method, Request, Response, StatusCode, Uri};
use hyper_rustls::HttpsConnector;
use hyper_util::client::legacy::Client;
use parking_lot::RwLock;
use serde_json::Value;
use serde_json::value::RawValue;
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use url::Url;

use crate::config::{AUTH_BEARER, AUTH_X_API_KEY, Config, RouteConfig, glob_match};
use crate::ids::rewrite_message_tool_ids;
use crate::inline_tools::hoist_inline_tools;
use crate::log;
use crate::metrics::{ApiUsage, Metrics, UsageSniffer, flatten_numeric};
use crate::network::Connector;
use crate::raw::RawObject;
use crate::schema::sanitize_schema;

pub const CLIENT_TOKEN_HEADER: &str = "x-llm-router-token";

pub type BoxError = Box<dyn StdError + Send + Sync>;
pub type ProxyBody = BoxBody<Bytes, BoxError>;
pub type HttpClient = Client<HttpsConnector<Connector>, ProxyBody>;

const HOP_BY_HOP: &[&str] = &[
    "connection",
    "proxy-connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
];

struct Upstream {
    origin: String,
    authority: String,
    base_path: String,
    base_query: String,
}

impl Upstream {
    fn parse(raw: &str) -> Result<Self, String> {
        let url = Url::parse(raw).map_err(|e| e.to_string())?;
        let host = url.host_str().ok_or("missing host")?;
        let authority = match url.port() {
            Some(port) => format!("{host}:{port}"),
            None => host.to_string(),
        };
        Ok(Self {
            origin: format!("{}://{authority}", url.scheme()),
            authority,
            base_path: url.path().to_string(),
            base_query: url.query().unwrap_or_default().to_string(),
        })
    }

    fn uri(&self, incoming: &Uri) -> Result<Uri, String> {
        let path = single_joining_slash(&self.base_path, incoming.path());
        let query = match (self.base_query.is_empty(), incoming.query()) {
            (true, None) => String::new(),
            (true, Some(q)) => format!("?{q}"),
            (false, None) => format!("?{}", self.base_query),
            (false, Some(q)) => format!("?{}&{q}", self.base_query),
        };
        format!("{}{}{}", self.origin, path, query)
            .parse()
            .map_err(|e| format!("upstream uri: {e}"))
    }
}

fn single_joining_slash(a: &str, b: &str) -> String {
    let a_slash = a.ends_with('/');
    let b_slash = b.starts_with('/');
    if a_slash && b_slash {
        format!("{a}{}", &b[1..])
    } else if !a_slash && !b_slash {
        format!("{a}/{b}")
    } else {
        format!("{a}{b}")
    }
}

pub struct Route {
    pub cfg: RouteConfig,
    pub token: String,
    pub disabled: bool,
    upstream: Upstream,
}

impl Route {
    fn matches(&self, model: &str) -> bool {
        self.cfg.rewrite.contains_key(model) || self.cfg.models.iter().any(|p| glob_match(p, model))
    }

    /// Applies model rewrites, field drops/sets and sanitization. Returns None when nothing
    /// changes, in which case the original bytes go upstream untouched.
    fn transform_body(
        &self,
        body: &[u8],
        model: &str,
        count_tokens: bool,
    ) -> Result<Option<Vec<u8>>, String> {
        let mut upstream_model: Option<String> = None;
        if let Some(v) = self.cfg.rewrite.get(model) {
            upstream_model = Some(v.clone());
        }
        if upstream_model.is_none()
            && !self.cfg.strip_prefix.is_empty()
            && let Some(rest) = model.strip_prefix(&self.cfg.strip_prefix)
        {
            upstream_model = Some(rest.to_string());
        }
        if count_tokens && !self.cfg.count_tokens_model.is_empty() {
            upstream_model = Some(self.cfg.count_tokens_model.clone());
        }
        let rewrite = upstream_model.is_some();
        let sanitize = &self.cfg.sanitize;
        let sanitize_tools = !sanitize.tool_schema.is_noop();
        if !rewrite
            && self.cfg.drop_fields.is_empty()
            && self.cfg.resolved_set_fields.is_empty()
            && self.cfg.drop_tool_fields.is_empty()
            && sanitize.is_noop()
        {
            return Ok(None);
        }

        let mut map: RawObject = serde_json::from_slice(body).map_err(|e| e.to_string())?;
        let mut changed = false;
        if let Some(model) = upstream_model {
            map.insert("model".to_string(), raw_value(&model)?);
            changed = true;
        }
        for field in &self.cfg.drop_fields {
            if map.remove(field.as_str()).is_some() {
                changed = true;
            }
        }
        for (field, value) in &self.cfg.resolved_set_fields {
            map.insert(field.clone(), raw_value(value)?);
            changed = true;
        }
        if sanitize.drop_metadata_user_id
            && let Some(raw) = map.get("metadata")
        {
            let mut metadata: Value =
                serde_json::from_str(raw.get()).map_err(|e| format!("metadata: {e}"))?;
            if let Some(object) = metadata.as_object_mut()
                && object.remove("user_id").is_some()
            {
                map.insert("metadata".to_string(), raw_value(&metadata)?);
                changed = true;
            }
        }
        let mut hoisted: Vec<Value> = Vec::new();
        if (sanitize.sanitize_tool_use_ids || sanitize.hoist_inline_tools)
            && let Some(raw) = map.get("messages")
        {
            let mut messages: Value =
                serde_json::from_str(raw.get()).map_err(|e| format!("messages: {e}"))?;
            let mut messages_changed = false;
            if sanitize.sanitize_tool_use_ids && rewrite_message_tool_ids(&mut messages) {
                messages_changed = true;
            }
            if sanitize.hoist_inline_tools && hoist_inline_tools(&mut messages, &mut hoisted) {
                messages_changed = true;
            }
            if messages_changed {
                map.insert("messages".to_string(), raw_value(&messages)?);
                changed = true;
            }
        }
        if !hoisted.is_empty() {
            let mut tools: Vec<RawObject> = match map.get("tools") {
                Some(raw) => serde_json::from_str(raw.get()).map_err(|e| format!("tools: {e}"))?,
                None => Vec::new(),
            };
            for definition in hoisted {
                let entry: RawObject = serde_json::from_value(definition)
                    .map_err(|e| format!("tool_definition: {e}"))?;
                let name = tool_name(&entry);
                if name.is_empty() || tools.iter().any(|tool| tool_name(tool) == name) {
                    continue;
                }
                tools.push(entry);
            }
            map.insert("tools".to_string(), raw_value(&tools)?);
            changed = true;
        }
        if (!self.cfg.drop_tool_fields.is_empty() || sanitize_tools)
            && let Some(raw) = map.get("tools")
        {
            {
                let mut tools: Vec<RawObject> =
                    serde_json::from_str(raw.get()).map_err(|e| format!("tools: {e}"))?;
                let mut tools_changed = false;
                for tool in &mut tools {
                    for field in &self.cfg.drop_tool_fields {
                        if tool.remove(field.as_str()).is_some() {
                            tools_changed = true;
                        }
                    }
                    if !sanitize_tools {
                        continue;
                    }
                    let rewritten = match tool.get("input_schema") {
                        Some(raw_schema) => {
                            let mut schema: Value = serde_json::from_str(raw_schema.get())
                                .map_err(|e| format!("input_schema: {e}"))?;
                            if sanitize_schema(&mut schema, &sanitize.tool_schema) {
                                Some(raw_value(&schema)?)
                            } else {
                                None
                            }
                        }
                        None => None,
                    };
                    if let Some(value) = rewritten {
                        tool.insert("input_schema".to_string(), value);
                        tools_changed = true;
                    }
                }
                if tools_changed {
                    map.insert("tools".to_string(), raw_value(&tools)?);
                    changed = true;
                }
            }
        }
        if !changed {
            return Ok(None);
        }
        serde_json::to_string(&map)
            .map(|s| Some(s.into_bytes()))
            .map_err(|e| e.to_string())
    }
}

/// Reads the `name` of a `tools[]` entry, empty when absent or not a string.
fn tool_name(tool: &RawObject) -> String {
    tool.get("name")
        .and_then(|raw| serde_json::from_str::<String>(raw.get()).ok())
        .unwrap_or_default()
}

/// Encodes a value as a raw JSON fragment for insertion into a body.
fn raw_value<T: serde::Serialize>(value: &T) -> Result<Box<RawValue>, String> {
    let encoded = serde_json::to_string(value).map_err(|e| e.to_string())?;
    RawValue::from_string(encoded).map_err(|e| e.to_string())
}

pub struct Router {
    pub routes: Vec<Arc<Route>>,
    pub fallback: Arc<Route>,
    token_sum: Option<[u8; 32]>,
    max_body: usize,
    pub metrics: Arc<Metrics>,
    client: HttpClient,
}

impl Router {
    pub fn new(
        cfg: &Config,
        client_token: &str,
        client: HttpClient,
        metrics: Arc<Metrics>,
    ) -> Result<Self, String> {
        let token_sum = if client_token.is_empty() {
            None
        } else {
            Some(Sha256::digest(client_token.as_bytes()).into())
        };
        let mut routes = Vec::with_capacity(cfg.routes.len());
        let mut fallback = None;
        for rc in &cfg.routes {
            let upstream =
                Upstream::parse(&rc.upstream).map_err(|e| format!("route {:?}: {e}", rc.name))?;
            let token = rc.auth.resolved.clone();
            let mut disabled = false;
            if rc.auth.mode != crate::config::AUTH_PASSTHROUGH && token.is_empty() {
                disabled = true;
                log::warn(
                    "route disabled: credential is empty",
                    vec![("route", Value::String(rc.name.clone()))],
                );
            }
            let route = Arc::new(Route {
                cfg: rc.clone(),
                token,
                disabled,
                upstream,
            });
            if rc.name == cfg.default_route {
                fallback = Some(route.clone());
            }
            routes.push(route);
        }
        let fallback = fallback.ok_or_else(|| "default route not found".to_string())?;
        Ok(Self {
            routes,
            fallback,
            token_sum,
            max_body: cfg.max_body_bytes.max(0) as usize,
            metrics,
            client,
        })
    }

    fn match_route(&self, model: &str) -> Arc<Route> {
        for r in &self.routes {
            if r.matches(model) {
                return r.clone();
            }
        }
        self.fallback.clone()
    }

    async fn proxy(&self, req: Request<Incoming>) -> Result<Response<ProxyBody>, Abort> {
        let start = Instant::now();
        let (parts, incoming) = req.into_parts();
        let path = parts.uri.path().to_string();
        let is_messages = parts.method == Method::POST && path.starts_with("/v1/messages");

        let mut model = String::new();
        let route;
        let body: ProxyBody;
        if is_messages {
            let raw = match read_limited(incoming, self.max_body).await {
                Ok(raw) => raw,
                Err(ReadBodyError::TooLarge) => {
                    return Ok(json_error(
                        StatusCode::PAYLOAD_TOO_LARGE,
                        "request_too_large",
                        "llm-router: request body too large",
                    ));
                }
                Err(ReadBodyError::Failed(e)) => {
                    return Ok(json_error(
                        StatusCode::BAD_REQUEST,
                        "invalid_request_error",
                        &format!("llm-router: read body: {e}"),
                    ));
                }
            };
            let probe: Value = match serde_json::from_slice(&raw) {
                Ok(v) => v,
                Err(e) => {
                    return Ok(json_error(
                        StatusCode::BAD_REQUEST,
                        "invalid_request_error",
                        &format!("llm-router: invalid JSON body: {e}"),
                    ));
                }
            };
            let Some(obj) = probe.as_object() else {
                return Ok(json_error(
                    StatusCode::BAD_REQUEST,
                    "invalid_request_error",
                    "llm-router: invalid JSON body: model must be a string",
                ));
            };
            model = match obj.get("model") {
                Some(Value::String(s)) => s.clone(),
                Some(Value::Null) | None => String::new(),
                Some(_) => {
                    return Ok(json_error(
                        StatusCode::BAD_REQUEST,
                        "invalid_request_error",
                        "llm-router: invalid JSON body: model must be a string",
                    ));
                }
            };
            route = self.match_route(&model);
            if route.disabled {
                log::warn(
                    "request rejected: route disabled",
                    vec![
                        ("route", Value::String(route.cfg.name.clone())),
                        ("model", Value::String(model.clone())),
                    ],
                );
                let mut resp = json_error(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "api_error",
                    &format!(
                        "llm-router: route {:?} is disabled: auth.token is empty",
                        route.cfg.name
                    ),
                );
                resp.headers_mut()
                    .insert("x-should-retry", HeaderValue::from_static("false"));
                return Ok(resp);
            }
            let count_tokens = path == "/v1/messages/count_tokens";
            let rewritten = match route.transform_body(&raw, &model, count_tokens) {
                Ok(v) => v,
                Err(e) => {
                    return Ok(json_error(
                        StatusCode::BAD_REQUEST,
                        "invalid_request_error",
                        &format!("llm-router: transform body: {e}"),
                    ));
                }
            };
            let bytes = rewritten
                .map(Bytes::from)
                .unwrap_or_else(|| Bytes::from(raw));
            body = full_body(bytes);
        } else {
            route = self.fallback.clone();
            body = incoming.map_err(|e| Box::new(e) as BoxError).boxed();
        }

        let target = match route.upstream.uri(&parts.uri) {
            Ok(uri) => uri,
            Err(e) => {
                return Ok(json_error(
                    StatusCode::BAD_GATEWAY,
                    "api_error",
                    &format!("llm-router: upstream {:?}: {e}", route.cfg.name),
                ));
            }
        };

        let mut headers = parts.headers.clone();
        remove_hop_by_hop(&mut headers);
        headers.remove(CLIENT_TOKEN_HEADER);
        match route.cfg.auth.mode.as_str() {
            AUTH_BEARER => {
                headers.remove("x-api-key");
                if let Ok(v) = HeaderValue::from_str(&format!("Bearer {}", route.token)) {
                    headers.insert(AUTHORIZATION, v);
                }
            }
            AUTH_X_API_KEY => {
                headers.remove(AUTHORIZATION);
                if let Ok(v) = HeaderValue::from_str(&route.token) {
                    headers.insert("x-api-key", v);
                }
            }
            _ => {}
        }
        for name in &route.cfg.drop_headers {
            if let Ok(name) = HeaderName::from_bytes(name.as_bytes()) {
                headers.remove(name);
            }
        }
        for (name, value) in &route.cfg.resolved_set_headers {
            if let (Ok(name), Ok(value)) = (
                HeaderName::from_bytes(name.as_bytes()),
                HeaderValue::from_str(value),
            ) {
                headers.insert(name, value);
            }
        }
        if let Ok(v) = HeaderValue::from_str(&route.upstream.authority) {
            headers.insert(HOST, v);
        }
        if is_messages {
            headers.insert(
                CONTENT_LENGTH,
                HeaderValue::from_str(&body_size(&body).unwrap_or(0).to_string())
                    .unwrap_or(HeaderValue::from_static("0")),
            );
            if path == "/v1/messages" {
                headers.insert(ACCEPT_ENCODING, HeaderValue::from_static("identity"));
            }
        }

        let mut out = Request::builder()
            .method(parts.method.clone())
            .uri(target)
            .body(body)
            .expect("request parts are valid");
        *out.headers_mut() = headers;

        let model_label = if model.is_empty() {
            "none".to_string()
        } else {
            model.clone()
        };
        let agent_id = parts
            .headers
            .get("x-claude-code-agent-id")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_string();
        let inflight = self
            .metrics
            .inflight
            .with_label_values(&[&model_label, &route.cfg.name]);
        inflight.inc();

        match self.client.request(out).await {
            Err(e) => {
                log::error(
                    "upstream error",
                    vec![
                        ("route", Value::String(route.cfg.name.clone())),
                        ("path", Value::String(path.clone())),
                        ("err", Value::String(e.to_string())),
                    ],
                );
                self.metrics
                    .upstream_errors
                    .with_label_values(&[&route.cfg.name])
                    .inc();
                let ttfb = start.elapsed();
                Meta {
                    metrics: self.metrics.clone(),
                    route: route.cfg.name.clone(),
                    model,
                    model_label,
                    method: parts.method.to_string(),
                    path,
                    agent_id,
                    status: StatusCode::BAD_GATEWAY.as_u16(),
                    start,
                    ttfb,
                    inflight,
                }
                .finish(ApiUsage::default());
                Ok(json_error(
                    StatusCode::BAD_GATEWAY,
                    "api_error",
                    &format!("llm-router: upstream {:?}: {e}", route.cfg.name),
                ))
            }
            Ok(resp) => {
                let (resp_parts, resp_body) = resp.into_parts();
                let status = resp_parts.status;
                let ttfb = start.elapsed();
                let mut headers = resp_parts.headers;
                remove_hop_by_hop(&mut headers);

                let sniff = is_messages
                    && path == "/v1/messages"
                    && status.is_success()
                    && !headers.contains_key(CONTENT_ENCODING);
                let sse = headers
                    .get(CONTENT_TYPE)
                    .and_then(|v| v.to_str().ok())
                    .is_some_and(|v| v.starts_with("text/event-stream"));
                let sniffer = if sniff {
                    Some(UsageSniffer::new(sse))
                } else {
                    None
                };

                let meta = Meta {
                    metrics: self.metrics.clone(),
                    route: route.cfg.name.clone(),
                    model: model.clone(),
                    model_label,
                    method: parts.method.to_string(),
                    path,
                    agent_id,
                    status: status.as_u16(),
                    start,
                    ttfb,
                    inflight,
                };
                let body = TailBody {
                    inner: resp_body.map_err(|e| Box::new(e) as BoxError).boxed(),
                    meta: Some(meta),
                    sniffer,
                }
                .boxed();
                let mut response = Response::builder()
                    .status(status)
                    .body(body)
                    .expect("valid response");
                *response.headers_mut() = headers;
                Ok(response)
            }
        }
    }
}

fn body_size(body: &ProxyBody) -> Option<u64> {
    body.size_hint().exact()
}

async fn read_limited(mut body: Incoming, max: usize) -> Result<Vec<u8>, ReadBodyError> {
    let mut buf = Vec::new();
    while let Some(frame) = body.frame().await {
        match frame {
            Ok(frame) => {
                if let Some(data) = frame.data_ref() {
                    if buf.len() + data.len() > max {
                        return Err(ReadBodyError::TooLarge);
                    }
                    buf.extend_from_slice(data);
                }
            }
            Err(e) => return Err(ReadBodyError::Failed(e.to_string())),
        }
    }
    Ok(buf)
}

enum ReadBodyError {
    TooLarge,
    Failed(String),
}

struct Meta {
    metrics: Arc<Metrics>,
    route: String,
    model: String,
    model_label: String,
    method: String,
    path: String,
    agent_id: String,
    status: u16,
    start: Instant,
    ttfb: Duration,
    inflight: prometheus::IntGauge,
}

impl Meta {
    fn finish(self, usage: ApiUsage) {
        let elapsed = self.start.elapsed();
        self.metrics
            .observe_usage(&self.route, &self.model_label, usage);
        self.metrics
            .duration
            .with_label_values(&[&self.model_label, &self.route])
            .observe(elapsed.as_secs_f64());
        self.metrics
            .ttfb
            .with_label_values(&[&self.model_label, &self.route])
            .observe(self.ttfb.as_secs_f64());
        self.metrics
            .requests
            .with_label_values(&[&self.status.to_string(), &self.model_label, &self.route])
            .inc();
        self.inflight.dec();
        log::info(
            "request",
            vec![
                ("method", Value::String(self.method)),
                ("path", Value::String(self.path)),
                ("route", Value::String(self.route)),
                ("model", Value::String(self.model)),
                ("status", Value::from(self.status)),
                ("ttfb_ms", Value::from(self.ttfb.as_millis() as u64)),
                ("duration_ms", Value::from(elapsed.as_millis() as u64)),
                ("input_tokens", Value::from(usage.input_tokens)),
                ("output_tokens", Value::from(usage.output_tokens)),
                ("cache_read_tokens", Value::from(usage.cache_read_tokens)),
                (
                    "cache_creation_tokens",
                    Value::from(usage.cache_creation_tokens),
                ),
                ("agent_id", Value::String(self.agent_id)),
            ],
        );
    }
}

/// Streams the upstream body through, sniffing usage, and records the request
/// metrics once the body ends (or the client goes away).
struct TailBody {
    inner: ProxyBody,
    meta: Option<Meta>,
    sniffer: Option<UsageSniffer>,
}

impl TailBody {
    fn finish(&mut self) {
        if let Some(meta) = self.meta.take() {
            let usage = self
                .sniffer
                .as_mut()
                .map(|s| s.finish())
                .unwrap_or_default();
            meta.finish(usage);
        }
    }
}

impl Drop for TailBody {
    fn drop(&mut self) {
        self.finish();
    }
}

impl Body for TailBody {
    type Data = Bytes;
    type Error = BoxError;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, BoxError>>> {
        let this = self.as_mut().get_mut();
        match Pin::new(&mut this.inner).poll_frame(cx) {
            Poll::Ready(Some(Ok(frame))) => {
                if let Some(data) = frame.data_ref()
                    && let Some(sniffer) = this.sniffer.as_mut()
                {
                    sniffer.feed(data);
                }
                Poll::Ready(Some(Ok(frame)))
            }
            Poll::Ready(Some(Err(e))) => {
                this.finish();
                Poll::Ready(Some(Err(e)))
            }
            Poll::Ready(None) => {
                this.finish();
                Poll::Ready(None)
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

fn remove_hop_by_hop(headers: &mut HeaderMap) {
    let connection_tokens: Vec<HeaderName> = headers
        .get_all("connection")
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .filter_map(|s| HeaderName::from_bytes(s.trim().as_bytes()).ok())
        .collect();
    for name in HOP_BY_HOP {
        headers.remove(*name);
    }
    for name in connection_tokens {
        headers.remove(name);
    }
}

fn full_body(bytes: Bytes) -> ProxyBody {
    Full::new(bytes)
        .map_err(|never: std::convert::Infallible| -> BoxError { match never {} })
        .boxed()
}

fn json_error(status: StatusCode, typ: &str, msg: &str) -> Response<ProxyBody> {
    let body = serde_json::json!({"type": "error", "error": {"type": typ, "message": msg}})
        .to_string()
        + "\n";
    Response::builder()
        .status(status)
        .header(CONTENT_TYPE, "application/json")
        .body(full_body(Bytes::from(body)))
        .expect("valid error response")
}

fn text_response(status: StatusCode, body: &'static str) -> Response<ProxyBody> {
    let mut response = Response::builder()
        .status(status)
        .header(CONTENT_TYPE, "text/plain; charset=utf-8")
        .body(full_body(Bytes::from_static(body.as_bytes())))
        .expect("valid text response");
    if status.is_client_error() || status.is_server_error() {
        response.headers_mut().insert(
            "x-content-type-options",
            HeaderValue::from_static("nosniff"),
        );
    }
    response
}

/// Marker error: the connection is closed without any response.
#[derive(Debug)]
pub struct Abort;

impl fmt::Display for Abort {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("connection aborted")
    }
}

impl StdError for Abort {}

#[derive(Clone)]
pub struct Handler {
    /// Swapped on SIGHUP; read per request so keep-alive connections see a reload.
    pub router: Arc<RwLock<Arc<Router>>>,
    pub metrics: Arc<Metrics>,
    pub ready: Arc<AtomicBool>,
    /// Serve /livez, /readyz and /metrics on this listener.
    pub probes: bool,
    /// Proxy requests on this listener.
    pub proxy_enabled: bool,
}

impl Handler {
    pub async fn call(&self, req: Request<Incoming>) -> Result<Response<ProxyBody>, Abort> {
        if self.probes && req.method() == Method::GET {
            match req.uri().path() {
                "/livez" => return Ok(text_response(StatusCode::OK, "ok\n")),
                "/readyz" => {
                    return Ok(if self.ready.load(std::sync::atomic::Ordering::Relaxed) {
                        text_response(StatusCode::OK, "ok\n")
                    } else {
                        text_response(StatusCode::SERVICE_UNAVAILABLE, "not ready\n")
                    });
                }
                "/metrics" => {
                    let (content_type, body) = self.metrics.encode();
                    return Ok(Response::builder()
                        .status(StatusCode::OK)
                        .header(CONTENT_TYPE, content_type)
                        .body(full_body(Bytes::from(body)))
                        .expect("valid metrics response"));
                }
                _ => {}
            }
        }
        if !self.proxy_enabled {
            return Ok(text_response(StatusCode::NOT_FOUND, "404 page not found\n"));
        }
        let router = self.router.read().clone();
        if let Some(expected) = router.token_sum {
            let provided = req
                .headers()
                .get(CLIENT_TOKEN_HEADER)
                .map(|v| v.as_bytes())
                .unwrap_or_default();
            let sum: [u8; 32] = Sha256::digest(provided).into();
            if !bool::from(sum.ct_eq(&expected)) {
                self.metrics.rejected.inc();
                return Err(Abort);
            }
        }
        let mut req = req;
        req.headers_mut().remove(CLIENT_TOKEN_HEADER);
        router.proxy(req).await
    }
}

pub fn quota_url(route: &Route) -> Option<&str> {
    route.cfg.quota.as_ref().map(|q| q.url.as_str())
}

/// Polls one quota endpoint and replaces the route's exported gauges.
pub async fn scrape_quota(client: &HttpClient, route: &Route, metrics: &Metrics) {
    match fetch_quota(client, route).await {
        Ok(values) => {
            metrics.set_quota(&route.cfg.name, &values);
            metrics
                .quota_scrapes
                .with_label_values(&["success", &route.cfg.name])
                .inc();
        }
        Err(e) => {
            metrics
                .quota_scrapes
                .with_label_values(&["error", &route.cfg.name])
                .inc();
            log::warn(
                "quota poll failed",
                vec![
                    ("route", Value::String(route.cfg.name.clone())),
                    ("err", Value::String(e)),
                ],
            );
        }
    }
}

async fn fetch_quota(
    client: &HttpClient,
    route: &Route,
) -> Result<std::collections::HashMap<String, f64>, String> {
    let url = quota_url(route).ok_or("no quota url")?;
    let mut req = Request::builder()
        .method(Method::GET)
        .uri(url)
        .body(full_body(Bytes::new()))
        .map_err(|e| e.to_string())?;
    match route.cfg.auth.mode.as_str() {
        AUTH_BEARER => {
            req.headers_mut().insert(
                AUTHORIZATION,
                HeaderValue::from_str(&format!("Bearer {}", route.token))
                    .map_err(|e| e.to_string())?,
            );
        }
        AUTH_X_API_KEY => {
            req.headers_mut().insert(
                "x-api-key",
                HeaderValue::from_str(&route.token).map_err(|e| e.to_string())?,
            );
        }
        _ => {}
    }
    let resp = tokio::time::timeout(Duration::from_secs(10), client.request(req))
        .await
        .map_err(|_| "timeout".to_string())?
        .map_err(|e| e.to_string())?;
    let status = resp.status();
    let body = resp
        .into_body()
        .collect()
        .await
        .map_err(|e| e.to_string())?
        .to_bytes();
    let body = &body[..body.len().min(1 << 20)];
    if status != StatusCode::OK {
        return Err(format!(
            "status {}: {}",
            status.as_u16(),
            String::from_utf8_lossy(trim_ascii(body))
        ));
    }
    let doc: Value = serde_json::from_slice(body).map_err(|e| e.to_string())?;
    let mut out = std::collections::HashMap::new();
    flatten_numeric("", &doc, &mut out);
    Ok(out)
}

fn trim_ascii(mut b: &[u8]) -> &[u8] {
    while let [first, rest @ ..] = b {
        if first.is_ascii_whitespace() {
            b = rest;
        } else {
            break;
        }
    }
    while let [rest @ .., last] = b {
        if last.is_ascii_whitespace() {
            b = rest;
        } else {
            break;
        }
    }
    b
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::net::SocketAddr;
    use std::sync::atomic::AtomicBool;

    use http_body_util::BodyExt;
    use hyper::server::conn::http1;
    use hyper::service::service_fn;
    use hyper_util::client::legacy::connect::HttpConnector;
    use hyper_util::rt::{TokioExecutor, TokioIo};
    use tokio::net::TcpListener;
    use tokio::sync::{mpsc, oneshot};

    use crate::config::{AuthConfig, RouteConfig, SanitizeConfig, ToolSchemaConfig};

    type TestClient = Client<HttpConnector, Full<Bytes>>;

    fn test_client() -> TestClient {
        Client::builder(TokioExecutor::new()).build_http()
    }

    fn proxy_client() -> HttpClient {
        let connector = Connector::new(None).expect("connector");
        let https = hyper_rustls::HttpsConnectorBuilder::new()
            .with_native_roots()
            .unwrap()
            .https_or_http()
            .enable_http1()
            .wrap_connector(connector);
        Client::builder(TokioExecutor::new()).build(https)
    }

    fn client_via_proxy(proxy: SocketAddr) -> HttpClient {
        let connector = Connector::new(Some(&format!("http://{proxy}"))).expect("connector");
        let https = hyper_rustls::HttpsConnectorBuilder::new()
            .with_native_roots()
            .unwrap()
            .https_or_http()
            .enable_http1()
            .wrap_connector(connector);
        Client::builder(TokioExecutor::new()).build(https)
    }

    /// Minimal HTTP CONNECT proxy: accepts any tunnel and relays bytes to its target.
    async fn spawn_connect_proxy() -> SocketAddr {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let Ok((mut client, _)) = listener.accept().await else {
                    break;
                };
                tokio::spawn(async move {
                    let mut request = Vec::new();
                    let mut chunk = [0u8; 1024];
                    loop {
                        let read = match client.read(&mut chunk).await {
                            Ok(read) => read,
                            Err(_) => return,
                        };
                        if read == 0 {
                            return;
                        }
                        request.extend_from_slice(&chunk[..read]);
                        if request.windows(4).any(|w| w == b"\r\n\r\n") || request.len() > 8192 {
                            break;
                        }
                    }
                    let head = String::from_utf8_lossy(&request);
                    let target = head
                        .lines()
                        .next()
                        .and_then(|line| line.strip_prefix("CONNECT "))
                        .and_then(|rest| rest.split_whitespace().next())
                        .map(str::to_string);
                    let Some(target) = target else {
                        return;
                    };
                    let Ok(mut upstream) = tokio::net::TcpStream::connect(target).await else {
                        return;
                    };
                    if client
                        .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
                        .await
                        .is_err()
                    {
                        return;
                    }
                    let _ = tokio::io::copy_bidirectional(&mut client, &mut upstream).await;
                });
            }
        });
        addr
    }

    #[derive(Debug, Clone)]
    struct Captured {
        path: String,
        query: Option<String>,
        headers: HeaderMap,
        body: Value,
        raw: Bytes,
    }

    async fn spawn_handler(handler: Handler) -> SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    break;
                };
                let handler = handler.clone();
                tokio::spawn(async move {
                    let service = service_fn(move |req| {
                        let handler = handler.clone();
                        async move { handler.call(req).await }
                    });
                    let _ = http1::Builder::new()
                        .serve_connection(TokioIo::new(stream), service)
                        .await;
                });
            }
        });
        addr
    }

    async fn spawn_upstream(
        status: u16,
        resp: &'static str,
    ) -> (SocketAddr, mpsc::UnboundedReceiver<Captured>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (tx, rx) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    break;
                };
                let tx = tx.clone();
                tokio::spawn(async move {
                    let service = service_fn(move |req: Request<Incoming>| {
                        let tx = tx.clone();
                        async move {
                            let (parts, body) = req.into_parts();
                            let raw = body.collect().await.unwrap().to_bytes();
                            let body = serde_json::from_slice(&raw).unwrap_or(Value::Null);
                            let _ = tx.send(Captured {
                                path: parts.uri.path().to_string(),
                                query: parts.uri.query().map(|q| q.to_string()),
                                headers: parts.headers,
                                body,
                                raw,
                            });
                            Ok::<_, std::convert::Infallible>(
                                Response::builder()
                                    .status(status)
                                    .header(CONTENT_TYPE, "application/json")
                                    .body(Full::new(Bytes::from_static(resp.as_bytes())))
                                    .unwrap(),
                            )
                        }
                    });
                    let _ = http1::Builder::new()
                        .serve_connection(TokioIo::new(stream), service)
                        .await;
                });
            }
        });
        (addr, rx)
    }

    fn two_route_config(ant: SocketAddr, syn: SocketAddr) -> Config {
        Config {
            default_route: "anthropic".to_string(),
            routes: vec![
                RouteConfig {
                    name: "synthetic".to_string(),
                    upstream: format!("http://{syn}/anthropic"),
                    models: vec!["hf:*".to_string(), "syn:*".to_string()],
                    rewrite: BTreeMap::from([(
                        "kimi".to_string(),
                        "hf:moonshotai/Kimi-K3".to_string(),
                    )]),
                    auth: AuthConfig {
                        mode: AUTH_BEARER.to_string(),
                        token: "{env:TEST_SYN_KEY}".to_string(),
                        ..Default::default()
                    },
                    drop_headers: vec!["anthropic-beta".to_string()],
                    drop_fields: vec!["context_management".to_string()],
                    drop_tool_fields: vec!["defer_loading".to_string()],
                    ..Default::default()
                },
                RouteConfig {
                    name: "anthropic".to_string(),
                    upstream: format!("http://{ant}"),
                    models: vec!["claude-*".to_string()],
                    auth: AuthConfig {
                        mode: crate::config::AUTH_PASSTHROUGH.to_string(),
                        ..Default::default()
                    },
                    ..Default::default()
                },
            ],
            ..Default::default()
        }
    }

    fn local_config(upstream: SocketAddr, sanitize: SanitizeConfig) -> Config {
        Config {
            default_route: "local".to_string(),
            routes: vec![RouteConfig {
                name: "local".to_string(),
                upstream: format!("http://{upstream}"),
                models: vec!["claude-*".to_string()],
                auth: AuthConfig {
                    mode: crate::config::AUTH_PASSTHROUGH.to_string(),
                    ..Default::default()
                },
                sanitize,
                ..Default::default()
            }],
            ..Default::default()
        }
    }

    fn build_router(cfg: &mut Config, token: &str, metrics: Arc<Metrics>) -> Arc<Router> {
        cfg.resolve_credentials(std::path::Path::new("."))
            .expect("resolve credentials");
        cfg.validate().expect("valid config");
        Arc::new(Router::new(cfg, token, proxy_client(), metrics).expect("router"))
    }

    fn handler(router: Arc<Router>, metrics: Arc<Metrics>, probes: bool) -> Handler {
        Handler {
            router: Arc::new(RwLock::new(router)),
            metrics,
            ready: Arc::new(AtomicBool::new(true)),
            probes,
            proxy_enabled: true,
        }
    }

    async fn request(
        client: &TestClient,
        addr: SocketAddr,
        method: &str,
        path: &str,
        body: &str,
        headers: &[(&str, &str)],
    ) -> Result<(StatusCode, HeaderMap, Bytes), Box<dyn std::error::Error>> {
        let mut builder = Request::builder()
            .method(method)
            .uri(format!("http://{addr}{path}"))
            .header(CONTENT_TYPE, "application/json");
        for (k, v) in headers {
            builder = builder.header(*k, *v);
        }
        let req = builder.body(Full::new(Bytes::from(body.to_string())))?;
        let resp = client.request(req).await?;
        let (parts, body) = resp.into_parts();
        let raw = body.collect().await?.to_bytes();
        Ok((parts.status, parts.headers, raw))
    }

    async fn post(
        client: &TestClient,
        addr: SocketAddr,
        path: &str,
        body: &str,
        headers: &[(&str, &str)],
    ) -> (StatusCode, HeaderMap, Bytes) {
        request(client, addr, "POST", path, body, headers)
            .await
            .expect("request")
    }

    #[tokio::test]
    async fn routing() {
        unsafe { std::env::set_var("TEST_SYN_KEY", "syn-secret") };
        let (ant, mut ant_rx) = spawn_upstream(200, r#"{"ok":"anthropic"}"#).await;
        let (syn, mut syn_rx) = spawn_upstream(200, r#"{"ok":"synthetic"}"#).await;
        let metrics = Arc::new(Metrics::new());
        let router = build_router(&mut two_route_config(ant, syn), "", metrics.clone());
        let addr = spawn_handler(handler(router, metrics, true)).await;
        let client = test_client();

        // Claude models pass through to anthropic.
        let (status, _, body) = post(
            &client,
            addr,
            "/v1/messages?beta=true",
            r#"{"model":"claude-opus-5","context_management":{"x":1}}"#,
            &[
                ("Authorization", "Bearer sk-ant-oat-user"),
                ("Anthropic-Beta", "oauth-2025-04-20"),
            ],
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, Bytes::from_static(br#"{"ok":"anthropic"}"#));
        let c = ant_rx.recv().await.expect("anthropic upstream");
        assert_eq!(c.path, "/v1/messages");
        assert_eq!(c.query.as_deref(), Some("beta=true"));
        assert_eq!(
            c.headers.get(AUTHORIZATION).unwrap(),
            "Bearer sk-ant-oat-user"
        );
        assert!(c.headers.get("anthropic-beta").is_some());
        assert!(c.body.get("context_management").is_some());

        // hf models go to synthetic with swapped credentials and dropped fields.
        let body = r#"{"model":"hf:deepseek-ai/DeepSeek-V4.1-Flash","context_management":{},"system":[{"type":"text","text":"a<b"}],"tools":[{"name":"Bash","defer_loading":true,"input_schema":{}}]}"#;
        let (status, _, _) = post(
            &client,
            addr,
            "/v1/messages?beta=true",
            body,
            &[
                ("X-Api-Key", "leak"),
                ("X-Claude-Code-Agent-Id", "a1"),
                ("Authorization", "Bearer sk-ant-oat-user"),
                ("Anthropic-Beta", "oauth-2025-04-20"),
            ],
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let c = syn_rx.recv().await.expect("synthetic upstream");
        assert_eq!(c.path, "/anthropic/v1/messages");
        assert_eq!(c.headers.get(AUTHORIZATION).unwrap(), "Bearer syn-secret");
        assert!(c.headers.get("x-api-key").is_none());
        assert!(c.headers.get("anthropic-beta").is_none());
        assert_eq!(c.headers.get("x-claude-code-agent-id").unwrap(), "a1");
        assert_eq!(c.headers.get(ACCEPT_ENCODING).unwrap(), "identity");
        assert!(c.body.get("context_management").is_none());
        let tool = c.body["tools"][0].as_object().unwrap();
        assert!(tool.get("defer_loading").is_none());
        assert_eq!(tool["name"], "Bash");
        assert_eq!(c.body["system"][0]["text"], "a<b");

        // rewrite keys match the route and rewrite the model.
        let (_, _, _) = post(
            &client,
            addr,
            "/v1/messages/count_tokens",
            r#"{"model":"kimi"}"#,
            &[],
        )
        .await;
        let c = syn_rx.recv().await.expect("synthetic upstream");
        assert_eq!(c.path, "/anthropic/v1/messages/count_tokens");
        assert_eq!(c.body["model"], "hf:moonshotai/Kimi-K3");

        // unknown model and non-message paths use the default route.
        let (_, _, _) = post(&client, addr, "/v1/messages", r#"{"model":"gpt-9"}"#, &[]).await;
        let c = ant_rx.recv().await.expect("anthropic upstream");
        assert_eq!(c.body["model"], "gpt-9");
        let (status, _, _) = request(&client, addr, "GET", "/v1/models?limit=1000", "", &[])
            .await
            .expect("request");
        assert_eq!(status, StatusCode::OK);
        let c = ant_rx.recv().await.expect("anthropic upstream");
        assert_eq!(c.path, "/v1/models");
    }

    #[tokio::test]
    async fn count_tokens_model_override() {
        unsafe { std::env::set_var("TEST_SYN_KEY", "syn-secret") };
        let (ant, _ant_rx) = spawn_upstream(200, "{}").await;
        let (syn, mut syn_rx) = spawn_upstream(200, "{}").await;
        let metrics = Arc::new(Metrics::new());
        let mut cfg = two_route_config(ant, syn);
        cfg.routes[0].count_tokens_model = "hf:deepseek-ai/DeepSeek-V4.1-Flash".to_string();
        let router = build_router(&mut cfg, "", metrics.clone());
        let addr = spawn_handler(handler(router, metrics, true)).await;
        let client = test_client();

        post(
            &client,
            addr,
            "/v1/messages/count_tokens",
            r#"{"model":"hf:moonshotai/Kimi-K3"}"#,
            &[],
        )
        .await;
        let c = syn_rx.recv().await.unwrap();
        assert_eq!(c.body["model"], "hf:deepseek-ai/DeepSeek-V4.1-Flash");

        post(
            &client,
            addr,
            "/v1/messages",
            r#"{"model":"hf:moonshotai/Kimi-K3"}"#,
            &[],
        )
        .await;
        let c = syn_rx.recv().await.unwrap();
        assert_eq!(c.body["model"], "hf:moonshotai/Kimi-K3");
    }

    #[tokio::test]
    async fn tool_schema_policy_rewrites_patterns() {
        let (upstream, mut rx) = spawn_upstream(200, "{}").await;
        let metrics = Arc::new(Metrics::new());
        let mut cfg = local_config(
            upstream,
            SanitizeConfig {
                tool_schema: ToolSchemaConfig {
                    normalize_nul_escape: true,
                    strip_lookaround: true,
                    string_formats: Some(vec!["date-time".to_string()]),
                    ..Default::default()
                },
                ..Default::default()
            },
        );
        let router = build_router(&mut cfg, "", metrics.clone());
        let addr = spawn_handler(handler(router, metrics, true)).await;
        let client = test_client();

        let body = serde_json::json!({
            "model": "claude-x",
            "tools": [{
                "name": "read",
                "input_schema": {
                    "type": "object",
                    "properties": {
                        "id": {
                            "type": "string",
                            "pattern": "^(?!\\.\\.?)[^\\0]*$",
                            "format": "uri"
                        },
                        "created": { "type": "string", "format": "date-time" }
                    }
                }
            }]
        });
        post(&client, addr, "/v1/messages", &body.to_string(), &[]).await;
        let c = rx.recv().await.expect("upstream request");
        let id = &c.body["tools"][0]["input_schema"]["properties"]["id"];
        assert_eq!(id["pattern"], r"^[^\x00]*$");
        assert!(id.get("format").is_none());
        assert_eq!(
            c.body["tools"][0]["input_schema"]["properties"]["created"]["format"],
            "date-time"
        );
    }

    #[tokio::test]
    async fn tool_schema_noop_forwards_bytes_unchanged() {
        let (upstream, mut rx) = spawn_upstream(200, "{}").await;
        let metrics = Arc::new(Metrics::new());
        let mut cfg = local_config(upstream, SanitizeConfig::default());
        let router = build_router(&mut cfg, "", metrics.clone());
        let addr = spawn_handler(handler(router, metrics, true)).await;
        let client = test_client();

        let body = r#"{"model":"claude-x","tools":[{"name":"t","input_schema":{"pattern":"^(?!x)[^\\0]*$","format":"uri"}}]}"#;
        post(&client, addr, "/v1/messages", body, &[]).await;
        let c = rx.recv().await.expect("upstream request");
        assert_eq!(c.raw.as_ref(), body.as_bytes());
    }

    #[tokio::test]
    async fn drop_metadata_user_id_removes_only_that_key() {
        let (upstream, mut rx) = spawn_upstream(200, "{}").await;
        let metrics = Arc::new(Metrics::new());
        let mut cfg = local_config(
            upstream,
            SanitizeConfig {
                drop_metadata_user_id: true,
                ..Default::default()
            },
        );
        let router = build_router(&mut cfg, "", metrics.clone());
        let addr = spawn_handler(handler(router, metrics, true)).await;
        let client = test_client();

        let body = r#"{"model":"claude-x","metadata":{"user_id":"bad id!","other":"keep"}}"#;
        post(&client, addr, "/v1/messages", body, &[]).await;
        let c = rx.recv().await.expect("upstream request");
        assert!(c.body["metadata"].get("user_id").is_none());
        assert_eq!(c.body["metadata"]["other"], "keep");
    }

    #[tokio::test]
    async fn drop_metadata_user_id_ignores_non_object_metadata() {
        let (upstream, mut rx) = spawn_upstream(200, "{}").await;
        let metrics = Arc::new(Metrics::new());
        let mut cfg = local_config(
            upstream,
            SanitizeConfig {
                drop_metadata_user_id: true,
                ..Default::default()
            },
        );
        let router = build_router(&mut cfg, "", metrics.clone());
        let addr = spawn_handler(handler(router, metrics, true)).await;
        let client = test_client();

        let body = r#"{"model":"claude-x","metadata":null}"#;
        let (status, _, _) = post(&client, addr, "/v1/messages", body, &[]).await;
        assert_eq!(status, StatusCode::OK);
        let c = rx.recv().await.expect("upstream request");
        assert!(c.body["metadata"].is_null());
    }

    #[tokio::test]
    async fn strip_prefix() {
        unsafe { std::env::set_var("TEST_SYN_KEY", "syn-secret") };
        let (up, mut got) = spawn_upstream(200, "{}").await;
        let mut cfg = two_route_config(up, up);
        cfg.routes.insert(
            0,
            RouteConfig {
                name: "nanogpt".to_string(),
                upstream: format!("http://{up}/api"),
                models: vec!["nano:*".to_string()],
                rewrite: BTreeMap::from([(
                    "nano:kimi".to_string(),
                    "moonshotai/kimi-k3".to_string(),
                )]),
                strip_prefix: "nano:".to_string(),
                auth: AuthConfig {
                    mode: AUTH_BEARER.to_string(),
                    token: "{env:TEST_NANO_STRIP_KEY}".to_string(),
                    ..Default::default()
                },
                ..Default::default()
            },
        );
        unsafe { std::env::set_var("TEST_NANO_STRIP_KEY", "nano-secret") };
        let metrics = Arc::new(Metrics::new());
        let router = build_router(&mut cfg, "", metrics.clone());
        let addr = spawn_handler(handler(router, metrics, true)).await;
        let client = test_client();

        let cases = [
            (
                "/v1/messages",
                "nano:deepseek/deepseek-v4.1-flash:thinking",
                "/api/v1/messages",
                "deepseek/deepseek-v4.1-flash:thinking",
            ),
            (
                "/v1/messages/count_tokens",
                "nano:z-ai/glm-5.3",
                "/api/v1/messages/count_tokens",
                "z-ai/glm-5.3",
            ),
            (
                "/v1/messages",
                "nano:kimi",
                "/api/v1/messages",
                "moonshotai/kimi-k3",
            ),
            (
                "/v1/messages",
                "claude-opus-5",
                "/v1/messages",
                "claude-opus-5",
            ),
        ];
        for (path, model, want_path, want_model) in cases {
            post(
                &client,
                addr,
                path,
                &format!(r#"{{"model":"{model}"}}"#),
                &[],
            )
            .await;
            let c = got.recv().await.unwrap();
            assert_eq!(c.path, want_path, "{path} {model}");
            assert_eq!(c.body["model"], want_model, "{path} {model}");
        }
    }

    #[tokio::test]
    async fn upstream_error_passthrough() {
        unsafe { std::env::set_var("TEST_SYN_KEY", "syn-secret") };
        let err_body =
            r#"{"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#;
        let (ant, _) = spawn_upstream(529, err_body).await;
        let metrics = Arc::new(Metrics::new());
        let router = build_router(&mut two_route_config(ant, ant), "", metrics.clone());
        let addr = spawn_handler(handler(router, metrics, true)).await;
        let client = test_client();
        let (status, _, body) = post(
            &client,
            addr,
            "/v1/messages",
            r#"{"model":"claude-opus-5"}"#,
            &[],
        )
        .await;
        assert_eq!(status.as_u16(), 529);
        assert_eq!(body, Bytes::from_static(err_body.as_bytes()));
    }

    #[tokio::test]
    async fn client_token() {
        unsafe { std::env::set_var("TEST_SYN_KEY", "syn-secret") };
        let (ant, mut got) = spawn_upstream(200, "{}").await;
        let metrics = Arc::new(Metrics::new());
        let router = build_router(&mut two_route_config(ant, ant), "s3cret", metrics.clone());
        let addr = spawn_handler(handler(router, metrics.clone(), true)).await;
        let client = test_client();

        for token in ["", "s3cret-not", "s3cre"] {
            let headers: Vec<(&str, &str)> = if token.is_empty() {
                vec![]
            } else {
                vec![(CLIENT_TOKEN_HEADER, token)]
            };
            // No response is written: the client only sees the connection close.
            let result = request(
                &client,
                addr,
                "POST",
                "/v1/messages",
                r#"{"model":"claude-opus-5"}"#,
                &headers,
            )
            .await;
            assert!(result.is_err(), "{token:?} token got a response");
        }
        assert_eq!(metrics.rejected.get(), 3.0);
        assert!(
            got.try_recv().is_err(),
            "unauthenticated request reached the upstream"
        );

        let (status, _, _) = post(
            &client,
            addr,
            "/v1/messages",
            r#"{"model":"claude-opus-5"}"#,
            &[(CLIENT_TOKEN_HEADER, "s3cret")],
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let c = got.recv().await.unwrap();
        assert!(c.headers.get(CLIENT_TOKEN_HEADER).is_none());
    }

    struct ChanBody(mpsc::UnboundedReceiver<Bytes>);

    impl Body for ChanBody {
        type Data = Bytes;
        type Error = std::convert::Infallible;

        fn poll_frame(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
        ) -> Poll<Option<Result<Frame<Bytes>, Self::Error>>> {
            match self.0.poll_recv(cx) {
                Poll::Ready(Some(b)) => Poll::Ready(Some(Ok(Frame::data(b)))),
                Poll::Ready(None) => Poll::Ready(None),
                Poll::Pending => Poll::Pending,
            }
        }
    }

    #[tokio::test]
    async fn streaming_is_not_buffered() {
        unsafe { std::env::set_var("TEST_SYN_KEY", "syn-secret") };
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let up = listener.local_addr().unwrap();
        let (release_tx, release_rx) = oneshot::channel::<()>();
        let release_rx = Arc::new(parking_lot::Mutex::new(Some(release_rx)));
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let service = service_fn(move |_req: Request<Incoming>| {
                let (tx, rx) = mpsc::unbounded_channel();
                let _ = tx.send(Bytes::from_static(b"event: ping\ndata: {}\n\n"));
                let tx2 = tx.clone();
                let release = release_rx.lock().take();
                if let Some(release) = release {
                    tokio::spawn(async move {
                        let _ = release.await;
                        let _ = tx2.send(Bytes::from_static(b"event: message_stop\ndata: {}\n\n"));
                        drop(tx2);
                    });
                }
                drop(tx);
                async move {
                    Ok::<_, std::convert::Infallible>(
                        Response::builder()
                            .header(CONTENT_TYPE, "text/event-stream")
                            .body(ChanBody(rx))
                            .unwrap(),
                    )
                }
            });
            let _ = http1::Builder::new()
                .serve_connection(TokioIo::new(stream), service)
                .await;
        });

        let metrics = Arc::new(Metrics::new());
        let router = build_router(&mut two_route_config(up, up), "", metrics.clone());
        let addr = spawn_handler(handler(router, metrics, true)).await;
        let client = test_client();

        let req = Request::builder()
            .method("POST")
            .uri(format!("http://{addr}/v1/messages"))
            .body(Full::new(Bytes::from_static(
                br#"{"model":"claude-opus-5","stream":true}"#,
            )))
            .unwrap();
        let resp = client.request(req).await.unwrap();
        let mut body = resp.into_body();
        let frame = tokio::time::timeout(Duration::from_secs(2), body.frame())
            .await
            .expect("first SSE event was buffered")
            .unwrap()
            .unwrap()
            .into_data()
            .unwrap();
        assert_eq!(frame, Bytes::from_static(b"event: ping\ndata: {}\n\n"));
        let _ = release_tx.send(());
        drop(body);
    }

    #[tokio::test]
    async fn disabled_route() {
        unsafe {
            std::env::set_var("TEST_SYN_KEY", "syn-secret");
            std::env::set_var("TEST_NANO_KEY", "");
        }
        let (up, mut got) = spawn_upstream(200, "{}").await;
        let mut cfg = two_route_config(up, up);
        cfg.routes.insert(
            0,
            RouteConfig {
                name: "nanogpt".to_string(),
                upstream: format!("http://{up}"),
                models: vec!["nano:*".to_string()],
                auth: AuthConfig {
                    mode: AUTH_BEARER.to_string(),
                    token: "{env:TEST_NANO_KEY}".to_string(),
                    ..Default::default()
                },
                ..Default::default()
            },
        );
        let metrics = Arc::new(Metrics::new());
        let router = build_router(&mut cfg, "", metrics.clone());
        let addr = spawn_handler(handler(router, metrics, true)).await;
        let client = test_client();

        let (status, headers, body) = post(
            &client,
            addr,
            "/v1/messages",
            r#"{"model":"nano:z-ai/glm-5.3-flash"}"#,
            &[],
        )
        .await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(headers.get("x-should-retry").unwrap(), "false");
        assert!(
            String::from_utf8_lossy(&body)
                .contains(r#"route \"nanogpt\" is disabled: auth.token is empty"#),
            "{body:?}"
        );
        assert!(got.try_recv().is_err(), "disabled route reached upstream");

        post(
            &client,
            addr,
            "/v1/messages",
            r#"{"model":"claude-opus-5"}"#,
            &[],
        )
        .await;
        let c = got.recv().await.unwrap();
        assert_eq!(c.body["model"], "claude-opus-5");
    }

    #[tokio::test]
    async fn admin_listener_separation() {
        unsafe { std::env::set_var("TEST_SYN_KEY", "syn-secret") };
        let metrics = Arc::new(Metrics::new());
        let cfg = two_route_config(
            "127.0.0.1:1".parse().unwrap(),
            "127.0.0.1:1".parse().unwrap(),
        );
        let mut cfg = cfg;
        let router = build_router(&mut cfg, "s3cret", metrics.clone());
        let client = test_client();

        let admin = Handler {
            router: Arc::new(RwLock::new(router.clone())),
            metrics: metrics.clone(),
            ready: Arc::new(AtomicBool::new(true)),
            probes: true,
            proxy_enabled: false,
        };
        let api = Handler {
            router: Arc::new(RwLock::new(router.clone())),
            metrics: metrics.clone(),
            ready: Arc::new(AtomicBool::new(true)),
            probes: false,
            proxy_enabled: true,
        };
        let admin_addr = spawn_handler(admin).await;
        let api_addr = spawn_handler(api.clone()).await;
        for path in ["/livez", "/readyz", "/metrics"] {
            let (status, _, _) = request(&client, admin_addr, "GET", path, "", &[])
                .await
                .expect("admin request");
            assert_eq!(status, StatusCode::OK, "admin {path}");
            let result = request(&client, api_addr, "GET", path, "", &[]).await;
            assert!(result.is_err(), "api {path} without token answered");
        }
        let combined_addr = spawn_handler(Handler {
            probes: true,
            ..api
        })
        .await;
        let (status, _, _) = request(&client, combined_addr, "GET", "/livez", "", &[])
            .await
            .expect("combined livez");
        assert_eq!(status, StatusCode::OK);
    }

    #[tokio::test]
    async fn metrics_end_to_end() {
        unsafe { std::env::set_var("TEST_SYN_KEY", "syn-secret") };
        const SSE_BODY: &str = concat!(
            "event: message_start\n",
            "data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":1200,\"cache_read_input_tokens\":30000,\"cache_creation_input_tokens\":500,\"output_tokens\":1}}}\n\n",
            "event: content_block_delta\n",
            "data: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"hi\"}}\n\n",
            "event: message_delta\n",
            "data: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":42}}\n\n",
        );
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let up = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let service = service_fn(move |_req: Request<Incoming>| async move {
                Ok::<_, std::convert::Infallible>(
                    Response::builder()
                        .header(CONTENT_TYPE, "text/event-stream")
                        .body(Full::new(Bytes::from_static(SSE_BODY.as_bytes())))
                        .unwrap(),
                )
            });
            let _ = http1::Builder::new()
                .serve_connection(TokioIo::new(stream), service)
                .await;
        });

        let metrics = Arc::new(Metrics::new());
        let router = build_router(&mut two_route_config(up, up), "", metrics.clone());
        let ready = Arc::new(AtomicBool::new(false));
        let addr = spawn_handler(Handler {
            router: Arc::new(RwLock::new(router)),
            metrics: metrics.clone(),
            ready: ready.clone(),
            probes: true,
            proxy_enabled: true,
        })
        .await;
        let client = test_client();

        post(
            &client,
            addr,
            "/v1/messages",
            r#"{"model":"hf:moonshotai/Kimi-K3","stream":true}"#,
            &[],
        )
        .await;
        assert_eq!(
            metrics
                .requests
                .with_label_values(&["200", "hf:moonshotai/Kimi-K3", "synthetic"])
                .get(),
            1.0
        );
        assert_eq!(
            metrics
                .tokens
                .with_label_values(&["hf:moonshotai/Kimi-K3", "synthetic", "cache_read"])
                .get(),
            30000.0
        );
        assert_eq!(
            metrics
                .inflight
                .with_label_values(&["hf:moonshotai/Kimi-K3", "synthetic"])
                .get(),
            0
        );

        let (status, _, _) = request(&client, addr, "GET", "/livez", "", &[])
            .await
            .unwrap();
        assert_eq!(status, StatusCode::OK);
        let (status, _, _) = request(&client, addr, "GET", "/readyz", "", &[])
            .await
            .unwrap();
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        ready.store(true, std::sync::atomic::Ordering::Relaxed);
        let (status, _, _) = request(&client, addr, "GET", "/readyz", "", &[])
            .await
            .unwrap();
        assert_eq!(status, StatusCode::OK);
        let (status, _, body) = request(&client, addr, "GET", "/metrics", "", &[])
            .await
            .unwrap();
        assert_eq!(status, StatusCode::OK);
        let text = String::from_utf8_lossy(&body);
        assert!(
            text.contains(
                r#"llm_router_tokens_total{model="hf:moonshotai/Kimi-K3",route="synthetic",type="output"} 42"#
            ),
            "{text}"
        );
    }

    #[tokio::test]
    async fn quota_poll() {
        unsafe { std::env::set_var("TEST_SYN_KEY", "syn-secret") };
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let quota = listener.local_addr().unwrap();
        let auth = Arc::new(parking_lot::Mutex::new(String::new()));
        let auth_seen = auth.clone();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let service = service_fn(move |req: Request<Incoming>| {
                let auth_seen = auth_seen.clone();
                async move {
                    *auth_seen.lock() = req
                        .headers()
                        .get(AUTHORIZATION)
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or_default()
                        .to_string();
                    Ok::<_, std::convert::Infallible>(
                        Response::builder().body(Full::new(Bytes::from_static(
                            br#"{"subscription":{"limit":500,"requests":12.5,"renewsAt":"2026-09-21T14:36:14.288Z","active":true,"plan":"pack"}}"#,
                        )))
                        .unwrap(),
                    )
                }
            });
            let _ = http1::Builder::new()
                .serve_connection(TokioIo::new(stream), service)
                .await;
        });

        let mut cfg = two_route_config(quota, quota);
        cfg.routes[0].quota = Some(crate::config::QuotaConfig {
            url: format!("http://{quota}/v2/quotas"),
            interval: Duration::ZERO,
        });
        let metrics = Arc::new(Metrics::new());
        let router = build_router(&mut cfg, "", metrics.clone());
        scrape_quota(&proxy_client(), &router.routes[0], &metrics).await;

        assert_eq!(*auth.lock(), "Bearer syn-secret");
        let checks = [
            ("subscription.limit", 500.0),
            ("subscription.requests", 12.5),
            ("subscription.renewsAt", 1790001374.0),
            ("subscription.active", 1.0),
        ];
        for (key, want) in checks {
            assert_eq!(
                metrics.quota.with_label_values(&[key, "synthetic"]).get(),
                want,
                "{key}"
            );
        }
    }

    #[tokio::test]
    async fn set_fields_apply_and_keep_key_order() {
        let (upstream, mut rx) = spawn_upstream(200, "{}").await;
        let metrics = Arc::new(Metrics::new());
        let mut cfg = local_config(upstream, SanitizeConfig::default());
        cfg.routes[0].set_fields = BTreeMap::from([
            ("max_tokens".to_string(), "100".to_string()),
            ("system_note".to_string(), "hello".to_string()),
            ("temperature".to_string(), "1".to_string()),
        ]);
        let router = build_router(&mut cfg, "", metrics.clone());
        let addr = spawn_handler(handler(router, metrics, true)).await;
        let client = test_client();

        let body = r#"{"model":"claude-x","temperature":0,"system":"s"}"#;
        post(&client, addr, "/v1/messages", body, &[]).await;
        let c = rx.recv().await.expect("upstream request");
        assert_eq!(
            c.raw.as_ref(),
            br#"{"model":"claude-x","temperature":1,"system":"s","max_tokens":100,"system_note":"hello"}"#
        );
        assert_eq!(c.body["temperature"], 1);
        assert_eq!(c.body["system_note"], "hello");
    }

    #[tokio::test]
    async fn sanitize_tool_use_ids_rewrites_history() {
        let (upstream, mut rx) = spawn_upstream(200, "{}").await;
        let metrics = Arc::new(Metrics::new());
        let mut cfg = local_config(
            upstream,
            SanitizeConfig {
                sanitize_tool_use_ids: true,
                ..Default::default()
            },
        );
        let router = build_router(&mut cfg, "", metrics.clone());
        let addr = spawn_handler(handler(router, metrics, true)).await;
        let client = test_client();

        let body = r#"{"model":"claude-x","messages":[{"role":"assistant","content":[{"type":"tool_use","id":"functions.Bash:0","name":"Bash","input":{}}]},{"role":"user","content":[{"type":"tool_result","tool_use_id":"functions.Bash:0","content":"ok"}]}]}"#;
        post(&client, addr, "/v1/messages", body, &[]).await;
        let c = rx.recv().await.expect("upstream request");
        let expected = crate::ids::sanitize_tool_use_id("functions.Bash:0").unwrap();
        assert_eq!(c.body["messages"][0]["content"][0]["id"], expected);
        assert_eq!(c.body["messages"][1]["content"][0]["tool_use_id"], expected);
        assert!(expected.len() <= 64);
    }

    #[tokio::test]
    async fn hoist_inline_tools_moves_definitions_into_tools() {
        let (upstream, mut rx) = spawn_upstream(200, "{}").await;
        let metrics = Arc::new(Metrics::new());
        let mut cfg = local_config(
            upstream,
            SanitizeConfig {
                hoist_inline_tools: true,
                ..Default::default()
            },
        );
        cfg.routes[0].drop_tool_fields = vec!["defer_loading".to_string()];
        let router = build_router(&mut cfg, "", metrics.clone());
        let addr = spawn_handler(handler(router, metrics, true)).await;
        let client = test_client();

        let body = r#"{"model":"claude-x","tools":[{"name":"Bash","input_schema":{"type":"object"}}],"messages":[{"role":"user","content":"hi"},{"role":"system","content":[{"type":"tool_addition","tool":{"type":"tool_definition","definition":{"name":"db_query","defer_loading":true,"input_schema":{"type":"object"}}}},{"type":"tool_addition","tool":{"type":"tool_definition","definition":{"name":"Bash","input_schema":{"type":"object"}}}},{"type":"tool_removal","tool":{"type":"tool_reference","name":"Bash"}}]}]}"#;
        post(&client, addr, "/v1/messages", body, &[]).await;
        let c = rx.recv().await.expect("upstream request");
        assert_eq!(c.body["messages"].as_array().unwrap().len(), 1);
        assert_eq!(c.body["messages"][0]["content"], "hi");
        let tools = c.body["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 2);
        assert_eq!(tools[1]["name"], "db_query");
        assert!(tools[1].get("defer_loading").is_none());
    }

    #[tokio::test]
    async fn routes_through_connect_proxy() {
        let (upstream, mut rx) = spawn_upstream(200, r#"{"ok":true}"#).await;
        let proxy = spawn_connect_proxy().await;
        let metrics = Arc::new(Metrics::new());
        let mut cfg = local_config(upstream, SanitizeConfig::default());
        cfg.resolve_credentials(std::path::Path::new("."))
            .expect("resolve credentials");
        cfg.validate().expect("valid config");
        let router = Arc::new(
            Router::new(&cfg, "", client_via_proxy(proxy), metrics.clone()).expect("router"),
        );
        let addr = spawn_handler(handler(router, metrics, true)).await;
        let client = test_client();

        let (status, _, body) = post(
            &client,
            addr,
            "/v1/messages",
            r#"{"model":"claude-x"}"#,
            &[],
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body.as_ref(), br#"{"ok":true}"#);
        let c = rx.recv().await.expect("upstream request");
        assert_eq!(c.body["model"], "claude-x");
    }

    #[tokio::test]
    async fn routes_through_authenticated_connect_proxy() {
        let (upstream, mut rx) = spawn_upstream(200, r#"{"ok":true}"#).await;
        let proxy = spawn_connect_proxy().await;
        let connector =
            Connector::new(Some(&format!("http://us%65r:p%40ss@{proxy}"))).expect("connector");
        let https = hyper_rustls::HttpsConnectorBuilder::new()
            .with_native_roots()
            .unwrap()
            .https_or_http()
            .enable_http1()
            .wrap_connector(connector);
        let metrics = Arc::new(Metrics::new());
        let mut cfg = local_config(upstream, SanitizeConfig::default());
        cfg.resolve_credentials(std::path::Path::new("."))
            .expect("resolve credentials");
        cfg.validate().expect("valid config");
        let router = Arc::new(
            Router::new(
                &cfg,
                "",
                Client::builder(TokioExecutor::new()).build(https),
                metrics.clone(),
            )
            .expect("router"),
        );
        let addr = spawn_handler(handler(router, metrics, true)).await;
        let (status, _, _) = post(
            &test_client(),
            addr,
            "/v1/messages",
            r#"{"model":"claude-x"}"#,
            &[],
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert!(rx.recv().await.is_some(), "upstream not reached");
    }

    #[tokio::test]
    async fn connect_proxy_failure_is_bad_gateway() {
        let connector = Connector::new(Some("http://127.0.0.1:1")).expect("connector");
        let https = hyper_rustls::HttpsConnectorBuilder::new()
            .with_native_roots()
            .unwrap()
            .https_or_http()
            .enable_http1()
            .wrap_connector(connector);
        let metrics = Arc::new(Metrics::new());
        let mut cfg = local_config("127.0.0.1:1".parse().unwrap(), SanitizeConfig::default());
        cfg.resolve_credentials(std::path::Path::new("."))
            .expect("resolve credentials");
        cfg.validate().expect("valid config");
        let router = Arc::new(
            Router::new(
                &cfg,
                "",
                Client::builder(TokioExecutor::new()).build(https),
                metrics.clone(),
            )
            .expect("router"),
        );
        let addr = spawn_handler(handler(router, metrics, true)).await;
        let (status, _, body) = post(
            &test_client(),
            addr,
            "/v1/messages",
            r#"{"model":"claude-x"}"#,
            &[],
        )
        .await;
        assert_eq!(status, StatusCode::BAD_GATEWAY);
        assert!(
            String::from_utf8_lossy(&body).contains("upstream"),
            "{body:?}"
        );
    }

    #[tokio::test]
    async fn rejects_malformed_messages_requests() {
        let (up, _rx) = spawn_upstream(200, "{}").await;
        let metrics = Arc::new(Metrics::new());
        let mut cfg = local_config(up, SanitizeConfig::default());
        cfg.max_body_bytes = 16;
        let router = build_router(&mut cfg, "", metrics.clone());
        let addr = spawn_handler(handler(router, metrics, true)).await;
        let client = test_client();

        let (status, _, body) = post(
            &client,
            addr,
            "/v1/messages",
            r#"{"model":"claude-a-longer-model"}"#,
            &[],
        )
        .await;
        assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
        assert!(String::from_utf8_lossy(&body).contains("request body too large"));

        let (status, _, body) = post(&client, addr, "/v1/messages", "{", &[]).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(String::from_utf8_lossy(&body).contains("invalid JSON body"));

        let (status, _, body) = post(&client, addr, "/v1/messages", "[1]", &[]).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(String::from_utf8_lossy(&body).contains("model must be a string"));

        let (status, _, body) = post(&client, addr, "/v1/messages", r#"{"model":1}"#, &[]).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(String::from_utf8_lossy(&body).contains("model must be a string"));
    }

    #[tokio::test]
    async fn upstream_unreachable_returns_bad_gateway() {
        let metrics = Arc::new(Metrics::new());
        let mut cfg = local_config("127.0.0.1:1".parse().unwrap(), SanitizeConfig::default());
        let router = build_router(&mut cfg, "", metrics.clone());
        let addr = spawn_handler(handler(router, metrics.clone(), true)).await;
        let (status, _, body) = post(
            &test_client(),
            addr,
            "/v1/messages",
            r#"{"model":"claude-x"}"#,
            &[],
        )
        .await;
        assert_eq!(status, StatusCode::BAD_GATEWAY);
        assert!(String::from_utf8_lossy(&body).contains("upstream"));
        assert_eq!(
            metrics.upstream_errors.with_label_values(&["local"]).get(),
            1.0
        );
    }

    #[tokio::test]
    async fn x_api_key_auth_replaces_authorization() {
        unsafe { std::env::set_var("TEST_X_API_KEY", "x-secret") };
        let (up, mut rx) = spawn_upstream(200, "{}").await;
        let metrics = Arc::new(Metrics::new());
        let mut cfg = local_config(up, SanitizeConfig::default());
        cfg.routes[0].auth = AuthConfig {
            mode: crate::config::AUTH_X_API_KEY.to_string(),
            token: "{env:TEST_X_API_KEY}".to_string(),
            ..Default::default()
        };
        let router = build_router(&mut cfg, "", metrics.clone());
        let addr = spawn_handler(handler(router, metrics, true)).await;
        let status = post(
            &test_client(),
            addr,
            "/v1/messages",
            r#"{"model":"claude-x"}"#,
            &[("authorization", "Bearer client")],
        )
        .await
        .0;
        assert_eq!(status, StatusCode::OK);
        let c = rx.recv().await.expect("upstream request");
        assert_eq!(c.headers.get("x-api-key").unwrap(), "x-secret");
        assert!(c.headers.get(AUTHORIZATION).is_none());
    }

    #[tokio::test]
    async fn invalid_set_headers_are_skipped() {
        let (up, mut rx) = spawn_upstream(200, "{}").await;
        let metrics = Arc::new(Metrics::new());
        let mut cfg = local_config(up, SanitizeConfig::default());
        cfg.routes[0].set_headers = BTreeMap::from([
            ("bad name\n".to_string(), "v".to_string()),
            ("x-bad-value".to_string(), "bad\nvalue".to_string()),
            ("x-good".to_string(), "1".to_string()),
        ]);
        let router = build_router(&mut cfg, "", metrics.clone());
        let addr = spawn_handler(handler(router, metrics, true)).await;
        post(
            &test_client(),
            addr,
            "/v1/messages",
            r#"{"model":"claude-x"}"#,
            &[],
        )
        .await;
        let c = rx.recv().await.expect("upstream request");
        assert_eq!(c.headers.get("x-good").unwrap(), "1");
        assert!(c.headers.get("x-bad-value").is_none());
    }

    #[test]
    fn trim_ascii_strips_both_ends() {
        assert_eq!(trim_ascii(b" \r\n hi \t"), b"hi");
        assert_eq!(trim_ascii(b"hi"), b"hi");
        assert_eq!(trim_ascii(b"   "), b"");
    }

    #[test]
    fn hop_by_hop_removes_connection_tokens() {
        let mut headers = HeaderMap::new();
        headers.insert("connection", HeaderValue::from_static("x-hop, keep-me"));
        headers.insert("x-hop", HeaderValue::from_static("1"));
        headers.insert("keep-alive", HeaderValue::from_static("timeout=5"));
        headers.insert("x-keep", HeaderValue::from_static("1"));
        remove_hop_by_hop(&mut headers);
        assert!(headers.get("x-hop").is_none());
        assert!(headers.get("keep-alive").is_none());
        assert!(headers.get("connection").is_none());
        assert_eq!(headers.get("x-keep").unwrap(), "1");
    }

    #[tokio::test]
    async fn quota_scrape_error_status_is_recorded() {
        unsafe { std::env::set_var("TEST_QUOTA_X_KEY", "quota-secret") };
        let (up, mut got) = spawn_upstream(500, " boom ").await;
        let mut cfg = two_route_config(up, up);
        cfg.routes[0].auth = AuthConfig {
            mode: crate::config::AUTH_X_API_KEY.to_string(),
            token: "{env:TEST_QUOTA_X_KEY}".to_string(),
            ..Default::default()
        };
        cfg.routes[0].quota = Some(crate::config::QuotaConfig {
            url: format!("http://{up}/quota"),
            interval: Duration::ZERO,
        });
        let metrics = Arc::new(Metrics::new());
        let router = build_router(&mut cfg, "", metrics.clone());
        scrape_quota(&proxy_client(), &router.routes[0], &metrics).await;

        let c = got.recv().await.expect("quota request");
        assert_eq!(c.headers.get("x-api-key").unwrap(), "quota-secret");
        assert!(c.headers.get(AUTHORIZATION).is_none());
        assert_eq!(
            metrics
                .quota_scrapes
                .with_label_values(&["error", "synthetic"])
                .get(),
            1.0
        );
    }

    #[tokio::test]
    async fn quota_scrape_invalid_json_is_recorded() {
        unsafe { std::env::set_var("TEST_SYN_KEY", "syn-secret") };
        let (up, _got) = spawn_upstream(200, "not json").await;
        let mut cfg = two_route_config(up, up);
        cfg.routes[0].quota = Some(crate::config::QuotaConfig {
            url: format!("http://{up}/quota"),
            interval: Duration::ZERO,
        });
        let metrics = Arc::new(Metrics::new());
        let router = build_router(&mut cfg, "", metrics.clone());
        scrape_quota(&proxy_client(), &router.routes[0], &metrics).await;
        assert_eq!(
            metrics
                .quota_scrapes
                .with_label_values(&["error", "synthetic"])
                .get(),
            1.0
        );
    }

    #[test]
    fn upstream_uri_combines_queries_and_rejects_bad_urls() {
        let up = Upstream::parse("http://h:8080/base?k=v").expect("parse");
        let uri = up.uri(&"/v1/messages?x=1".parse().unwrap()).expect("uri");
        assert_eq!(uri.to_string(), "http://h:8080/base/v1/messages?k=v&x=1");
        let uri = up.uri(&"/v1/messages".parse().unwrap()).expect("uri");
        assert_eq!(uri.to_string(), "http://h:8080/base/v1/messages?k=v");
        let plain = Upstream::parse("http://h/base").expect("parse");
        let uri = plain.uri(&"/x?y".parse().unwrap()).expect("uri");
        assert_eq!(uri.to_string(), "http://h/base/x?y");
        assert_eq!(single_joining_slash("a", "b"), "a/b");
        assert!(Upstream::parse("not a url").is_err());
    }

    #[test]
    fn router_rejects_invalid_upstream() {
        let cfg = Config {
            default_route: "a".to_string(),
            routes: vec![RouteConfig {
                name: "a".to_string(),
                upstream: "not a url".to_string(),
                ..Default::default()
            }],
            ..Default::default()
        };
        let err = Router::new(&cfg, "", proxy_client(), Arc::new(Metrics::new()))
            .err()
            .expect("invalid upstream");
        assert!(err.contains("route \"a\""), "{err}");
    }

    #[test]
    fn abort_displays_a_message() {
        assert_eq!(Abort.to_string(), "connection aborted");
    }

    #[tokio::test]
    async fn missing_model_uses_fallback_route() {
        let (up, mut rx) = spawn_upstream(200, "{}").await;
        let metrics = Arc::new(Metrics::new());
        let mut cfg = local_config(up, SanitizeConfig::default());
        let router = build_router(&mut cfg, "", metrics.clone());
        let addr = spawn_handler(handler(router, metrics, true)).await;
        let (status, _, _) = post(
            &test_client(),
            addr,
            "/v1/messages",
            r#"{"messages":[]}"#,
            &[],
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let c = rx.recv().await.expect("upstream request");
        assert!(c.body.get("model").is_none());
    }

    #[tokio::test]
    async fn quota_with_passthrough_auth_sends_no_credentials() {
        let (up, mut got) = spawn_upstream(200, "{}").await;
        let mut cfg = two_route_config(up, up);
        cfg.routes[0].auth = AuthConfig {
            mode: crate::config::AUTH_PASSTHROUGH.to_string(),
            ..Default::default()
        };
        cfg.routes[0].quota = Some(crate::config::QuotaConfig {
            url: format!("http://{up}/quota"),
            interval: Duration::ZERO,
        });
        cfg.resolve_credentials(std::path::Path::new("."))
            .expect("resolve credentials");
        let metrics = Arc::new(Metrics::new());
        let router =
            Arc::new(Router::new(&cfg, "", proxy_client(), metrics.clone()).expect("router"));
        scrape_quota(&proxy_client(), &router.routes[0], &metrics).await;
        let c = got.recv().await.expect("quota request");
        assert!(c.headers.get(AUTHORIZATION).is_none());
        assert!(c.headers.get("x-api-key").is_none());
    }

    #[tokio::test]
    async fn incomplete_body_is_a_bad_request() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let (up, _rx) = spawn_upstream(200, "{}").await;
        let metrics = Arc::new(Metrics::new());
        let mut cfg = local_config(up, SanitizeConfig::default());
        let router = build_router(&mut cfg, "", metrics.clone());
        let addr = spawn_handler(handler(router, metrics, true)).await;

        let mut sock = tokio::net::TcpStream::connect(addr).await.expect("connect");
        sock.write_all(b"POST /v1/messages HTTP/1.1\r\nHost: x\r\nContent-Length: 100\r\n\r\n{")
            .await
            .expect("write");
        sock.shutdown().await.expect("shutdown");
        let mut resp = Vec::new();
        sock.read_to_end(&mut resp).await.expect("read");
        let text = String::from_utf8_lossy(&resp);
        assert!(text.starts_with("HTTP/1.1 400"), "{text}");
        assert!(text.contains("read body"), "{text}");
    }
}
