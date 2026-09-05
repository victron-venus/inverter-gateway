use parking_lot::RwLock;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{broadcast, mpsc};

use crate::config::Config;

#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct Snapshot {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system: Option<SystemInfo>,
    #[serde(skip_serializing_if = "HashMap::is_empty")]
    pub vebus: HashMap<String, Value>,
    #[serde(skip_serializing_if = "HashMap::is_empty")]
    pub solarcharger: HashMap<String, Value>,
    #[serde(skip_serializing_if = "HashMap::is_empty")]
    pub tank: HashMap<String, Value>,
}

#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct SystemInfo {
    pub state: Option<i32>,
    pub state_name: Option<String>,
    pub firmware_version: Option<String>,
    pub product_id: Option<String>,
    pub device_mode: Option<String>,
}

pub struct Shared {
    pub snapshot: RwLock<Snapshot>,
    pub sse_tx: broadcast::Sender<Snapshot>,
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
            mqtt_connected: RwLock::new(false),
            command_tx: parking_lot::Mutex::new(None),
        })
    }

    pub fn update(&self, update: ParsedUpdate) {
        {
            let mut snap = self.snapshot.write();
            apply_update(&mut snap, update);
        }
        let snap = self.snapshot.read().clone();
        let _ = self.sse_tx.send(snap);
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ParsedUpdate {
    pub service: String,
    pub values: Value,
}

fn apply_update(snap: &mut Snapshot, update: ParsedUpdate) {
    let ParsedUpdate { service, values } = update;
    match service.as_str() {
        "system" => {
            let entry = snap.system.get_or_insert_with(SystemInfo::default);
            if let Some(v) = values.get("State").and_then(|v| v.as_i64()) {
                entry.state = Some(v as i32);
            }
            if let Some(v) = values.get("StateName").and_then(|v| v.as_str()) {
                entry.state_name = Some(v.to_string());
            }
        }
        s if s.starts_with("vebus/") => {
            merge_into(&mut snap.vebus, &service, values);
        }
        s if s.starts_with("solarcharger/") => {
            merge_into(&mut snap.solarcharger, &service, values);
        }
        s if s.starts_with("tank/") => {
            merge_into(&mut snap.tank, &service, values);
        }
        _ => {}
    }
}

fn merge_into(map: &mut HashMap<String, Value>, key: &str, values: Value) {
    let merged = match map.remove(key) {
        Some(existing) => {
            let Value::Object(mut am) = existing else {
                return;
            };
            if let Value::Object(bm) = values {
                for (k, v) in bm {
                    am.insert(k, v);
                }
            }
            Value::Object(am)
        }
        None => values,
    };
    map.insert(key.to_string(), merged);
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

    #[test]
    fn apply_update_system() {
        let update = ParsedUpdate {
            service: "system".to_string(),
            values: serde_json::json!({"State": 9, "StateName": "Inverting"}),
        };
        let mut snap = Snapshot::default();
        apply_update(&mut snap, update);
        let sys = snap.system.unwrap();
        assert_eq!(sys.state, Some(9));
        assert_eq!(sys.state_name, Some("Inverting".to_string()));
    }

    #[test]
    fn apply_update_merges_vebus() {
        let u1 = ParsedUpdate {
            service: "vebus/0".to_string(),
            values: serde_json::json!({"AcPower": 100.0}),
        };
        let u2 = ParsedUpdate {
            service: "vebus/0".to_string(),
            values: serde_json::json!({"DcPower": 50.0}),
        };
        let mut snap = Snapshot::default();
        apply_update(&mut snap, u1);
        apply_update(&mut snap, u2);
        let entry = snap.vebus.get("vebus/0").unwrap();
        assert_eq!(entry.get("AcPower").and_then(|v| v.as_f64()), Some(100.0));
        assert_eq!(entry.get("DcPower").and_then(|v| v.as_f64()), Some(50.0));
    }

    #[test]
    fn mqtt_connected_starts_false() {
        let shared = Shared::new();
        assert!(!*shared.mqtt_connected.read());
    }
}
