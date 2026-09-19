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
    pub load_power_sources: Vec<String>,
    pub grid_power_sources: Vec<String>,
    pub battery_power_sources: Vec<String>,
    pub max_age: Duration,
}

impl Default for EnergyConfig {
    fn default() -> Self {
        Self {
            battery_sources: vec!["system/0/Dc/Battery/Soc".into()],
            solar_power_sources: vec![],
            solar_today_sources: vec![],
            alarm_sources: vec![],
            load_power_sources: vec![],
            grid_power_sources: vec![],
            battery_power_sources: vec![],
            max_age: Duration::from_secs(120),
        }
    }
}

impl EnergyConfig {
    pub fn with_flow_sources(
        mut self,
        load: &str,
        grid: &str,
        battery: &str,
    ) -> Result<Self, ConfigError> {
        self.load_power_sources = parse_sources(load, MetricKind::LoadPower)?;
        self.grid_power_sources = parse_sources(grid, MetricKind::GridPower)?;
        self.battery_power_sources = parse_sources(battery, MetricKind::BatteryPower)?;
        if self.battery_power_sources.len() > 1 {
            return Err(
                "GATEWAY_ENERGY_BATTERY_POWER_SOURCE must select one system battery".into(),
            );
        }
        let mut instances = self
            .load_power_sources
            .iter()
            .chain(&self.grid_power_sources)
            .chain(&self.battery_power_sources)
            .filter_map(|source| source.split('/').nth(1));
        if let Some(first) = instances.next() {
            if instances.any(|instance| instance != first) {
                return Err("energy flow sources must use one system instance".into());
            }
        }
        for source in &self.load_power_sources {
            if let Some((prefix, phase)) = source.split_once("/Ac/Consumption/") {
                for component in ["ConsumptionOnInput", "ConsumptionOnOutput"] {
                    if self
                        .load_power_sources
                        .contains(&format!("{prefix}/Ac/{component}/{phase}"))
                    {
                        return Err("AC consumption must not combine a phase total with its input or output component".into());
                    }
                }
            }
        }
        Ok(self)
    }

    fn flow_enabled(&self) -> bool {
        !self.load_power_sources.is_empty()
            || !self.grid_power_sources.is_empty()
            || !self.battery_power_sources.is_empty()
    }

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
            load_power_sources: vec![],
            grid_power_sources: vec![],
            battery_power_sources: vec![],
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
    LoadPower,
    GridPower,
    BatteryPower,
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
        ("system", "Dc/Battery/Power") => Some(MetricKind::BatteryPower),
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
            if parts.next() != Some("Ac") {
                return None;
            }
            let family = parts.next()?;
            let phase = parts.next()?;
            if parts.next() != Some("Power") || parts.next().is_some() {
                return None;
            }
            match (family, phase) {
                ("PvOnGrid" | "PvOnOutput" | "PvOnGenset", "L1" | "L2" | "L3" | "Total") => {
                    Some(MetricKind::Power)
                }
                (
                    "Consumption" | "ConsumptionOnInput" | "ConsumptionOnOutput",
                    "L1" | "L2" | "L3",
                ) => Some(MetricKind::LoadPower),
                ("Grid", "L1" | "L2" | "L3") => Some(MetricKind::GridPower),
                _ => None,
            }
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub load_power: Option<Metric>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub grid_power: Option<Metric>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub battery_power: Option<Metric>,
}

#[derive(Debug, Serialize)]
pub struct Report {
    pub text: String,
    pub status: Status,
    /// Optional concise speech, with the same freshness and alarm disclosures.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub brief_text: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct Reports {
    pub battery: Report,
    pub solar: Report,
    pub solar_today: Report,
    pub alarms: Report,
    pub status: Report,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub flow: Option<Report>,
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
            load_power: cfg.flow_enabled().then(|| {
                metric(
                    &cfg.load_power_sources,
                    "W",
                    connected,
                    readings,
                    now,
                    cfg.max_age,
                )
            }),
            grid_power: cfg.flow_enabled().then(|| {
                signed_power_metric(
                    &cfg.grid_power_sources,
                    connected,
                    readings,
                    now,
                    cfg.max_age,
                )
            }),
            battery_power: cfg.flow_enabled().then(|| {
                signed_power_metric(
                    &cfg.battery_power_sources,
                    connected,
                    readings,
                    now,
                    cfg.max_age,
                )
            }),
        };
        let flow = match (
            &metrics.load_power,
            &metrics.grid_power,
            &metrics.battery_power,
        ) {
            (Some(load), Some(grid), Some(battery)) => {
                Some(flow_report(load, grid, battery, connected))
            }
            _ => None,
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
            brief_text: Some(if connected {
                let mut parts = vec![
                    brief_metric(&metrics.battery_soc, &battery, "Battery"),
                    brief_metric(&metrics.solar_power, &solar, "Solar"),
                    brief_metric(
                        &metrics.solar_today,
                        &solar_today,
                        if daily_label == "Solar charger generation today" {
                            "Solar chargers today"
                        } else {
                            "Solar today"
                        },
                    ),
                ];
                // Retain the complete bounded alarm report, including incomplete coverage.
                if alarms.active.is_empty() {
                    parts.push(alarm_report.text.clone());
                } else {
                    parts.insert(0, alarm_report.text.clone());
                }
                parts.join(" ")
            } else {
                "Energy data is unavailable because the gateway is disconnected from Cerbo GX."
                    .into()
            }),
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
                flow,
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
    collect_metric(sources, unit, connected, readings, now, max_age, false)
}

