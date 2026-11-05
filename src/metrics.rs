use parking_lot::Mutex;
use std::collections::HashMap;

use chrono::DateTime;
use prometheus::{
    Counter, CounterVec, Encoder, GaugeVec, HistogramOpts, HistogramVec, IntGaugeVec, Opts,
    Registry, TextEncoder, exponential_buckets,
};
use serde::Deserialize;
use serde_json::Value;

const MAX_JSON_USAGE_BODY: usize = 4 << 20;

pub struct Metrics {
    pub registry: Registry,
    pub requests: CounterVec,
    pub duration: HistogramVec,
    pub ttfb: HistogramVec,
    pub inflight: IntGaugeVec,
    pub tokens: CounterVec,
    pub upstream_errors: CounterVec,
    pub quota: GaugeVec,
    pub quota_scrapes: CounterVec,
    pub rejected: Counter,
    /// Label values of currently exported quota gauges, per route.
    quota_keys: Mutex<HashMap<String, Vec<String>>>,
}

impl Metrics {
    pub fn new() -> Self {
        let registry = Registry::new();
        let metric = |name: &str, help: &str| Opts::new(name, help);
        let m = Metrics {
            requests: CounterVec::new(
                metric(
                    "llm_router_requests_total",
                    "Proxied requests by route, model and upstream status code.",
                ),
                &["code", "model", "route"],
            )
            .unwrap(),
            duration: HistogramVec::new(
                HistogramOpts::new(
                    "llm_router_request_duration_seconds",
                    "Full request duration including the streamed response body.",
                )
                .buckets(exponential_buckets(0.25, 2.0, 13).unwrap()),
                &["model", "route"],
            )
            .unwrap(),
            ttfb: HistogramVec::new(
                HistogramOpts::new(
                    "llm_router_time_to_first_byte_seconds",
                    "Time until upstream response headers; includes upstream queueing.",
                )
                .buckets(exponential_buckets(0.05, 2.0, 14).unwrap()),
                &["model", "route"],
            )
            .unwrap(),
            inflight: IntGaugeVec::new(
                metric(
                    "llm_router_inflight_requests",
                    "Requests currently being proxied.",
                ),
                &["model", "route"],
            )
            .unwrap(),
            tokens: CounterVec::new(
                metric(
                    "llm_router_tokens_total",
                    "Tokens reported in upstream usage blocks (type: input, output, cache_read, cache_creation).",
                ),
                &["model", "route", "type"],
            )
            .unwrap(),
            upstream_errors: CounterVec::new(
                metric(
                    "llm_router_upstream_errors_total",
                    "Transport-level failures reaching the upstream.",
                ),
                &["route"],
            )
            .unwrap(),
            quota: GaugeVec::new(
                metric(
                    "llm_router_upstream_quota",
                    "Numeric leaves of the upstream quota endpoint response (timestamps as unix seconds).",
                ),
                &["key", "route"],
            )
            .unwrap(),
            quota_scrapes: CounterVec::new(
                metric(
                    "llm_router_quota_scrapes_total",
                    "Quota endpoint polls by result.",
                ),
                &["result", "route"],
            )
            .unwrap(),
            rejected: Counter::new(
                "llm_router_rejected_requests_total",
                "Requests dropped without a response for a missing or wrong client token.",
            )
            .unwrap(),
            quota_keys: Mutex::new(HashMap::new()),
            registry,
        };
        m.registry.register(Box::new(m.requests.clone())).unwrap();
        m.registry.register(Box::new(m.duration.clone())).unwrap();
        m.registry.register(Box::new(m.ttfb.clone())).unwrap();
        m.registry.register(Box::new(m.inflight.clone())).unwrap();
        m.registry.register(Box::new(m.tokens.clone())).unwrap();
        m.registry
            .register(Box::new(m.upstream_errors.clone()))
            .unwrap();
        m.registry.register(Box::new(m.quota.clone())).unwrap();
        m.registry
            .register(Box::new(m.quota_scrapes.clone()))
            .unwrap();
        m.registry.register(Box::new(m.rejected.clone())).unwrap();
        #[cfg(target_os = "linux")]
        m.registry
            .register(Box::new(
                prometheus::process_collector::ProcessCollector::for_self(),
            ))
            .unwrap();
        m
    }

