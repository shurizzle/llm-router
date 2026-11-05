mod config;
mod ids;
mod inline_tools;
mod log;
mod metrics;
mod network;
mod proxy;
mod raw;
mod schema;

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use clap::Parser;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper_util::client::legacy::Client;
use hyper_util::rt::{TokioExecutor, TokioIo, TokioTimer};
use parking_lot::RwLock;
use serde_json::Value;
use tokio::net::TcpListener;
use tokio::sync::watch;
use tokio::task::JoinSet;

use crate::config::{NetworkConfig, load_config, parse_duration};
use crate::metrics::Metrics;
use crate::proxy::{Handler, HttpClient, Router, scrape_quota};

fn main() {
    let args = Args::parse();
    let runtime = tokio::runtime::Runtime::new().expect("tokio runtime");
    if let Err(e) = runtime.block_on(run(args)) {
        log::error("fatal", vec![("err", Value::String(e))]);
        std::process::exit(1);
    }
}

#[derive(Parser)]
#[command(
    name = "llm-router",
    version,
    about = "Route Anthropic Messages API requests to Anthropic-compatible backends"
)]
struct Args {
    /// JSON configuration file
    #[arg(
        long,
        value_name = "PATH",
        env = "LLM_ROUTER_CONFIG",
        default_value = "/etc/llm-router/config.json"
    )]
    config: String,

    /// Probe a running instance through /livez and exit
    #[arg(long)]
    healthcheck: bool,

    /// Validate the configuration file and exit
    #[arg(long)]
    check_config: bool,
}

fn first_non_empty(values: impl IntoIterator<Item = String>) -> String {
    values
        .into_iter()
        .find(|v| !v.is_empty())
        .unwrap_or_default()
}

async fn run(args: Args) -> Result<(), String> {
    let Args {
        config,
        healthcheck,
        check_config,
    } = args;
    let config_path = config;
    let cfg = load_config(&config_path)?;
    if check_config {
        log::info(
            "config ok",
            vec![("path", Value::String(config_path.clone()))],
        );
        return Ok(());
    }
    let listen = first_non_empty([
        std::env::var("LLM_ROUTER_LISTEN").unwrap_or_default(),
        cfg.listen.clone(),
        ":8787".to_string(),
    ]);
    let admin_listen = std::env::var("LLM_ROUTER_ADMIN_LISTEN").unwrap_or_default();

    if healthcheck {
        return probe(if admin_listen.is_empty() {
            &listen
        } else {
            &admin_listen
        });
    }

    let drain_delay = match std::env::var("LLM_ROUTER_DRAIN_DELAY") {
        Ok(v) if !v.is_empty() => {
            parse_duration(&v).map_err(|e| format!("LLM_ROUTER_DRAIN_DELAY: {e}"))?
        }
        _ => Duration::from_secs(5),
    };

    let client = http_client(&cfg.network)?;
    let metrics = Arc::new(Metrics::new());
    let client_token = std::env::var("LLM_ROUTER_CLIENT_TOKEN").unwrap_or_default();
    let router = Arc::new(Router::new(
        &cfg,
        &client_token,
        client.clone(),
        metrics.clone(),
    )?);
    let shared = Arc::new(RwLock::new(router.clone()));
    let ready = Arc::new(AtomicBool::new(false));
    let separate_admin = !admin_listen.is_empty();

    let api = Handler {
        router: shared.clone(),
        metrics: metrics.clone(),
        ready: ready.clone(),
        probes: !separate_admin,
        proxy_enabled: true,
    };
    let admin = separate_admin.then(|| Handler {
        router: shared.clone(),
        metrics: metrics.clone(),
        ready: ready.clone(),
        probes: true,
        proxy_enabled: false,
    });

    let api_listener = bind(&listen).await?;
    let admin_listener = match &admin_listen {
        addr if separate_admin => Some(bind(addr).await?),
        _ => None,
    };
    let addrs: Vec<String> = [Some(&api_listener), admin_listener.as_ref()]
        .into_iter()
        .flatten()
        .map(|l| l.local_addr().map(|a| a.to_string()).unwrap_or_default())
        .collect();

    let mut quota_tasks = JoinSet::new();
    spawn_quota_tasks(&router, &client, &metrics, &mut quota_tasks);

    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let mut servers = JoinSet::new();
    servers.spawn(serve(api_listener, api, shutdown_rx.clone()));
    if let Some(listener) = admin_listener {
        let admin = admin.expect("admin handler");
        servers.spawn(serve(listener, admin, shutdown_rx));
    }
    ready.store(true, Ordering::Relaxed);

    let routes: Vec<Value> = cfg
        .routes
        .iter()
        .map(|r| Value::String(format!("{}={}", r.name, r.upstream)))
        .collect();
    log::info(
        "listening",
        vec![
            (
                "addr",
                Value::String(addrs.first().cloned().unwrap_or_default()),
            ),
            (
                "admin_addr",
                Value::String(addrs.get(1).cloned().unwrap_or_default()),
            ),
            ("default_route", Value::String(cfg.default_route.clone())),
            ("routes", Value::Array(routes)),
            ("client_token", Value::Bool(!client_token.is_empty())),
        ],
    );

    #[cfg(unix)]
    let mut hangup = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup())
        .expect("SIGHUP handler");
    #[cfg(unix)]
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .expect("SIGTERM handler");
    #[cfg(feature = "watch")]
    let mut watcher = ConfigWatcher::new(&config_path);

    loop {
        #[cfg(feature = "watch")]
        let config_changed = watcher.changed();
        #[cfg(not(feature = "watch"))]
        let config_changed = std::future::pending::<()>();
        #[cfg(unix)]
        let hup = hangup.recv();
        #[cfg(not(unix))]
        let hup = std::future::pending::<Option<()>>();
        #[cfg(unix)]
        let terminated = terminate.recv();
        #[cfg(not(unix))]
        let terminated = std::future::pending::<Option<()>>();

        tokio::select! {
            _ = config_changed => apply_reload(&config_path, &metrics, &shared, &mut quota_tasks),
            _ = hup => apply_reload(&config_path, &metrics, &shared, &mut quota_tasks),
            _ = tokio::signal::ctrl_c() => break,
            _ = terminated => break,
        }
    }

    // Fail readiness first so endpoints are removed before listeners close.
    ready.store(false, Ordering::Relaxed);
    log::info(
        "draining",
        vec![("delay", Value::String(format!("{drain_delay:?}")))],
    );
    tokio::time::sleep(drain_delay).await;
    let _ = shutdown_tx.send(true);

    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    while !servers.is_empty() {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if tokio::time::timeout(remaining, servers.join_next())
            .await
            .is_err()
        {
            servers.abort_all();
            break;
        }
    }
    quota_tasks.abort_all();
    log::info("stopped", vec![]);
    Ok(())
}

