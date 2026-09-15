use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use rumqttc::{AsyncClient, Event, EventLoop, MqttOptions, Packet, QoS, Transport};
use rustls::{ClientConfig, RootCertStore};
use tokio::sync::broadcast;
use tracing::{debug, error, info, trace, warn};

use crate::config::{Config, ConfigError};
use crate::state::{ParsedUpdate, Shared};

/// Victron GX drops N/ publishes unless clients periodically ping R/<portal>/keepalive.
const KEEPALIVE_INTERVAL_SECS: u64 = 45;

pub struct MqttBridge {
    client: AsyncClient,
    handle: parking_lot::Mutex<Option<tokio::task::JoinHandle<()>>>,
    shutdown_tx: broadcast::Sender<()>,
    /// Held to keep the channel alive even if unused externally.
    #[allow(dead_code)]
    command_tx: tokio::sync::mpsc::UnboundedSender<crate::state::CommandRequest>,
}

impl MqttBridge {
    pub fn start(state: Arc<crate::state::AppState>, cfg: Config) -> Result<Self, ConfigError> {
        // Validate TLS material before spawning the reconnect loop or serving requests.
        let (client, eventloop) = Self::new_client(&cfg)?;
        let (shutdown_tx, _) = broadcast::channel(1);
        let (command_tx, command_rx) = tokio::sync::mpsc::unbounded_channel();
        let shared = state.shared.clone();

        // Give the sender to Shared so whitelist::execute can enqueue commands.
        *state.shared.command_tx.lock() = Some(command_tx.clone());

        let handle = tokio::spawn(Self::run_loop(
            eventloop,
            shared,
            cfg.topic_prefix.clone(),
            cfg.inverter_topic_prefix.clone(),
            shutdown_tx.subscribe(),
            client.clone(),
            command_rx,
        ));

        Ok(MqttBridge {
            client,
            handle: parking_lot::Mutex::new(Some(handle)),
            shutdown_tx,
            command_tx,
        })
    }

    fn new_client(cfg: &Config) -> Result<(AsyncClient, EventLoop), ConfigError> {
        let mut opts = MqttOptions::new(&cfg.mqtt_client_id, &cfg.mqtt_host, cfg.mqtt_port);
        opts.set_credentials(&cfg.mqtt_username, &cfg.mqtt_password);
        opts.set_keep_alive(Duration::from_secs(30));
        opts.set_max_packet_size(1024 * 1024, 64 * 1024);
        opts.set_transport(mqtt_transport(cfg.mqtt_tls, cfg.mqtt_ca_file.as_deref())?);
        Ok(AsyncClient::new(opts, 256))
    }

    fn portal_id(prefix: &str) -> Option<&str> {
        prefix
            .strip_prefix("N/")
            .map(|s| s.trim_end_matches('/'))
            .filter(|s| !s.is_empty() && !s.contains('<'))
    }

    async fn subscribe_portal(client: &AsyncClient, prefix: &str, inverter_prefix: &str) {
        // Match desktop: multi-level wildcards under each Victron service.
        // Subscribe only to the two ESS settings, not thousands of unused leaves.
        let filters = [
            "system/+/#",
            "vebus/+/#",
            "battery/+/#",
            "solarcharger/+/#",
            "pvinverter/+/#",
            "tank/+/#",
            "pump/+/#",
            "ev/+/#",
            "evcharger/+/#",
            "acload/+/#",
            // GUIv2 / Venus-platform notification slots (AcknowledgeAll target).
            "platform/+/#",
            "settings/+/Settings/CGwacs/Hub4Mode",
            "settings/+/Settings/CGwacs/BatteryLife/State",
        ];
        for filter in filters {
            let full = format!("{prefix}{filter}");
            match client.subscribe(&full, QoS::AtLeastOnce).await {
                Ok(()) => debug!(topic = %full, "subscribed"),
                Err(e) => warn!(topic = %full, error = %e, "subscribe failed"),
            }
        }
        if let Err(e) = client
            .subscribe(format!("{inverter_prefix}/state"), QoS::AtLeastOnce)
            .await
        {
            warn!(error = %e, "inverter-control subscribe failed");
        }
    }