    pub fn encode(&self) -> (String, Vec<u8>) {
        let encoder = TextEncoder::new();
        let mut buf = Vec::new();
        encoder.encode(&self.registry.gather(), &mut buf).unwrap();
        (encoder.format_type().to_string(), buf)
    }

    pub fn observe_usage(&self, route: &str, model: &str, u: ApiUsage) {
        for (typ, n) in [
            ("input", u.input_tokens),
            ("output", u.output_tokens),
            ("cache_read", u.cache_read_tokens),
            ("cache_creation", u.cache_creation_tokens),
        ] {
            if n > 0 {
                self.tokens
                    .with_label_values(&[model, route, typ])
                    .inc_by(n as f64);
            }
        }
    }

    /// Replaces the quota gauges for one route with the freshly scraped values.
    pub fn set_quota(&self, route: &str, values: &HashMap<String, f64>) {
        let mut keys = self.quota_keys.lock();
        if let Some(previous) = keys.remove(route) {
            for key in previous {
                let _ = self.quota.remove_label_values(&[&key, route]);
            }
        }
        let mut current = Vec::with_capacity(values.len());
        for (key, value) in values {
            self.quota.with_label_values(&[key, route]).set(*value);
            current.push(key.clone());
        }
        keys.insert(route.to_string(), current);
    }
}

impl Default for Metrics {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ApiUsage {
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_read_tokens: i64,
    pub cache_creation_tokens: i64,
}

/// Tracks which fields an event actually carried.
#[derive(Deserialize, Default)]
struct RawUsage {
    input_tokens: Option<i64>,
    output_tokens: Option<i64>,
    cache_read_input_tokens: Option<i64>,
    cache_creation_input_tokens: Option<i64>,
}

#[derive(Deserialize, Default)]
struct UsageEvent {
    usage: Option<RawUsage>,
    message: Option<MessageUsage>,
}

#[derive(Deserialize, Default)]
struct MessageUsage {
    usage: Option<RawUsage>,
}

/// Extracts usage from a /v1/messages response without buffering streamed bodies.
/// SSE usage values are cumulative, so the latest event carrying a field wins.
pub struct UsageSniffer {
    sse: bool,
    buf: Vec<u8>,
    overflow: bool,
    usage: ApiUsage,
}

impl UsageSniffer {
    pub fn new(sse: bool) -> Self {
        Self {
            sse,
            buf: Vec::new(),
            overflow: false,
            usage: ApiUsage::default(),
        }
    }

    pub fn feed(&mut self, b: &[u8]) {
        if self.overflow {
            return;
        }
        self.buf.extend_from_slice(b);
        if !self.sse {
            if self.buf.len() > MAX_JSON_USAGE_BODY {
                self.overflow = true;
                self.buf.clear();
            }
            return;
        }
        let mut consumed = 0;
        while let Some(idx) = self.buf[consumed..].iter().position(|&c| c == b'\n') {
            let end = consumed + idx;
            let line = self.buf[consumed..end].to_vec();
            self.line(&line);
            consumed = end + 1;
        }
        self.buf.drain(..consumed);
        if self.buf.len() > MAX_JSON_USAGE_BODY {
            self.buf.clear();
        }
    }

    fn line(&mut self, l: &[u8]) {
        let l = l.strip_suffix(b"\r").unwrap_or(l);
        let Some(data) = l.strip_prefix(b"data:") else {
            return;
        };
        if !data.windows(7).any(|w| w == b"\"usage\"") {
            return;
        }
        self.parse(data);
    }

