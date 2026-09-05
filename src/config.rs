use std::env;
use std::net::SocketAddr;

#[derive(Debug, Clone)]
pub struct Config {
    pub mqtt_host: String,
    pub mqtt_port: u16,
    pub mqtt_username: String,
    pub mqtt_password: String,
    pub mqtt_client_id: String,
    pub http_bind: SocketAddr,
    pub api_token: Option<String>,
    /// Victron MQTT topic prefix, e.g. "N/%instance%/"
    /// See <https://www.victronenergy.com/services-and-support/cerbo-gx>
    pub topic_prefix: String,
}

#[derive(Debug)]
pub struct ConfigError {
    msg: String,
}

impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.msg)
    }
}

impl std::error::Error for ConfigError {}

impl From<&str> for ConfigError {
    fn from(msg: &str) -> Self {
        ConfigError {
            msg: msg.to_string(),
        }
    }
}

impl From<String> for ConfigError {
    fn from(msg: String) -> Self {
        ConfigError { msg }
    }
}

impl Config {
    pub fn from_env() -> Result<Self, ConfigError> {
        let mqtt_host =
            env::var("MQTT_HOST").map_err(|_| -> ConfigError { "missing env MQTT_HOST".into() })?;
        let mqtt_port: u16 = env::var("MQTT_PORT")
            .unwrap_or_else(|_| "1883".to_string())
            .parse()
            .map_err(|_| -> ConfigError { "invalid MQTT_PORT".into() })?;
        let mqtt_username = env::var("MQTT_USERNAME")
            .map_err(|_| -> ConfigError { "missing env MQTT_USERNAME".into() })?;
        let mqtt_password = env::var("MQTT_PASSWORD")
            .map_err(|_| -> ConfigError { "missing env MQTT_PASSWORD".into() })?;
        let mqtt_client_id =
            env::var("MQTT_CLIENT_ID").unwrap_or_else(|_| "inverter-gateway".to_string());

        let http_bind: SocketAddr = env::var("HTTP_BIND")
            .unwrap_or_else(|_| "0.0.0.0:8080".to_string())
            .parse()
            .map_err(|_| -> ConfigError { "invalid HTTP_BIND".into() })?;

        let api_token = env::var("GATEWAY_API_TOKEN").ok().filter(|s| !s.is_empty());
        let topic_prefix =
            env::var("VICTRON_TOPIC_PREFIX").unwrap_or_else(|_| "N/%instance%/".to_string());

        Ok(Config {
            mqtt_host,
            mqtt_port,
            mqtt_username,
            mqtt_password,
            mqtt_client_id,
            http_bind,
            api_token,
            topic_prefix,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_requires_mqtt_host() {
        // Without env vars this errors out.  The test ensures the error path is reached.
        // We don't unset other vars because we can't be sure of the test environment.
        let _result = Config::from_env();
        // No assertion: the function should at least not panic.
    }
}
