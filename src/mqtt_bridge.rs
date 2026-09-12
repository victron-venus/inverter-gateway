use std::sync::Arc;
use std::time::Duration;

use rumqttc::{AsyncClient, Event, EventLoop, MqttOptions, Packet, QoS};
use tokio::sync::broadcast;
use tracing::{debug, error, info, trace, warn};

use crate::config::Config;
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
    pub fn start(state: Arc<crate::state::AppState>, cfg: Config) -> Self {
        let (shutdown_tx, _) = broadcast::channel(1);
        let (command_tx, command_rx) = tokio::sync::mpsc::unbounded_channel();
        let (client, eventloop) = Self::new_client(&cfg);
        let shared = state.shared.clone();

        // Give the sender to Shared so whitelist::execute can enqueue commands.
        *state.shared.command_tx.lock() = Some(command_tx.clone());

        let handle = tokio::spawn(Self::run_loop(
            eventloop,
            shared,
            cfg.topic_prefix.clone(),
            shutdown_tx.subscribe(),
            client.clone(),
            command_rx,
        ));

        MqttBridge {
            client,
            handle: parking_lot::Mutex::new(Some(handle)),
            shutdown_tx,
            command_tx,
        }
    }

    fn new_client(cfg: &Config) -> (AsyncClient, EventLoop) {
        let mut opts = MqttOptions::new(&cfg.mqtt_client_id, &cfg.mqtt_host, cfg.mqtt_port);
        opts.set_credentials(&cfg.mqtt_username, &cfg.mqtt_password);
        opts.set_keep_alive(Duration::from_secs(30));
        AsyncClient::new(opts, 256)
    }

    fn portal_id(prefix: &str) -> Option<&str> {
        prefix
            .strip_prefix("N/")
            .map(|s| s.trim_end_matches('/'))
            .filter(|s| !s.is_empty() && !s.contains('<'))
    }

    async fn subscribe_portal(client: &AsyncClient, prefix: &str) {
        // Match desktop: multi-level wildcards under each Victron service.
        // No settings/+ — thousands of unused leaves; desktop never maps them.
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
        ];
        for filter in filters {
            let full = format!("{prefix}{filter}");
            match client.subscribe(&full, QoS::AtLeastOnce).await {
                Ok(()) => debug!(topic = %full, "subscribed"),
                Err(e) => warn!(topic = %full, error = %e, "subscribe failed"),
            }
        }
    }

    async fn run_loop(
        eventloop: EventLoop,
        shared: Arc<Shared>,
        topic_prefix: String,
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
            _ = Self::poll_events(eventloop, &shared, &topic_prefix, connected_tx) => {},
            _ = Self::send_requests(&client, &topic_prefix, command_rx, connected_rx) => {},
        }
        *shared.mqtt_connected.write() = false;
        info!("mqtt loop stopped");
    }

    async fn poll_events(
        mut eventloop: EventLoop,
        shared: &Shared,
        topic_prefix: &str,
        connected: tokio::sync::watch::Sender<()>,
    ) {
        loop {
            match eventloop.poll().await {
                Ok(Event::Incoming(Packet::Publish(publish))) => {
                    if let Some(update) =
                        Self::parse(&publish.topic, &publish.payload, topic_prefix)
                    {
                        trace!(service = %update.service, path = %update.path, "mqtt update");
                        shared.update(update);
                    }
                }
                Ok(Event::Incoming(Packet::ConnAck(_))) => {
                    info!("mqtt connected");
                    *shared.mqtt_connected.write() = true;
                    // Wake the producer without blocking the queue's consumer.
                    connected.send_replace(());
                }
                Ok(Event::Incoming(Packet::Disconnect)) => {
                    info!("mqtt disconnected by broker");
                    *shared.mqtt_connected.write() = false;
                }
                Ok(Event::Incoming(Packet::PingResp)) | Ok(Event::Outgoing(_)) => {}
                Ok(Event::Incoming(other)) => debug!(packet = ?other, "mqtt packet"),
                Err(e) => {
                    error!(error = %e, "mqtt connection error");
                    *shared.mqtt_connected.write() = false;
                    tokio::time::sleep(Duration::from_secs(5)).await;
                }
            }
        }
    }

    async fn send_requests(
        client: &AsyncClient,
        topic_prefix: &str,
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
                    Self::subscribe_portal(client, topic_prefix).await;
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
                        if let Err(e) = client.publish(&topic, QoS::AtLeastOnce, false, payload.as_bytes()).await {
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

        let payload_val: serde_json::Value = serde_json::from_slice(payload).ok()?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    async fn read_packet(socket: &mut tokio::net::TcpStream) -> std::io::Result<(u8, Vec<u8>)> {
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
                while subscriptions < 11 || !command || !keepalive {
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
            if round == 0 {
                drop(socket);
                tokio::time::timeout(Duration::from_secs(1), async {
                    while *shared.mqtt_connected.read() {
                        tokio::task::yield_now().await;
                    }
                })
                .await
                .unwrap();
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
        assert!(!*shared.mqtt_connected.read());
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
}