    async fn run_loop(
        eventloop: EventLoop,
        shared: Arc<Shared>,
        topic_prefix: String,
        inverter_prefix: String,
        mut shutdown: broadcast::Receiver<()>,
        client: AsyncClient,
        command_rx: tokio::sync::mpsc::UnboundedReceiver<crate::state::CommandRequest>,
    ) {
        info!("mqtt loop started");
        let (connected_tx, connected_rx) = tokio::sync::watch::channel(());
        // Polling must continue while a producer waits for room in rumqttc's
        // bounded request queue. Keep both futures alive until shutdown rather
        // than cancelling an in-progress poll whenever a command arrives.
        // Dropping this select cancels both futures; no worker task is detached.
        tokio::select! {
            _ = shutdown.recv() => info!("mqtt shutdown signal"),
            _ = Self::poll_events(eventloop, &shared, &topic_prefix, &inverter_prefix, connected_tx) => {},
            _ = Self::send_requests(&client, &topic_prefix, &inverter_prefix, command_rx, connected_rx) => {},
        }
        shared.set_connected(false);
        info!("mqtt loop stopped");
    }

    async fn poll_events(
        mut eventloop: EventLoop,
        shared: &Shared,
        topic_prefix: &str,
        inverter_prefix: &str,
        connected: tokio::sync::watch::Sender<()>,
    ) {
        loop {
            match eventloop.poll().await {
                Ok(Event::Incoming(Packet::Publish(publish))) => {
                    if publish.topic == format!("{inverter_prefix}/state") {
                        if let Some(value) = Self::parse_inverter(&publish.payload) {
                            shared.update_inverter(value);
                        }
                        continue;
                    }
                    if let Some(update) =
                        Self::parse(&publish.topic, &publish.payload, topic_prefix)
                    {
                        trace!(service = %update.service, path = %update.path, "mqtt update");
                        shared.update(update);
                    }
                }
                Ok(Event::Incoming(Packet::ConnAck(_))) => {
                    info!("mqtt connected");
                    shared.set_connected(true);
                    // Wake the producer without blocking the queue's consumer.
                    connected.send_replace(());
                }
                Ok(Event::Incoming(Packet::Disconnect)) => {
                    info!("mqtt disconnected by broker");
                    shared.set_connected(false);
                }
                Ok(Event::Incoming(Packet::PingResp)) | Ok(Event::Outgoing(_)) => {}
                Ok(Event::Incoming(other)) => debug!(packet = ?other, "mqtt packet"),
                Err(e) => {
                    error!(error = %e, "mqtt connection error");
                    shared.set_connected(false);
                    tokio::time::sleep(Duration::from_secs(5)).await;
                }
            }
        }
    }

