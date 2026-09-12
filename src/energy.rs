//! Stable energy values and English reports for independent voice adapters.

use std::collections::HashMap;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::Serialize;
use serde_json::Value;

use crate::alarms::{alarm_label, Alarms};
use crate::config::ConfigError;

#[derive(Debug, Clone)]
pub struct EnergyConfig {
    pub battery_sources: Vec<String>,
    pub solar_power_sources: Vec<String>,
    pub solar_today_sources: Vec<String>,
    pub alarm_sources: Vec<String>,
    pub max_age: Duration,
}

impl Default for EnergyConfig {
    fn default() -> Self {
        Self {
            battery_sources: vec!["system/0/Dc/Battery/Soc".into()],
            solar_power_sources: vec![],
            solar_today_sources: vec![],
            alarm_sources: vec![],
            max_age: Duration::from_secs(120),
        }
    }
}

impl EnergyConfig {
    pub fn with_alarm_sources(mut self, raw: &str) -> Result<Self, ConfigError> {
        self.alarm_sources = parse_sources(raw, MetricKind::Alarm)?;
        Ok(self)
    }

    pub fn parse(
        battery: &str,
        power: &str,
        today: &str,
        max_age: &str,
    ) -> Result<Self, ConfigError> {
        let battery_sources = parse_sources(battery, MetricKind::Battery)?;
        if battery_sources.len() > 1 {
            return Err(
                "GATEWAY_ENERGY_BATTERY_SOURCE must select exactly one battery source".into(),
            );
        }
        let solar_power_sources = parse_sources(power, MetricKind::Power)?;
        validate_power_overlap(&solar_power_sources)?;
        let solar_today_sources = parse_sources(today, MetricKind::Today)?;
        let max_age: u64 = max_age
            .parse()
            .map_err(|_| ConfigError::from("invalid GATEWAY_ENERGY_MAX_AGE_SECS"))?;
        if max_age == 0 {
            return Err("GATEWAY_ENERGY_MAX_AGE_SECS must be greater than zero".into());
        }
        Ok(Self {
            battery_sources,
            solar_power_sources,
            solar_today_sources,
            alarm_sources: vec![],
            max_age: Duration::from_secs(max_age),
        })
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum MetricKind {
    Battery,
    Power,
    Today,
    Alarm,
}

fn metric_kind(service: &str, path: &str) -> Option<MetricKind> {
    let (instance, leaf) = path.split_once('/')?;
    // Require canonical numeric instances, so alternate spellings cannot bypass deduplication.
    if !instance.bytes().all(|byte| byte.is_ascii_digit())
        || (instance.len() > 1 && instance.starts_with('0'))
        || instance.parse::<u32>().is_err()
    {
        return None;
    }
    match (service, leaf) {
        ("system", "Dc/Battery/Soc") | ("battery" | "vebus", "Soc") => Some(MetricKind::Battery),
        ("system", "Dc/Pv/Power")
        | ("solarcharger", "Yield/Power")
        | ("pvinverter", "Ac/Power" | "Ac/L1/Power" | "Ac/L2/Power" | "Ac/L3/Power") => {
            Some(MetricKind::Power)
        }
        ("solarcharger", "History/Daily/0/Yield") | ("pvinverter", "Ac/Energy/Daily") => {
            Some(MetricKind::Today)
        }
        ("battery" | "vebus", leaf) if alarm_label(service, leaf).is_some() => {
            Some(MetricKind::Alarm)
        }
        ("system", leaf) => {
            let mut parts = leaf.split('/');
            (parts.next() == Some("Ac")
                && matches!(parts.next(), Some("PvOnGrid" | "PvOnOutput" | "PvOnGenset"))
                && matches!(parts.next(), Some("L1" | "L2" | "L3" | "Total"))
                && parts.next() == Some("Power")
                && parts.next().is_none())
            .then_some(MetricKind::Power)
        }
        _ => None,
    }
}

pub(crate) fn is_energy_path(service: &str, path: &str) -> bool {
    metric_kind(service, path).is_some()
}

fn parse_sources(raw: &str, kind: MetricKind) -> Result<Vec<String>, ConfigError> {
    if raw.trim().is_empty() {
        return Ok(vec![]);
    }
    let mut sources = Vec::new();
    for source in raw.split(',').map(str::trim) {
        let valid = source
            .split_once('/')
            .and_then(|(service, path)| metric_kind(service, path))
            == Some(kind);
        if !valid {
            return Err(format!("unsupported energy source: {source}").into());
        }
        sources.push(source.to_owned());
    }
    sources.sort();
    if sources.windows(2).any(|pair| pair[0] == pair[1]) {
        return Err("duplicate energy sources would double count readings".into());
    }
    Ok(sources)
}

fn validate_power_overlap(sources: &[String]) -> Result<(), ConfigError> {
    let system_dc = sources.iter().any(|s| s.ends_with("/Dc/Pv/Power"));
    let chargers = sources.iter().any(|s| s.starts_with("solarcharger/"));
    let system_ac = sources.iter().any(|s| s.contains("/Ac/PvOn"));
    let inverters = sources.iter().any(|s| s.starts_with("pvinverter/"));
    if (system_dc && chargers) || (system_ac && inverters) {
        return Err(
            "solar power sources must not combine a system aggregate with its device readings"
                .into(),
        );
    }
    for source in sources {
        let phase_prefix = if let Some(prefix) = source.strip_suffix("/Total/Power") {
            Some(format!("{prefix}/L"))
        } else {
            source
                .strip_suffix("/Ac/Power")
                .map(|prefix| format!("{prefix}/Ac/L"))
        };
        if phase_prefix.is_some_and(|prefix| sources.iter().any(|s| s.starts_with(&prefix))) {
            return Err(
                "solar power sources must not combine total power with its phase readings".into(),
            );
        }
    }
    // One Cerbo system service owns the aggregate; multiple instances would overlap.
    let mut system_instances = sources
        .iter()
        .filter(|s| s.starts_with("system/"))
        .filter_map(|s| s.split('/').nth(1));
    if let Some(first) = system_instances.next() {
        if system_instances.any(|instance| instance != first) {
            return Err("solar power system aggregates must use one system instance".into());
        }
    }
    Ok(())
}

pub(crate) struct Reading {
    pub value: Value,
    pub received_at: Instant,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Fresh,
    Stale,
    Unavailable,
    Unconfigured,
}

#[derive(Debug, Serialize)]
pub struct Metric {
    pub value: Option<f64>,
    pub unit: &'static str,
    pub status: Status,
    pub sources: Vec<String>,
    pub age_seconds: Option<u64>,
}

#[derive(Debug, Serialize)]
pub struct Metrics {
    pub battery_soc: Metric,
    pub solar_power: Metric,
    pub solar_today: Metric,
}

#[derive(Debug, Serialize)]
pub struct Report {
    pub text: String,
    pub status: Status,
}

#[derive(Debug, Serialize)]
pub struct Reports {
    pub battery: Report,
    pub solar: Report,
    pub solar_today: Report,
    pub alarms: Report,
    pub status: Report,
}

#[derive(Debug, Serialize)]
pub struct EnergyResponse {
    pub schema_version: u8,
    pub generated_at: u64,
    pub mqtt_connected: bool,
    pub metrics: Metrics,
    pub alarms: Alarms,
    pub reports: Reports,
}

impl EnergyResponse {
    pub(crate) fn build(
        cfg: &EnergyConfig,
        connected: bool,
        readings: &HashMap<String, Reading>,
        now: Instant,
    ) -> Self {
        let metrics = Metrics {
            battery_soc: metric(
                &cfg.battery_sources,
                "%",
                connected,
                readings,
                now,
                cfg.max_age,
            ),
            solar_power: metric(
                &cfg.solar_power_sources,
                "W",
                connected,
                readings,
                now,
                cfg.max_age,
            ),
            solar_today: metric(
                &cfg.solar_today_sources,
                "kWh",
                connected,
                readings,
                now,
                cfg.max_age,
            ),
        };
        let battery = report(&metrics.battery_soc, "Battery charge", connected);
        let solar = report(&metrics.solar_power, "Solar power", connected);
        let daily_label = if !cfg.solar_today_sources.is_empty()
            && cfg
                .solar_today_sources
                .iter()
                .all(|source| source.starts_with("solarcharger/"))
        {
            "Solar charger generation today"
        } else {
            "Solar generation today"
        };
        let solar_today = report(&metrics.solar_today, daily_label, connected);
        let alarms = Alarms::build(cfg, connected, readings, now);
        let alarm_report = alarms.report(connected);
        let status = Report {
            status: if connected {
                worst_status([
                    battery.status,
                    solar.status,
                    solar_today.status,
                    alarm_report.status,
                ])
            } else {
                Status::Unavailable
            },
            text: if connected {
                format!(
                    "{} {} {} {}",
                    battery.text, solar.text, solar_today.text, alarm_report.text
                )
            } else {
                "Energy data is unavailable because the gateway is disconnected from Cerbo GX."
                    .into()
            },
        };
        Self {
            schema_version: 1,
            generated_at: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs(),
            mqtt_connected: connected,
            metrics,
            alarms,
            reports: Reports {
                battery,
                solar,
                solar_today,
                alarms: alarm_report,
                status,
            },
        }
    }
}

fn metric(
    sources: &[String],
    unit: &'static str,
    connected: bool,
    readings: &HashMap<String, Reading>,
    now: Instant,
    max_age: Duration,
) -> Metric {
    let mut metric = Metric {
        value: None,
        unit,
        status: if sources.is_empty() {
            Status::Unconfigured
        } else {
            Status::Unavailable
        },
        sources: sources.to_vec(),
        age_seconds: None,
    };
    if sources.is_empty() || !connected {
        return metric;
    }
    let mut sum = 0.0;
    let mut oldest = Duration::ZERO;
    let mut unavailable = false;
    let mut complete_age = true;
    for source in sources {
        let Some(reading) = readings.get(source) else {
            unavailable = true;
            complete_age = false;
            continue;
        };
        oldest = oldest.max(now.saturating_duration_since(reading.received_at));
        match reading.value.as_f64() {
            Some(value) if value.is_finite() && value >= 0.0 && (unit != "%" || value <= 100.0) => {
                sum += value
            }
            _ => unavailable = true,
        }
    }
    metric.age_seconds = complete_age.then_some(oldest.as_secs());
    if unavailable || !sum.is_finite() {
        return metric;
    }
    if oldest > max_age {
        metric.status = Status::Stale;
        return metric;
    }
    metric.value = Some(sum);
    metric.status = Status::Fresh;
    metric
}

fn worst_status<const N: usize>(statuses: [Status; N]) -> Status {
    for status in [Status::Unavailable, Status::Stale, Status::Unconfigured] {
        if statuses.contains(&status) {
            return status;
        }
    }
    Status::Fresh
}

fn decimal(value: f64, digits: usize) -> String {
    let value = if value == 0.0 { 0.0 } else { value };
    // Bound spoken numbers as well as alarm summaries for voice service limits.
    if value >= 1_000_000_000.0 {
        let scientific = format!("{value:.digits$e}");
        if let Some((coefficient, exponent)) = scientific.split_once('e') {
            let coefficient = if coefficient.contains('.') {
                coefficient.trim_end_matches('0').trim_end_matches('.')
            } else {
                coefficient
            };
            return format!("{coefficient} times ten to the power of {exponent}");
        }
    }
    let text = format!("{value:.digits$}");
    if text.contains('.') {
        text.trim_end_matches('0').trim_end_matches('.').into()
    } else {
        text
    }
}

fn report(metric: &Metric, label: &str, connected: bool) -> Report {
    let text = match (metric.status, metric.value) {
        (Status::Fresh, Some(value)) => {
            let (amount, unit) = match metric.unit {
                "%" => (decimal(value, 1), "percent"),
                "W" if value >= 1000.0 => (decimal(value / 1000.0, 2), "kilowatts"),
                "W" => (decimal(value, 0), "watts"),
                _ => (decimal(value, 2), "kilowatt hours"),
            };
            let unit = if amount == "1" {
                match unit {
                    "watts" => "watt",
                    "kilowatts" => "kilowatt",
                    "kilowatt hours" => "kilowatt hour",
                    _ => unit,
                }
            } else {
                unit
            };
            format!("{label} is {amount} {unit}.")
        }
        (Status::Unconfigured, _) => format!("{label} is not configured."),
        (Status::Stale, _) => {
            format!("{label} data is stale. Please try again after telemetry updates.")
        }
        _ if !connected => {
            format!("{label} is unavailable because the gateway is disconnected from Cerbo GX.")
        }
        _ => format!("{label} is unavailable because current telemetry is missing or invalid."),
    };
    Report {
        text,
        status: metric.status,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn readings(now: Instant, values: &[(&str, Value, u64)]) -> HashMap<String, Reading> {
        values
            .iter()
            .map(|(source, value, age)| {
                (
                    (*source).into(),
                    Reading {
                        value: value.clone(),
                        received_at: now - Duration::from_secs(*age),
                    },
                )
            })
            .collect()
    }

    #[test]
    fn config_rejects_wrong_units_duplicates_and_overlapping_sources() {
        for power in [
            "system/0/Dc/Pv/Power,system/0/Dc/Pv/Power",
            "system/0/Dc/Pv/Power,solarcharger/1/Yield/Power",
            "system/0/Ac/PvOnGrid/L1/Power,pvinverter/1/Ac/Power",
            "pvinverter/1/Ac/Power,pvinverter/1/Ac/L1/Power",
            "system/0/Ac/PvOnOutput/Total/Power,system/0/Ac/PvOnOutput/L3/Power",
            "system/0/Dc/Pv/Power,system/1/Ac/PvOnGrid/L1/Power",
            "system/0/Dc/Pv/Power,",
            "system/00/Dc/Pv/Power",
            "N/portal/system/0/Dc/Pv/Power",
        ] {
            assert!(
                EnergyConfig::parse("system/0/Dc/Battery/Soc", power, "", "120").is_err(),
                "{power}"
            );
        }
        assert!(EnergyConfig::parse("battery/2/Soc,battery/3/Soc", "", "", "120").is_err());
        assert!(EnergyConfig::parse(
            "system/0/Dc/Battery/Soc",
            "",
            "solarcharger/2/Yield/User",
            "120"
        )
        .is_err());
        assert!(EnergyConfig::parse("", "", "", "0").is_err());
        assert!(EnergyConfig::parse("", "", "", "oops").is_err());
    }

    #[test]
    fn aggregates_explicit_sources_and_formats_shared_english_reports() {
        let now = Instant::now();
        let cfg = EnergyConfig::parse(
            "system/0/Dc/Battery/Soc",
            "system/0/Dc/Pv/Power,system/0/Ac/PvOnOutput/L1/Power",
            "solarcharger/2/History/Daily/0/Yield,solarcharger/1/History/Daily/0/Yield",
            "120",
        )
        .unwrap();
        let data = readings(
            now,
            &[
                ("battery/9/Soc", json!(99), 0),
                ("system/0/Dc/Battery/Soc", json!(77.54), 3),
                ("system/0/Dc/Pv/Power", json!(1200), 2),
                ("system/0/Ac/PvOnOutput/L1/Power", json!(350), 9),
                ("solarcharger/1/History/Daily/0/Yield", json!(3.1), 0),
                ("solarcharger/2/History/Daily/0/Yield", json!(0.15), 4),
            ],
        );
        let response = EnergyResponse::build(&cfg, true, &data, now);
        assert_eq!(response.metrics.battery_soc.value, Some(77.54));
        assert_eq!(response.metrics.solar_power.value, Some(1550.0));
        assert_eq!(response.metrics.solar_power.age_seconds, Some(9));
        assert_eq!(response.metrics.solar_today.value, Some(3.25));
        assert_eq!(
            response.reports.battery.text,
            "Battery charge is 77.5 percent."
        );
        assert_eq!(
            response.reports.solar.text,
            "Solar power is 1.55 kilowatts."
        );
        assert_eq!(
            response.reports.solar_today.text,
            "Solar charger generation today is 3.25 kilowatt hours."
        );
        assert_eq!(response.reports.status.status, Status::Unconfigured);
        assert_eq!(
            response.metrics.solar_today.sources[0],
            "solarcharger/1/History/Daily/0/Yield"
        );
    }

    #[test]
    fn incomplete_invalid_and_stale_aggregates_never_return_partial_numbers() {
        let now = Instant::now();
        let cfg = EnergyConfig::parse(
            "",
            "solarcharger/1/Yield/Power,solarcharger/2/Yield/Power",
            "",
            "120",
        )
        .unwrap();
        for (second, age, expected) in [
            (None, 0, Status::Unavailable),
            (Some(Value::Null), 0, Status::Unavailable),
            (Some(json!("400")), 0, Status::Unavailable),
            (Some(json!(-1)), 0, Status::Unavailable),
            (Some(json!(400)), 121, Status::Stale),
        ] {
            let mut data = readings(now, &[("solarcharger/1/Yield/Power", json!(600), 0)]);
            if let Some(value) = second {
                data.insert(
                    "solarcharger/2/Yield/Power".into(),
                    Reading {
                        value,
                        received_at: now - Duration::from_secs(age),
                    },
                );
            }
            let response = EnergyResponse::build(&cfg, true, &data, now);
            assert_eq!(response.metrics.solar_power.status, expected);
            assert_eq!(response.metrics.solar_power.value, None);
            assert!(!response.reports.solar.text.contains("600"));
        }
    }

    #[test]
    fn real_zero_is_fresh_and_expiration_uses_monotonic_duration() {
        let now = Instant::now();
        let cfg = EnergyConfig::default();
        let data = readings(now, &[("system/0/Dc/Battery/Soc", json!(0), 120)]);
        let fresh = EnergyResponse::build(&cfg, true, &data, now);
        assert_eq!(fresh.metrics.battery_soc.value, Some(0.0));
        assert_eq!(fresh.reports.battery.text, "Battery charge is 0 percent.");
        let stale = EnergyResponse::build(&cfg, true, &data, now + Duration::from_millis(1));
        assert_eq!(stale.metrics.battery_soc.status, Status::Stale);
        assert_eq!(stale.metrics.battery_soc.value, None);
        assert_eq!(stale.metrics.solar_power.status, Status::Unconfigured);
        let offline = EnergyResponse::build(&cfg, false, &data, now);
        assert_eq!(offline.metrics.battery_soc.status, Status::Unavailable);
        assert_eq!(offline.metrics.battery_soc.age_seconds, None);
        assert!(offline
            .reports
            .status
            .text
            .contains("disconnected from Cerbo GX"));
    }

    #[test]
    fn invalid_soc_and_nonfinite_aggregate_are_unavailable() {
        let now = Instant::now();
        let cfg = EnergyConfig::parse(
            "system/0/Dc/Battery/Soc",
            "solarcharger/1/Yield/Power,solarcharger/2/Yield/Power",
            "",
            "120",
        )
        .unwrap();
        let data = readings(
            now,
            &[
                ("system/0/Dc/Battery/Soc", json!(101), 0),
                ("solarcharger/1/Yield/Power", json!(f64::MAX), 0),
                ("solarcharger/2/Yield/Power", json!(f64::MAX), 0),
            ],
        );
        let response = EnergyResponse::build(&cfg, true, &data, now);
        assert_eq!(response.metrics.battery_soc.value, None);
        assert_eq!(response.metrics.solar_power.status, Status::Unavailable);
        assert!(serde_json::to_string(&response).is_ok());
    }

    #[test]
    fn report_rounding_and_units_are_readable() {
        for (value, unit, expected) in [
            (100.0, "%", "Value is 100 percent."),
            (0.0, "W", "Value is 0 watts."),
            (1.0, "W", "Value is 1 watt."),
            (999.4, "W", "Value is 999 watts."),
            (1000.0, "W", "Value is 1 kilowatt."),
            (1254.0, "W", "Value is 1.25 kilowatts."),
            (1.0, "kWh", "Value is 1 kilowatt hour."),
            (12.346, "kWh", "Value is 12.35 kilowatt hours."),
        ] {
            let metric = Metric {
                value: Some(value),
                unit,
                status: Status::Fresh,
                sources: vec![],
                age_seconds: Some(0),
            };
            assert_eq!(report(&metric, "Value", true).text, expected);
        }
    }

    #[test]
    fn custom_pv_daily_counter_combines_with_charger_yield() {
        let now = Instant::now();
        let cfg = EnergyConfig::parse(
            "system/0/Dc/Battery/Soc",
            "system/0/Dc/Pv/Power",
            "solarcharger/290/History/Daily/0/Yield,pvinverter/369/Ac/Energy/Daily",
            "120",
        )
        .unwrap()
        .with_alarm_sources("battery/512/Alarms/LowVoltage")
        .unwrap();
        let data = readings(
            now,
            &[
                ("system/0/Dc/Battery/Soc", json!(66.4), 0),
                ("system/0/Dc/Pv/Power", json!(4000), 0),
                ("solarcharger/290/History/Daily/0/Yield", json!(3.1), 0),
                ("pvinverter/369/Ac/Energy/Daily", json!(1.25), 0),
                ("battery/512/Alarms/LowVoltage", json!(0), 0),
            ],
        );
        let response = EnergyResponse::build(&cfg, true, &data, now);
        assert_eq!(response.metrics.solar_today.value, Some(4.35));
        assert_eq!(
            response.reports.solar_today.text,
            "Solar generation today is 4.35 kilowatt hours."
        );
        assert_eq!(response.reports.status.status, Status::Fresh);
        assert!(response
            .reports
            .status
            .text
            .ends_with("No active alarms in the monitored sources."));
        assert!(EnergyConfig::parse("", "", "pvinverter/369/Ac/Energy/Forward", "120").is_err());
    }

    #[test]
    fn extreme_finite_metrics_keep_reports_within_voice_adapter_limits() {
        let now = Instant::now();
        let cfg = EnergyConfig::parse(
            "system/0/Dc/Battery/Soc",
            "system/0/Dc/Pv/Power",
            "pvinverter/369/Ac/Energy/Daily",
            "120",
        )
        .unwrap()
        .with_alarm_sources("battery/512/Alarms/LowVoltage")
        .unwrap();
        let data = readings(
            now,
            &[
                ("system/0/Dc/Battery/Soc", json!(100), 0),
                ("system/0/Dc/Pv/Power", json!(f64::MAX), 0),
                ("pvinverter/369/Ac/Energy/Daily", json!(f64::MAX), 0),
                ("battery/512/Alarms/LowVoltage", json!(2), 0),
            ],
        );
        let response = EnergyResponse::build(&cfg, true, &data, now);
        assert_eq!(response.metrics.solar_power.value, Some(f64::MAX));
        assert!(response
            .reports
            .solar
            .text
            .contains("times ten to the power of"));
        assert!(response.reports.status.text.len() <= 1200);
    }
}
