//! Optional native HTTPS. HTTP stays available during the client migration.
use axum::Router;
use axum_server::{tls_rustls::RustlsConfig, Handle};
use std::{future::Future, io, net::SocketAddr, path::PathBuf, time::Duration};

use crate::config::ConfigError;

#[derive(Debug, Clone)]
pub struct HttpsConfig {
    pub bind: SocketAddr,
    pub cert_file: PathBuf,
    pub key_file: PathBuf,
}

impl HttpsConfig {
    pub fn from_env(http_bind: SocketAddr) -> Result<Option<Self>, ConfigError> {
        Self::parse(
            http_bind,
            crate::config::optional_env("HTTPS_BIND")?,
            crate::config::optional_env("GATEWAY_TLS_CERT_FILE")?,
            crate::config::optional_env("GATEWAY_TLS_KEY_FILE")?,
        )
    }

    fn parse(
        http_bind: SocketAddr,
        bind: Option<String>,
        cert: Option<String>,
        key: Option<String>,
    ) -> Result<Option<Self>, ConfigError> {
        match (bind, cert, key) {
            (None, None, None) => Ok(None),
            (Some(bind), Some(cert), Some(key)) if !cert.is_empty() && !key.is_empty() => {
                let bind: SocketAddr = bind
                    .parse()
                    .map_err(|_| ConfigError::from("invalid HTTPS_BIND"))?;
                if bind == http_bind {
                    return Err("HTTPS_BIND must differ from HTTP_BIND".into());
                }
                Ok(Some(Self {
                    bind,
                    cert_file: cert.into(),
                    key_file: key.into(),
                }))
            }
            _ => Err("HTTPS requires HTTPS_BIND, GATEWAY_TLS_CERT_FILE and GATEWAY_TLS_KEY_FILE together".into()),
        }
    }
}

pub struct Listeners {
    http: std::net::TcpListener,
    https: Option<(std::net::TcpListener, RustlsConfig)>,
}

impl Listeners {
    /// Load the certificate and bind BOTH sockets before any server or MQTT task starts.
    /// A broken HTTPS configuration must never silently fall back to HTTP.
    pub async fn bind(http_bind: SocketAddr, https: Option<&HttpsConfig>) -> io::Result<Self> {
        let https = match https {
            Some(config) => {
                let tls = RustlsConfig::from_pem_file(&config.cert_file, &config.key_file).await?;
                let socket = std::net::TcpListener::bind(config.bind)?;
                socket.set_nonblocking(true)?;
                Some((socket, tls))
            }
            None => None,
        };
        let http = std::net::TcpListener::bind(http_bind)?;
        http.set_nonblocking(true)?;
        Ok(Self { http, https })
    }

