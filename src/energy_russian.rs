//! Deterministic Russian wording over already validated energy facts.
//!
//! This module never parses English prose, changes telemetry or calls an LLM.
use crate::alarms::{Alarms, Severity};
use crate::energy::{EnergyResponse, Metric, Status};

const DISCONNECTED: &str = "Энергетические данные недоступны: нет связи с Cerbo GX.";

pub(crate) fn localize(response: &mut EnergyResponse) {
    let connected = response.mqtt_connected;
    let metrics = &response.metrics;
    let charger_only = !metrics.solar_today.sources.is_empty()
        && metrics
            .solar_today
            .sources
            .iter()
            .all(|source| source.starts_with("solarcharger/"));
    let daily = if charger_only {
        "Выработка солнечных контроллеров сегодня"
    } else {
        "Солнечная выработка сегодня"
    };
    response.reports.battery.text =
        metric_report(&metrics.battery_soc, "Заряд аккумулятора", connected);
    response.reports.solar.text =
        metric_report(&metrics.solar_power, "Солнечная мощность", connected);
    response.reports.solar_today.text = metric_report(&metrics.solar_today, daily, connected);
    for alarm in &mut response.alarms.active {
        alarm.label = alarm_label(&alarm.source);
    }
    response.reports.alarms.text = alarm_report(&response.alarms, connected);
    response.reports.status.text = if connected {
        [
            &response.reports.battery.text,
            &response.reports.solar.text,
            &response.reports.solar_today.text,
            &response.reports.alarms.text,
        ]
        .into_iter()
        .cloned()
        .collect::<Vec<_>>()
        .join(" ")
    } else {
        DISCONNECTED.into()
    };
    response.reports.status.brief_text = Some(if connected {
        let mut parts = vec![
            metric_report(&metrics.battery_soc, "Аккумулятор", connected),
            metric_report(&metrics.solar_power, "Солнце", connected),
            metric_report(
                &metrics.solar_today,
                if charger_only {
                    "Солнечные контроллеры сегодня"
                } else {
                    "Солнце сегодня"
                },
                connected,
            ),
        ];
        if response.alarms.active.is_empty() {
            parts.push(response.reports.alarms.text.clone());
        } else {
            parts.insert(0, response.reports.alarms.text.clone());
        }
        parts.join(" ")
    } else {
        DISCONNECTED.into()
    });
    if let (Some(report), Some(load), Some(grid), Some(battery)) = (
        &mut response.reports.flow,
        &metrics.load_power,
        &metrics.grid_power,
        &metrics.battery_power,
    ) {
        report.text = if connected {
            format!(
                "{} {} {}",
                metric_report(load, "Выбранные нагрузки переменного тока", true),
                directional_report(grid, true),
                directional_report(battery, false)
            )
        } else {
            "Данные о потоках энергии недоступны: нет связи с Cerbo GX.".into()
        };
    }
}

fn metric_report(metric: &Metric, label: &str, connected: bool) -> String {
    match (metric.status, metric.value) {
        (Status::Fresh, Some(value)) => format!("{label}: {}.", amount(value, metric.unit)),
        (Status::Unconfigured, _) => format!("{label}: источник не настроен."),
        (Status::Stale, _) => format!("{label}: данные устарели, дождитесь обновления телеметрии."),
        _ if !connected => format!("{label}: данные недоступны, нет связи с Cerbo GX."),
        _ => format!("{label}: текущие данные отсутствуют или некорректны."),
    }
}

fn directional_report(metric: &Metric, grid: bool) -> String {
    let label = if grid {
        "Суммарная мощность сети"
    } else {
        "Мощность системного аккумулятора"
    };
    let (Status::Fresh, Some(value)) = (metric.status, metric.value) else {
        return metric_report(metric, label, true);
    };
    if value == 0.0 {
        return format!("{label}: ноль ватт.");
    }
    let direction = match (grid, value > 0.0) {
        (true, true) => "Получение энергии из сети",
        (true, false) => "Отдача энергии в сеть",
        (false, true) => "Зарядка системного аккумулятора",
        (false, false) => "Разрядка системного аккумулятора",
    };
    let power = if value.abs() < 1.0 {
        "меньше одного ватта".into()
    } else {
        amount(value.abs(), "W")
    };
    format!("{direction}: {power}.")
}

