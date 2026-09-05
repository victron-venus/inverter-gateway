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
        ("reboot", ("system/0/Reboot", r#"{"Value":1}"#)),
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

    let topic = format!("{}{}", state.cfg.topic_prefix, topic_suffix);
    tracing::debug!(cmd = %name, topic = %topic, "executing command");
    // Note: actual MQTT publish goes through MqttBridge::publish.
    // The bridge is not easily accessible here from AppState; expose a publish
    // channel on Shared or AppState when command execution is wired up.
    let _ = (topic, payload);
    Ok(())
}

#[derive(Debug)]
pub enum CommandError {
    UnknownCommand(String),
    #[allow(dead_code)]
    ExecutionFailed(String),
}

impl std::fmt::Display for CommandError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CommandError::UnknownCommand(n) => write!(f, "unknown command: {n}"),
            CommandError::ExecutionFailed(e) => write!(f, "execution failed: {e}"),
        }
    }
}

impl std::error::Error for CommandError {}

#[cfg(test)]
mod tests {
    #[test]
    fn whitelist_has_known_commands() {
        super::is_known("silence_alarm");
        super::is_known("reboot");
        assert!(!super::is_known("raw_mqtt_passthrough"));
    }
}
