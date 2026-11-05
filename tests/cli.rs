//! Process-level tests: the binary reads a config file, resolves environment overrides,
//! binds listeners, answers probes, reloads on SIGHUP and shuts down. Behavior inside the
//! request path is covered by the inline modules in `src/`.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

const MINIMAL_CONFIG: &str = r#"{
  "default_route": "local",
  "routes": [
    { "name": "local", "upstream": "http://127.0.0.1:9", "auth": { "mode": "passthrough" } }
  ]
}"#;

fn bin() -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_llm-router"));
    cmd.env_remove("LLM_ROUTER_CLIENT_TOKEN")
        .env_remove("LLM_ROUTER_ADMIN_LISTEN")
        .env_remove("LLM_ROUTER_CONFIG");
    cmd
}

/// Config file removed when the test ends.
struct TempConfig(PathBuf);

impl TempConfig {
    fn new(name: &str, body: &str) -> Self {
        let path =
            std::env::temp_dir().join(format!("llm-router-{name}-{}.json", std::process::id()));
        let config = Self(path);
        config.write(body);
        config
    }

    fn write(&self, body: &str) {
        std::fs::write(&self.0, body).expect("write config");
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempConfig {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// Running router, killed when the test ends.
struct Server {
    child: Child,
    lines: Arc<Mutex<Vec<String>>>,
    addr: String,
    admin_addr: String,
}

impl Server {
    fn pid(&self) -> u32 {
        self.child.id()
    }

    fn lines(&self) -> Vec<String> {
        self.lines.lock().expect("lines").clone()
    }

    fn wait_for_exit(&mut self, timeout: Duration) -> std::process::ExitStatus {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(status) = self.child.try_wait().expect("try_wait") {
                return status;
            }
            assert!(Instant::now() < deadline, "router did not exit in time");
            thread::sleep(Duration::from_millis(20));
        }
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        // Terminate gracefully: SIGKILL skips the exit path (and the coverage profile write).
        if self.child.try_wait().expect("try_wait").is_none() {
            let _ = Command::new("/bin/kill")
                .arg("-TERM")
                .arg(self.child.id().to_string())
                .status();
        }
        let deadline = Instant::now() + Duration::from_secs(5);
        while self.child.try_wait().expect("try_wait").is_none() {
            if Instant::now() >= deadline {
                let _ = self.child.kill();
                break;
            }
            thread::sleep(Duration::from_millis(20));
        }
        let _ = self.child.wait();
    }
}

fn start(config: &Path, listen: &str, admin: Option<&str>) -> Server {
    let mut cmd = bin();
    cmd.arg("--config")
        .arg(config)
        .env("LLM_ROUTER_LISTEN", listen)
        .env("LLM_ROUTER_DRAIN_DELAY", "0s")
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    if let Some(admin) = admin {
        cmd.env("LLM_ROUTER_ADMIN_LISTEN", admin);
    }
    let mut child = cmd.spawn().expect("spawn router");
    let mut stdout = BufReader::new(child.stdout.take().expect("stdout"));
    let mut line = String::new();
    loop {
        line.clear();
        let n = stdout.read_line(&mut line).expect("read router stdout");
        assert!(n > 0, "router exited before logging its listeners");
        if line.contains("\"msg\":\"listening\"") {
            break;
        }
    }
    let log: serde_json::Value = serde_json::from_str(&line).expect("listening log is JSON");
    let field = |key: &str| log[key].as_str().unwrap_or_default().to_string();
    let lines = Arc::new(Mutex::new(vec![line.clone()]));
    let collected = lines.clone();
    // Drain stdout for the rest of the run: a closed pipe aborts the child on its next log.
    thread::spawn(move || {
        for line in stdout.lines() {
            let Ok(line) = line else { break };
            collected.lock().expect("lines").push(line);
        }
    });
    Server {
        child,
        lines,
        addr: field("addr"),
        admin_addr: field("admin_addr"),
    }
}

/// Connects to a currently free loopback port and closes the socket.
fn free_addr() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral");
    listener.local_addr().expect("local addr").to_string()
}

fn signal(pid: u32, name: &str) {
    let status = Command::new("/bin/kill")
        .arg(format!("-{name}"))
        .arg(pid.to_string())
        .status()
        .expect("run kill");
    assert!(status.success(), "kill -{name} {pid} failed");
}

fn wait_until(mut check: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if check() {
            return;
        }
        thread::sleep(Duration::from_millis(50));
    }
    panic!("condition not met within 5s");
}