fn signed_power_metric(
    sources: &[String],
    connected: bool,
    readings: &HashMap<String, Reading>,
    now: Instant,
    max_age: Duration,
) -> Metric {
    collect_metric(sources, "W", connected, readings, now, max_age, true)
}

fn collect_metric(
    sources: &[String],
    unit: &'static str,
    connected: bool,
    readings: &HashMap<String, Reading>,
    now: Instant,
    max_age: Duration,
    signed: bool,
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
            Some(value)
                if value.is_finite()
                    && (signed || value >= 0.0)
                    && (unit != "%" || value <= 100.0) =>
            {
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

fn spoken_amount(value: f64, unit: &str) -> String {
    let (amount, unit) = match unit {
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
    format!("{amount} {unit}")
}

fn report(metric: &Metric, label: &str, connected: bool) -> Report {
    let text = match (metric.status, metric.value) {
        (Status::Fresh, Some(value)) => {
            format!("{label} is {}.", spoken_amount(value, metric.unit))
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
        brief_text: None,
    }
}

fn brief_metric(metric: &Metric, detailed: &Report, label: &str) -> String {
    if let (Status::Fresh, Some(value)) = (metric.status, metric.value) {
        format!("{label} {}.", spoken_amount(value, metric.unit))
    } else {
        // Do not shorten away why a number cannot be spoken as current.
        detailed.text.clone()
    }
}

fn flow_report(load: &Metric, grid: &Metric, battery: &Metric, connected: bool) -> Report {
    if !connected {
        return Report {
            text: "Energy flow is unavailable because the gateway is disconnected from Cerbo GX."
                .into(),
            status: Status::Unavailable,
            brief_text: None,
        };
    }
    let load_report = report(load, "Configured AC consumption", connected);
    let grid_report = directional_power_report(grid, true);
    let battery_report = directional_power_report(battery, false);
    Report {
        text: format!(
            "{} {} {}",
            load_report.text, grid_report.text, battery_report.text
        ),
        status: worst_status([load.status, grid.status, battery.status]),
        brief_text: None,
    }
}

fn directional_power_report(metric: &Metric, grid: bool) -> Report {
    let label = if grid {
        "Net grid power"
    } else {
        "System battery power"
    };
    let (Status::Fresh, Some(value)) = (metric.status, metric.value) else {
        return report(metric, label, true);
    };
    let magnitude = value.abs();
    let amount = if magnitude > 0.0 && magnitude < 1.0 {
        "less than 1 watt".into()
    } else {
        spoken_amount(magnitude, "W")
    };
    let text = match (grid, value.total_cmp(&0.0)) {
        (_, _) if value == 0.0 => format!("{label} is 0 watts."),
        (true, std::cmp::Ordering::Greater) => format!("Net grid import is {amount}."),
        (true, _) => format!("Net grid export is {amount}."),
        (false, std::cmp::Ordering::Greater) => format!("System battery is charging at {amount}."),
        (false, _) => format!("System battery is discharging at {amount}."),
    };
    Report {
        text,
        status: metric.status,
        brief_text: None,
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

    #[test]
    fn brief_status_shortens_only_fresh_values_and_prioritizes_alerts() {
        let now = Instant::now();
        let cfg = EnergyConfig::parse(
            "system/0/Dc/Battery/Soc",
            "system/0/Dc/Pv/Power",
            "solarcharger/1/History/Daily/0/Yield",
            "120",
        )
        .unwrap()
        .with_alarm_sources("battery/1/Alarms/LowVoltage")
        .unwrap();
        let mut data = readings(
            now,
            &[
                ("system/0/Dc/Battery/Soc", json!(70), 0),
                ("system/0/Dc/Pv/Power", json!(2000), 0),
                ("solarcharger/1/History/Daily/0/Yield", json!(3.2), 0),
                ("battery/1/Alarms/LowVoltage", json!(0), 0),
            ],
        );
        let healthy = EnergyResponse::build(&cfg, true, &data, now);
        assert_eq!(healthy.reports.status.brief_text.as_deref(), Some(
            "Battery 70 percent. Solar 2 kilowatts. Solar chargers today 3.2 kilowatt hours. No active alarms in the monitored sources."
        ));
        assert!(
            healthy.reports.status.brief_text.as_ref().unwrap().len()
                < healthy.reports.status.text.len()
        );
        data.get_mut("battery/1/Alarms/LowVoltage").unwrap().value = json!(2);
        let alert = EnergyResponse::build(&cfg, true, &data, now);
        assert!(alert
            .reports
            .status
            .brief_text
            .unwrap()
            .starts_with("Alarm: Low battery voltage."));
        assert!(alert.reports.status.text.starts_with("Battery charge is"));
    }

    #[test]
    fn brief_status_retains_missing_stale_unconfigured_and_alarm_coverage() {
        let now = Instant::now();
        let cfg = EnergyConfig::parse("system/0/Dc/Battery/Soc", "system/0/Dc/Pv/Power", "", "120")
            .unwrap()
            .with_alarm_sources("battery/1/Alarms/LowVoltage,battery/1/Alarms/HighVoltage")
            .unwrap();
        let data = readings(
            now,
            &[
                ("system/0/Dc/Battery/Soc", json!(70), 121),
                ("battery/1/Alarms/LowVoltage", json!(2), 0),
            ],
        );
        let response = EnergyResponse::build(&cfg, true, &data, now);
        let brief = response.reports.status.brief_text.as_ref().unwrap();
        for report in [
            &response.reports.battery,
            &response.reports.solar,
            &response.reports.solar_today,
            &response.reports.alarms,
        ] {
            assert!(brief.contains(&report.text));
        }
        assert_eq!(response.reports.status.status, Status::Unavailable);
        assert!(!brief.contains("70 percent"));
        let offline = EnergyResponse::build(&cfg, false, &data, now);
        assert_eq!(
            offline.reports.status.brief_text.as_ref(),
            Some(&offline.reports.status.text)
        );
    }

    #[test]
    fn optional_contract_is_additive_and_flow_is_absent_by_default() {
        let response = EnergyResponse::build(
            &EnergyConfig::default(),
            true,
            &HashMap::new(),
            Instant::now(),
        );
        let json = serde_json::to_value(response).unwrap();
        assert_eq!(json["schema_version"], 1);
        assert!(json["reports"]["status"]["brief_text"].is_string());
        assert!(json["reports"]["battery"].get("brief_text").is_none());
        assert!(json["reports"].get("flow").is_none());
        for key in ["load_power", "grid_power", "battery_power"] {
            assert!(json["metrics"].get(key).is_none());
        }
    }

    #[test]
    fn synthetic_flow_fixture_matches_the_real_response_serializer() {
        let now = Instant::now();
        let cfg = EnergyConfig::parse(
            "system/0/Dc/Battery/Soc",
            "system/0/Dc/Pv/Power",
            "solarcharger/1/History/Daily/0/Yield",
            "120",
        )
        .unwrap()
        .with_alarm_sources("battery/1/Alarms/LowVoltage")
        .unwrap()
        .with_flow_sources(
            "system/0/Ac/ConsumptionOnOutput/L1/Power",
            "system/0/Ac/Grid/L1/Power",
            "system/0/Dc/Battery/Power",
        )
        .unwrap();
        let data = readings(
            now,
            &[
                ("system/0/Dc/Battery/Soc", json!(70), 2),
                ("system/0/Dc/Pv/Power", json!(2500), 3),
                ("solarcharger/1/History/Daily/0/Yield", json!(4.5), 4),
                ("system/0/Ac/ConsumptionOnOutput/L1/Power", json!(1250), 2),
                ("system/0/Ac/Grid/L1/Power", json!(-500), 2),
                ("system/0/Dc/Battery/Power", json!(750), 2),
                ("battery/1/Alarms/LowVoltage", json!(0), 1),
            ],
        );
        let mut response = EnergyResponse::build(&cfg, true, &data, now);
        // This fixture contains only synthetic values; no host clock or installation data.
        response.generated_at = 1_700_000_000;
        let expected: Value =
            serde_json::from_str(include_str!("../tests/fixtures/energy-flow-v1.json")).unwrap();
        assert_eq!(serde_json::to_value(response).unwrap(), expected);
    }

    #[test]
    fn brief_preserves_bounded_alarm_summary_with_full_failure_notices() {
        let now = Instant::now();
        let mut cfg = EnergyConfig::parse(
            "system/0/Dc/Battery/Soc",
            "system/0/Dc/Pv/Power",
            "solarcharger/1/History/Daily/0/Yield",
            "120",
        )
        .unwrap();
        let mut data = HashMap::new();
        for instance in 0..20 {
            let source = format!("battery/{instance}/Alarms/HighDischargeCurrent");
            cfg.alarm_sources.push(source.clone());
            data.insert(
                source,
                Reading {
                    value: json!(2),
                    received_at: now,
                },
            );
        }
        // An absent source makes alarm coverage incomplete while retaining confirmed alerts.
        cfg.alarm_sources
            .push("battery/99/Alarms/LowVoltage".into());
        let response = EnergyResponse::build(&cfg, true, &data, now);
        let brief = response.reports.status.brief_text.as_ref().unwrap();
        assert!(brief.starts_with(&response.reports.alarms.text));
        assert!(brief.contains("20 alarms"));
        assert!(brief.contains("Plus 15 more active alerts"));
        assert!(brief.contains("Alarm coverage is incomplete."));
        assert!(
            brief.len() > 600,
            "Safety disclosures must not be truncated to a healthy-answer budget"
        );
        assert!(brief.len() <= 1200);
    }

    #[test]
    fn flow_config_rejects_overlap_noncanonical_sources_and_multiple_systems() {
        for (load, grid, battery) in [
            (
                "system/0/Ac/Consumption/L1/Power,system/0/Ac/ConsumptionOnInput/L1/Power",
                "",
                "",
            ),
            (
                "system/0/Ac/Consumption/L2/Power,system/0/Ac/ConsumptionOnOutput/L2/Power",
                "",
                "",
            ),
            (
                "system/0/Ac/ConsumptionOnOutput/L1/Power,system/0/Ac/ConsumptionOnOutput/L1/Power",
                "",
                "",
            ),
            ("system/0/Ac/Consumption/Total/Power", "", ""),
            ("system/00/Ac/Consumption/L1/Power", "", ""),
            (
                "system/0/Ac/Consumption/L1/Power",
                "system/1/Ac/Grid/L1/Power",
                "",
            ),
            ("", "system/0/Ac/Grid/L1/Power", "system/1/Dc/Battery/Power"),
            ("", "system/0/Ac/ActiveIn/L1/Power", ""),
            ("", "system/0/Ac/Genset/L1/Power", ""),
            ("", "system/0/Ac/Grid/Total/Power", ""),
            ("", "grid/0/Ac/L1/Power", ""),
            ("", "", "battery/0/Dc/0/Power"),
            (
                "",
                "",
                "system/0/Dc/Battery/Power,system/1/Dc/Battery/Power",
            ),
        ] {
            assert!(
                EnergyConfig::default()
                    .with_flow_sources(load, grid, battery)
                    .is_err(),
                "{load};{grid};{battery}"
            );
        }
        assert!(EnergyConfig::default().with_flow_sources(
            "system/0/Ac/ConsumptionOnInput/L1/Power,system/0/Ac/ConsumptionOnOutput/L1/Power,system/0/Ac/Consumption/L2/Power", "", ""
        ).is_ok());
    }

    #[test]
    fn flow_preserves_signed_net_power_provenance_and_default_status() {
        let now = Instant::now();
        let baseline = EnergyConfig::default();
        let cfg = baseline
            .clone()
            .with_flow_sources(
                "system/0/Ac/ConsumptionOnOutput/L1/Power,system/0/Ac/ConsumptionOnInput/L1/Power",
                "system/0/Ac/Grid/L1/Power,system/0/Ac/Grid/L2/Power",
                "system/0/Dc/Battery/Power",
            )
            .unwrap();
        let data = readings(
            now,
            &[
                ("system/0/Ac/ConsumptionOnOutput/L1/Power", json!(1000), 2),
                ("system/0/Ac/ConsumptionOnInput/L1/Power", json!(250), 3),
                ("system/0/Ac/Grid/L1/Power", json!(500), 1),
                ("system/0/Ac/Grid/L2/Power", json!(-1000), 4),
                ("system/0/Dc/Battery/Power", json!(750), 1),
            ],
        );
        let response = EnergyResponse::build(&cfg, true, &data, now);
        assert_eq!(
            response.metrics.load_power.as_ref().unwrap().value,
            Some(1250.0)
        );
        let grid = response.metrics.grid_power.as_ref().unwrap();
        assert_eq!(grid.value, Some(-500.0));
        assert_eq!(grid.age_seconds, Some(4));
        assert_eq!(grid.sources, cfg.grid_power_sources);
        let flow = response.reports.flow.as_ref().unwrap();
        assert_eq!(flow.status, Status::Fresh);
        assert_eq!(flow.text, "Configured AC consumption is 1.25 kilowatts. Net grid export is 500 watts. System battery is charging at 750 watts.");
        assert_eq!(
            serde_json::to_value(&response.reports.status).unwrap(),
            serde_json::to_value(
                EnergyResponse::build(&baseline, true, &data, now)
                    .reports
                    .status
            )
            .unwrap()
        );
    }

    #[test]
    fn flow_never_converts_missing_invalid_or_stale_phases_to_zero() {
        let now = Instant::now();
        let cfg = EnergyConfig::default()
            .with_flow_sources(
                "",
                "system/0/Ac/Grid/L1/Power,system/0/Ac/Grid/L2/Power",
                "",
            )
            .unwrap();
        for (value, age, expected) in [
            (None, 0, Status::Unavailable),
            (Some(json!(null)), 0, Status::Unavailable),
            (Some(json!("-10")), 0, Status::Unavailable),
            (Some(json!(true)), 0, Status::Unavailable),
            (Some(json!(-10)), 121, Status::Stale),
        ] {
            let mut data = readings(now, &[("system/0/Ac/Grid/L1/Power", json!(20), 0)]);
            if let Some(value) = value {
                data.extend(readings(now, &[("system/0/Ac/Grid/L2/Power", value, age)]));
            }
            let response = EnergyResponse::build(&cfg, true, &data, now);
            let grid = response.metrics.grid_power.unwrap();
            assert_eq!(grid.status, expected);
            assert_eq!(grid.value, None);
            assert_eq!(
                response.metrics.load_power.unwrap().status,
                Status::Unconfigured
            );
            assert_eq!(response.reports.flow.as_ref().unwrap().status, expected);
            assert!(!response.reports.flow.unwrap().text.contains("20 watts"));
        }
    }

    #[test]
    fn flow_directions_zero_and_overflow_are_explicit() {
        let now = Instant::now();
        let cfg = EnergyConfig::default()
            .with_flow_sources(
                "system/0/Ac/Consumption/L1/Power",
                "system/0/Ac/Grid/L1/Power,system/0/Ac/Grid/L2/Power",
                "system/0/Dc/Battery/Power",
            )
            .unwrap();
        let mut data = readings(
            now,
            &[
                ("system/0/Ac/Consumption/L1/Power", json!(-1), 0),
                ("system/0/Ac/Grid/L1/Power", json!(f64::MAX), 0),
                ("system/0/Ac/Grid/L2/Power", json!(f64::MAX), 0),
                ("system/0/Dc/Battery/Power", json!(-500), 0),
            ],
        );
        let response = EnergyResponse::build(&cfg, true, &data, now);
        assert_eq!(
            response.metrics.load_power.unwrap().status,
            Status::Unavailable
        );
        assert_eq!(response.metrics.grid_power.unwrap().value, None);
        assert!(response
            .reports
            .flow
            .unwrap()
            .text
            .contains("System battery is discharging at 500 watts."));
        for (grid_value, battery_value, expected_grid, expected_battery) in [
            (
                500.0,
                0.0,
                "Net grid import is 500 watts.",
                "System battery power is 0 watts.",
            ),
            (
                -0.0,
                -0.1,
                "Net grid power is 0 watts.",
                "System battery is discharging at less than 1 watt.",
            ),
        ] {
            data.get_mut("system/0/Ac/Grid/L1/Power").unwrap().value = json!(grid_value);
            data.get_mut("system/0/Ac/Grid/L2/Power").unwrap().value = json!(0);
            data.get_mut("system/0/Dc/Battery/Power").unwrap().value = json!(battery_value);
            let response = EnergyResponse::build(&cfg, true, &data, now);
            let flow = response.reports.flow.unwrap();
            assert!(flow.text.contains(expected_grid));
            assert!(flow.text.contains(expected_battery));
        }
        let offline = EnergyResponse::build(&cfg, false, &data, now);
        let grid = offline.metrics.grid_power.unwrap();
        assert_eq!(grid.value, None);
        assert_eq!(grid.age_seconds, None);
        assert!(offline
            .reports
            .flow
            .unwrap()
            .text
            .contains("disconnected from Cerbo GX"));
    }
}