    async fn send_requests(
        client: &AsyncClient,
        topic_prefix: &str,
        inverter_prefix: &str,
        mut command_rx: tokio::sync::mpsc::UnboundedReceiver<crate::state::CommandRequest>,
        mut connected: tokio::sync::watch::Receiver<()>,
    ) {
        let keepalive_topic = Self::portal_id(topic_prefix).map(|id| format!("R/{id}/keepalive"));
        let mut keepalive = tokio::time::interval(Duration::from_secs(KEEPALIVE_INTERVAL_SECS));
        keepalive.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                changed = connected.changed() => {
                    if changed.is_err() {
                        break;
                    }
                    Self::subscribe_portal(client, topic_prefix, inverter_prefix).await;
                    if let Some(ref topic) = keepalive_topic {
                        let _ = client.publish(topic, QoS::AtMostOnce, false, "").await;
                    }
                }
                _ = keepalive.tick() => {
                    if let Some(ref topic) = keepalive_topic {
                        if let Err(e) = client.publish(topic, QoS::AtMostOnce, false, "").await {
                            warn!(error = %e, "keepalive publish failed");
                        }
                    }
                }
                cmd = command_rx.recv() => {
                    if let Some((topic, payload)) = cmd {
                        debug!(topic = %topic, "publishing command");
                        // The daemon's legacy ESS action toggles; do not request
                        // QoS 1 redelivery for this non-idempotent command.
                        let qos = if topic == format!("{inverter_prefix}/cmd/ess_mode") {
                            QoS::AtMostOnce
                        } else {
                            QoS::AtLeastOnce
                        };
                        if let Err(e) = client.publish(&topic, qos, false, payload.as_bytes()).await {
                            warn!(error = %e, "command publish failed");
                        }
                    } else {
                        info!("command channel closed");
                        break;
                    }
                }
            }
        }
    }

    fn parse_inverter(payload: &[u8]) -> Option<serde_json::Value> {
        if payload.is_empty() {
            return Some(serde_json::Value::Null);
        }
        let value: serde_json::Value = serde_json::from_slice(payload).ok()?;
        (value.is_object() || value.is_null()).then_some(value)
    }

    /// Parse a Victron MQTT topic+payload into a ParsedUpdate.
    /// Topics look like: `N/<portal>/system/0/Dc/Battery/Soc` with payload `{"value": 55.2}`.
    fn parse(topic: &str, payload: &[u8], prefix: &str) -> Option<ParsedUpdate> {
        let stripped = topic.strip_prefix(prefix)?;
        if stripped.is_empty() {
            return None;
        }
        let (service, path) = match stripped.split_once('/') {
            Some((svc, rest)) => (svc.to_string(), rest.to_string()),
            None => (stripped.to_string(), String::new()),
        };
        if service.is_empty() {
            return None;
        }

        // MQTT retained-message removal has an empty payload.
        let payload_val: serde_json::Value = if payload.is_empty() {
            serde_json::Value::Null
        } else {
            serde_json::from_slice(payload).ok()?
        };
        let value = payload_val.get("value").cloned().unwrap_or(payload_val);

        Some(ParsedUpdate {
            service,
            path,
            value,
        })
    }

    /// Publish an MQTT message. Used by command execution.
    #[allow(dead_code)]
    pub async fn publish(&self, topic: &str, payload: &str) -> Result<(), String> {
        self.client
            .publish(topic, QoS::AtLeastOnce, false, payload.as_bytes())
            .await
            .map_err(|e| e.to_string())
    }

    pub async fn shutdown(&self) {
        // Best effort: never wait for queue space while stopping its consumer.
        let _ = self.client.try_disconnect();
        let _ = self.shutdown_tx.send(());
        let handle = { self.handle.lock().take() };
        if let Some(h) = handle {
            let _ = h.await;
        }
    }
}

fn mqtt_transport(tls: bool, ca_file: Option<&Path>) -> Result<Transport, ConfigError> {
    if !tls {
        if ca_file.is_some() {
            return Err("MQTT_CA_FILE requires MQTT_TLS=1".into());
        }
        return Ok(Transport::Tcp);
    }

    let roots = if let Some(path) = ca_file {
        let pem = std::fs::read(path)
            .map_err(|e| ConfigError::from(format!("cannot read MQTT_CA_FILE: {e}")))?;
        mqtt_ca_roots(&pem)?
    } else {
        let certs = rustls_native_certs::load_native_certs();
        let mut roots = RootCertStore::empty();
        roots.add_parsable_certificates(certs.certs);
        if roots.is_empty() {
            return Err(
                "no system CA certificates available for MQTT TLS; set MQTT_CA_FILE".into(),
            );
        }
        roots
    };
    Ok(verified_mqtt_tls(roots))
}

fn mqtt_ca_roots(pem: &[u8]) -> Result<RootCertStore, ConfigError> {
    let mut roots = RootCertStore::empty();
    for cert in rustls_pemfile::certs(&mut std::io::Cursor::new(pem)) {
        let cert = cert.map_err(|e| ConfigError::from(format!("invalid MQTT_CA_FILE PEM: {e}")))?;
        roots
            .add(cert)
            .map_err(|e| ConfigError::from(format!("invalid MQTT_CA_FILE certificate: {e}")))?;
    }
    if roots.is_empty() {
        return Err("MQTT_CA_FILE contains no CA certificates".into());
    }
    Ok(roots)
}

