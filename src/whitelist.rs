use serde_json::Value;
use std::collections::HashMap;

use crate::state::AppState;

type CommandDef = (&'static str, &'static str);
type Whitelist = HashMap<&'static str, CommandDef>;

fn builtin_whitelist() -> Whitelist {
    vec![
        (
            "silence_alarm",
            ("vebus/0/Alarm", r#"{"SilenceAlarm":"1"}"#),
        ),
        // Venus-platform notifications (GUIv2 / dbus wiki AcknowledgeAll).
        // Per-slot W/.../Notifications/N/Acknowledged is often ignored by
        // dbus-flashmq; AcknowledgeAll is the write that actually updates Cerbo.
        (
            "acknowledge_all_notifications",
            ("platform/0/Notifications/AcknowledgeAll", r#"{"value":1}"#),
        ),
    ]
    .into_iter()
    .collect()
}

pub fn is_known(name: &str) -> bool {
    static WL: std::sync::LazyLock<Whitelist> = std::sync::LazyLock::new(builtin_whitelist);
    WL.contains_key(name) || matches!(name, "toggle" | "dry_run" | "ess_mode")
}

const CONTROL_FLAGS: &[&str] = &[
    "only_charging",
    "no_feed",
    "house_support",
    "charge_battery",
    "do_not_supply_charger",
    "set_limit_to_ev_charger",
    "minimize_charging",
];

fn controller_payload(name: &str, body: Value) -> Result<Value, CommandError> {
    let invalid = || CommandError::InvalidBody("invalid inverter-control command payload".into());
    let object = body.as_object().ok_or_else(invalid)?;
    match name {
        "toggle" => {
            if object.len() != 2 {
                return Err(invalid());
            }
            let entity = object
                .get("entity")
                .and_then(Value::as_str)
                .ok_or_else(invalid)?
                .trim();
            let key = entity.strip_prefix("input_boolean.").unwrap_or(entity);
            if !CONTROL_FLAGS.contains(&key) {
                return Err(invalid());
            }
            let enabled = match object.get("state") {
                Some(Value::Bool(value)) => *value,
                Some(Value::Number(value)) if value.as_i64() == Some(0) => false,
                Some(Value::Number(value)) if value.as_i64() == Some(1) => true,
                Some(Value::String(value)) => match value.trim().to_ascii_lowercase().as_str() {
                    "on" | "true" | "1" => true,
                    "off" | "false" | "0" => false,
                    _ => return Err(invalid()),
                },
                _ => return Err(invalid()),
            };
            Ok(serde_json::json!({"entity": key, "state": if enabled { "on" } else { "off" }}))
        }
        "dry_run" if object.len() == 1 && object.get("value").is_some_and(Value::is_boolean) => {
            Ok(body)
        }
        "ess_mode" if object.is_empty() => Ok(body),
        _ => Err(invalid()),
    }
}

pub async fn execute(state: &AppState, name: &str, body: Value) -> Result<(), CommandError> {
    static WL: std::sync::LazyLock<Whitelist> = std::sync::LazyLock::new(builtin_whitelist);

    let (topic, payload) = if matches!(name, "toggle" | "dry_run" | "ess_mode") {
        let payload = controller_payload(name, body)?;
        if state
            .shared
            .snapshot()
            .is_none_or(|snapshot| snapshot.inverter.is_none())
        {
            return Err(CommandError::Unavailable);
        }
        (
            format!("{}/cmd/{name}", state.cfg.inverter_topic_prefix),
            payload.to_string(),
        )
    } else {
        let (topic_suffix, payload) = WL
            .get(name)
            .ok_or_else(|| CommandError::UnknownCommand(name.to_string()))?;
        (
            format!("{}{}", state.cfg.write_topic_prefix, topic_suffix),
            payload.to_string(),
        )
    };
    tracing::debug!(cmd = %name, topic = %topic, "executing command");

    // Publish through the MQTT client held by the bridge.
    // Bridge is not accessible here directly — expose publish via a channel on Shared.
    let tx = state.shared.command_tx.lock();
    let tx = tx.as_ref().ok_or(CommandError::NotWired)?;
    tx.send((topic, payload))
        .map_err(|e| CommandError::PublishFailed(e.to_string()))?;
    Ok(())
}

#[derive(Debug)]
pub enum CommandError {
    UnknownCommand(String),
    NotWired,
    PublishFailed(String),
    InvalidBody(String),
    Unavailable,
}

impl std::fmt::Display for CommandError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CommandError::UnknownCommand(n) => write!(f, "unknown command: {n}"),
            CommandError::NotWired => write!(f, "command not wired to MQTT"),
            CommandError::PublishFailed(e) => write!(f, "mqtt publish failed: {e}"),
            CommandError::InvalidBody(e) => f.write_str(e),
            CommandError::Unavailable => f.write_str("inverter-control state unavailable"),
        }
    }
}

impl std::error::Error for CommandError {}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn controller_commands_are_scoped_explicit_and_unavailable_without_state() {
        let mut cfg = crate::http::tests::cfg_with_token("test-token");
        cfg.inverter_topic_prefix = "house/control".into();
        let state = AppState::new(cfg);
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        *state.shared.command_tx.lock() = Some(tx);
        assert!(matches!(
            execute(&state, "toggle", json!({"entity":"no_feed","state":"on"})).await,
            Err(CommandError::Unavailable)
        ));
        state.shared.set_connected(true);
        state
            .shared
            .update_inverter(json!({"booleans":{"no_feed":false}}));
        for key in CONTROL_FLAGS {
            execute(
                &state,
                "toggle",
                json!({"entity":format!("input_boolean.{key}"),"state":false}),
            )
            .await
            .unwrap();
            let (topic, payload) = rx.try_recv().unwrap();
            assert_eq!(topic, "house/control/cmd/toggle");
            assert_eq!(
                serde_json::from_str::<Value>(&payload).unwrap(),
                json!({"entity":key,"state":"off"})
            );
        }
        for invalid in [
            json!({"entity":"switch.no_feed","state":"on"}),
            json!({"entity":"no_feed"}),
            json!({"entity":"no_feed","state":null}),
            json!({"entity":"no_feed","state":"toggle"}),
            json!({"entity":"no_feed","state":"on","topic":"arbitrary"}),
        ] {
            assert!(matches!(
                execute(&state, "toggle", invalid).await,
                Err(CommandError::InvalidBody(_))
            ));
        }
        assert!(rx.try_recv().is_err());
        execute(&state, "dry_run", json!({"value":true}))
            .await
            .unwrap();
        assert_eq!(
            rx.try_recv().unwrap(),
            (
                "house/control/cmd/dry_run".into(),
                "{\"value\":true}".into()
            )
        );
        execute(&state, "ess_mode", json!({})).await.unwrap();
        assert_eq!(
            rx.try_recv().unwrap(),
            ("house/control/cmd/ess_mode".into(), "{}".into())
        );
        assert!(execute(&state, "dry_run", json!({})).await.is_err());
        assert!(execute(&state, "ess_mode", json!({"is_external":true}))
            .await
            .is_err());
        assert!(execute(&state, "setpoint", json!({"value":1000}))
            .await
            .is_err());
        state.shared.set_connected(false);
        assert!(matches!(
            execute(&state, "ess_mode", json!({})).await,
            Err(CommandError::Unavailable)
        ));
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn whitelist_has_known_commands() {
        super::is_known("silence_alarm");
        assert!(super::is_known("acknowledge_all_notifications"));
        assert!(!super::is_known("reboot"));
        assert!(!super::is_known("raw_mqtt_passthrough"));
    }
}
