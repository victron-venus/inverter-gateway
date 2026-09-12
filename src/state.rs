use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use parking_lot::RwLock;
use serde_json::Value;
use tokio::sync::{broadcast, mpsc};

use crate::config::Config;

/// Aggregated Cerbo MQTT leaf values, keyed by path under each service
/// (e.g. system["0/Dc/Battery/Soc"] = 77.5).
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct Snapshot {
    #[serde(skip_serializing_if = "HashMap::is_empty")]
    pub system: HashMap<String, Value>,
    #[serde(skip_serializing_if = "HashMap::is_empty")]
    pub vebus: HashMap<String, Value>,
    #[serde(skip_serializing_if = "HashMap::is_empty")]
    pub battery: HashMap<String, Value>,
    #[serde(skip_serializing_if = "HashMap::is_empty")]
    pub solarcharger: HashMap<String, Value>,
    #[serde(skip_serializing_if = "HashMap::is_empty")]
    pub pvinverter: HashMap<String, Value>,
    #[serde(skip_serializing_if = "HashMap::is_empty")]
    pub tank: HashMap<String, Value>,
    #[serde(skip_serializing_if = "HashMap::is_empty")]
    pub pump: HashMap<String, Value>,
    #[serde(skip_serializing_if = "HashMap::is_empty")]
    pub ev: HashMap<String, Value>,
    #[serde(skip_serializing_if = "HashMap::is_empty")]
    pub evcharger: HashMap<String, Value>,
    #[serde(skip_serializing_if = "HashMap::is_empty")]
    pub acload: HashMap<String, Value>,
    /// Venus-platform notification slots (GUIv2 Notifications/[0-19]).
    #[serde(skip_serializing_if = "HashMap::is_empty")]
    pub platform: HashMap<String, Value>,
    #[serde(skip_serializing_if = "HashMap::is_empty")]
    pub settings: HashMap<String, Value>,
}

#[derive(Default)]
struct Telemetry {
    snapshot: Snapshot,
    connected: bool,
    ready: bool,
    generation: u64,
    dirty: bool,
}

#[derive(Clone)]
pub struct SnapshotEvent {
    pub generation: u64,
    pub snapshot: Option<Snapshot>,
}

pub struct Shared {
    telemetry: RwLock<Telemetry>,
    sse_tx: broadcast::Sender<SnapshotEvent>,
    pub command_tx: parking_lot::Mutex<Option<mpsc::UnboundedSender<CommandRequest>>>,
}

pub type CommandRequest = (String, String); // (topic, payload)

impl Shared {
    pub fn new() -> Arc<Self> {
        let (sse_tx, _) = broadcast::channel(64);
        Arc::new(Shared {
            telemetry: RwLock::new(Telemetry::default()),
            sse_tx,
            command_tx: parking_lot::Mutex::new(None),
        })
    }

    pub fn is_connected(&self) -> bool {
        self.telemetry.read().connected
    }

    pub fn set_connected(&self, connected: bool) {
        let mut telemetry = self.telemetry.write();
        if telemetry.connected == connected {
            return;
        }
        telemetry.connected = connected;
        if !connected {
            telemetry.generation += 1;
            telemetry.snapshot = Snapshot::default();
            telemetry.ready = false;
            telemetry.dirty = false;
            // Send under the same lock as coalescing: no old clone can follow this event.
            let _ = self.sse_tx.send(SnapshotEvent {
                generation: telemetry.generation,
                snapshot: None,
            });
        }
    }

    pub fn snapshot(&self) -> Option<Snapshot> {
        let telemetry = self.telemetry.read();
        (telemetry.connected && telemetry.ready).then(|| telemetry.snapshot.clone())
    }

    pub fn subscribe(&self) -> Option<(u64, broadcast::Receiver<SnapshotEvent>)> {
        let telemetry = self.telemetry.read();
        let rx = self.sse_tx.subscribe();
        (telemetry.connected && telemetry.ready).then_some((telemetry.generation, rx))
    }

    pub fn generation_is_connected(&self, generation: u64) -> bool {
        let telemetry = self.telemetry.read();
        telemetry.connected && telemetry.generation == generation
    }

    /// Apply only whitelisted leaves received during the current connection.
    pub fn update(&self, update: ParsedUpdate) {
        if !path_keep(&update.service, &update.path) {
            return;
        }
        let mut telemetry = self.telemetry.write();
        if !telemetry.connected {
            return;
        }
        apply_update(&mut telemetry.snapshot, update);
        telemetry.ready = true;
        telemetry.dirty = self.sse_tx.receiver_count() > 0;
    }

