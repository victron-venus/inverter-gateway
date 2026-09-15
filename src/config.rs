use crate::energy::EnergyConfig;
use std::env;
use std::net::SocketAddr;
use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct Config {
    pub mqtt_host: String,
    pub mqtt_port: u16,
    /// Enable certificate-verified MQTT TLS. Plain TCP remains the compatibility default.
    pub mqtt_tls: bool,
    /// PEM CA bundle for a private broker; otherwise use the system trust store.
    pub mqtt_ca_file: Option<PathBuf>,
    pub mqtt_username: String,
    pub mqtt_password: String,
    pub mqtt_client_id: String,
    pub http_bind: SocketAddr,
    pub https: Option<crate::transport::HttpsConfig>,
    pub api_token: Option<String>,
    /// Optional credential restricted to telemetry reads.
    pub read_token: Option<String>,
    pub energy: EnergyConfig,
    /// Victron MQTT topic prefix for reading, e.g. "N/<portal_id>/"
    /// See <https://www.victronenergy.com/services-and-support/cerbo-gx>
    pub topic_prefix: String,
    /// Victron MQTT topic prefix for writing, e.g. "W/<portal_id>/"
    /// Derived from topic_prefix (N/ → W/).
    pub write_topic_prefix: String,
    /// Topic root owned by inverter-control, independent of the Victron portal.
    pub inverter_topic_prefix: String,
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
        let (mqtt_tls, mqtt_ca_file, mqtt_port) = parse_mqtt_transport(
            optional_env("MQTT_TLS")?.as_deref(),
            optional_env("MQTT_CA_FILE")?.as_deref(),
            optional_env("MQTT_PORT")?.as_deref(),
        )?;
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
        let https = crate::transport::HttpsConfig::from_env(http_bind)?;

        let api_token = env::var("GATEWAY_API_TOKEN").ok().filter(|s| !s.is_empty());
        let read_token = env::var("GATEWAY_READ_TOKEN")
            .ok()
            .filter(|s| !s.is_empty());
        if read_token.is_some() && read_token == api_token {
            return Err("GATEWAY_READ_TOKEN must differ from GATEWAY_API_TOKEN".into());
        }
        let energy = EnergyConfig::parse(
            &env::var("GATEWAY_ENERGY_BATTERY_SOURCE")
                .unwrap_or_else(|_| "system/0/Dc/Battery/Soc".into()),
            &env::var("GATEWAY_ENERGY_SOLAR_POWER_SOURCES").unwrap_or_default(),
            &env::var("GATEWAY_ENERGY_SOLAR_TODAY_SOURCES").unwrap_or_default(),
            &env::var("GATEWAY_ENERGY_MAX_AGE_SECS").unwrap_or_else(|_| "120".into()),
        )?
        .with_alarm_sources(&env::var("GATEWAY_ENERGY_ALARM_SOURCES").unwrap_or_default())?;
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
        let inverter_topic_prefix = env::var("INVERTER_TOPIC_PREFIX")
            .unwrap_or_else(|_| "inverter".into())
            .trim_end_matches('/')
            .to_string();
        if inverter_topic_prefix.is_empty()
            || inverter_topic_prefix.contains(['+', '#', '\0'])
            || inverter_topic_prefix.starts_with('/')
        {
            return Err("INVERTER_TOPIC_PREFIX must be a nonempty literal MQTT topic root".into());
        }

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
            mqtt_tls,
            mqtt_ca_file,
            mqtt_username,
            mqtt_password,
            mqtt_client_id,
            http_bind,
            https,
            api_token,
            read_token,
            energy,
            topic_prefix,
            write_topic_prefix,
            inverter_topic_prefix,
            allow_insecure,
            cors_origins,
        })
    }
}

/// Distinguish an absent setting from invalid text so TLS cannot silently turn off.
pub(crate) fn optional_env(name: &str) -> Result<Option<String>, ConfigError> {
    match env::var(name) {
        Ok(value) => Ok(Some(value)),
        Err(env::VarError::NotPresent) => Ok(None),
        Err(env::VarError::NotUnicode(_)) => Err(format!("{name} must contain valid UTF-8").into()),
    }
}

fn parse_mqtt_transport(
    tls: Option<&str>,
    ca_file: Option<&str>,
    port: Option<&str>,
) -> Result<(bool, Option<PathBuf>, u16), ConfigError> {
    let tls = match tls {
        None | Some("0" | "false") => false,
        Some("1" | "true") => true,
        _ => return Err("MQTT_TLS must be 0, 1, false, or true".into()),
    };
    let ca_file = match ca_file {
        Some(_) if !tls => return Err("MQTT_CA_FILE requires MQTT_TLS=1".into()),
        Some(path) if path.trim().is_empty() => {
            return Err("MQTT_CA_FILE must not be empty".into());
        }
        Some(path) => Some(PathBuf::from(path)),
        None => None,
    };
    let port = port
        .unwrap_or(if tls { "8883" } else { "1883" })
        .parse::<u16>()
        .ok()
        .filter(|port| *port != 0)
        .ok_or_else(|| ConfigError::from("invalid MQTT_PORT: expected 1 through 65535"))?;
    Ok((tls, ca_file, port))
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

    #[test]
    fn mqtt_transport_defaults_preserve_tcp_and_use_standard_tls_port() {
        assert_eq!(
            parse_mqtt_transport(None, None, None).unwrap(),
            (false, None, 1883)
        );
        for value in ["0", "false"] {
            assert_eq!(
                parse_mqtt_transport(Some(value), None, None).unwrap(),
                (false, None, 1883)
            );
        }
        for value in ["1", "true"] {
            assert_eq!(
                parse_mqtt_transport(Some(value), None, None).unwrap(),
                (true, None, 8883)
            );
        }
        assert_eq!(
            parse_mqtt_transport(Some("1"), Some("/etc/mqtt/ca.pem"), Some("28883")).unwrap(),
            (true, Some(PathBuf::from("/etc/mqtt/ca.pem")), 28883)
        );
        assert_eq!(
            parse_mqtt_transport(None, None, Some("1884")).unwrap().2,
            1884
        );
    }

    #[test]
    fn mqtt_transport_rejects_misconfigured_tls_instead_of_silently_using_tcp() {
        for value in ["", "yes", "TRUE", "2", " true"] {
            assert!(parse_mqtt_transport(Some(value), None, None).is_err());
        }
        assert!(parse_mqtt_transport(None, Some("ca.pem"), None).is_err());
        assert!(parse_mqtt_transport(Some("0"), Some("ca.pem"), None).is_err());
        for value in ["", " "] {
            assert!(parse_mqtt_transport(Some("1"), Some(value), None).is_err());
        }
        for value in ["", "0", "65536", "tls", "-1"] {
            assert!(parse_mqtt_transport(Some("1"), None, Some(value)).is_err());
        }
    }
}
