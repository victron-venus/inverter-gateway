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
        mut eventloop: EventLoop,
        shared: Arc<Shared>,
        topic_prefix: String,
        mut shutdown: broadcast::Receiver<()>,
        client: AsyncClient,
        mut command_rx: tokio::sync::mpsc::UnboundedReceiver<crate::state::CommandRequest>,
    ) {
        info!("mqtt loop started");

        let portal = Self::portal_id(&topic_prefix).map(str::to_string);
        let keepalive_topic = portal.as_ref().map(|id| format!("R/{id}/keepalive"));
        let mut keepalive = tokio::time::interval(Duration::from_secs(KEEPALIVE_INTERVAL_SECS));
        // Don't fire immediately before ConnAck; first tick after connect is fine.
        keepalive.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut subscribed = false;

        loop {
            tokio::select! {
                _ = shutdown.recv() => {
                    info!("mqtt shutdown signal");
                    break;
                }
                _ = keepalive.tick() => {
                    if let Some(ref topic) = keepalive_topic {
                        if let Err(e) = client
                            .publish(topic, QoS::AtMostOnce, false, "")
                            .await
                        {
                            warn!(error = %e, "keepalive publish failed");
                        } else {
                            debug!(topic = %topic, "keepalive sent");
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
                event = eventloop.poll() => {
                    match event {
                        Ok(Event::Incoming(Packet::Publish(publish))) => {
                            let topic = publish.topic.as_str();
                            let payload = &publish.payload;
                            if let Some(update) = Self::parse(topic, payload, &topic_prefix) {
                                trace!(service = %update.service, path = %update.path, "mqtt update");
                                shared.update(update);
                            }
                        }
                        Ok(Event::Incoming(Packet::ConnAck(_))) => {
                            info!("mqtt connected");
                            *shared.mqtt_connected.write() = true;
                            if !subscribed {
                                Self::subscribe_portal(&client, &topic_prefix).await;
                                subscribed = true;
                            }
                            // Immediate keepalive so Cerbo starts dumping retained + live N/ topics.
                            if let Some(ref topic) = keepalive_topic {
                                let _ = client
                                    .publish(topic, QoS::AtMostOnce, false, "")
                                    .await;
                            }
                        }
                        Ok(Event::Incoming(Packet::PingResp)) => {}
                        Ok(Event::Incoming(Packet::Disconnect)) => {
                            info!("mqtt disconnected by broker");
                            *shared.mqtt_connected.write() = false;
                            subscribed = false;
                        }
                        Ok(Event::Incoming(other)) => {
                            debug!(packet = ?other, "mqtt packet");
                        }
                        Ok(Event::Outgoing(_)) => {}
                        Err(e) => {
                            error!(error = %e, "mqtt connection error");
                            *shared.mqtt_connected.write() = false;
                            subscribed = false;
                            tokio::time::sleep(Duration::from_secs(5)).await;
                        }
                    }
                }
            }
        }
        info!("mqtt loop stopped");
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
        let _ = self.shutdown_tx.send(());
        let _ = self.client.disconnect().await;
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