    pub(crate) fn broadcast_snapshot(&self) {
        let mut telemetry = self.telemetry.write();
        if !telemetry.connected || !telemetry.dirty {
            return;
        }
        telemetry.dirty = false;
        if self.sse_tx.receiver_count() > 0 {
            let _ = self.sse_tx.send(SnapshotEvent {
                generation: telemetry.generation,
                snapshot: Some(telemetry.snapshot.clone()),
            });
        }
    }

    /// Coalesce SSE broadcasts to at most once per second with the latest snapshot.
    pub fn start_sse_coalesce(self: &Arc<Self>) {
        let this = Arc::clone(self);
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(1));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tick.tick().await;
                this.broadcast_snapshot();
            }
        });
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ParsedUpdate {
    pub service: String,
    pub path: String,
    pub value: Value,
}

/// Keep only leaves the desktop mapper (and light extras) actually read.
/// Drops the bulk of Cerbo chatter (settings, debug, unused AC phases, …).
fn path_keep(service: &str, path: &str) -> bool {
    // path is everything after "<instance>/" — or empty.
    let (_inst, leaf) = match path.split_once('/') {
        Some((i, rest)) => (i, rest),
        None => ("", path),
    };
    match service {
        "settings" => false,
        "system" => matches!(
            leaf,
            "Ac/Grid/L1/Power"
                | "Ac/Grid/L2/Power"
                | "Ac/Consumption/L1/Power"
                | "Ac/Consumption/L2/Power"
                | "Dc/Battery/Voltage"
                | "Dc/Battery/Current"
                | "Dc/Battery/Power"
                | "Dc/Pv/Power"
                | "Dc/Pv/Current"
        ),
        "vebus" => {
            leaf == "State"
                || leaf == "Hub4/L1/AcPowerSetpoint"
                || leaf.starts_with("Ac/Out/")
                || leaf.starts_with("Ac/ActiveIn/")
                || leaf.starts_with("Alarms/")
        }
        "battery" => {
            matches!(
                leaf,
                "Soc"
                    | "Dc/0/Voltage"
                    | "Dc/0/Current"
                    | "Dc/0/Power"
                    | "ProductName"
                    | "CustomName"
                    | "Serial"
                    | "TimeToGo"
                    | "System/MaxCellVoltage"
                    | "System/MinCellVoltage"
                    | "System/MaxVoltageCellId"
                    | "System/MinVoltageCellId"
            ) || leaf.starts_with("Alarms/")
        }
        "solarcharger" => matches!(
            leaf,
            "Yield/Power"
                | "Dc/0/Power"
                | "Dc/0/Current"
                | "Pv/V"
                | "ProductName"
                | "CustomName"
                | "Serial"
        ),
        "pvinverter" => {
            matches!(
                leaf,
                "Ac/Power"
                    | "Ac/L1/Power"
                    | "Ac/L2/Power"
                    | "Ac/L1/Voltage"
                    | "Ac/L2/Voltage"
                    | "Ac/L1/Current"
                    | "Ac/L2/Current"
                    | "ProductName"
                    | "CustomName"
                    | "Serial"
            )
        }
        "tank" => leaf == "Level" || leaf == "ProductName" || leaf == "CustomName",
        "pump" => leaf == "Status" || leaf == "ProductName" || leaf == "CustomName",
        "ev" | "evcharger" => {
            leaf.contains("Power")
                || leaf == "ProductName"
                || leaf == "CustomName"
                || leaf == "Status"
                || leaf == "Mode"
        }
        "acload" => {
            matches!(
                leaf,
                "Ac/Power" | "Ac/L1/Power" | "ProductName" | "CustomName"
            )
        }
        // GUIv2 notification slots: Notifications/<slot>/<Field>
        // (same fields inverter-desktop maps into banner notifications).
        "platform" => {
            let mut parts = leaf.split('/');
            let kind = parts.next();
            let slot = parts.next().and_then(|s| s.parse::<u32>().ok());
            let field = parts.next();
            kind == Some("Notifications")
                && slot.is_some_and(|s| s <= 20)
                && parts.next().is_none()
                && matches!(
                    field,
                    Some("Description")
                        | Some("DeviceName")
                        | Some("Service")
                        | Some("DateTime")
                        | Some("Type")
                        | Some("Active")
                        | Some("Acknowledged")
                        | Some("Silenced")
                )
        }
        _ => false,
    }
}