    fn parse(&mut self, data: &[u8]) {
        let Ok(ev) = serde_json::from_slice::<UsageEvent>(trim(data)) else {
            return;
        };
        if let Some(u) = ev.usage {
            self.merge(&u);
        }
        if let Some(u) = ev.message.and_then(|m| m.usage) {
            self.merge(&u);
        }
    }

    fn merge(&mut self, v: &RawUsage) {
        if let Some(x) = v.input_tokens {
            self.usage.input_tokens = x;
        }
        if let Some(x) = v.output_tokens {
            self.usage.output_tokens = x;
        }
        if let Some(x) = v.cache_read_input_tokens {
            self.usage.cache_read_tokens = x;
        }
        if let Some(x) = v.cache_creation_input_tokens {
            self.usage.cache_creation_tokens = x;
        }
    }

    pub fn finish(&mut self) -> ApiUsage {
        if !self.sse && !self.overflow && !self.buf.is_empty() {
            let buf = std::mem::take(&mut self.buf);
            self.parse(&buf);
        }
        self.usage
    }
}

fn trim(mut b: &[u8]) -> &[u8] {
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

/// Flattens the numeric leaves of a JSON value into dotted keys.
pub fn flatten_numeric(prefix: &str, v: &Value, out: &mut HashMap<String, f64>) {
    match v {
        Value::Object(map) => {
            for (k, child) in map {
                flatten_numeric(&join_key(prefix, k), child, out);
            }
        }
        Value::Array(items) => {
            for (i, child) in items.iter().enumerate() {
                flatten_numeric(&join_key(prefix, &i.to_string()), child, out);
            }
        }
        Value::Number(n) => {
            if let Some(f) = n.as_f64() {
                out.insert(prefix.to_string(), f);
            }
        }
        Value::Bool(b) => {
            out.insert(prefix.to_string(), if *b { 1.0 } else { 0.0 });
        }
        Value::String(s) => {
            if let Ok(ts) = DateTime::parse_from_rfc3339(s) {
                out.insert(prefix.to_string(), ts.timestamp() as f64);
            }
        }
        Value::Null => {}
    }
}

fn join_key(prefix: &str, k: &str) -> String {
    if prefix.is_empty() {
        k.to_string()
    } else {
        format!("{prefix}.{k}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SSE_BODY: &str = concat!(
        "event: message_start\n",
        "data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":1200,\"cache_read_input_tokens\":30000,\"cache_creation_input_tokens\":500,\"output_tokens\":1}}}\n\n",
        "event: content_block_delta\n",
        "data: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"mentions \\\"usage\\\" in text\"}}\n\n",
        "event: message_delta\n",
        "data: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":42}}\n\n",
    );

    #[test]
    fn sse_across_chunks() {
        let mut u = UsageSniffer::new(true);
        let bytes = SSE_BODY.as_bytes();
        let mut i = 0;
        while i < bytes.len() {
            let end = (i + 7).min(bytes.len());
            u.feed(&bytes[i..end]);
            i = end;
        }
        assert_eq!(
            u.finish(),
            ApiUsage {
                input_tokens: 1200,
                output_tokens: 42,
                cache_read_tokens: 30000,
                cache_creation_tokens: 500
            }
        );
    }

    #[test]
    fn latest_field_wins() {
        let body = concat!(
            "data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":126,\"output_tokens\":0,\"cache_creation_input_tokens\":0,\"cache_read_input_tokens\":0}}}\n",
            "data: {\"type\":\"message_delta\",\"usage\":{\"input_tokens\":19,\"output_tokens\":23,\"cache_creation_input_tokens\":19,\"cache_read_input_tokens\":108}}\n",
        );
        let mut u = UsageSniffer::new(true);
        u.feed(body.as_bytes());
        assert_eq!(
            u.finish(),
            ApiUsage {
                input_tokens: 19,
                output_tokens: 23,
                cache_read_tokens: 108,
                cache_creation_tokens: 19
            }
        );
    }

    #[test]
    fn json_body_split_across_chunks() {
        let mut u = UsageSniffer::new(false);
        u.feed(br#"{"type":"message","usage":{"input_tokens":12,"#);
        u.feed(br#""output_tokens":8}}"#);
        let got = u.finish();
        assert_eq!((got.input_tokens, got.output_tokens), (12, 8));
    }

    #[test]
    fn flatten() {
        let v: Value = serde_json::from_str(
            r#"{"subscription":{"limit":500,"requests":12.5,"renewsAt":"2026-09-21T14:36:14.288Z","active":true,"plan":"pack"}}"#,
        )
        .unwrap();
        let mut out = HashMap::new();
        flatten_numeric("", &v, &mut out);
        assert_eq!(out.get("subscription.limit"), Some(&500.0));
        assert_eq!(out.get("subscription.requests"), Some(&12.5));
        assert_eq!(out.get("subscription.renewsAt"), Some(&1790001374.0));
        assert_eq!(out.get("subscription.active"), Some(&1.0));
        assert!(!out.contains_key("subscription.plan"));
    }

    #[test]
    fn sse_ignores_lines_without_usage_and_strips_cr() {
        let mut u = UsageSniffer::new(true);
        u.feed(
            b"event: ping\r\ndata: no usage here\r\n\r\ndata: {\"usage\":{\"output_tokens\":3}}\r\n",
        );
        assert_eq!(u.finish().output_tokens, 3);
    }

    #[test]
    fn oversized_json_body_is_dropped() {
        let mut u = UsageSniffer::new(false);
        u.feed(&vec![b'x'; MAX_JSON_USAGE_BODY + 1]);
        u.feed(br#"{"usage":{"output_tokens":5}}"#);
        assert_eq!(u.finish(), ApiUsage::default());
    }

    #[test]
    fn oversized_sse_buffer_is_dropped() {
        let mut u = UsageSniffer::new(true);
        u.feed(&vec![b'x'; MAX_JSON_USAGE_BODY + 1]);
        u.feed(b"\ndata: {\"usage\":{\"output_tokens\":7}}\n");
        assert_eq!(u.finish().output_tokens, 7);
    }

    #[test]
    fn flatten_arrays_bools_and_non_metrics() {
        let v: Value =
            serde_json::from_str(r#"{"a":[1,{"b":false}],"c":"not-a-date","d":null}"#).unwrap();
        let mut out = HashMap::new();
        flatten_numeric("", &v, &mut out);
        assert_eq!(out.get("a.0"), Some(&1.0));
        assert_eq!(out.get("a.1.b"), Some(&0.0));
        assert!(!out.contains_key("c"));
        assert!(!out.contains_key("d"));
    }

    #[test]
    fn set_quota_replaces_previous_keys() {
        use prometheus::core::Collector;

        let metrics = Metrics::default();
        metrics.set_quota(
            "r",
            &HashMap::from([("a".to_string(), 1.0), ("b".to_string(), 2.0)]),
        );
        metrics.set_quota("r", &HashMap::from([("b".to_string(), 3.0)]));
        assert_eq!(metrics.quota.with_label_values(&["b", "r"]).get(), 3.0);
        let families = metrics.quota.collect();
        let series = families[0].get_metric();
        assert_eq!(series.len(), 1, "previous key must be removed");
        assert_eq!(series[0].get_label()[0].value(), "b");
    }

    #[test]
    fn invalid_usage_json_is_ignored() {
        let mut u = UsageSniffer::new(true);
        u.feed(b"data: \"usage\" not json\n");
        assert_eq!(u.finish(), ApiUsage::default());
    }

    #[test]
    fn trim_strips_trailing_whitespace() {
        assert_eq!(trim(b" {\"a\":1} \r\n"), b"{\"a\":1}");
        assert_eq!(trim(b"x"), b"x");
    }
}
