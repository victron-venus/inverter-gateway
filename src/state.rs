use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
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

pub struct Shared {
    pub snapshot: RwLock<Snapshot>,
    pub sse_tx: broadcast::Sender<Snapshot>,
    /// Set when snapshot changed and at least one SSE client may need a push.
    sse_dirty: AtomicBool,
    pub mqtt_connected: RwLock<bool>,
    pub command_tx: parking_lot::Mutex<Option<mpsc::UnboundedSender<CommandRequest>>>,
}

pub type CommandRequest = (String, String); // (topic, payload)

impl Shared {
    pub fn new() -> Arc<Self> {
        let (sse_tx, _) = broadcast::channel(64);
        Arc::new(Shared {
            snapshot: RwLock::new(Snapshot::default()),
            sse_tx,
            sse_dirty: AtomicBool::new(false),
            mqtt_connected: RwLock::new(false),
            command_tx: parking_lot::Mutex::new(None),
        })
    }

    /// Apply one MQTT leaf. Never clones the whole snapshot unless an SSE
    /// subscriber exists — Victron floods hundreds of retained/live topics and
    /// cloning ~100KB+ per message pegged Synology at ~60%+ CPU.
    pub fn update(&self, update: ParsedUpdate) {
        if !path_keep(&update.service, &update.path) {
            return;
        }
        {
            let mut snap = self.snapshot.write();
            apply_update(&mut snap, update);
        }
        if self.sse_tx.receiver_count() == 0 {
            return;
        }
        self.sse_dirty.store(true, Ordering::Relaxed);
    }

    /// Coalesce SSE broadcasts to at most once per second with the latest snapshot.
    pub fn start_sse_coalesce(self: &Arc<Self>) {
        let this = Arc::clone(self);
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(1));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tick.tick().await;
                if !this.sse_dirty.swap(false, Ordering::Relaxed) {
                    continue;
                }
                if this.sse_tx.receiver_count() == 0 {
                    continue;
                }
                let snap = this.snapshot.read().clone();
                let _ = this.sse_tx.send(snap);
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

    pub fn snapshot(&self) -> Snapshot {
        self.shared.snapshot.read().clone()
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
        assert!(!*shared.mqtt_connected.read());
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
        assert!(path_keep(
            "platform",
            "0/Notifications/3/Description"
        ));
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
        // No start_sse_coalesce needed — receiver_count is 0.
        shared.update(ParsedUpdate {
            service: "system".into(),
            path: "0/Dc/Battery/Current".into(),
            value: json!(23.5),
        });
        assert_eq!(
            shared.snapshot.read().system.get("0/Dc/Battery/Current"),
            Some(&json!(23.5))
        );
        // Noise path ignored
        shared.update(ParsedUpdate {
            service: "settings".into(),
            path: "0/Settings/X".into(),
            value: json!(1),
        });
        assert!(shared.snapshot.read().settings.is_empty());
    }
}