    pub async fn serve(self, app: Router, shutdown: impl Future<Output = ()>) -> io::Result<()> {
        let http_handle = Handle::new();
        let https_handle = Handle::new();
        tracing::info!(addr = %self.http.local_addr()?, "http listening (legacy compatibility)");
        let http = axum_server::from_tcp(self.http)?
            .handle(http_handle.clone())
            .serve(app.clone().into_make_service());
        let https = async {
            if let Some((socket, config)) = self.https {
                tracing::info!(addr = %socket.local_addr()?, "https listening");
                axum_server::from_tcp_rustls(socket, config)?
                    .handle(https_handle.clone())
                    .serve(app.into_make_service())
                    .await?;
            }
            Ok::<(), io::Error>(())
        };
        let servers = async { tokio::try_join!(http, https).map(|_| ()) };
        tokio::pin!(servers);
        tokio::select! {
            result = &mut servers => {
                http_handle.shutdown();
                https_handle.shutdown();
                result
            }
            _ = shutdown => {
                // SSE streams are long-lived; bound draining so SIGTERM finishes.
                http_handle.graceful_shutdown(Some(Duration::from_secs(5)));
                https_handle.graceful_shutdown(Some(Duration::from_secs(5)));
                servers.await
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use tokio::io::{
        AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader,
    };
    use tokio_rustls::TlsConnector;

    const IO_TIMEOUT: Duration = Duration::from_secs(3);

    async fn bounded<T>(future: impl Future<Output = T>) -> T {
        tokio::time::timeout(IO_TIMEOUT, future)
            .await
            .expect("local transport operation timed out")
    }

    struct CertificateFiles {
        _directory: tempfile::TempDir,
        config: HttpsConfig,
        certificate: rcgen::CertifiedKey,
    }

    impl CertificateFiles {
        fn new() -> Self {
            let directory = tempfile::tempdir().unwrap();
            let certificate = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
            let config = HttpsConfig {
                bind: "127.0.0.1:0".parse().unwrap(),
                cert_file: directory.path().join("cert.pem"),
                key_file: directory.path().join("key.pem"),
            };
            std::fs::write(&config.cert_file, certificate.cert.pem()).unwrap();
            std::fs::write(&config.key_file, certificate.key_pair.serialize_pem()).unwrap();
            Self {
                _directory: directory,
                config,
                certificate,
            }
        }

        fn connector(&self) -> TlsConnector {
            let mut roots = rustls::RootCertStore::empty();
            roots.add(self.certificate.cert.der().clone()).unwrap();
            TlsConnector::from(Arc::new(
                rustls::ClientConfig::builder()
                    .with_root_certificates(roots)
                    .with_no_client_auth(),
            ))
        }
    }

    async fn response(mut stream: impl AsyncRead + AsyncWrite + Unpin, request: &str) -> String {
        bounded(async {
            stream.write_all(request.as_bytes()).await.unwrap();
            let mut response = String::new();
            stream.read_to_string(&mut response).await.unwrap();
            response
        })
        .await
    }

    fn ready_state() -> Arc<crate::state::AppState> {
        let state = crate::state::AppState::new(crate::http::tests::cfg_with_token("test-secret"));
        state.shared.set_connected(true);
        update_soc(&state, 50.0);
        state
    }

    fn update_soc(state: &crate::state::AppState, soc: f64) {
        state.shared.update(crate::state::ParsedUpdate {
            service: "system".into(),
            path: "0/Dc/Battery/Soc".into(),
            value: serde_json::json!(soc),
        });
    }

    #[test]
    fn https_configuration_is_atomic_and_http_remains_default() {
        let http = "127.0.0.1:8080".parse().unwrap();
        assert!(HttpsConfig::parse(http, None, None, None)
            .unwrap()
            .is_none());
        for mask in 1..7 {
            assert!(HttpsConfig::parse(
                http,
                (mask & 1 != 0).then(|| "127.0.0.1:8443".into()),
                (mask & 2 != 0).then(|| "cert.pem".into()),
                (mask & 4 != 0).then(|| "key.pem".into()),
            )
            .is_err());
        }
        for bind in ["invalid", "127.0.0.1:8080"] {
            assert!(HttpsConfig::parse(
                http,
                Some(bind.into()),
                Some("cert".into()),
                Some("key".into())
            )
            .is_err());
        }
        assert!(HttpsConfig::parse(
            http,
            Some("127.0.0.1:8443".into()),
            Some("".into()),
            Some("key".into())
        )
        .is_err());
        let config = HttpsConfig::parse(
            http,
            Some("127.0.0.1:8443".into()),
            Some("cert".into()),
            Some("key".into()),
        )
        .unwrap()
        .unwrap();
        assert_eq!(config.bind.port(), 8443);
    }

    #[tokio::test]
    async fn missing_certificate_fails_before_http_binds() {
        let directory = tempfile::tempdir().unwrap();
        let config = HttpsConfig {
            bind: "127.0.0.1:0".parse().unwrap(),
            cert_file: directory.path().join("missing-cert.pem"),
            key_file: directory.path().join("missing-key.pem"),
        };
        // Occupy HTTP too: the error must identify the TLS file, proving TLS
        // validation runs before an HTTP socket or healthy legacy server exists.
        let http = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let error = bounded(Listeners::bind(http.local_addr().unwrap(), Some(&config)))
            .await
            .err()
            .expect("missing certificate accepted");
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
    }

    #[tokio::test]
    async fn invalid_and_mismatched_pem_never_fall_back_to_http() {
        for malformed in ["certificate", "key", "mismatched key"] {
            let files = CertificateFiles::new();
            match malformed {
                "certificate" => {
                    std::fs::write(&files.config.cert_file, "not a PEM certificate").unwrap()
                }
                "key" => std::fs::write(&files.config.key_file, "not a PEM key").unwrap(),
                _ => {
                    let different_key = rcgen::KeyPair::generate().unwrap();
                    std::fs::write(&files.config.key_file, different_key.serialize_pem()).unwrap();
                }
            }
            let http = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let error = bounded(Listeners::bind(
                http.local_addr().unwrap(),
                Some(&files.config),
            ))
            .await
            .err()
            .unwrap_or_else(|| panic!("{malformed} accepted"));
            assert_ne!(
                error.kind(),
                io::ErrorKind::AddrInUse,
                "HTTP bound before rejecting {malformed}"
            );
        }
    }

    #[tokio::test]
    async fn occupied_https_port_fails_and_http_bind_failure_releases_https() {
        let mut files = CertificateFiles::new();
        let occupied_https = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        files.config.bind = occupied_https.local_addr().unwrap();
        let error = bounded(Listeners::bind(
            "127.0.0.1:0".parse().unwrap(),
            Some(&files.config),
        ))
        .await
        .err()
        .expect("occupied HTTPS port accepted");
        assert_eq!(error.kind(), io::ErrorKind::AddrInUse);

        drop(occupied_https);
        let occupied_http = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let error = bounded(Listeners::bind(
            occupied_http.local_addr().unwrap(),
            Some(&files.config),
        ))
        .await
        .err()
        .expect("occupied HTTP port accepted");
        assert_eq!(error.kind(), io::ErrorKind::AddrInUse);
        // The HTTPS socket acquired first must be dropped when HTTP binding fails.
        let _released_https = std::net::TcpListener::bind(files.config.bind).unwrap();
    }

    #[tokio::test]
    async fn dual_listeners_verify_tls_preserve_auth_and_do_not_redirect() {
        let files = CertificateFiles::new();
        let listeners = bounded(Listeners::bind(
            "127.0.0.1:0".parse().unwrap(),
            Some(&files.config),
        ))
        .await
        .unwrap();
        let http_addr = listeners.http.local_addr().unwrap();
        let https_addr = listeners.https.as_ref().unwrap().0.local_addr().unwrap();
        let (stop, stopped) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(listeners.serve(crate::http::router(ready_state()), async {
            let _ = stopped.await;
        }));
        let trusted = files.connector();
        for (method, path, authorization, status) in [
            ("GET", "/health", "", "200"),
            ("GET", "/v1/snapshot", "", "401"),
            ("GET", "/v1/events", "", "401"),
            (
                "GET",
                "/v1/snapshot",
                "Authorization: Bearer test-secret\r\n",
                "200",
            ),
            (
                "GET",
                "/v1/snapshot",
                "Authorization: Bearer read-secret\r\n",
                "200",
            ),
            (
                "GET",
                "/v1/energy",
                "Authorization: Bearer test-secret\r\n",
                "200",
            ),
            (
                "GET",
                "/v1/energy",
                "Authorization: Bearer read-secret\r\n",
                "200",
            ),
            (
                "GET",
                "/v1/commands/unknown",
                "Authorization: Bearer read-secret\r\n",
                "401",
            ),
            (
                "POST",
                "/v1/commands/unknown",
                "Authorization: Bearer read-secret\r\n",
                "401",
            ),
        ] {
            let body = if method == "POST" {
                "Content-Type: application/json\r\nContent-Length: 2\r\n"
            } else {
                ""
            };
            let payload = if method == "POST" { "{}" } else { "" };
            let request = format!("{method} {path} HTTP/1.1\r\nHost: localhost\r\n{authorization}{body}Connection: close\r\n\r\n{payload}");
            let plain = bounded(tokio::net::TcpStream::connect(http_addr))
                .await
                .unwrap();
            let plain_response = response(plain, &request).await;
            let stream = bounded(tokio::net::TcpStream::connect(https_addr))
                .await
                .unwrap();
            let secure = bounded(trusted.connect("localhost".try_into().unwrap(), stream))
                .await
                .unwrap();
            let secure_response = response(secure, &request).await;
            for response in [plain_response, secure_response] {
                assert!(
                    response.starts_with(&format!("HTTP/1.1 {status}")),
                    "{method} {path}: {response}"
                );
                assert!(!response.to_lowercase().contains("\r\nlocation:"));
            }
        }
        let stream = bounded(tokio::net::TcpStream::connect(https_addr))
            .await
            .unwrap();
        assert!(
            bounded(trusted.connect("wrong.example".try_into().unwrap(), stream))
                .await
                .is_err()
        );
        let untrusted = rustls::ClientConfig::builder()
            .with_root_certificates(rustls::RootCertStore::empty())
            .with_no_client_auth();
        let stream = bounded(tokio::net::TcpStream::connect(https_addr))
            .await
            .unwrap();
        assert!(bounded(
            TlsConnector::from(Arc::new(untrusted))
                .connect("localhost".try_into().unwrap(), stream)
        )
        .await
        .is_err());
        stop.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(7), server)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
    }

    async fn open_events<S: AsyncRead + AsyncWrite + Unpin>(mut stream: S) -> BufReader<S> {
        bounded(async {
            stream.write_all(b"GET /v1/events HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer read-secret\r\n\r\n").await.unwrap();
            let mut reader = BufReader::new(stream);
            let mut headers = String::new();
            loop {
                let mut line = String::new();
                assert_ne!(reader.read_line(&mut line).await.unwrap(), 0, "SSE headers ended early");
                headers.push_str(&line);
                if line == "\r\n" { break; }
            }
            assert!(headers.starts_with("HTTP/1.1 200"), "{headers}");
            assert!(headers.to_lowercase().contains("content-type: text/event-stream"), "{headers}");
            assert!(headers.to_lowercase().contains("transfer-encoding: chunked"), "{headers}");
            assert!(!headers.to_lowercase().contains("\r\nlocation:"));
            reader
        }).await
    }

    async fn read_event<S: AsyncRead + Unpin>(reader: &mut BufReader<S>) -> serde_json::Value {
        bounded(async {
            // Decode HTTP/1.1 chunks so TCP and HTTP chunk boundaries cannot
            // make a valid event flaky or let a truncated payload pass.
            let mut data = Vec::new();
            loop {
                let mut size = String::new();
                assert_ne!(
                    reader.read_line(&mut size).await.unwrap(),
                    0,
                    "SSE stream ended before event"
                );
                let size =
                    usize::from_str_radix(size.trim().split(';').next().unwrap(), 16).unwrap();
                assert!(
                    (1..=16384).contains(&size),
                    "unexpected SSE chunk size: {size}"
                );
                let offset = data.len();
                data.resize(offset + size, 0);
                reader.read_exact(&mut data[offset..]).await.unwrap();
                let mut crlf = [0; 2];
                reader.read_exact(&mut crlf).await.unwrap();
                assert_eq!(&crlf, b"\r\n");
                if let Some(end) = data.windows(2).position(|bytes| bytes == b"\n\n") {
                    let event = std::str::from_utf8(&data[..end]).unwrap();
                    return serde_json::from_str(
                        event
                            .strip_prefix("data: ")
                            .expect("snapshot SSE data field"),
                    )
                    .unwrap();
                }
                assert!(data.len() <= 16384, "SSE event exceeded fixture size");
            }
        })
        .await
    }

    #[tokio::test]
    async fn authenticated_sse_works_on_both_listeners_and_shutdown_is_bounded() {
        let files = CertificateFiles::new();
        let listeners = bounded(Listeners::bind(
            "127.0.0.1:0".parse().unwrap(),
            Some(&files.config),
        ))
        .await
        .unwrap();
        let http_addr = listeners.http.local_addr().unwrap();
        let https_addr = listeners.https.as_ref().unwrap().0.local_addr().unwrap();
        let state = ready_state();
        let (stop, stopped) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(listeners.serve(crate::http::router(state.clone()), async {
            let _ = stopped.await;
        }));
        let plain = bounded(tokio::net::TcpStream::connect(http_addr))
            .await
            .unwrap();
        let secure = bounded(tokio::net::TcpStream::connect(https_addr))
            .await
            .unwrap();
        let secure = bounded(
            files
                .connector()
                .connect("localhost".try_into().unwrap(), secure),
        )
        .await
        .unwrap();
        let mut plain = open_events(plain).await;
        let mut secure = open_events(secure).await;

        update_soc(&state, 52.5);
        state.shared.broadcast_snapshot();
        for event in [read_event(&mut plain).await, read_event(&mut secure).await] {
            assert_eq!(event["system"]["0/Dc/Battery/Soc"], 52.5);
        }

        // Leave both authenticated event streams open. They would keep a
        // graceful shutdown with no deadline alive indefinitely.
        stop.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(7), server)
            .await
            .expect("active SSE prevented bounded dual-listener shutdown")
            .unwrap()
            .unwrap();
        assert!(bounded(tokio::net::TcpStream::connect(http_addr))
            .await
            .is_err());
        assert!(bounded(tokio::net::TcpStream::connect(https_addr))
            .await
            .is_err());
    }
}
