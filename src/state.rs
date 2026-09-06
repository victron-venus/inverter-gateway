use std::collections::HashMap;
use std::sync::Arc;

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
    #[serde(skip_serializing_if = "HashMap::is_empty")]
    pub settings: HashMap<String, Value>,
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
    pub path: String,
    pub value: Value,
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
}
