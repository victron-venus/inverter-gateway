//! Explicit, read-only alarm monitoring with fixed English labels.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use serde::Serialize;

use crate::energy::{EnergyConfig, Reading, Report, Status};

pub(crate) fn alarm_label(service: &str, leaf: &str) -> Option<String> {
    let name = leaf.strip_prefix("Alarms/")?;
    let (name, phase) = if service == "vebus" {
        match name.split_once('/') {
            Some((phase @ ("L1" | "L2" | "L3"), name))
                if matches!(
                    name,
                    "HighTemperature"
                        | "LowBattery"
                        | "Overload"
                        | "Ripple"
                        | "InverterImbalance"
                        | "MainsImbalance"
                ) =>
            {
                (name, Some(phase))
            }
            Some(_) => return None,
            None => (name, None),
        }
    } else {
        (name, None)
    };
    let label = match (service, name) {
        ("vebus", "HighDcCurrent") => "High inverter DC current",
        ("vebus", "HighDcVoltage") => "High inverter DC voltage",
        ("vebus", "LowBattery") => "Low inverter battery voltage",
        ("vebus", "PhaseRotation") => "Incorrect AC phase rotation",
        ("vebus", "Ripple") => "High DC ripple",
        ("vebus", "TemperatureSensor") => "Battery temperature sensor fault",
        ("vebus", "VoltageSensor") => "Battery voltage sensor fault",
        ("vebus", "GridLost") => "Grid connection lost",
        ("vebus", "BmsConnectionLost") => "Battery management connection lost",
        ("vebus", "InverterImbalance") => "Inverter imbalance",
        ("vebus", "MainsImbalance") => "Mains imbalance",
        ("vebus", "HighTemperature") => "High inverter temperature",
        ("vebus", "Overload") => "Inverter overload",
        ("battery", "Alarm") => "Battery alarm",
        ("battery", "CommunicationError") => "Battery communication error",
        ("battery", "LowVoltage") => "Low battery voltage",
        ("battery", "HighVoltage") => "High battery voltage",
        ("battery", "HighCellVoltage") => "High battery cell voltage",
        ("battery", "LowCellVoltage") => "Low battery cell voltage",
        ("battery", "LowStarterVoltage") => "Low starter battery voltage",
        ("battery", "HighStarterVoltage") => "High starter battery voltage",
        ("battery", "LowSoc") => "Low battery charge",
        ("battery", "HighChargeCurrent") => "High battery charging current",
        ("battery", "HighDischargeCurrent") => "High battery discharging current",
        ("battery", "HighCurrent") => "High battery current",
        ("battery", "CellImbalance") => "Battery cell imbalance",
        ("battery", "InternalFailure") => "Internal battery fault",
        ("battery", "HighChargeTemperature") => "High battery charging temperature",
        ("battery", "LowChargeTemperature") => "Low battery charging temperature",
        ("battery", "LowTemperature") => "Low battery temperature",
        ("battery", "HighTemperature") => "High battery temperature",
        ("battery", "MidVoltage") => "Battery midpoint voltage deviation",
        ("battery", "Contactor") => "Battery contactor fault",
        ("battery", "BmsCable") => "Battery management cable fault",
        ("battery", "HighInternalTemperature") => "High internal battery temperature",
        ("battery", "FuseBlown") => "Battery fuse blown",
        ("battery", "CircuitBreakerTripped") => "Battery circuit breaker tripped",
        _ => return None,
    };
    Some(match phase {
        Some(phase) => format!("{label} on phase {phase}"),
        None => label.into(),
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    Warning,
    Alarm,
}

#[derive(Debug, Serialize)]
pub struct ActiveAlarm {
    pub source: String,
    pub severity: Severity,
    pub label: String,
}

#[derive(Debug, Serialize)]
pub struct Alarms {
    pub status: Status,
    pub active: Vec<ActiveAlarm>,
    pub sources: Vec<String>,
    pub age_seconds: Option<u64>,
}

impl Alarms {
    pub(crate) fn build(
        cfg: &EnergyConfig,
        connected: bool,
        readings: &HashMap<String, Reading>,
        now: Instant,
    ) -> Self {
        let mut alarms = Self {
            status: if cfg.alarm_sources.is_empty() {
                Status::Unconfigured
            } else {
                Status::Unavailable
            },
            active: vec![],
            sources: cfg.alarm_sources.clone(),
            age_seconds: None,
        };
        if alarms.sources.is_empty() || !connected {
            return alarms;
        }
        let mut unavailable = false;
        let mut stale = false;
        let mut complete_age = true;
        let mut oldest = Duration::ZERO;
        for source in &alarms.sources {
            let Some(reading) = readings.get(source) else {
                unavailable = true;
                complete_age = false;
                continue;
            };
            let age = now.saturating_duration_since(reading.received_at);
            oldest = oldest.max(age);
            let severity = match reading.value.as_f64() {
                Some(0.0) => None,
                Some(1.0) => Some(Severity::Warning),
                Some(2.0) => Some(Severity::Alarm),
                _ => {
                    unavailable = true;
                    continue;
                }
            };
            if age > cfg.max_age {
                stale = true;
                continue;
            }
            if let Some(severity) = severity {
                let label = source.split_once('/').and_then(|(service, path)| {
                    path.split_once('/')
                        .and_then(|(_, leaf)| alarm_label(service, leaf))
                });
                if let Some(label) = label {
                    alarms.active.push(ActiveAlarm {
                        source: source.clone(),
                        severity,
                        label,
                    });
                } else {
                    unavailable = true;
                }
            }
        }
        alarms.age_seconds = complete_age.then_some(oldest.as_secs());
        alarms.status = if unavailable {
            Status::Unavailable
        } else if stale {
            Status::Stale
        } else {
            Status::Fresh
        };
        alarms
    }

    pub(crate) fn report(&self, connected: bool) -> Report {
        const SPOKEN_ALARM_LIMIT: usize = 5;
        let mut messages = Vec::new();
        let summarized = self.active.len() > SPOKEN_ALARM_LIMIT;
        if summarized {
            let alarms = self
                .active
                .iter()
                .filter(|alarm| alarm.severity == Severity::Alarm)
                .count();
            let warnings = self.active.len() - alarms;
            let alarm_word = if alarms == 1 { "alarm" } else { "alarms" };
            let warning_word = if warnings == 1 { "warning" } else { "warnings" };
            messages.push(format!(
                "Monitored sources report {alarms} {alarm_word} and {warnings} {warning_word}."
            ));
        }
        // Preserve full source order for small reports, prioritizing alarms in bounded summaries.
        let spoken: Vec<_> = if summarized {
            self.active
                .iter()
                .filter(|alarm| alarm.severity == Severity::Alarm)
                .chain(
                    self.active
                        .iter()
                        .filter(|alarm| alarm.severity == Severity::Warning),
                )
                .take(SPOKEN_ALARM_LIMIT)
                .collect()
        } else {
            self.active.iter().collect()
        };
        for alarm in spoken {
            let severity = match alarm.severity {
                Severity::Warning => "Warning",
                Severity::Alarm => "Alarm",
            };
            messages.push(format!("{severity}: {}.", alarm.label));
        }
        if summarized {
            let remaining = self.active.len() - SPOKEN_ALARM_LIMIT;
            let alert_word = if remaining == 1 { "alert" } else { "alerts" };
            messages.push(format!(
                "Plus {remaining} more active {alert_word}. The full list is available from the gateway."
            ));
        }
        let coverage = match self.status {
            Status::Unconfigured => Some("Alarm monitoring is not configured."),
            Status::Unavailable if !connected => {
                Some("Alarm data is unavailable because the gateway is disconnected from Cerbo GX.")
            }
            Status::Unavailable => Some(
                "Some monitored alarm data is missing or invalid. Alarm coverage is incomplete.",
            ),
            Status::Stale => {
                Some("Some monitored alarm data is stale. Alarm coverage is incomplete.")
            }
            Status::Fresh if self.active.is_empty() => {
                Some("No active alarms in the monitored sources.")
            }
            Status::Fresh => None,
        };
        if let Some(coverage) = coverage {
            messages.push(coverage.into());
        }
        Report {
            text: messages.join(" "),
            status: self.status,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Value};

    fn cfg() -> EnergyConfig {
        EnergyConfig::default().with_alarm_sources("vebus/0/Alarms/L1/Overload,battery/512/Alarms/LowVoltage,battery/512/Alarms/HighVoltage").unwrap()
    }

    fn readings(now: Instant, values: [(Value, u64); 3]) -> HashMap<String, Reading> {
        cfg()
            .alarm_sources
            .into_iter()
            .zip(values)
            .map(|(source, (value, age))| {
                (
                    source,
                    Reading {
                        value,
                        received_at: now - Duration::from_secs(age),
                    },
                )
            })
            .collect()
    }

    #[test]
    fn validates_known_alarm_sources_and_fixed_labels() {
        for source in [
            "vebus/0/Alarms/L4/Overload",
            "battery/512/Alarms/CustomMessage",
            "platform/0/Notifications/0/Active",
            "solarcharger/1/Alarms/LowVoltage",
            "vebus/0/Alarms/L1/GridLost",
            "battery/01/Alarms/LowSoc",
        ] {
            assert!(
                EnergyConfig::default().with_alarm_sources(source).is_err(),
                "{source}"
            );
        }
        assert!(EnergyConfig::default()
            .with_alarm_sources("battery/1/Alarms/LowSoc,battery/1/Alarms/LowSoc")
            .is_err());
        assert_eq!(
            alarm_label("vebus", "Alarms/L3/HighTemperature").unwrap(),
            "High inverter temperature on phase L3"
        );
    }

    #[test]
    fn no_alarm_claim_requires_every_selected_source_to_be_fresh_and_zero() {
        let now = Instant::now();
        let data = readings(now, [(json!(0), 0), (json!(0), 120), (json!(0), 2)]);
        let alarms = Alarms::build(&cfg(), true, &data, now);
        assert_eq!(alarms.status, Status::Fresh);
        assert!(alarms.active.is_empty());
        assert_eq!(alarms.age_seconds, Some(120));
        assert_eq!(
            alarms.report(true).text,
            "No active alarms in the monitored sources."
        );
        let mut partial = data;
        partial.remove("battery/512/Alarms/HighVoltage");
        let alarms = Alarms::build(&cfg(), true, &partial, now);
        assert_eq!(alarms.status, Status::Unavailable);
        assert_eq!(alarms.age_seconds, None);
        assert!(alarms.report(true).text.contains("coverage is incomplete"));
        assert!(!alarms.report(true).text.contains("No active alarms"));
    }

    #[test]
    fn active_list_preserves_each_confirmed_warning_and_alarm() {
        let now = Instant::now();
        let data = readings(now, [(json!(2), 3), (json!(1), 4), (json!(2), 5)]);
        let alarms = Alarms::build(&cfg(), true, &data, now);
        assert_eq!(alarms.status, Status::Fresh);
        assert_eq!(alarms.active.len(), 3);
        assert_eq!(alarms.active[0].source, "battery/512/Alarms/HighVoltage");
        assert_eq!(alarms.active[0].severity, Severity::Alarm);
        assert_eq!(alarms.active[1].severity, Severity::Warning);
        assert_eq!(alarms.active[2].label, "Inverter overload on phase L1");
        let text = alarms.report(true).text;
        assert_eq!(text, "Alarm: High battery voltage. Warning: Low battery voltage. Alarm: Inverter overload on phase L1.");
        assert!(serde_json::to_string(&alarms)
            .unwrap()
            .contains("\"severity\":\"warning\""));
    }

    #[test]
    fn stale_and_invalid_alarms_do_not_hide_confirmed_active_sources() {
        let now = Instant::now();
        for (invalid, age, expected) in [
            (json!(2), 121, Status::Stale),
            (Value::Null, 0, Status::Unavailable),
            (json!(3), 0, Status::Unavailable),
            (json!(1.5), 0, Status::Unavailable),
            (json!("0"), 0, Status::Unavailable),
            (json!(false), 0, Status::Unavailable),
        ] {
            let data = readings(now, [(invalid, age), (json!(1), 0), (json!(0), 0)]);
            let alarms = Alarms::build(&cfg(), true, &data, now);
            assert_eq!(alarms.status, expected);
            assert_eq!(alarms.active.len(), 1);
            assert_eq!(alarms.active[0].severity, Severity::Warning);
            let text = alarms.report(true).text;
            assert!(text.starts_with("Warning: Low battery voltage."));
            assert!(text.contains("coverage is incomplete"));
            assert!(!text.contains("No active alarms"));
        }
    }

    #[test]
    fn offline_and_unconfigured_alarm_monitoring_never_claims_all_clear() {
        let now = Instant::now();
        let data = readings(now, [(json!(0), 0), (json!(0), 0), (json!(0), 0)]);
        let offline = Alarms::build(&cfg(), false, &data, now);
        assert_eq!(offline.status, Status::Unavailable);
        assert_eq!(offline.age_seconds, None);
        assert!(offline
            .report(false)
            .text
            .contains("disconnected from Cerbo GX"));
        let unconfigured = Alarms::build(&EnergyConfig::default(), true, &data, now);
        assert_eq!(unconfigured.status, Status::Unconfigured);
        assert_eq!(
            unconfigured.report(true).text,
            "Alarm monitoring is not configured."
        );
    }

    #[test]
    fn large_alarm_sets_keep_full_data_and_bound_spoken_output() {
        let now = Instant::now();
        let mut cfg = EnergyConfig::default();
        let mut data = HashMap::new();
        for instance in 0..100 {
            let source = format!("battery/{instance}/Alarms/HighInternalTemperature");
            cfg.alarm_sources.push(source.clone());
            data.insert(
                source,
                Reading {
                    value: json!(if instance == 99 { 2 } else { 1 }),
                    received_at: now,
                },
            );
        }
        cfg.alarm_sources.sort();
        let alarms = Alarms::build(&cfg, true, &data, now);
        assert_eq!(alarms.active.len(), 100);
        let text = alarms.report(true).text;
        assert!(text.len() <= 1200);
        assert!(text.contains("1 alarm and 99 warnings"));
        assert!(text.contains("95 more active alerts"));
        assert!(text.contains("Alarm: High internal battery temperature."));
        let response = crate::energy::EnergyResponse::build(&cfg, true, &data, now);
        assert!(response.reports.status.text.len() <= 1200);
    }

    #[test]
    fn observed_device_alarm_extensions_have_fixed_english_labels() {
        for (service, leaf) in [
            ("vebus", "BmsConnectionLost"),
            ("vebus", "L1/InverterImbalance"),
            ("vebus", "L2/MainsImbalance"),
            ("vebus", "L3/InverterImbalance"),
            ("battery", "Alarm"),
            ("battery", "CommunicationError"),
        ] {
            let source = format!("{service}/1/Alarms/{leaf}");
            assert!(
                EnergyConfig::default().with_alarm_sources(&source).is_ok(),
                "{source}"
            );
            assert!(alarm_label(service, &format!("Alarms/{leaf}")).is_some());
        }
    }
}
