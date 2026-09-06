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
    WL.contains_key(name)
}

pub async fn execute(state: &AppState, name: &str, _body: Value) -> Result<(), CommandError> {
    static WL: std::sync::LazyLock<Whitelist> = std::sync::LazyLock::new(builtin_whitelist);

    let (topic_suffix, payload) = WL
        .get(name)
        .ok_or_else(|| CommandError::UnknownCommand(name.to_string()))?;

    let topic = format!("{}{}", state.cfg.write_topic_prefix, topic_suffix);
    tracing::debug!(cmd = %name, topic = %topic, "executing command");

    // Publish through the MQTT client held by the bridge.
    // Bridge is not accessible here directly — expose publish via a channel on Shared.
    let tx = state.shared.command_tx.lock();
    let tx = tx.as_ref().ok_or(CommandError::NotWired)?;
    tx.send((topic, payload.to_string()))
        .map_err(|e| CommandError::PublishFailed(e.to_string()))?;
    Ok(())
}

#[derive(Debug)]
pub enum CommandError {
    UnknownCommand(String),
    NotWired,
    PublishFailed(String),
}

impl std::fmt::Display for CommandError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CommandError::UnknownCommand(n) => write!(f, "unknown command: {n}"),
            CommandError::NotWired => write!(f, "command not wired to MQTT"),
            CommandError::PublishFailed(e) => write!(f, "mqtt publish failed: {e}"),
        }
    }
}

impl std::error::Error for CommandError {}

#[cfg(test)]
mod tests {
    #[test]
    fn whitelist_has_known_commands() {
        super::is_known("silence_alarm");
        assert!(super::is_known("acknowledge_all_notifications"));
        assert!(!super::is_known("reboot"));
        assert!(!super::is_known("raw_mqtt_passthrough"));
    }
}