fn verified_mqtt_tls(roots: RootCertStore) -> Transport {
    // Rustls's standard verifier checks both the trust chain and MQTT_HOST.
    let config = ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    Transport::tls_with_config(config.into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    async fn read_packet<S: tokio::io::AsyncRead + Unpin>(
        socket: &mut S,
    ) -> std::io::Result<(u8, Vec<u8>)> {
        use tokio::io::AsyncReadExt;
        let header = socket.read_u8().await?;
        let mut length = 0;
        let mut multiplier = 1;
        loop {
            let byte = socket.read_u8().await?;
            length += usize::from(byte & 127) * multiplier;
            if byte & 128 == 0 {
                break;
            }
            multiplier *= 128;
        }
        let mut body = vec![0; length];
        socket.read_exact(&mut body).await?;
        Ok((header, body))
    }

    fn test_cfg(port: u16, ca_file: Option<std::path::PathBuf>) -> Config {
        Config {
            mqtt_host: "127.0.0.1".into(),
            mqtt_port: port,
            mqtt_tls: true,
            mqtt_ca_file: ca_file,
            mqtt_username: "tls-test-user".into(),
            mqtt_password: "tls-test-password".into(),
            mqtt_client_id: "tls-test".into(),
            http_bind: "127.0.0.1:0".parse().unwrap(),
            https: None,
            api_token: Some("test-token".into()),
            read_token: None,
            energy: Default::default(),
            topic_prefix: "N/test/".into(),
            write_topic_prefix: "W/test/".into(),
            inverter_topic_prefix: "inverter".into(),
            allow_insecure: false,
            cors_origins: Vec::new(),
        }
    }

    #[test]
    fn mqtt_ca_errors_fail_before_the_reconnect_loop() {
        let directory = tempfile::tempdir().unwrap();
        let ca_path = directory.path().join("ca.pem");
        let cfg = test_cfg(8883, Some(ca_path.clone()));
        assert!(MqttBridge::new_client(&cfg).is_err());
        for content in [
            "",
            "this is not a certificate",
            "-----BEGIN CERTIFICATE-----\ninvalid!\n-----END CERTIFICATE-----",
            "-----BEGIN CERTIFICATE-----\nAQID\n-----END CERTIFICATE-----",
        ] {
            std::fs::write(&ca_path, content).unwrap();
            assert!(MqttBridge::new_client(&cfg).is_err());
        }
        assert!(mqtt_transport(false, Some(&ca_path)).is_err());
    }

    #[test]
    fn mqtt_plain_tcp_remains_explicit_and_compatible() {
        let mut cfg = test_cfg(1883, None);
        cfg.mqtt_tls = false;
        let (_, eventloop) = MqttBridge::new_client(&cfg).unwrap();
        assert!(matches!(eventloop.mqtt_options.transport(), Transport::Tcp));
    }

    async fn mqtt_tls_handshake(
        server_name: &str,
        trusted: bool,
    ) -> Result<(), Box<rumqttc::ConnectionError>> {
        use rustls::pki_types::{PrivateKeyDer, PrivatePkcs8KeyDer};
        use tokio::io::AsyncWriteExt;

        let identity = rcgen::generate_simple_self_signed(vec![server_name.into()]).unwrap();
        let server_config = rustls::ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(
                vec![identity.cert.der().clone()],
                PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(identity.key_pair.serialize_der())),
            )
            .unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let directory = tempfile::tempdir().unwrap();
        let ca_path = directory.path().join("ca.pem");
        let ca = if trusted {
            identity.cert.pem()
        } else {
            rcgen::generate_simple_self_signed(vec!["other-ca.example".into()])
                .unwrap()
                .cert
                .pem()
        };
        std::fs::write(&ca_path, ca).unwrap();
        let cfg = test_cfg(listener.local_addr().unwrap().port(), Some(ca_path));
        let (_client, mut eventloop) = MqttBridge::new_client(&cfg).unwrap();
        assert!(matches!(
            eventloop.mqtt_options.transport(),
            Transport::Tls(_)
        ));

        let server = async {
            let (socket, _) = listener.accept().await.unwrap();
            let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(server_config));
            let Ok(mut socket) = acceptor.accept(socket).await else {
                return false;
            };
            let (header, body) = read_packet(&mut socket).await.unwrap();
            assert_eq!(header, 0x10);
            // The authenticated CONNECT is only readable after verified TLS.
            for credential in [b"tls-test-user".as_slice(), b"tls-test-password"] {
                assert!(body
                    .windows(credential.len())
                    .any(|window| window == credential));
            }
            socket.write_all(&[0x20, 2, 0, 0]).await.unwrap();
            true
        };
        let client = async {
            loop {
                match eventloop.poll().await {
                    Ok(Event::Incoming(Packet::ConnAck(_))) => return Ok(()),
                    Ok(_) => {}
                    Err(error) => return Err(Box::new(error)),
                }
            }
        };
        let (connected, result) = tokio::time::timeout(Duration::from_secs(5), async {
            tokio::join!(server, client)
        })
        .await
        .expect("MQTT TLS handshake timed out");
        assert_eq!(connected, result.is_ok());
        result
    }

    fn certificate_error(error: &rumqttc::ConnectionError) -> &rustls::CertificateError {
        let tls_error = match error {
            rumqttc::ConnectionError::Tls(rumqttc::TlsError::TLS(error)) => Some(error),
            rumqttc::ConnectionError::Tls(rumqttc::TlsError::Io(error)) => error
                .get_ref()
                .and_then(|error| error.downcast_ref::<rustls::Error>()),
            _ => None,
        };
        match tls_error {
            Some(rustls::Error::InvalidCertificate(error)) => error,
            _ => panic!("expected a broker certificate verification failure, got {error:?}"),
        }
    }

    #[tokio::test]
    async fn mqtt_tls_authenticates_over_a_trusted_connection() {
        mqtt_tls_handshake("127.0.0.1", true).await.unwrap();
    }

    #[tokio::test]
    async fn mqtt_tls_rejects_untrusted_broker_before_connect_credentials() {
        let error = mqtt_tls_handshake("127.0.0.1", false).await.unwrap_err();
        assert!(matches!(
            certificate_error(&error),
            rustls::CertificateError::UnknownIssuer | rustls::CertificateError::BadSignature
        ));
    }

    #[tokio::test]
    async fn mqtt_tls_rejects_wrong_broker_name_before_connect_credentials() {
        let error = mqtt_tls_handshake("wrong-broker.example", true)
            .await
            .unwrap_err();
        assert!(matches!(
            certificate_error(&error),
            rustls::CertificateError::NotValidForName
                | rustls::CertificateError::NotValidForNameContext { .. }
        ));
    }

    #[tokio::test]
    async fn small_outbound_queue_keeps_polling_and_shuts_down() {
        use tokio::io::AsyncWriteExt;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let opts = MqttOptions::new(
            "queue-test",
            "127.0.0.1",
            listener.local_addr().unwrap().port(),
        );
        let (client, eventloop) = AsyncClient::new(opts, 1);
        let shared = Shared::new();
        let (shutdown_tx, _) = broadcast::channel(1);
        let (command_tx, command_rx) = tokio::sync::mpsc::unbounded_channel();
        let handle = tokio::spawn(MqttBridge::run_loop(
            eventloop,
            shared.clone(),
            "N/test/".into(),
            "inverter".into(),
            shutdown_tx.subscribe(),
            client.clone(),
            command_rx,
        ));
        let mut traffic_ok = true;
        for round in 0..2 {
            let mut socket = tokio::time::timeout(Duration::from_secs(8), async {
                loop {
                    let (mut socket, _) = listener.accept().await.unwrap();
                    if matches!(read_packet(&mut socket).await, Ok((0x10, _))) {
                        break socket;
                    }
                }
            })
            .await
            .unwrap();
            socket.write_all(&[0x20, 2, 0, 0]).await.unwrap();
            let traffic = tokio::time::timeout(Duration::from_secs(2), async {
                let mut subscriptions = 0;
                let mut command = false;
                let mut keepalive = false;
                while subscriptions < 14 || !command || !keepalive {
                    let (header, body) = read_packet(&mut socket).await.unwrap();
                    match header {
                        0x82 => {
                            assert_eq!(*body.last().unwrap(), 1);
                            subscriptions += 1;
                            if subscriptions == 1 && round == 0 {
                                command_tx
                                    .send((
                                        "W/test/vebus/0/Alarm".into(),
                                        "{\"SilenceAlarm\":\"1\"}".into(),
                                    ))
                                    .unwrap();
                            }
                            socket
                                .write_all(&[0x90, 3, body[0], body[1], 1])
                                .await
                                .unwrap();
                        }
                        0x30 | 0x32 => {
                            let len = usize::from(u16::from_be_bytes([body[0], body[1]]));
                            let topic = std::str::from_utf8(&body[2..2 + len]).unwrap();
                            if header == 0x32 {
                                assert_eq!(topic, "W/test/vebus/0/Alarm");
                                assert_eq!(&body[4 + len..], b"{\"SilenceAlarm\":\"1\"}");
                                socket
                                    .write_all(&[0x40, 2, body[2 + len], body[3 + len]])
                                    .await
                                    .unwrap();
                                command = true;
                            } else {
                                assert_eq!(topic, "R/test/keepalive");
                                assert_eq!(body.len(), 2 + len);
                                keepalive = true;
                            }
                        }
                        other => panic!("Unexpected MQTT packet {other:x}"),
                    }
                }
            })
            .await;
            traffic_ok &= traffic.is_ok();
            if !traffic_ok {
                break;
            }
            // ConnAck alone cannot revive values retained from the previous session.
            assert!(shared.is_connected());
            assert!(shared.snapshot().is_none());
            let topic = b"N/test/battery/0/Dc/0/Voltage";
            let payload = format!("{{\"value\":{}}}", 52 - round);
            let mut packet = vec![0x30, (2 + topic.len() + payload.len()) as u8];
            packet.extend_from_slice(&(topic.len() as u16).to_be_bytes());
            packet.extend_from_slice(topic);
            packet.extend_from_slice(payload.as_bytes());
            socket.write_all(&packet).await.unwrap();
            tokio::time::timeout(Duration::from_secs(1), async {
                while shared.snapshot().is_none() {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
            assert_eq!(
                shared.snapshot().unwrap().battery["0/Dc/0/Voltage"],
                52 - round
            );
            // The retained daemon envelope is independent of N/<portal> leaves.
            let topic = b"inverter/state";
            let payload = b"{\"booleans\":{\"only_charging\":true}}";
            let mut packet = vec![0x31, (2 + topic.len() + payload.len()) as u8];
            packet.extend_from_slice(&(topic.len() as u16).to_be_bytes());
            packet.extend_from_slice(topic);
            packet.extend_from_slice(payload);
            socket.write_all(&packet).await.unwrap();
            tokio::time::timeout(Duration::from_secs(1), async {
                while shared.snapshot().unwrap().inverter.is_none() {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
            assert_eq!(
                shared.snapshot().unwrap().inverter.unwrap()["booleans"]["only_charging"],
                true
            );
            if round == 0 {
                drop(socket);
                tokio::time::timeout(Duration::from_secs(1), async {
                    while shared.is_connected() {
                        tokio::task::yield_now().await;
                    }
                })
                .await
                .unwrap();
                assert!(shared.snapshot().is_none());
                assert!(shared.subscribe().is_none());
                // This request arrives during the reconnect delay and must
                // survive until the replacement broker connection is ready.
                command_tx
                    .send((
                        "W/test/vebus/0/Alarm".into(),
                        "{\"SilenceAlarm\":\"1\"}".into(),
                    ))
                    .unwrap();
            }
        }
        shutdown_tx.send(()).unwrap();
        let mut handle = handle;
        let stopped = tokio::time::timeout(Duration::from_secs(1), &mut handle).await;
        if stopped.is_err() {
            handle.abort();
            let _ = handle.await;
        }
        assert!(traffic_ok, "outbound queue prevented event-loop progress");
        assert!(stopped.is_ok(), "blocked enqueue prevented shutdown");
        assert!(!shared.is_connected());
    }

    #[test]
    fn controller_payload_accepts_objects_and_retained_removal_only() {
        for payload in [b"".as_slice(), b"null"] {
            assert_eq!(
                MqttBridge::parse_inverter(payload),
                Some(serde_json::Value::Null)
            );
        }
        assert!(
            MqttBridge::parse_inverter(b"{\"booleans\":{\"no_feed\":false}}")
                .unwrap()
                .is_object()
        );
        for invalid in [b"[]".as_slice(), b"false", b"1", b"invalid"] {
            assert!(MqttBridge::parse_inverter(invalid).is_none());
        }
    }

    #[tokio::test]
    async fn shutdown_cancels_enqueue_while_broker_withholds_connack() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let opts = MqttOptions::new(
            "shutdown-test",
            "127.0.0.1",
            listener.local_addr().unwrap().port(),
        );
        let (client, eventloop) = AsyncClient::new(opts, 1);
        client
            .try_publish("W/test/vebus/0/Alarm", QoS::AtLeastOnce, false, "{}")
            .unwrap();
        let (shutdown_tx, _) = broadcast::channel(1);
        let (command_tx, command_rx) = tokio::sync::mpsc::unbounded_channel();
        let shared = Shared::new();
        let handle = tokio::spawn(MqttBridge::run_loop(
            eventloop,
            shared,
            "N/test/".into(),
            "inverter".into(),
            shutdown_tx.subscribe(),
            client.clone(),
            command_rx,
        ));
        let bridge = MqttBridge {
            client,
            handle: parking_lot::Mutex::new(Some(handle)),
            shutdown_tx,
            command_tx,
        };
        let mut socket = tokio::time::timeout(Duration::from_secs(2), async {
            let (mut socket, _) = listener.accept().await.unwrap();
            assert_eq!(read_packet(&mut socket).await.unwrap().0, 0x10);
            socket
        })
        .await
        .unwrap();
        // Leave the broker handshake incomplete and the outbound queue full.
        assert!(bridge
            .client
            .try_publish("R/test/keepalive", QoS::AtMostOnce, false, "")
            .is_err());
        let stopped = tokio::time::timeout(Duration::from_secs(1), bridge.shutdown()).await;
        if stopped.is_err() {
            if let Some(handle) = bridge.handle.lock().take() {
                handle.abort();
            }
        }
        assert!(
            stopped.is_ok(),
            "shutdown waited for outbound queue capacity"
        );
        // Reader and producer are gone, so the loopback connection closes.
        assert!(
            tokio::time::timeout(Duration::from_secs(1), read_packet(&mut socket))
                .await
                .unwrap()
                .is_err()
        );
    }

    #[test]
    fn parse_value_payload() {
        let prefix = "N/abc123/";
        let topic = "N/abc123/system/0/Dc/Battery/Soc";
        let payload = br#"{"value": 77.5}"#;
        let u = MqttBridge::parse(topic, payload, prefix).expect("parse");
        assert_eq!(u.service, "system");
        assert_eq!(u.path, "0/Dc/Battery/Soc");
        assert_eq!(u.value, json!(77.5));
    }

    #[test]
    fn parse_rejects_trailing_slash_requirement() {
        // Regression: old code required topic.endswith('/') and dropped everything.
        let prefix = "N/abc123/";
        let topic = "N/abc123/vebus/0/Ac/Out/L1/P";
        let payload = br#"{"value": 1200}"#;
        assert!(MqttBridge::parse(topic, payload, prefix).is_some());
    }

    #[test]
    fn parse_wrong_prefix() {
        assert!(MqttBridge::parse("N/other/system/0/X", b"{\"value\":1}", "N/abc123/").is_none());
    }

    #[test]
    fn portal_id_from_prefix() {
        assert_eq!(
            MqttBridge::portal_id("N/b827ebea1ece/"),
            Some("b827ebea1ece")
        );
        assert_eq!(MqttBridge::portal_id("N/<portal_id>/"), None);
    }

    #[test]
    fn empty_retained_message_and_json_null_invalidate_a_leaf() {
        for payload in [b"".as_slice(), br#"{"value":null}"#, b"null"] {
            let update =
                MqttBridge::parse("N/test/system/0/Dc/Battery/Soc", payload, "N/test/").unwrap();
            assert!(update.value.is_null());
        }
        assert!(
            MqttBridge::parse("N/test/system/0/Dc/Battery/Soc", b"invalid", "N/test/").is_none()
        );
    }
}