fn status_line(addr: &str, method: &str, path: &str) -> String {
    let mut stream = TcpStream::connect(addr).expect("connect");
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("read timeout");
    write!(
        stream,
        "{method} {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n"
    )
    .expect("write request");
    let mut response = String::new();
    stream.read_to_string(&mut response).expect("read response");
    let line = response.lines().next().unwrap_or_default().to_string();
    assert!(
        line.starts_with("HTTP/1.1 "),
        "malformed response: {response}"
    );
    line
}

/// POSTs a messages request and returns the response body.
fn post_body(addr: &str) -> String {
    let body = r#"{"model":"claude-x"}"#;
    let mut stream = TcpStream::connect(addr).expect("connect");
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("read timeout");
    write!(
        stream,
        "POST /v1/messages HTTP/1.1\r\nHost: {addr}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
    .expect("write request");
    let mut response = String::new();
    stream.read_to_string(&mut response).expect("read response");
    response
        .split_once("\r\n\r\n")
        .map(|(_, body)| body.to_string())
        .unwrap_or_default()
}

/// Minimal plain-HTTP upstream on its own thread, answering every request with `body`.
fn spawn_upstream(body: &'static str) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind upstream");
    let addr = listener.local_addr().expect("upstream addr").to_string();
    thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { break };
            let mut request = [0u8; 4096];
            let _ = stream.read(&mut request);
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(response.as_bytes());
        }
    });
    addr
}

fn config_for(upstream: &str) -> String {
    format!(
        r#"{{
      "default_route": "local",
      "routes": [
        {{ "name": "local", "upstream": "http://{upstream}", "auth": {{ "mode": "passthrough" }} }}
      ]
    }}"#
    )
}