fn spawn_quota_tasks(
    router: &Router,
    client: &HttpClient,
    metrics: &Arc<Metrics>,
    tasks: &mut JoinSet<()>,
) {
    for route in router
        .routes
        .iter()
        .filter(|r| r.cfg.quota.is_some() && !r.disabled)
    {
        let route = route.clone();
        let client = client.clone();
        let metrics = metrics.clone();
        tasks.spawn(async move {
            loop {
                scrape_quota(&client, &route, &metrics).await;
                let interval = route
                    .cfg
                    .quota
                    .as_ref()
                    .map(|q| q.interval)
                    .unwrap_or(Duration::from_secs(60));
                tokio::time::sleep(interval).await;
            }
        });
    }
}

/// Builds a new router from the config file and swaps it in, keeping the old one on error.
fn apply_reload(
    config_path: &str,
    metrics: &Arc<Metrics>,
    shared: &RwLock<Arc<Router>>,
    quota_tasks: &mut JoinSet<()>,
) {
    match reload_router(config_path, metrics) {
        Ok((new_router, new_client)) => {
            *shared.write() = new_router.clone();
            quota_tasks.abort_all();
            spawn_quota_tasks(&new_router, &new_client, metrics, quota_tasks);
            log::info(
                "reloaded",
                vec![("path", Value::String(config_path.to_string()))],
            );
        }
        Err(e) => log::error("reload failed", vec![("err", Value::String(e))]),
    }
}

/// Watches the config file and signals when it changes on disk.
#[cfg(feature = "watch")]
struct ConfigWatcher {
    inner: Option<(
        notify::RecommendedWatcher,
        tokio::sync::mpsc::UnboundedReceiver<()>,
    )>,
}

#[cfg(feature = "watch")]
impl ConfigWatcher {
    fn new(path: &str) -> Self {
        let inner = Self::spawn(path);
        if inner.is_none() {
            log::warn(
                "config watch disabled",
                vec![("path", Value::String(path.to_string()))],
            );
        }
        Self { inner }
    }