fn form(value: u64, one: &'static str, few: &'static str, many: &'static str) -> &'static str {
    if (11..=14).contains(&(value % 100)) {
        many
    } else {
        match value % 10 {
            1 => one,
            2..=4 => few,
            _ => many,
        }
    }
}

fn integer_words(value: u64, feminine: bool) -> String {
    if value == 0 {
        return "ноль".into();
    }
    let mut parts = Vec::new();
    for (divisor, one, few, many, female) in [
        (1_000_000_000, "миллиард", "миллиарда", "миллиардов", false),
        (1_000_000, "миллион", "миллиона", "миллионов", false),
        (1_000, "тысяча", "тысячи", "тысяч", true),
    ] {
        let group = value / divisor % 1000;
        if group > 0 {
            parts.push(under_thousand(group, female));
            parts.push(form(group, one, few, many).into());
        }
    }
    if !value.is_multiple_of(1000) {
        parts.push(under_thousand(value % 1000, feminine));
    }
    parts.join(" ")
}

fn under_thousand(value: u64, feminine: bool) -> String {
    let hundreds = [
        "",
        "сто",
        "двести",
        "триста",
        "четыреста",
        "пятьсот",
        "шестьсот",
        "семьсот",
        "восемьсот",
        "девятьсот",
    ];
    let tens = [
        "",
        "",
        "двадцать",
        "тридцать",
        "сорок",
        "пятьдесят",
        "шестьдесят",
        "семьдесят",
        "восемьдесят",
        "девяносто",
    ];
    let teens = [
        "десять",
        "одиннадцать",
        "двенадцать",
        "тринадцать",
        "четырнадцать",
        "пятнадцать",
        "шестнадцать",
        "семнадцать",
        "восемнадцать",
        "девятнадцать",
    ];
    let ones = [
        "",
        if feminine { "одна" } else { "один" },
        if feminine { "две" } else { "два" },
        "три",
        "четыре",
        "пять",
        "шесть",
        "семь",
        "восемь",
        "девять",
    ];
    let mut parts = Vec::new();
    if value >= 100 {
        parts.push(hundreds[(value / 100) as usize]);
    }
    let tail = value % 100;
    if (10..20).contains(&tail) {
        parts.push(teens[(tail - 10) as usize]);
    } else {
        if tail >= 20 {
            parts.push(tens[(tail / 10) as usize]);
        }
        if !tail.is_multiple_of(10) {
            parts.push(ones[(tail % 10) as usize]);
        }
    }
    parts.join(" ")
}

fn decimal_words(text: &str) -> String {
    let (whole, fraction) = text.split_once('.').unwrap_or((text, ""));
    let whole = whole
        .parse::<u64>()
        .expect("bounded internally formatted number");
    let fraction = fraction.trim_end_matches('0');
    if fraction.is_empty() {
        return integer_words(whole, false);
    }
    let numerator = fraction
        .parse::<u64>()
        .expect("internally formatted fraction");
    let denominator = if fraction.len() == 1 {
        form(numerator, "десятая", "десятых", "десятых")
    } else {
        form(numerator, "сотая", "сотых", "сотых")
    };
    format!(
        "{} {} {} {denominator}",
        integer_words(whole, true),
        form(whole, "целая", "целых", "целых"),
        integer_words(numerator, true)
    )
}

fn amount(value: f64, unit: &str) -> String {
    let value = if value == 0.0 { 0.0 } else { value };
    let (value, digits, one, few, many) = match unit {
        "%" => (value, 1, "процент", "процента", "процентов"),
        "W" if value >= 1000.0 => (value / 1000.0, 2, "киловатт", "киловатта", "киловатт"),
        "W" => (value, 0, "ватт", "ватта", "ватт"),
        _ => (value, 2, "киловатт-час", "киловатт-часа", "киловатт-часов"),
    };
    // The upstream metric validator admits finite large values. Preserve bounded
    // scientific wording rather than overflowing an integer conversion.
    if value >= 1_000_000_000.0 {
        let scientific = format!("{value:.digits$e}");
        let (coefficient, exponent) = scientific.split_once('e').expect("scientific format");
        return format!(
            "{} умножить на десять в степени {} {many}",
            decimal_words(coefficient),
            integer_words(exponent.parse().expect("positive finite exponent"), false)
        );
    }
    let number = format!("{value:.digits$}");
    let trimmed = if number.contains('.') {
        number.trim_end_matches('0').trim_end_matches('.')
    } else {
        &number
    };
    let unit = if trimmed.contains('.') {
        few
    } else {
        form(
            trimmed.parse().expect("bounded positive number"),
            one,
            few,
            many,
        )
    };
    format!("{} {unit}", decimal_words(trimmed))
}

fn alarm_label(source: &str) -> String {
    let Some((service, path)) = source.split_once('/') else {
        return "Сигнал контролируемого источника".into();
    };
    let Some((_, leaf)) = path.split_once('/') else {
        return "Сигнал контролируемого источника".into();
    };
    let Some(name) = leaf.strip_prefix("Alarms/") else {
        return "Сигнал контролируемого источника".into();
    };
    let (name, phase) = match name.split_once('/') {
        Some((phase @ ("L1" | "L2" | "L3"), name)) => (name, Some(phase)),
        _ => (name, None),
    };
    let label = match (service, name) {
        ("vebus", "HighDcCurrent") => "Высокий постоянный ток инвертора",
        ("vebus", "HighDcVoltage") => "Высокое постоянное напряжение инвертора",
        ("vebus", "LowBattery") => "Низкое напряжение аккумулятора инвертора",
        ("vebus", "PhaseRotation") => "Неверное чередование фаз",
        ("vebus", "Ripple") => "Высокие пульсации постоянного напряжения",
        ("vebus", "TemperatureSensor") => "Неисправность датчика температуры аккумулятора",
        ("vebus", "VoltageSensor") => "Неисправность датчика напряжения аккумулятора",
        ("vebus", "GridLost") => "Потеря соединения с сетью",
        ("vebus", "BmsConnectionLost") => "Потеря связи с системой управления аккумулятором",
        ("vebus", "InverterImbalance") => "Дисбаланс инвертора",
        ("vebus", "MainsImbalance") => "Дисбаланс сети",
        ("vebus", "HighTemperature") => "Высокая температура инвертора",
        ("vebus", "Overload") => "Перегрузка инвертора",
        ("battery", "Alarm") => "Аварийный сигнал аккумулятора",
        ("battery", "CommunicationError") => "Ошибка связи с аккумулятором",
        ("battery", "LowVoltage") => "Низкое напряжение аккумулятора",
        ("battery", "HighVoltage") => "Высокое напряжение аккумулятора",
        ("battery", "HighCellVoltage") => "Высокое напряжение ячейки аккумулятора",
        ("battery", "LowCellVoltage") => "Низкое напряжение ячейки аккумулятора",
        ("battery", "LowStarterVoltage") => "Низкое напряжение стартерного аккумулятора",
        ("battery", "HighStarterVoltage") => "Высокое напряжение стартерного аккумулятора",
        ("battery", "LowSoc") => "Низкий заряд аккумулятора",
        ("battery", "HighChargeCurrent") => "Высокий ток зарядки аккумулятора",
        ("battery", "HighDischargeCurrent") => "Высокий ток разрядки аккумулятора",
        ("battery", "HighCurrent") => "Высокий ток аккумулятора",
        ("battery", "CellImbalance") => "Дисбаланс ячеек аккумулятора",
        ("battery", "InternalFailure") => "Внутренняя неисправность аккумулятора",
        ("battery", "HighChargeTemperature") => "Высокая температура зарядки аккумулятора",
        ("battery", "LowChargeTemperature") => "Низкая температура зарядки аккумулятора",
        ("battery", "LowTemperature") => "Низкая температура аккумулятора",
        ("battery", "HighTemperature") => "Высокая температура аккумулятора",
        ("battery", "MidVoltage") => "Отклонение напряжения средней точки аккумулятора",
        ("battery", "Contactor") => "Неисправность контактора аккумулятора",
        ("battery", "BmsCable") => "Неисправность кабеля системы управления аккумулятором",
        ("battery", "HighInternalTemperature") => "Высокая внутренняя температура аккумулятора",
        ("battery", "FuseBlown") => "Перегорел предохранитель аккумулятора",
        ("battery", "CircuitBreakerTripped") => "Сработал автоматический выключатель аккумулятора",
        _ => "Сигнал контролируемого источника",
    };
    match phase {
        Some(phase) => format!(
            "{label}, фаза {}",
            match phase {
                "L1" => "один",
                "L2" => "два",
                _ => "три",
            }
        ),
        None => label.into(),
    }
}

fn neuter_words(value: u64) -> String {
    let words = integer_words(value, false);
    if value % 10 == 1 && value % 100 != 11 {
        format!(
            "{}одно",
            words.strip_suffix("один").expect("masculine final one")
        )
    } else {
        words
    }
}

fn alarm_report(alarms: &Alarms, connected: bool) -> String {
    const LIMIT: usize = 5;
    let summarized = alarms.active.len() > LIMIT;
    let mut messages = Vec::new();
    if summarized {
        let count = alarms
            .active
            .iter()
            .filter(|alarm| alarm.severity == Severity::Alarm)
            .count() as u64;
        let warnings = alarms.active.len() as u64 - count;
        messages.push(format!(
            "В контролируемых источниках: {} {} и {} {}.",
            integer_words(count, true),
            form(count, "авария", "аварии", "аварий"),
            neuter_words(warnings),
            form(
                warnings,
                "предупреждение",
                "предупреждения",
                "предупреждений"
            )
        ));
    }
    let spoken: Vec<_> = if summarized {
        alarms
            .active
            .iter()
            .filter(|alarm| alarm.severity == Severity::Alarm)
            .chain(
                alarms
                    .active
                    .iter()
                    .filter(|alarm| alarm.severity == Severity::Warning),
            )
            .take(LIMIT)
            .collect()
    } else {
        alarms.active.iter().collect()
    };
    for alarm in spoken {
        let severity = match alarm.severity {
            Severity::Warning => "Предупреждение",
            Severity::Alarm => "Авария",
        };
        messages.push(format!("{severity}: {}.", alarm.label));
    }
    if summarized {
        let remaining = (alarms.active.len() - LIMIT) as u64;
        messages.push(format!(
            "Ещё {} {}. Полный список доступен в шлюзе.",
            integer_words(remaining, false),
            form(
                remaining,
                "активный сигнал",
                "активных сигнала",
                "активных сигналов"
            )
        ));
    }
    let coverage = match alarms.status {
        Status::Unconfigured => Some("Контроль аварий не настроен."),
        Status::Unavailable if !connected => {
            Some("Данные об авариях недоступны: нет связи с Cerbo GX.")
        }
        Status::Unavailable => {
            Some("Часть данных об авариях отсутствует или некорректна. Контроль аварий неполный.")
        }
        Status::Stale => Some("Часть данных об авариях устарела. Контроль аварий неполный."),
        Status::Fresh if alarms.active.is_empty() => {
            Some("В контролируемых источниках активных аварий нет.")
        }
        Status::Fresh => None,
    };
    if let Some(coverage) = coverage {
        messages.push(coverage.into());
    }
    messages.join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::energy::{EnergyConfig, EnergyLocale, Reading};
    use serde_json::{json, Value};
    use std::collections::HashMap;
    use std::time::{Duration, Instant};

    fn sample(connected: bool, age: u64) -> EnergyResponse {
        let cfg = EnergyConfig::parse(
            "system/0/Dc/Battery/Soc",
            "system/0/Dc/Pv/Power",
            "solarcharger/290/History/Daily/0/Yield",
            "120",
        )
        .unwrap()
        .with_flow_sources(
            "system/0/Ac/Consumption/L1/Power",
            "system/0/Ac/Grid/L1/Power",
            "system/0/Dc/Battery/Power",
        )
        .unwrap()
        .with_alarm_sources("vebus/0/Alarms/L1/Overload,battery/512/Alarms/LowVoltage")
        .unwrap();
        let now = Instant::now();
        let readings: HashMap<_, _> = [
            ("system/0/Dc/Battery/Soc", 77.5),
            ("system/0/Dc/Pv/Power", 1550.0),
            ("solarcharger/290/History/Daily/0/Yield", 2.21),
            ("system/0/Ac/Consumption/L1/Power", 900.0),
            ("system/0/Ac/Grid/L1/Power", -250.0),
            ("system/0/Dc/Battery/Power", 400.0),
            ("vebus/0/Alarms/L1/Overload", 2.0),
            ("battery/512/Alarms/LowVoltage", 1.0),
        ]
        .into_iter()
        .map(|(source, value)| {
            (
                source.into(),
                Reading {
                    value: json!(value),
                    received_at: now - Duration::from_secs(age),
                },
            )
        })
        .collect();
        EnergyResponse::build(&cfg, connected, &readings, now)
    }

    #[test]
    fn numbers_units_and_fractional_forms_are_ready_for_russian_tts() {
        for (value, unit, expected) in [
            (0.0, "%", "ноль процентов"),
            (-0.0, "W", "ноль ватт"),
            (1.0, "%", "один процент"),
            (2.0, "%", "два процента"),
            (5.0, "%", "пять процентов"),
            (11.0, "%", "одиннадцать процентов"),
            (21.0, "%", "двадцать один процент"),
            (77.5, "%", "семьдесят семь целых пять десятых процента"),
            (1.0, "W", "один ватт"),
            (22.0, "W", "двадцать два ватта"),
            (114.0, "W", "сто четырнадцать ватт"),
            (1000.0, "W", "один киловатт"),
            (1550.0, "W", "одна целая пятьдесят пять сотых киловатта"),
            (2010.0, "W", "две целых одна сотая киловатта"),
            (0.01, "kWh", "ноль целых одна сотая киловатт-часа"),
            (0.02, "kWh", "ноль целых две сотых киловатт-часа"),
            (1.1, "kWh", "одна целая одна десятая киловатт-часа"),
            (2.21, "kWh", "две целых двадцать одна сотая киловатт-часа"),
            (1001.0, "kWh", "одна тысяча один киловатт-час"),
            (
                1e12,
                "W",
                "один умножить на десять в степени девять киловатт",
            ),
        ] {
            assert_eq!(amount(value, unit), expected, "{value} {unit}");
        }
    }

    #[test]
    fn translation_preserves_every_measurement_status_source_and_age() {
        let response = sample(true, 3);
        let mut before = serde_json::to_value(&response).unwrap();
        let russian = response.localized(EnergyLocale::Ru);
        assert_eq!(russian.locale, EnergyLocale::Ru);
        assert_eq!(
            russian.reports.battery.text,
            "Заряд аккумулятора: семьдесят семь целых пять десятых процента."
        );
        assert!(russian
            .reports
            .solar_today
            .text
            .starts_with("Выработка солнечных контроллеров сегодня:"));
        assert!(russian
            .reports
            .flow
            .as_ref()
            .unwrap()
            .text
            .contains("Отдача энергии в сеть: двести пятьдесят ватт."));
        assert!(russian
            .reports
            .flow
            .as_ref()
            .unwrap()
            .text
            .contains("Зарядка системного аккумулятора: четыреста ватт."));
        assert!(russian
            .reports
            .alarms
            .text
            .contains("Авария: Перегрузка инвертора, фаза один."));
        assert!(russian
            .reports
            .status
            .brief_text
            .as_ref()
            .unwrap()
            .starts_with(&russian.reports.alarms.text));
        let mut after = serde_json::to_value(russian).unwrap();
        for value in [&mut before, &mut after] {
            value.as_object_mut().unwrap().remove("locale");
            for report in value["reports"].as_object_mut().unwrap().values_mut() {
                report.as_object_mut().unwrap().remove("text");
                report.as_object_mut().unwrap().remove("brief_text");
            }
            for alarm in value["alarms"]["active"].as_array_mut().unwrap() {
                alarm.as_object_mut().unwrap().remove("label");
            }
        }
        assert_eq!(before, after);
    }

    #[test]
    fn english_localization_is_an_exact_noop() {
        let response = sample(true, 0);
        let expected = serde_json::to_value(&response).unwrap();
        assert_eq!(
            expected,
            serde_json::to_value(response.localized(EnergyLocale::En)).unwrap()
        );
    }

    #[test]
    fn all_failure_states_preserve_warnings_and_hide_invalid_values() {
        let stale = sample(true, 121).localized(EnergyLocale::Ru);
        assert_eq!(stale.reports.status.status, Status::Stale);
        assert!(stale.reports.status.text.contains("данные устарели"));
        assert!(stale
            .reports
            .status
            .text
            .contains("Контроль аварий неполный"));
        assert!(!stale.reports.status.text.contains("семьдесят семь"));
        let offline = sample(false, 0).localized(EnergyLocale::Ru);
        assert_eq!(offline.reports.status.text, DISCONNECTED);
        assert_eq!(
            offline.reports.status.brief_text.as_deref(),
            Some(DISCONNECTED)
        );
        assert!(offline.reports.alarms.text.contains("нет связи"));
        assert!(offline.reports.flow.unwrap().text.contains("нет связи"));
        let missing = EnergyResponse::build(
            &EnergyConfig::default(),
            true,
            &HashMap::new(),
            Instant::now(),
        )
        .localized(EnergyLocale::Ru);
        assert!(missing
            .reports
            .battery
            .text
            .contains("отсутствуют или некорректны"));
        assert!(missing.reports.solar.text.contains("источник не настроен"));
        assert_eq!(missing.reports.alarms.text, "Контроль аварий не настроен.");
        assert!(!missing.reports.status.text.contains("ноль"));
    }

    #[test]
    fn directional_power_distinguishes_zero_import_export_and_tiny_values() {
        for (value, grid, text) in [
            (0.0, true, "Суммарная мощность сети: ноль ватт."),
            (-0.0, false, "Мощность системного аккумулятора: ноль ватт."),
            (2.0, true, "Получение энергии из сети: два ватта."),
            (-2.0, false, "Разрядка системного аккумулятора: два ватта."),
            (-0.1, true, "Отдача энергии в сеть: меньше одного ватта."),
            (
                0.1,
                false,
                "Зарядка системного аккумулятора: меньше одного ватта.",
            ),
        ] {
            let metric = Metric {
                value: Some(value),
                unit: "W",
                status: Status::Fresh,
                sources: vec![],
                age_seconds: Some(0),
            };
            assert_eq!(directional_report(&metric, grid), text);
        }
    }

    #[test]
    fn each_supported_alarm_has_a_fixed_russian_label_and_no_source_interpolation() {
        for service in ["vebus", "battery"] {
            for leaf in [
                "HighDcCurrent",
                "HighDcVoltage",
                "LowBattery",
                "PhaseRotation",
                "Ripple",
                "TemperatureSensor",
                "VoltageSensor",
                "GridLost",
                "BmsConnectionLost",
                "InverterImbalance",
                "MainsImbalance",
                "HighTemperature",
                "Overload",
                "Alarm",
                "CommunicationError",
                "LowVoltage",
                "HighVoltage",
                "HighCellVoltage",
                "LowCellVoltage",
                "LowStarterVoltage",
                "HighStarterVoltage",
                "LowSoc",
                "HighChargeCurrent",
                "HighDischargeCurrent",
                "HighCurrent",
                "CellImbalance",
                "InternalFailure",
                "HighChargeTemperature",
                "LowChargeTemperature",
                "LowTemperature",
                "MidVoltage",
                "Contactor",
                "BmsCable",
                "HighInternalTemperature",
                "FuseBlown",
                "CircuitBreakerTripped",
            ] {
                let path = format!("Alarms/{leaf}");
                if crate::alarms::alarm_label(service, &path).is_some() {
                    let translated = alarm_label(&format!("{service}/0/{path}"));
                    assert_ne!(
                        translated, "Сигнал контролируемого источника",
                        "{service}/{leaf}"
                    );
                    assert!(!translated.is_ascii());
                }
            }
        }
        assert_eq!(
            alarm_label("<script>alert(1)</script>"),
            "Сигнал контролируемого источника"
        );
    }

    #[test]
    fn bounded_alarm_summary_retains_counts_priority_and_incomplete_coverage() {
        let cfg = EnergyConfig::default()
            .with_alarm_sources(
                &(0..20)
                    .map(|n| format!("battery/{n}/Alarms/BmsCable"))
                    .collect::<Vec<_>>()
                    .join(","),
            )
            .unwrap();
        let now = Instant::now();
        let readings = cfg
            .alarm_sources
            .iter()
            .enumerate()
            .filter(|(n, _)| *n != 19)
            .map(|(n, source)| {
                (
                    source.clone(),
                    Reading {
                        value: if n == 0 { json!(1) } else { json!(2) },
                        received_at: now,
                    },
                )
            })
            .collect();
        let response =
            EnergyResponse::build(&cfg, true, &readings, now).localized(EnergyLocale::Ru);
        assert!(response
            .reports
            .alarms
            .text
            .contains("восемнадцать аварий и одно предупреждение"));
        assert_eq!(response.reports.alarms.text.matches("Авария:").count(), 5);
        assert!(response
            .reports
            .alarms
            .text
            .contains("Ещё четырнадцать активных сигналов."));
        assert!(response
            .reports
            .alarms
            .text
            .contains("Контроль аварий неполный."));
        let json = serde_json::to_value(response).unwrap();
        for report in json["reports"].as_object().unwrap().values() {
            for key in ["text", "brief_text"] {
                if let Some(text) = report[key].as_str() {
                    assert!(text.chars().count() <= 1200, "{text}");
                }
            }
        }
    }

    #[test]
    fn extreme_finite_values_have_bounded_words_without_panicking() {
        let mut response = sample(true, 0);
        response.metrics.solar_power.value = Some(f64::MAX);
        response.metrics.solar_today.value = Some(f64::MAX);
        let response = response.localized(EnergyLocale::Ru);
        for text in [
            &response.reports.solar.text,
            &response.reports.solar_today.text,
            &response.reports.status.text,
        ] {
            assert!(text.chars().count() <= 1200);
            assert!(!text.contains("inf"));
        }
        for value in [0.0001, 0.004, 0.005, 0.999, 999.999, 999_999_999.999] {
            assert!(!amount(value, "kWh").is_empty());
        }
    }

    #[test]
    fn fresh_all_clear_and_mixed_solar_scope_are_explicit() {
        let cfg = EnergyConfig::parse(
            "system/0/Dc/Battery/Soc",
            "",
            "pvinverter/0/Ac/Energy/Daily",
            "120",
        )
        .unwrap()
        .with_alarm_sources("battery/0/Alarms/LowVoltage")
        .unwrap();
        let now = Instant::now();
        let readings = [
            (
                "pvinverter/0/Ac/Energy/Daily".into(),
                Reading {
                    value: json!(1),
                    received_at: now,
                },
            ),
            (
                "battery/0/Alarms/LowVoltage".into(),
                Reading {
                    value: Value::from(0),
                    received_at: now,
                },
            ),
        ]
        .into_iter()
        .collect();
        let response =
            EnergyResponse::build(&cfg, true, &readings, now).localized(EnergyLocale::Ru);
        assert_eq!(
            response.reports.solar_today.text,
            "Солнечная выработка сегодня: один киловатт-час."
        );
        assert_eq!(
            response.reports.alarms.text,
            "В контролируемых источниках активных аварий нет."
        );
    }
}