#[test]
fn invalid_config_exits_nonzero() {
    let config = TempConfig::new("invalid", r#"{"default_route": "missing", "routes": []}"#);
    let out = bin()
        .arg("--config")
        .arg(config.path())
        .output()
        .expect("run router");
    assert!(!out.status.success());
}

#[test]
fn unknown_config_field_is_rejected() {
    let config = TempConfig::new("unknown", r#"{"unknown_field": true}"#);
    let out = bin()
        .arg("--config")
        .arg(config.path())
        .output()
        .expect("run router");
    assert!(!out.status.success());
}

#[test]
fn check_config_accepts_valid_file_without_listening() {
    let config = TempConfig::new("check-ok", MINIMAL_CONFIG);
    let out = bin()
        .arg("--config")
        .arg(config.path())
        .arg("--check-config")
        .output()
        .expect("run check-config");
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("\"msg\":\"config ok\""), "{stdout}");
    assert!(!stdout.contains("\"msg\":\"listening\""), "{stdout}");
}

#[test]
fn check_config_rejects_invalid_file() {
    let config = TempConfig::new("check-bad", r#"{"unknown_field": true}"#);
    let out = bin()
        .arg("--config")
        .arg(config.path())
        .arg("--check-config")
        .output()
        .expect("run check-config");
    assert!(!out.status.success());
}

#[test]
fn listen_env_overrides_config() {
    let body = r#"{
      "listen": "127.0.0.1:1",
      "default_route": "local",
      "routes": [
        { "name": "local", "upstream": "http://127.0.0.1:9", "auth": { "mode": "passthrough" } }
      ]
    }"#;
    let config = TempConfig::new("listen", body);
    // Port 1 is unprivileged-unbindable, so success proves the env override won.
    let server = start(config.path(), "127.0.0.1:0", None);
    assert!(server.addr.starts_with("127.0.0.1:"));
    let port: u16 = server
        .addr
        .rsplit_once(':')
        .and_then(|(_, p)| p.parse().ok())
        .expect("addr carries a port");
    assert_ne!(port, 0);
    assert_ne!(port, 1);
}

#[test]
fn healthcheck_succeeds_against_running_server() {
    let config = TempConfig::new("healthcheck-ok", MINIMAL_CONFIG);
    let server = start(config.path(), "127.0.0.1:0", Some("127.0.0.1:0"));
    let status = bin()
        .arg("--config")
        .arg(config.path())
        .arg("--healthcheck")
        .env("LLM_ROUTER_ADMIN_LISTEN", &server.admin_addr)
        .status()
        .expect("run healthcheck");
    assert!(status.success());
}

#[test]
fn healthcheck_uses_api_listener_without_admin() {
    let config = TempConfig::new("healthcheck-api", MINIMAL_CONFIG);
    let server = start(config.path(), "127.0.0.1:0", None);
    let status = bin()
        .arg("--config")
        .arg(config.path())
        .arg("--healthcheck")
        .env("LLM_ROUTER_LISTEN", &server.addr)
        .status()
        .expect("run healthcheck");
    assert!(status.success());
}

#[test]
fn healthcheck_fails_without_server() {
    let config = TempConfig::new("healthcheck-down", MINIMAL_CONFIG);
    let addr = free_addr();
    let out = bin()
        .arg("--config")
        .arg(config.path())
        .arg("--healthcheck")
        .env("LLM_ROUTER_ADMIN_LISTEN", addr)
        .output()
        .expect("run healthcheck");
    assert!(!out.status.success());
}

#[test]
fn admin_listener_serves_probes_only() {
    let config = TempConfig::new("admin", MINIMAL_CONFIG);
    let server = start(config.path(), "127.0.0.1:0", Some("127.0.0.1:0"));
    assert!(status_line(&server.admin_addr, "GET", "/metrics").contains(" 200 "));
    assert!(status_line(&server.admin_addr, "GET", "/v1/messages").contains(" 404 "));
}

#[test]
fn sigterm_drains_and_exits_cleanly() {
    let config = TempConfig::new("sigterm", MINIMAL_CONFIG);
    let mut server = start(config.path(), "127.0.0.1:0", None);
    signal(server.pid(), "TERM");
    let status = server.wait_for_exit(Duration::from_secs(10));
    assert!(status.success(), "{status:?}");
    let lines = server.lines();
    assert!(
        lines.iter().any(|l| l.contains("\"msg\":\"draining\"")),
        "{lines:?}"
    );
    assert!(
        lines.iter().any(|l| l.contains("\"msg\":\"stopped\"")),
        "{lines:?}"
    );
}

#[test]
fn sighup_reloads_config_and_keeps_old_on_error() {
    let upstream_a = spawn_upstream(r#"{"from":"a"}"#);
    let upstream_b = spawn_upstream(r#"{"from":"b"}"#);
    let config = TempConfig::new("reload", &config_for(&upstream_a));
    let server = start(config.path(), "127.0.0.1:0", None);
    assert_eq!(post_body(&server.addr), r#"{"from":"a"}"#);

    config.write(&config_for(&upstream_b));
    signal(server.pid(), "HUP");
    wait_until(|| post_body(&server.addr) == r#"{"from":"b"}"#);

    // A broken reload must leave the running router untouched.
    config.write(r#"{"unknown_field": true}"#);
    signal(server.pid(), "HUP");
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline && !server.lines().iter().any(|l| l.contains("reload failed")) {
        thread::sleep(Duration::from_millis(50));
    }
    assert_eq!(post_body(&server.addr), r#"{"from":"b"}"#);
    assert!(
        server
            .lines()
            .iter()
            .any(|l| l.contains("\"msg\":\"reload failed\"")),
        "{:?}",
        server.lines()
    );
}

#[cfg(feature = "watch")]
#[test]
fn config_file_change_reloads_without_signal() {
    let upstream_a = spawn_upstream(r#"{"from":"a"}"#);
    let upstream_b = spawn_upstream(r#"{"from":"b"}"#);
    let config = TempConfig::new("watch", &config_for(&upstream_a));
    let server = start(config.path(), "127.0.0.1:0", None);
    assert_eq!(post_body(&server.addr), r#"{"from":"a"}"#);

    config.write(&config_for(&upstream_b));
    wait_until(|| post_body(&server.addr) == r#"{"from":"b"}"#);
    assert!(
        server
            .lines()
            .iter()
            .any(|l| l.contains("\"msg\":\"reloaded\"")),
        "{:?}",
        server.lines()
    );
}
