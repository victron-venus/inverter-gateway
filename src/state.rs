use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::RwLock;
use serde_json::Value;
use tokio::sync::{broadcast, mpsc};

use crate::config::Config;
use crate::energy::{is_energy_path, EnergyConfig, EnergyResponse, Reading};

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
    energy_readings: HashMap<String, Reading>,
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
            telemetry.energy_readings.clear();
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

    pub fn energy(&self, cfg: &EnergyConfig) -> EnergyResponse {
        let telemetry = self.telemetry.read();
        EnergyResponse::build(
            cfg,
            telemetry.connected,
            &telemetry.energy_readings,
            Instant::now(),
        )
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
        // An empty/null device root invalidates every previously received leaf.
        let removed_device = update.value.is_null()
            && !update.path.contains('/')
            && update.path.parse::<u32>().is_ok();
        if !removed_device && !path_keep(&update.service, &update.path) {
            return;
        }
        let mut telemetry = self.telemetry.write();
        if !telemetry.connected {
            return;
        }
        if removed_device {
            let prefix = format!("{}/{}/", update.service, update.path);
            telemetry
                .energy_readings
                .retain(|source, _| !source.starts_with(&prefix));
            if let Some(bucket) = telemetry.snapshot.bucket_mut(&update.service) {
                let prefix = format!("{}/", update.path);
                bucket.retain(|path, _| !path.starts_with(&prefix));
            }
        } else {
            if is_energy_path(&update.service, &update.path) {
                let source = format!("{}/{}", update.service, update.path);
                if update.value.is_null() {
                    telemetry.energy_readings.remove(&source);
                } else {
                    telemetry.energy_readings.insert(
                        source,
                        Reading {
                            value: update.value.clone(),
                            received_at: Instant::now(),
                        },
                    );
                }
            }
            apply_update(&mut telemetry.snapshot, update);
            telemetry.ready = true;
        }
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
    if is_energy_path(service, path) {
        return true;
    }
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
    let Some(bucket) = snap.bucket_mut(&service) else {
        return;
    };
    bucket.insert(key, value);
}

impl Snapshot {
    fn bucket_mut(&mut self, service: &str) -> Option<&mut HashMap<String, Value>> {
        match service {
            "system" => Some(&mut self.system),
            "vebus" => Some(&mut self.vebus),
            "battery" => Some(&mut self.battery),
            "solarcharger" => Some(&mut self.solarcharger),
            "pvinverter" => Some(&mut self.pvinverter),
            "tank" => Some(&mut self.tank),
            "pump" => Some(&mut self.pump),
            "ev" => Some(&mut self.ev),
            "evcharger" => Some(&mut self.evcharger),
            "acload" => Some(&mut self.acload),
            "platform" => Some(&mut self.platform),
            "settings" => Some(&mut self.settings),
            _ => None,
        }
    }
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
        assert!(super::path_keep(
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

    #[test]
    fn energy_invalidation_follows_null_device_removal_and_reconnect() {
        use crate::energy::{EnergyConfig, Status};

        let shared = Shared::new();
        let cfg = EnergyConfig::default();
        shared.set_connected(true);
        let update = |value| ParsedUpdate {
            service: "system".into(),
            path: "0/Dc/Battery/Soc".into(),
            value,
        };
        shared.update(update(json!(65)));
        assert_eq!(shared.energy(&cfg).metrics.battery_soc.value, Some(65.0));
        shared.update(update(Value::Null));
        assert_eq!(
            shared.energy(&cfg).metrics.battery_soc.status,
            Status::Unavailable
        );
        assert_eq!(shared.energy(&cfg).metrics.battery_soc.age_seconds, None);
        shared.update(update(json!(66)));
        shared.update(ParsedUpdate {
            service: "system".into(),
            path: "0".into(),
            value: Value::Null,
        });
        assert_eq!(shared.energy(&cfg).metrics.battery_soc.value, None);
        assert!(!shared
            .snapshot()
            .unwrap()
            .system
            .contains_key("0/Dc/Battery/Soc"));
        shared.update(update(json!(67)));
        shared.set_connected(false);
        shared.update(update(json!(99)));
        assert!(!shared.energy(&cfg).mqtt_connected);
        shared.set_connected(true);
        assert_eq!(shared.energy(&cfg).metrics.battery_soc.value, None);
        assert_eq!(shared.energy(&cfg).metrics.battery_soc.age_seconds, None);
        shared.update(update(json!(68)));
        assert_eq!(shared.energy(&cfg).metrics.battery_soc.value, Some(68.0));
    }

    #[test]
    fn unrelated_telemetry_cannot_refresh_an_energy_source() {
        use crate::energy::{EnergyConfig, Status};

        let shared = Shared::new();
        shared.set_connected(true);
        shared.update(ParsedUpdate {
            service: "system".into(),
            path: "0/Dc/Battery/Soc".into(),
            value: json!(70),
        });
        shared
            .telemetry
            .write()
            .energy_readings
            .get_mut("system/0/Dc/Battery/Soc")
            .unwrap()
            .received_at = Instant::now() - Duration::from_secs(121);
        shared.update(ParsedUpdate {
            service: "battery".into(),
            path: "0/Dc/0/Voltage".into(),
            value: json!(52),
        });
        let response = shared.energy(&EnergyConfig::default());
        assert_eq!(response.metrics.battery_soc.status, Status::Stale);
        assert_eq!(response.metrics.battery_soc.value, None);
        assert_eq!(response.metrics.battery_soc.age_seconds, Some(121));
    }

    #[test]
    fn alarm_removal_and_disconnect_clear_confirmed_active_list() {
        use crate::energy::{EnergyConfig, Status};

        let shared = Shared::new();
        let cfg = EnergyConfig::default()
            .with_alarm_sources("battery/512/Alarms/LowVoltage")
            .unwrap();
        shared.set_connected(true);
        let update = |value| ParsedUpdate {
            service: "battery".into(),
            path: "512/Alarms/LowVoltage".into(),
            value,
        };
        shared.update(update(json!(2)));
        assert_eq!(shared.energy(&cfg).alarms.active.len(), 1);
        shared.update(update(Value::Null));
        let removed = shared.energy(&cfg);
        assert_eq!(removed.alarms.status, Status::Unavailable);
        assert!(removed.alarms.active.is_empty());
        assert!(!removed.reports.alarms.text.contains("No active alarms"));
        shared.update(update(json!(0)));
        assert_eq!(
            shared.energy(&cfg).reports.alarms.text,
            "No active alarms in the monitored sources."
        );
        shared.update(ParsedUpdate {
            service: "battery".into(),
            path: "512".into(),
            value: Value::Null,
        });
        assert_eq!(shared.energy(&cfg).alarms.status, Status::Unavailable);
        shared.update(update(json!(1)));
        shared.set_connected(false);
        assert!(shared.energy(&cfg).alarms.active.is_empty());
        shared.set_connected(true);
        assert_eq!(shared.energy(&cfg).alarms.status, Status::Unavailable);
        assert!(!shared
            .energy(&cfg)
            .reports
            .alarms
            .text
            .contains("No active alarms"));
    }
}
