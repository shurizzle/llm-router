//! Outbound connector: direct, or through an HTTP CONNECT proxy, plus the TLS root store.

use std::future::Future;
use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};

use base64::Engine;
use hyper::Uri;
use hyper::rt::{Read, ReadBufCursor, Write};
use hyper_util::client::legacy::connect::proxy::Tunnel;
use hyper_util::client::legacy::connect::{Connected, Connection, HttpConnector};
use rustls::ClientConfig;
use tower_service::Service;

pub type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// A connection whose concrete type depends on whether a proxy is used.
pub struct BoxConn(Pin<Box<dyn ConnectionStream + Send>>);

trait ConnectionStream: Read + Write + Connection + Send + Unpin {}
impl<T: Read + Write + Connection + Send + Unpin> ConnectionStream for T {}

impl Read for BoxConn {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: ReadBufCursor<'_>,
    ) -> Poll<Result<(), io::Error>> {
        self.get_mut().0.as_mut().poll_read(cx, buf)
    }
}

impl Write for BoxConn {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<Result<usize, io::Error>> {
        self.get_mut().0.as_mut().poll_write(cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), io::Error>> {
        self.get_mut().0.as_mut().poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), io::Error>> {
        self.get_mut().0.as_mut().poll_shutdown(cx)
    }
}

impl Connection for BoxConn {
    fn connected(&self) -> Connected {
        Connection::connected(self.0.as_ref().get_ref())
    }
}

/// Connector used by the proxy client. With a proxy configured, every upstream is reached
/// through an HTTP CONNECT tunnel, including plain `http://` ones.
#[derive(Clone, Debug)]
pub struct Connector {
    inner: HttpConnector,
    proxy: Option<Uri>,
    auth: Option<hyper::header::HeaderValue>,
}

impl Connector {
    /// `proxy` is an `http://` URL; empty disables the proxy. Credentials in the URL are
    /// sent as HTTP Basic auth on the CONNECT request.
    pub fn new(proxy: Option<&str>) -> Result<Self, String> {
        let mut inner = HttpConnector::new();
        inner.enforce_http(false);
        let (proxy, auth) = match proxy.filter(|p| !p.is_empty()) {
            None => (None, None),
            Some(raw) => {
                let url = url::Url::parse(raw).map_err(|e| format!("network.proxy: {e}"))?;
                if url.scheme() != "http" {
                    return Err(format!(
                        "network.proxy: unsupported scheme {:?}, use http://",
                        url.scheme()
                    ));
                }
                let host = url
                    .host_str()
                    .ok_or_else(|| "network.proxy: missing host".to_string())?;
                let port = url
                    .port_or_known_default()
                    .ok_or_else(|| "network.proxy: missing port".to_string())?;
                let uri: Uri = format!("http://{host}:{port}")
                    .parse()
                    .map_err(|e| format!("network.proxy: {e}"))?;
                let auth = if url.username().is_empty() {
                    None
                } else {
                    let credentials = format!(
                        "{}:{}",
                        percent_decode(url.username()),
                        percent_decode(url.password().unwrap_or(""))
                    );
                    let encoded = base64::engine::general_purpose::STANDARD.encode(credentials);
                    let value = hyper::header::HeaderValue::from_str(&format!("Basic {encoded}"))
                        .map_err(|e| format!("network.proxy: {e}"))?;
                    Some(value)
                };
                (Some(uri), auth)
            }
        };
        Ok(Self { inner, proxy, auth })
    }
}

impl Service<Uri> for Connector {
    type Response = BoxConn;
    type Error = BoxError;
    type Future = Pin<Box<dyn Future<Output = Result<BoxConn, BoxError>> + Send>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), BoxError>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, dst: Uri) -> Self::Future {
        let mut inner = self.inner.clone();
        let proxy = self.proxy.clone();
        let auth = self.auth.clone();
        Box::pin(async move {
            match proxy {
                Some(proxy) => {
                    let mut tunnel = Tunnel::new(proxy, inner);
                    if let Some(auth) = auth {
                        tunnel = tunnel.with_auth(auth);
                    }
                    let conn = tunnel.call(dst).await.map_err(tunnel_error)?;
                    Ok(BoxConn(Box::pin(conn)))
                }
                None => {
                    let conn = inner.call(dst).await.map_err(|e| Box::new(e) as BoxError)?;
                    Ok(BoxConn(Box::pin(conn)))
                }
            }
        })
    }
}

