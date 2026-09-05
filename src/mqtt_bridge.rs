use std::sync::Arc;
use std::time::Duration;

use rumqttc::{AsyncClient, Event, EventLoop, MqttOptions, Packet, QoS};
use tokio::sync::broadcast;
use tracing::{debug, error, info, warn};

use crate::config::Config;
use crate::state::{ParsedUpdate, Shared};

pub struct MqttBridge {
    client: AsyncClient,
    handle: parking_lot::Mutex<Option<tokio::task::JoinHandle<()>>>,
    shutdown_tx: broadcast::Sender<()>,
}

impl MqttBridge {
    pub fn start(state: Arc<crate::state::AppState>, cfg: Config) -> Self {
        let (shutdown_tx, _) = broadcast::channel(1);
        let (client, eventloop) = Self::new_client(&cfg);
        let shared = state.shared.clone();

        let handle = tokio::spawn(Self::run_loop(
            eventloop,
            shared,
            cfg.topic_prefix.clone(),
            shutdown_tx.subscribe(),
            client.clone(),
        ));

        MqttBridge {
            client,
            handle: parking_lot::Mutex::new(Some(handle)),
            shutdown_tx,
        }
    }

    fn new_client(cfg: &Config) -> (AsyncClient, EventLoop) {
        let mut opts = MqttOptions::new(&cfg.mqtt_client_id, &cfg.mqtt_host, cfg.mqtt_port);
        opts.set_credentials(&cfg.mqtt_username, &cfg.mqtt_password);
        opts.set_keep_alive(Duration::from_secs(30));
        AsyncClient::new(opts, 256)
    }

    async fn run_loop(
        mut eventloop: EventLoop,
        shared: Arc<Shared>,
        topic_prefix: String,
        mut shutdown: broadcast::Receiver<()>,
        client: AsyncClient,
    ) {
        info!("mqtt loop started");

        // Subscribe to Victron services
        let topics = [
            "system",
            "vebus/+",
            "solarcharger/+",
            "tank/+",
            "settings/+",
        ];
        for topic in &topics {
            let full = format!("{}{}", topic_prefix, topic);
            let client = client.clone();
            tokio::spawn(async move {
                if let Err(e) = client.subscribe(&full, QoS::AtLeastOnce).await {
                    warn!(topic = %full, error = %e, "subscribe failed");
                } else {
                    debug!(topic = %full, "subscribed");
                }
            });
        }

        loop {
            tokio::select! {
                _ = shutdown.recv() => {
                    info!("mqtt shutdown signal");
                    break;
                }
                event = eventloop.poll() => {
                    match event {
                        Ok(Event::Incoming(Packet::Publish(publish))) => {
                            let topic = publish.topic.as_str();
                            let payload = &publish.payload;
                            if let Some(update) = Self::parse(topic, payload, &topic_prefix) {
                                debug!(service = %update.service, "mqtt update");
                                shared.update(update);
                            }
                        }
                        Ok(Event::Incoming(Packet::ConnAck(_))) => {
                            info!("mqtt connected");
                        }
                        Ok(Event::Incoming(Packet::PingResp)) => {}
                        Ok(Event::Incoming(other)) => {
                            debug!(packet = ?other, "mqtt packet");
                        }
                        Ok(Event::Outgoing(_)) => {}
                        Err(e) => {
                            error!(error = %e, "mqtt connection error");
                            tokio::time::sleep(Duration::from_secs(5)).await;
                        }
                    }
                }
            }
        }
        info!("mqtt loop stopped");
    }

    /// Parse a Victron MQTT topic+payload into a ParsedUpdate.
    /// Topics look like: "N/%instance%/system" or "N/%instance%/vebus/0/Ac/ActiveIn/L1/P"
    fn parse(topic: &str, payload: &[u8], prefix: &str) -> Option<ParsedUpdate> {
        let stripped = topic.strip_prefix(prefix)?.strip_suffix('/')?;
        let slash_pos = stripped.find('/')?;
        let service = stripped[..slash_pos].to_string();
        let rest = &stripped[slash_pos + 1..];

        let values: serde_json::Value = serde_json::from_slice(payload).ok()?;
        let values = match values {
            serde_json::Value::Object(map) => serde_json::Value::Object(map),
            other => {
                let mut map = serde_json::Map::new();
                map.insert(rest.to_string(), other);
                serde_json::Value::Object(map)
            }
        };

        Some(ParsedUpdate { service, values })
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
        // ponytail: take handle outside the lock before awaiting
        let handle = { self.handle.lock().take() };
        if let Some(h) = handle {
            let _ = h.await;
        }
    }
}
