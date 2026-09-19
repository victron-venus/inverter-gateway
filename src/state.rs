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
    pub capabilities: Capabilities,
    /// Retained inverter-control state. Null means unavailable; it is not HA state.
    pub inverter: Option<serde_json::Map<String, Value>>,
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

#[derive(Debug, Clone, serde::Serialize)]
pub struct Capabilities {
    pub water_mode: bool,
    pub setpoint_override: bool,
}

impl Default for Capabilities {
    fn default() -> Self {
        Self {
            water_mode: true,
            setpoint_override: true,
        }
    }
}

#[derive(Default)]
struct Telemetry {
    snapshot: Snapshot,
    energy_readings: HashMap<String, Reading>,
    connected: bool,
    ready: bool,
    generation: u64,
    dirty: bool,
    inverter_received_at: Option<Instant>,
    // Dedicated acknowledgements own this field. Never revive an older value
    // embedded in a retained inverter/state while waiting for the current ACK.
    setpoint_override: Value,
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

#[derive(Debug)]
pub struct CommandRequest {
    pub topic: String,
    pub payload: String,
    guard: Option<CommandGuard>,
}

#[derive(Debug, Clone, Copy)]
enum CommandTarget {
    Controller,
    SetpointOverride,
    Water(u32),
}

#[derive(Debug)]
struct CommandGuard {
    generation: u64,
    queued: Instant,
    target: CommandTarget,
}

impl CommandRequest {
    pub fn is_guarded(&self) -> bool {
        self.guard.is_some()
    }

    pub fn is_water_command(&self) -> bool {
        self.guard
            .as_ref()
            .is_some_and(|guard| matches!(guard.target, CommandTarget::Water(_)))
    }
}

impl From<(String, String)> for CommandRequest {
    fn from((topic, payload): (String, String)) -> Self {
        Self {
            topic,
            payload,
            guard: None,
        }
    }
}

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

    pub fn controller_command(&self, topic: String, payload: String) -> Option<CommandRequest> {
        self.guarded_command(topic, payload, CommandTarget::Controller)
    }

    pub fn setpoint_override_command(
        &self,
        topic: String,
        payload: String,
    ) -> Option<CommandRequest> {
        self.guarded_command(topic, payload, CommandTarget::SetpointOverride)
    }

    pub fn water_command(
        &self,
        topic: String,
        payload: String,
        instance: u32,
    ) -> Option<CommandRequest> {
        self.guarded_command(topic, payload, CommandTarget::Water(instance))
    }

    fn guarded_command(
        &self,
        topic: String,
        payload: String,
        target: CommandTarget,
    ) -> Option<CommandRequest> {
        let telemetry = self.telemetry.read();
        (telemetry.connected && telemetry.command_target_available(target)).then(|| {
            CommandRequest {
                topic,
                payload,
                guard: Some(CommandGuard {
                    generation: telemetry.generation,
                    queued: Instant::now(),
                    target,
                }),
            }
        })
    }

    /// Hold the connection guard through nonblocking enqueue. A disconnect
    /// invalidates old commands even if retained state arrives after reconnect.
    pub fn send_if_current(&self, request: &CommandRequest, send: impl FnOnce()) -> bool {
        let telemetry = self.telemetry.read();
        if let Some(guard) = &request.guard {
            if !telemetry.connected
                || guard.generation != telemetry.generation
                || guard.queued.elapsed() > Duration::from_secs(5)
                || !telemetry.command_target_available(guard.target)
            {
                return false;
            }
        }
        send();
        true
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
            telemetry.inverter_received_at = None;
            telemetry.setpoint_override = Value::Null;
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
        (telemetry.connected && telemetry.ready).then(|| telemetry.current_snapshot())
    }

    /// Keep daemon metadata separate from native leaf overlays and invalidate
    /// it when the daemon stops publishing while the broker remains connected.
    pub fn update_inverter(&self, value: Value) {
        if !value.is_object() && !value.is_null() {
            return;
        }
        let mut telemetry = self.telemetry.write();
        if !telemetry.connected {
            return;
        }
        if value.is_null() {
            telemetry.setpoint_override = Value::Null;
        }
        telemetry.snapshot.inverter = value.as_object().cloned();
        telemetry.inverter_received_at = value.is_object().then(Instant::now);
        telemetry.ready |= value.is_object();
        telemetry.dirty = self.sse_tx.receiver_count() > 0;
    }