    fn spawn(
        path: &str,
    ) -> Option<(
        notify::RecommendedWatcher,
        tokio::sync::mpsc::UnboundedReceiver<()>,
    )> {
        use notify::{RecursiveMode, Watcher};

        // Canonicalize once: backends report absolute paths, symlinks and macOS
        // /private prefixes included.
        let target = std::path::Path::new(path).canonicalize().ok()?;
        // Watch the parent directory: editors replace the file with a rename,
        // which invalidates a watch on the file itself.
        let dir = target.parent()?.to_path_buf();
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let watched = target.clone();
        let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
            let Ok(event) = res else { return };
            // Reading the file during a reload must not trigger another reload.
            if matches!(event.kind, notify::event::EventKind::Access(_)) {
                return;
            }
            let hit = event
                .paths
                .iter()
                .any(|p| p == &watched || p.canonicalize().is_ok_and(|c| c == watched));
            if hit {
                let _ = tx.send(());
            }
        })
        .ok()?;
        watcher.watch(&dir, RecursiveMode::NonRecursive).ok()?;
        Some((watcher, rx))
    }

    /// Resolves on the first change, after a settle window that absorbs the
    /// event burst of a single save.
    async fn changed(&mut self) {
        let Some((_, rx)) = &mut self.inner else {
            std::future::pending::<()>().await;
            return;
        };
        if rx.recv().await.is_none() {
            std::future::pending::<()>().await;
            return;
        }
        tokio::time::sleep(Duration::from_millis(150)).await;
        while rx.try_recv().is_ok() {}
    }
}

/// Builds a new router from the config file for a reload (SIGHUP or file change).
fn reload_router(
    config_path: &str,
    metrics: &Arc<Metrics>,
) -> Result<(Arc<Router>, HttpClient), String> {
    let cfg = load_config(config_path)?;
    let client = http_client(&cfg.network)?;
    let client_token = std::env::var("LLM_ROUTER_CLIENT_TOKEN").unwrap_or_default();
    let router = Router::new(&cfg, &client_token, client.clone(), metrics.clone())?;
    Ok((Arc::new(router), client))
}

fn http_client(network: &NetworkConfig) -> Result<HttpClient, String> {
    let connector = crate::network::Connector::new(Some(&network.resolved_proxy))?;
    let https = hyper_rustls::HttpsConnectorBuilder::new()
        .with_tls_config(crate::network::tls_config(&network.resolved_ca_file)?)
        .https_or_http()
        .enable_http1()
        .wrap_connector(connector);
    Ok(Client::builder(TokioExecutor::new()).build(https))
}

async fn bind(listen: &str) -> Result<TcpListener, String> {
    let addr = match listen.strip_prefix(':') {
        Some(rest) => format!("0.0.0.0:{rest}"),
        None => listen.to_string(),
    };
    TcpListener::bind(&addr)
        .await
        .map_err(|e| format!("listen {addr}: {e}"))
}

async fn serve(listener: TcpListener, handler: Handler, mut shutdown: watch::Receiver<bool>) {
    let mut connections = JoinSet::new();
    loop {
        tokio::select! {
            _ = shutdown.changed() => break,
            Some(_) = connections.join_next(), if !connections.is_empty() => {}
            accepted = listener.accept() => {
                match accepted {
                    Ok((stream, _)) => {
                        let handler = handler.clone();
                        connections.spawn(async move {
                            let service = service_fn(move |req| {
                                let handler = handler.clone();
                                async move { handler.call(req).await }
                            });
                            let conn = http1::Builder::new()
                                .timer(TokioTimer::new())
                                .header_read_timeout(Duration::from_secs(10))
                                .serve_connection(TokioIo::new(stream), service);
                            let _ = conn.await;
                        });
                    }
                    Err(e) => {
                        log::error("accept", vec![("err", Value::String(e.to_string()))]);
                        break;
                    }
                }
            }
        }
    }
    let drain = async { while connections.join_next().await.is_some() {} };
    if tokio::time::timeout(Duration::from_secs(60), drain)
        .await
        .is_err()
    {
        connections.abort_all();
    }
}

