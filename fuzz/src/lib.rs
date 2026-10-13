use inverter_gateway::{energy::EnergyConfig, whitelist::controller_payload};
use serde_json::Value;
use std::collections::BTreeSet;

pub fn energy_sources(data: &[u8]) {
    if data.len() > 8192 {
        return;
    }
    let Ok(text) = std::str::from_utf8(data) else {
        return;
    };
    let fields: Vec<_> = text.splitn(4, '\n').collect();
    let field = |i| fields.get(i).copied().unwrap_or("");
    if let Ok(config) = EnergyConfig::parse(field(0), field(1), field(2), field(3)) {
        assert!(config.max_age.as_secs() > 0);
        assert!(config.battery_sources.len() <= 1);
        for sources in [
            &config.battery_sources,
            &config.solar_power_sources,
            &config.solar_today_sources,
        ] {
            assert_eq!(sources.len(), sources.iter().collect::<BTreeSet<_>>().len());
            for source in sources {
                assert!(!source.contains(['#', '+']));
                let instance = source.split('/').nth(1).unwrap();
                assert_eq!(instance.parse::<u32>().unwrap().to_string(), instance);
            }
        }
        // Accepted normal forms must remain valid and preserve every source.
        let second = EnergyConfig::parse(
            &config.battery_sources.join(","),
            &config.solar_power_sources.join(","),
            &config.solar_today_sources.join(","),
            &config.max_age.as_secs().to_string(),
        )
        .unwrap();
        assert_eq!(config.solar_power_sources, second.solar_power_sources);
        assert_eq!(config.battery_sources, second.battery_sources);
        assert_eq!(config.solar_today_sources, second.solar_today_sources);
    }
    if let Ok(config) = EnergyConfig::default().with_flow_sources(field(0), field(1), field(2)) {
        let instances: BTreeSet<_> = config
            .load_power_sources
            .iter()
            .chain(&config.grid_power_sources)
            .chain(&config.battery_power_sources)
            .map(|source| source.split('/').nth(1).unwrap())
            .collect();
        assert!(instances.len() <= 1);
        assert!(config.battery_power_sources.len() <= 1);
        for total in &config.load_power_sources {
            for component in ["ConsumptionOnInput", "ConsumptionOnOutput"] {
                if total.contains("/Ac/Consumption/") {
                    assert!(!config.load_power_sources.contains(
                        &total.replace("/Ac/Consumption/", &format!("/Ac/{component}/"))
                    ));
                }
            }
        }
    }
    let _ = EnergyConfig::default().with_alarm_sources(text);
}

pub fn command_payload(data: &[u8]) {
    if data.len() > 100_001 {
        return;
    }
    let Ok(body) = serde_json::from_slice::<Value>(data) else {
        return;
    };
    for name in [
        "toggle",
        "dry_run",
        "ess_mode",
        "set_ess_mode",
        "electricity_tariff",
        "setpoint_override",
    ] {
        if let Ok(normalized) = controller_payload(name, body.clone()) {
            assert!(normalized.is_object());
            assert_eq!(
                controller_payload(name, normalized.clone()).unwrap(),
                normalized
            );
            let mut extra = body.clone();
            extra
                .as_object_mut()
                .unwrap()
                .insert("untrusted_extra_field".into(), Value::Null);
            assert!(controller_payload(name, extra).is_err());
            match name {
                "toggle" => {
                    assert_eq!(normalized.as_object().unwrap().len(), 2);
                    assert!(matches!(normalized["state"].as_str(), Some("on" | "off")));
                    assert!(!normalized["entity"]
                        .as_str()
                        .unwrap()
                        .contains(['/', '#', '+']));
                }
                "dry_run" => assert!(normalized["value"].is_boolean()),
                "ess_mode" => assert!(normalized.as_object().unwrap().is_empty()),
                "setpoint_override" => {
                    let value = &normalized["value"];
                    assert!(
                        value.is_null() || value.as_i64().is_some_and(|v| i32::try_from(v).is_ok())
                    );
                }
                _ => (),
            }
        }
    }
    // No unknown command may be transformed into a controller write.
    assert!(controller_payload("raw_mqtt_passthrough", body).is_err());
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn corpus_regressions() {
        for (name, run) in [
            ("energy_sources", energy_sources as fn(&[u8])),
            ("command_payload", command_payload as fn(&[u8])),
        ] {
            let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("corpus")
                .join(name);
            for entry in std::fs::read_dir(path).unwrap() {
                run(&std::fs::read(entry.unwrap().path()).unwrap());
            }
            run(b"");
            run(&[0xff, 0]);
        }
    }
}