    /// The daemon publishes command acknowledgements independently of its
    /// slower full state. Receiving one must not renew controller liveness.
    pub fn update_setpoint_override(&self, value: Value) {
        let mut telemetry = self.telemetry.write();
        if !telemetry.connected {
            return;
        }
        telemetry.setpoint_override = value;
        telemetry.dirty = self.sse_tx.receiver_count() > 0;
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
        // Expiry is itself a state change, even when every MQTT publisher is
        // silent. Existing SSE clients must receive the unavailable state.
        if telemetry.snapshot.inverter.is_some()
            && telemetry
                .inverter_received_at
                .is_none_or(|at| at.elapsed() > Duration::from_secs(120))
        {
            telemetry.snapshot.inverter = None;
            telemetry.inverter_received_at = None;
            telemetry.dirty = self.sse_tx.receiver_count() > 0;
        }
        if !telemetry.connected || !telemetry.dirty {
            return;
        }
        telemetry.dirty = false;
        if self.sse_tx.receiver_count() > 0 {
            let _ = self.sse_tx.send(SnapshotEvent {
                generation: telemetry.generation,
                snapshot: Some(telemetry.current_snapshot()),
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

impl Telemetry {
    fn command_target_available(&self, target: CommandTarget) -> bool {
        match target {
            CommandTarget::Controller => self.current_snapshot().inverter.is_some(),
            CommandTarget::SetpointOverride => self
                .current_snapshot()
                .inverter
                .as_ref()
                .and_then(|inverter| inverter.get("setpoint_override"))
                .is_some_and(Value::is_object),
            CommandTarget::Water(instance) => {
                let pump = &self.snapshot.pump;
                let valid_mode = pump
                    .get(&format!("{instance}/Mode"))
                    .and_then(Value::as_u64)
                    .is_some_and(|mode| mode <= 2);
                let connected = pump
                    .get(&format!("{instance}/Connected"))
                    .is_none_or(|value| value.as_u64().is_some_and(|connected| connected == 1));
                valid_mode && connected
            }
        }
    }

    fn current_snapshot(&self) -> Snapshot {
        let mut snapshot = self.snapshot.clone();
        if self
            .inverter_received_at
            .is_none_or(|at| at.elapsed() > Duration::from_secs(120))
        {
            snapshot.inverter = None;
        }
        if let Some(inverter) = snapshot.inverter.as_mut() {
            inverter.insert("setpoint_override".into(), self.setpoint_override.clone());
        }
        snapshot
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
        "settings" => matches!(
            leaf,
            "Settings/CGwacs/Hub4Mode" | "Settings/CGwacs/BatteryLife/State"
        ),
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
                    | "Connected"
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
        "tank" => matches!(
            leaf,
            "Level" | "Status" | "Connected" | "ProductName" | "CustomName"
        ),
        "pump" => matches!(
            leaf,
            "State" | "Mode" | "Status" | "Connected" | "ProductName" | "CustomName"
        ),
        "ev" | "evcharger" => {
            leaf.contains("Power")
                || leaf == "Soc"
                || leaf == "VIN"
                || leaf == "Connected"
                || leaf == "ProductName"
                || leaf == "CustomName"
                || leaf == "Status"
                || leaf == "Mode"
        }
        "acload" => {
            matches!(
                leaf,
                "Ac/Power" | "Ac/L1/Power" | "Connected" | "ProductName" | "CustomName"
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
    fn controller_snapshot_tracks_replacement_removal_and_freshness() {
        let shared = Shared::new();
        shared.set_connected(true);
        shared.update_inverter(json!({"booleans":{"only_charging":true},"ess_mode":{"is_external":true},"ui_config":{"header_toggles":[]}}));
        let snap = shared.snapshot().unwrap();
        assert_eq!(snap.inverter.unwrap()["booleans"]["only_charging"], true);
        shared.update_inverter(json!({"booleans":{"no_feed":false}}));
        assert!(!shared.snapshot().unwrap().inverter.unwrap()["booleans"]
            .as_object()
            .unwrap()
            .contains_key("only_charging"));
        shared.telemetry.write().inverter_received_at =
            Some(Instant::now() - Duration::from_secs(121));
        shared.update(ParsedUpdate {
            service: "ev".into(),
            path: "99/Soc".into(),
            value: json!(0),
        });
        let stale = shared.snapshot().unwrap();
        assert!(stale.inverter.is_none());
        assert_eq!(stale.ev["99/Soc"], 0);
        assert!(serde_json::to_value(stale).unwrap()["inverter"].is_null());
        shared.update_inverter(json!({"booleans":{"no_feed":true}}));
        shared.update_inverter(Value::Null);
        assert!(shared.snapshot().unwrap().inverter.is_none());
        shared.update_inverter(json!({"booleans":{"no_feed":true}}));
        shared.set_connected(false);
        shared.set_connected(true);
        shared.update(ParsedUpdate {
            service: "evcharger".into(),
            path: "71/Ac/Power".into(),
            value: json!(0),
        });
        assert!(shared.snapshot().unwrap().inverter.is_none());
    }

    #[test]
    fn snapshot_keeps_soc_and_only_the_ess_settings() {
        let shared = Shared::new();
        shared.set_connected(true);
        for (service, path, value) in [
            ("ev", "812/Soc", json!(42)),
            ("evcharger", "17/Soc", json!(0)),
            ("settings", "0/Settings/CGwacs/Hub4Mode", json!(3)),
            ("settings", "0/Settings/CGwacs/BatteryLife/State", json!(9)),
            (
                "settings",
                "0/Settings/InverterControl/OnlyCharging",
                json!(1),
            ),
        ] {
            shared.update(ParsedUpdate {
                service: service.into(),
                path: path.into(),
                value,
            });
        }
        let snapshot = shared.snapshot().unwrap();
        assert_eq!(snapshot.ev["812/Soc"], 42);
        assert_eq!(snapshot.evcharger["17/Soc"], 0);
        assert_eq!(snapshot.settings.len(), 2);
        assert_eq!(snapshot.settings["0/Settings/CGwacs/Hub4Mode"], 3);
    }

    #[test]
    fn dedicated_override_ack_is_authoritative_and_does_not_renew_controller() {
        let shared = Shared::new();
        shared.set_connected(true);
        let old = json!({"value":10,"last_error":null,"request_id":"old"});
        let ack = json!({"value":-25,"last_error":null,"request_id":"new"});
        shared.update_inverter(json!({"setpoint_override":old}));
        assert!(shared.snapshot().unwrap().inverter.unwrap()["setpoint_override"].is_null());
        let controller_received = shared.telemetry.read().inverter_received_at;
        let mut events = shared.subscribe().unwrap().1;
        shared.update_setpoint_override(ack.clone());
        assert_eq!(
            shared.telemetry.read().inverter_received_at,
            controller_received
        );
        shared.broadcast_snapshot();
        assert_eq!(
            events
                .try_recv()
                .unwrap()
                .snapshot
                .unwrap()
                .inverter
                .unwrap()["setpoint_override"],
            ack
        );
        shared.update_inverter(json!({"setpoint_override":old}));
        assert_eq!(
            shared.snapshot().unwrap().inverter.unwrap()["setpoint_override"],
            ack
        );
        // A status tombstone must dominate an older embedded envelope.
        shared.update_setpoint_override(Value::Null);
        shared.update_inverter(json!({"setpoint_override":old}));
        assert!(shared.snapshot().unwrap().inverter.unwrap()["setpoint_override"].is_null());
        shared.telemetry.write().inverter_received_at =
            Some(Instant::now() - Duration::from_secs(121));
        shared.update_setpoint_override(ack.clone());
        assert!(
            shared.snapshot().unwrap().inverter.is_none(),
            "ACK cannot revive expired controller"
        );
        shared.set_connected(false);
        shared.update_setpoint_override(old.clone());
        shared.set_connected(true);
        shared.update_inverter(json!({"setpoint_override":old}));
        assert!(shared.snapshot().unwrap().inverter.unwrap()["setpoint_override"].is_null());
        // The dedicated retained publication may arrive before the full state.
        shared.update_inverter(Value::Null);
        shared.update_setpoint_override(ack.clone());
        assert!(shared.snapshot().unwrap().inverter.is_none());
        shared.update_inverter(json!({"setpoint_override":old}));
        assert_eq!(
            shared.snapshot().unwrap().inverter.unwrap()["setpoint_override"],
            ack
        );
    }

    #[test]
    fn override_commands_revalidate_ack_controller_age_queue_age_and_connection() {
        let shared = Shared::new();
        let command = || {
            shared.setpoint_override_command(
                "site/control/cmd/setpoint_override".into(),
                "{\"value\":null,\"request_id\":\"id\"}".into(),
            )
        };
        let ack = json!({"value":null,"last_error":null,"request_id":null});
        shared.set_connected(true);
        shared.update_inverter(json!({"booleans":{}}));
        assert!(command().is_none());
        shared.update_setpoint_override(ack.clone());
        let queued = command().unwrap();
        shared.update_setpoint_override(Value::Null);
        assert!(!shared.send_if_current(&queued, || panic!("unknown ACK")));
        shared.update_setpoint_override(ack.clone());
        shared.telemetry.write().inverter_received_at =
            Some(Instant::now() - Duration::from_secs(121));
        assert!(command().is_none());
        assert!(!shared.send_if_current(&queued, || panic!("expired controller")));
        shared.update_inverter(json!({"booleans":{}}));
        let mut expired = command().unwrap();
        expired.guard.as_mut().unwrap().queued = Instant::now() - Duration::from_secs(6);
        assert!(!shared.send_if_current(&expired, || panic!("expired queue")));
        let queued = command().unwrap();
        shared.set_connected(false);
        shared.set_connected(true);
        shared.update_inverter(json!({"booleans":{}}));
        shared.update_setpoint_override(ack);
        assert!(!shared.send_if_current(&queued, || panic!("crossed connection")));
        assert!(shared.send_if_current(&command().unwrap(), || {}));
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
    fn queued_controller_commands_expire_and_cannot_cross_connections() {
        let shared = Shared::new();
        shared.set_connected(true);
        shared.update_inverter(json!({"booleans": {"only_charging": false}}));
        let request = shared
            .controller_command("inverter/cmd/ess_mode".into(), "{}".into())
            .unwrap();
        assert!(shared.send_if_current(&request, || {}));
        shared.set_connected(false);
        assert!(!shared.send_if_current(&request, || panic!("disconnected publish")));
        shared.set_connected(true);
        shared.update_inverter(json!({"booleans": {"only_charging": false}}));
        assert!(!shared.send_if_current(&request, || panic!("replayed publish")));
        let mut fresh = shared
            .controller_command("inverter/cmd/toggle".into(), "{}".into())
            .unwrap();
        fresh.guard.as_mut().unwrap().queued = Instant::now() - Duration::from_secs(6);
        assert!(!shared.send_if_current(&fresh, || panic!("expired queued publish")));
        let fresh = shared
            .controller_command("inverter/cmd/toggle".into(), "{}".into())
            .unwrap();
        shared.update_inverter(Value::Null);
        assert!(!shared.send_if_current(&fresh, || panic!("unavailable controller publish")));
    }

    fn update_pump(shared: &Shared, path: &str, value: Value) {
        shared.update(ParsedUpdate {
            service: "pump".into(),
            path: path.into(),
            value,
        });
    }

    #[test]
    fn water_commands_require_native_mode_and_revalidate_target_before_handoff() {
        let shared = Shared::new();
        let request =
            || shared.water_command("W/test/pump/2/Mode".into(), "{\"value\":1}".into(), 2);
        assert!(request().is_none());
        shared.set_connected(true);
        assert!(request().is_none());
        update_pump(&shared, "1/Mode", json!(0));
        assert!(
            request().is_none(),
            "a different pump cannot authorize this target"
        );
        for mode in [
            Value::Null,
            json!("0"),
            json!(false),
            json!(0.0),
            json!(-1),
            json!(3),
        ] {
            update_pump(&shared, "2/Mode", mode);
            assert!(request().is_none());
        }
        update_pump(&shared, "2/Mode", json!(0));
        let queued = request().unwrap();
        assert!(shared.snapshot().unwrap().inverter.is_none());
        assert!(shared.send_if_current(&queued, || {}));
        update_pump(&shared, "2/Mode", Value::Null);
        assert!(!shared.send_if_current(&queued, || panic!("unknown mode publish")));
        update_pump(&shared, "2/Mode", json!(2));
        for connected in [json!(0), Value::Null, json!("1"), json!(false)] {
            update_pump(&shared, "2/Connected", connected);
            assert!(request().is_none());
            assert!(!shared.send_if_current(&queued, || panic!("unavailable pump publish")));
        }
        update_pump(&shared, "2/Connected", json!(1));
        let mut fresh = request().unwrap();
        fresh.guard.as_mut().unwrap().queued = Instant::now() - Duration::from_secs(6);
        assert!(!shared.send_if_current(&fresh, || panic!("expired water publish")));
        update_pump(&shared, "2", Value::Null);
        assert!(request().is_none());
        assert!(!shared.send_if_current(&queued, || panic!("removed device publish")));
        update_pump(&shared, "2/Mode", json!(0));
        let queued = request().unwrap();
        shared.set_connected(false);
        assert!(!shared.send_if_current(&queued, || panic!("disconnected water publish")));
        shared.set_connected(true);
        update_pump(&shared, "2/Mode", json!(0));
        assert!(!shared.send_if_current(&queued, || panic!("replayed water publish")));
        assert!(shared.send_if_current(&request().unwrap(), || {}));
    }

    #[test]
    fn snapshot_advertises_water_support_and_preserves_native_units_and_unknowns() {
        let shared = Shared::new();
        shared.set_connected(true);
        for (service, path, value) in [
            ("tank", "21/Level", json!(0.5)),
            ("tank", "21/Connected", json!(1)),
            ("pump", "2/State", json!(0)),
            ("pump", "2/Mode", json!(0)),
            ("pump", "2/Connected", json!(1)),
            ("acload", "71/Connected", json!(0)),
            ("battery", "289/Connected", json!(0)),
        ] {
            shared.update(ParsedUpdate {
                service: service.into(),
                path: path.into(),
                value,
            });
        }
        let snapshot = serde_json::to_value(shared.snapshot().unwrap()).unwrap();
        assert_eq!(snapshot["capabilities"]["water_mode"], true);
        assert_eq!(snapshot["tank"]["21/Level"], 0.5);
        assert_eq!(snapshot["pump"]["2/State"], 0);
        assert_eq!(snapshot["pump"]["2/Mode"], 0);
        assert_eq!(snapshot["pump"]["2/Connected"], 1);
        assert_eq!(snapshot["acload"]["71/Connected"], 0);
        assert_eq!(snapshot["battery"]["289/Connected"], 0);
        update_pump(&shared, "2/State", Value::Null);
        assert_eq!(shared.snapshot().unwrap().pump["2/State"], Value::Null);
    }

    #[test]
    fn controller_expiry_notifies_idle_sse_clients_once() {
        let shared = Shared::new();
        shared.set_connected(true);
        shared.update_inverter(json!({"booleans": {"only_charging": true}}));
        let (_, mut events) = shared.subscribe().unwrap();
        shared.telemetry.write().inverter_received_at =
            Some(Instant::now() - Duration::from_secs(121));
        assert!(!shared.telemetry.read().dirty);

        shared.broadcast_snapshot();
        assert!(events
            .try_recv()
            .unwrap()
            .snapshot
            .unwrap()
            .inverter
            .is_none());
        assert!(shared.snapshot().unwrap().inverter.is_none());
        shared.broadcast_snapshot();
        assert!(events.try_recv().is_err());
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
    fn flow_energy_leaves_follow_removal_and_connection_lifecycle() {
        use crate::energy::{EnergyConfig, Status};

        let shared = Shared::new();
        let cfg = EnergyConfig::default()
            .with_flow_sources(
                "system/0/Ac/ConsumptionOnOutput/L1/Power",
                "system/0/Ac/Grid/L1/Power",
                "system/0/Dc/Battery/Power",
            )
            .unwrap();
        let update = |path: &str, value| ParsedUpdate {
            service: "system".into(),
            path: path.into(),
            value,
        };
        shared.set_connected(true);
        for (path, value) in [
            ("0/Ac/ConsumptionOnOutput/L1/Power", 1000),
            ("0/Ac/Grid/L1/Power", -500),
            ("0/Dc/Battery/Power", 750),
        ] {
            shared.update(update(path, json!(value)));
        }
        assert_eq!(
            shared.energy(&cfg).reports.flow.unwrap().status,
            Status::Fresh
        );
        shared.update(update("0/Ac/Grid/L1/Power", Value::Null));
        let removed = shared.energy(&cfg).metrics.grid_power.unwrap();
        assert_eq!(removed.status, Status::Unavailable);
        assert_eq!(removed.value, None);
        assert_eq!(removed.age_seconds, None);
        shared.update(update("0", Value::Null));
        assert_eq!(
            shared.energy(&cfg).metrics.battery_power.unwrap().value,
            None
        );
        shared.update(update("0/Dc/Battery/Power", json!(-200)));
        shared.set_connected(false);
        shared.set_connected(true);
        assert_eq!(
            shared.energy(&cfg).metrics.battery_power.unwrap().value,
            None
        );
        shared.update(update("0/Dc/Battery/Power", json!(-300)));
        assert_eq!(
            shared.energy(&cfg).metrics.battery_power.unwrap().value,
            Some(-300.0)
        );
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
