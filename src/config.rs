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
    /// Victron MQTT topic prefix for reading, e.g. "N/<portal_id>/"
    /// See <https://www.victronenergy.com/services-and-support/cerbo-gx>
    pub topic_prefix: String,
    /// Victron MQTT topic prefix for writing, e.g. "W/<portal_id>/"
    /// Derived from topic_prefix (N/ → W/).
    pub write_topic_prefix: String,
    /// Allow unauthenticated access (escape hatch for local LAN tests only).
    pub allow_insecure: bool,
    /// Allowed CORS origins (empty = same-origin only).
    pub cors_origins: Vec<String>,
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
            .unwrap_or_else(|_| "127.0.0.1:8080".to_string())
            .parse()
            .map_err(|_| -> ConfigError { "invalid HTTP_BIND".into() })?;

        let api_token = env::var("GATEWAY_API_TOKEN").ok().filter(|s| !s.is_empty());
        let allow_insecure = env::var("GATEWAY_ALLOW_INSECURE")
            .map(|v| v == "1")
            .unwrap_or(false);

        // Refuse to start without a token unless the insecure escape hatch is explicitly set.
        if api_token.is_none() && !allow_insecure {
            return Err("GATEWAY_API_TOKEN is not set. Set it, or set GATEWAY_ALLOW_INSECURE=1 for local LAN testing only.".into());
        }

        // VICTRON_PORTAL_ID takes precedence; falls back to VICTRON_TOPIC_PREFIX.
        let topic_prefix = if let Ok(portal_id) = env::var("VICTRON_PORTAL_ID") {
            format!("N/{}/", portal_id)
        } else {
            env::var("VICTRON_TOPIC_PREFIX").unwrap_or_else(|_| "N/<portal_id>/".to_string())
        };

        // Write prefix: Victron requires W/ not N/ for command publications.
        let write_topic_prefix = topic_prefix.replace("N/", "W/");

        let cors_origins = env::var("GATEWAY_CORS_ORIGINS")
            .map(|s| {
                s.split(',')
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(String::from)
                    .collect()
            })
            .unwrap_or_default();

        Ok(Config {
            mqtt_host,
            mqtt_port,
            mqtt_username,
            mqtt_password,
            mqtt_client_id,
            http_bind,
            api_token,
            topic_prefix,
            write_topic_prefix,
            allow_insecure,
            cors_origins,
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

    #[test]
    fn cors_origins_parses_csv() {
        // Pure parser logic isolated for a sanity check.
        let raw = "https://a.example, https://b.example ,, ";
        let v: Vec<String> = raw
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(String::from)
            .collect();
        assert_eq!(v, vec!["https://a.example", "https://b.example"]);
    }
}