fn probe(listen: &str) -> Result<(), String> {
    let port = listen
        .rsplit_once(':')
        .map(|(_, port)| port)
        .ok_or_else(|| format!("invalid listen address {listen:?}"))?;
    let addr: SocketAddr = format!("127.0.0.1:{port}")
        .parse()
        .map_err(|e| format!("invalid listen address {listen:?}: {e}"))?;
    let mut stream =
        TcpStream::connect_timeout(&addr, Duration::from_secs(3)).map_err(|e| e.to_string())?;
    stream
        .set_read_timeout(Some(Duration::from_secs(3)))
        .map_err(|e| e.to_string())?;
    stream
        .write_all(b"GET /livez HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n")
        .map_err(|e| e.to_string())?;
    let mut response = String::new();
    stream
        .read_to_string(&mut response)
        .map_err(|e| e.to_string())?;
    let status_line = response.lines().next().unwrap_or_default();
    if status_line.split_whitespace().nth(1) != Some("200") {
        return Err(format!("livez returned {status_line}"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_config(name: &str, body: &str) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!(
            "llm-router-main-{name}-{}.json",
            std::process::id()
        ));
        std::fs::write(&path, body).expect("write config");
        path
    }

    const VALID_CONFIG: &str = r#"{
      "default_route": "local",
      "routes": [
        { "name": "local", "upstream": "http://127.0.0.1:9", "auth": { "mode": "passthrough" } }
      ]
    }"#;

    #[tokio::test]
    async fn bind_accepts_colon_prefix() {
        let listener = bind(":0").await.expect("bind");
        assert!(listener.local_addr().expect("addr").port() > 0);
    }

    #[tokio::test]
    async fn bind_reports_bad_address() {
        let err = bind("not-an-address").await.expect_err("bad address");
        assert!(err.contains("listen not-an-address"), "{err}");
    }

    #[test]
    fn reload_router_builds_and_rejects_invalid_config() {
        let metrics = Arc::new(Metrics::new());
        let path = temp_config("reload", VALID_CONFIG);
        let (router, _client) = reload_router(path.to_str().unwrap(), &metrics).expect("reload");
        assert_eq!(router.routes[0].cfg.name, "local");

        let bad = temp_config("reload-bad", "{ not json");
        let err = match reload_router(bad.to_str().unwrap(), &metrics) {
            Ok(_) => panic!("invalid config must fail"),
            Err(e) => e,
        };
        assert!(
            err.contains("invalid config") || err.contains("parse"),
            "{err}"
        );
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(&bad);
    }

    #[tokio::test]
    async fn apply_reload_swaps_router_and_keeps_old_on_error() {
        let metrics = Arc::new(Metrics::new());
        let client = http_client(&NetworkConfig::default()).expect("client");
        let router = Router::new(
            &load_config(temp_config("initial", VALID_CONFIG).to_str().unwrap()).expect("config"),
            "",
            client,
            metrics.clone(),
        )
        .expect("router");
        let shared = RwLock::new(Arc::new(router));
        let mut tasks = JoinSet::new();

        let updated = r#"{
          "default_route": "other",
          "routes": [
            { "name": "other", "upstream": "http://127.0.0.1:9", "auth": { "mode": "passthrough" } }
          ]
        }"#;
        let path = temp_config("apply", updated);
        apply_reload(path.to_str().unwrap(), &metrics, &shared, &mut tasks);
        assert_eq!(shared.read().routes[0].cfg.name, "other");

        let bad = temp_config("apply-bad", "{}");
        apply_reload(bad.to_str().unwrap(), &metrics, &shared, &mut tasks);
        assert_eq!(shared.read().routes[0].cfg.name, "other");
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(&bad);
    }

    #[tokio::test]
    async fn spawn_quota_tasks_skips_routes_without_quota() {
        let mut cfg = load_config(temp_config("quota", VALID_CONFIG).to_str().unwrap()).unwrap();
        cfg.routes[0].quota = Some(crate::config::QuotaConfig {
            url: "http://127.0.0.1:9/quota".to_string(),
            interval: Duration::from_millis(50),
        });
        let metrics = Arc::new(Metrics::new());
        let client = http_client(&NetworkConfig::default()).expect("client");
        let router = Router::new(&cfg, "", client.clone(), metrics.clone()).expect("router");
        let mut tasks = JoinSet::new();
        spawn_quota_tasks(&router, &client, &metrics, &mut tasks);
        assert_eq!(tasks.len(), 1, "quota route must get a task");
        tokio::time::sleep(Duration::from_millis(20)).await;
        tasks.abort_all();
    }

    fn spawn_http(status: &'static str) -> String {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr").to_string();
        std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let mut buf = [0u8; 1024];
                let _ = stream.read(&mut buf);
                let _ = stream.write_all(
                    format!("HTTP/1.1 {status}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                        .as_bytes(),
                );
            }
        });
        addr
    }

    #[test]
    fn probe_reports_status() {
        let addr = spawn_http("200 OK");
        probe(&addr).expect("probe 200");
        let addr = spawn_http("503 Service Unavailable");
        let err = probe(&addr).expect_err("probe 503");
        assert!(err.contains("livez returned"), "{err}");
        let err = probe("no-port").expect_err("invalid listen");
        assert!(err.contains("invalid listen address"), "{err}");
    }

    #[test]
    fn http_client_builds_with_default_network() {
        http_client(&NetworkConfig::default()).expect("default client");
    }

    #[cfg(feature = "watch")]
    #[tokio::test]
    async fn watcher_without_file_stays_pending() {
        let mut watcher = ConfigWatcher::new("/nonexistent/llm-router/config.json");
        assert!(watcher.inner.is_none());
        let pending = tokio::time::timeout(Duration::from_millis(50), watcher.changed());
        assert!(pending.await.is_err(), "watcher must stay pending");
    }
}