fn tunnel_error(error: impl std::fmt::Display) -> BoxError {
    Box::new(io::Error::other(format!("proxy CONNECT: {error}")))
}

/// Decodes `%XX` escapes in a proxy URL's userinfo.
fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let high = (bytes[i + 1] as char).to_digit(16);
            let low = (bytes[i + 2] as char).to_digit(16);
            if let (Some(high), Some(low)) = (high, low) {
                out.push((high * 16 + low) as u8);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Builds the TLS client configuration: native roots plus the optional extra PEM bundle.
pub fn tls_config(ca_file: &str) -> Result<ClientConfig, String> {
    let mut roots = rustls::RootCertStore::empty();
    for cert in rustls_native_certs::load_native_certs().certs {
        let _ = roots.add(cert);
    }
    if !ca_file.is_empty() {
        let file = std::fs::File::open(ca_file)
            .map_err(|e| format!("network.ca_file {ca_file:?}: {e}"))?;
        let mut reader = std::io::BufReader::new(file);
        let mut added = 0usize;
        for cert in rustls_pemfile::certs(&mut reader) {
            let cert = cert.map_err(|e| format!("network.ca_file {ca_file:?}: {e}"))?;
            roots
                .add(cert)
                .map_err(|e| format!("network.ca_file {ca_file:?}: {e}"))?;
            added += 1;
        }
        if added == 0 {
            return Err(format!(
                "network.ca_file {ca_file:?}: no certificates found"
            ));
        }
    }
    if roots.is_empty() {
        return Err("no trusted root certificates found".to_string());
    }
    Ok(ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_http_proxy_with_credentials() {
        let connector = Connector::new(Some("http://user:p%40ss@proxy.local:3128")).unwrap();
        assert_eq!(
            connector.proxy.as_ref().unwrap().to_string(),
            "http://proxy.local:3128/"
        );
        let auth = connector.auth.unwrap();
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(auth.to_str().unwrap().trim_start_matches("Basic "))
            .unwrap();
        assert_eq!(decoded, b"user:p@ss");
    }

    #[test]
    fn empty_proxy_is_direct() {
        let connector = Connector::new(Some("")).unwrap();
        assert!(connector.proxy.is_none());
        assert!(connector.auth.is_none());
        assert!(Connector::new(None).unwrap().proxy.is_none());
    }

    #[test]
    fn rejects_other_proxy_schemes() {
        let err = Connector::new(Some("socks5://proxy:1080")).unwrap_err();
        assert!(err.contains("unsupported scheme"), "{err}");
        let err = Connector::new(Some("not a url")).unwrap_err();
        assert!(err.contains("network.proxy"), "{err}");
    }

    #[test]
    fn missing_ca_file_is_an_error() {
        let err = tls_config("/nonexistent/llm-router-ca.pem").unwrap_err();
        assert!(err.contains("network.ca_file"), "{err}");
    }

    #[test]
    fn ca_file_without_certificates_is_an_error() {
        let path =
            std::env::temp_dir().join(format!("llm-router-empty-ca-{}.pem", std::process::id()));
        std::fs::write(&path, "not a certificate").unwrap();
        let err = tls_config(path.to_str().unwrap()).unwrap_err();
        assert!(err.contains("no certificates"), "{err}");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn native_roots_load() {
        assert!(tls_config("").is_ok());
    }

    #[test]
    fn percent_decode_handles_escapes_and_junk() {
        assert_eq!(percent_decode("a%20b"), "a b");
        assert_eq!(percent_decode("%40%3a"), "@:");
        assert_eq!(percent_decode("100%"), "100%");
        assert_eq!(percent_decode("a%zzb"), "a%zzb");
    }

    #[test]
    fn invalid_certificate_pem_is_an_error() {
        let path =
            std::env::temp_dir().join(format!("llm-router-bad-ca-{}.pem", std::process::id()));
        std::fs::write(
            &path,
            "-----BEGIN CERTIFICATE-----\nAAAA\n-----END CERTIFICATE-----\n",
        )
        .unwrap();
        let err = tls_config(path.to_str().unwrap()).unwrap_err();
        assert!(err.contains("network.ca_file"), "{err}");
        let _ = std::fs::remove_file(&path);
    }
}