fn apply_update(snap: &mut Snapshot, update: ParsedUpdate) {
    let ParsedUpdate {
        service,
        path,
        value,
    } = update;
    let key = if path.is_empty() {
        "_".to_string()
    } else {
        path
    };
    let bucket = match service.as_str() {
        "system" => &mut snap.system,
        "vebus" => &mut snap.vebus,
        "battery" => &mut snap.battery,
        "solarcharger" => &mut snap.solarcharger,
        "pvinverter" => &mut snap.pvinverter,
        "tank" => &mut snap.tank,
        "pump" => &mut snap.pump,
        "ev" => &mut snap.ev,
        "evcharger" => &mut snap.evcharger,
        "acload" => &mut snap.acload,
        "platform" => &mut snap.platform,
        "settings" => &mut snap.settings,
        _ => return,
    };
    bucket.insert(key, value);
}

/// App state owned by the axum router.
pub struct AppState {
    pub cfg: Config,
    pub shared: Arc<Shared>,
}

impl AppState {
    pub fn new(cfg: Config) -> Arc<Self> {
        let shared = Shared::new();
        Arc::new(AppState { cfg, shared })
    }

    pub fn snapshot(&self) -> Option<Snapshot> {
        self.shared.snapshot()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn apply_update_system_path() {
        let mut snap = Snapshot::default();
        apply_update(
            &mut snap,
            ParsedUpdate {
                service: "system".into(),
                path: "0/Dc/Battery/Soc".into(),
                value: json!(55.0),
            },
        );
        assert_eq!(snap.system.get("0/Dc/Battery/Soc"), Some(&json!(55.0)));
    }

    #[test]
    fn apply_update_merges_vebus() {
        let mut snap = Snapshot::default();
        apply_update(
            &mut snap,
            ParsedUpdate {
                service: "vebus".into(),
                path: "0/Ac/Out/L1/P".into(),
                value: json!(100),
            },
        );
        apply_update(
            &mut snap,
            ParsedUpdate {
                service: "vebus".into(),
                path: "0/Ac/Out/L2/P".into(),
                value: json!(200),
            },
        );
        assert_eq!(snap.vebus.len(), 2);
        assert_eq!(snap.vebus.get("0/Ac/Out/L1/P"), Some(&json!(100)));
    }

    #[test]
    fn mqtt_connected_starts_false() {
        let shared = Shared::new();
        assert!(!shared.is_connected());
    }

    #[test]
    fn unknown_service_ignored() {
        let mut snap = Snapshot::default();
        apply_update(
            &mut snap,
            ParsedUpdate {
                service: "unknown".into(),
                path: "x".into(),
                value: json!(1),
            },
        );
        assert!(snap.system.is_empty());
    }

    #[test]
    fn path_keep_drops_settings_and_noise() {
        assert!(!super::path_keep("settings", "0/Settings/Foo"));
        assert!(!super::path_keep(
            "system",
            "0/Debug/BatteryOperationalLimits/SolarVoltageOffset"
        ));
        assert!(super::path_keep("system", "0/Dc/Battery/Current"));
        assert!(super::path_keep("battery", "289/Dc/0/Current"));
        assert!(super::path_keep("solarcharger", "290/Yield/Power"));
        assert!(!super::path_keep(
            "solarcharger",
            "290/History/Daily/0/Yield"
        ));
    }

    #[test]
    fn platform_notification_leaf_kept_and_applied() {
        assert!(path_keep("platform", "0/Notifications/3/Description"));
        assert!(path_keep("battery", "512/Alarms/HighVoltage"));
        assert!(path_keep("vebus", "0/Alarms/GridLost"));
        assert!(!path_keep("platform", "0/SomethingElse"));
        assert!(!path_keep("platform", "0/Notifications/3/UnknownField"));
        assert!(!path_keep("platform", "0/Notifications/99/Description"));

        let mut snap = Snapshot::default();
        apply_update(
            &mut snap,
            ParsedUpdate {
                service: "platform".into(),
                path: "0/Notifications/3/Description".into(),
                value: json!("High cell voltage"),
            },
        );
        assert_eq!(
            snap.platform.get("0/Notifications/3/Description"),
            Some(&json!("High cell voltage"))
        );
    }

    #[test]
    fn update_without_sse_subscribers_skips_clone_broadcast() {
        let shared = Shared::new();
        shared.set_connected(true);
        // No start_sse_coalesce needed — receiver_count is 0.
        shared.update(ParsedUpdate {
            service: "system".into(),
            path: "0/Dc/Battery/Current".into(),
            value: json!(23.5),
        });
        assert_eq!(
            shared
                .telemetry
                .read()
                .snapshot
                .system
                .get("0/Dc/Battery/Current"),
            Some(&json!(23.5))
        );
        // Noise path ignored
        shared.update(ParsedUpdate {
            service: "settings".into(),
            path: "0/Settings/X".into(),
            value: json!(1),
        });
        assert!(shared.telemetry.read().snapshot.settings.is_empty());
    }
}
