# Energy API and voice reports

`GET /v1/energy` provides a versioned, read-only energy contract. Cerbo GX remains
the telemetry source. The gateway runs on a separate server, subscribes to MQTT,
and computes this response from its existing memory cache. A voice request does
not poll Cerbo, run a model on Cerbo, or require Home Assistant.

Alexa and Google adapters should forward the selected `reports.<name>.text`.
They must not compute their own totals or replace unavailable readings with zero.
Home Assistant can consume this endpoint as an optional Google Home adapter.

## Authentication and transport

Send `Authorization: Bearer <GATEWAY_READ_TOKEN>`. This optional token grants access
to `/v1/energy`, `/v1/snapshot`, and `/v1/events` only. It cannot inspect or execute
`/v1/commands/*`. Keep it different from `GATEWAY_API_TOKEN`, which retains full
access. The gateway refuses identical read and full tokens at startup.

When the gateway is behind Cloudflare Access, the adapter's server also sends
`CF-Access-Client-Id` and `CF-Access-Client-Secret`. These credentials remain on the
adapter server; they do not belong in a skill interaction model or browser client.
The existing tunnel can forward this endpoint without Home Assistant in the path.

The endpoint returns `Cache-Control: no-store`. Configure proxies to respect it.
Do not cache successful voice responses between questions.

- Valid authentication returns HTTP 200 even when MQTT is disconnected or the
  configured metric has not arrived. Read the metric/report status and text.
- Invalid or missing authentication returns HTTP 401.
- Transport errors, invalid JSON, unsupported `schema_version`, and timeouts must
  produce the adapter's explicit service-unavailable answer.
- `/health`, `/v1/snapshot`, `/v1/events`, and commands retain their existing
  behavior; snapshot/SSE still return HTTP 503 without current-session telemetry.

## Configuration

All source identifiers are topic suffixes under `N/<portal_id>/`, in the form
`service/instance/path`. Instances must be canonical nonnegative integers. Do not
include the portal prefix, leading slash, wildcard, or duplicate source.

- `GATEWAY_ENERGY_BATTERY_SOURCE` defaults to `system/0/Dc/Battery/Soc`, the battery
  selected by Venus OS system calculation. An explicit `battery/<instance>/Soc`
  or `vebus/<instance>/Soc` is also accepted. Select one source; an empty value
  disables the metric. There is no fallback to the first available battery.
- `GATEWAY_ENERGY_SOLAR_POWER_SOURCES` is an explicit comma-separated list. Its
  default is empty (`unconfigured`). Supported paths are `system/<instance>/Dc/Pv/Power`,
  `system/<instance>/Ac/PvOnGrid/<phase>/Power`,
  `system/<instance>/Ac/PvOnOutput/<phase>/Power`,
  `system/<instance>/Ac/PvOnGenset/<phase>/Power`,
  `solarcharger/<instance>/Yield/Power`, `pvinverter/<instance>/Ac/Power`, and
  `pvinverter/<instance>/Ac/<phase>/Power`. AC phases are `L1`, `L2`, or `L3`;
  system AC paths also permit `Total` when present on that Venus OS version.
- `GATEWAY_ENERGY_SOLAR_TODAY_SOURCES` is an explicit comma-separated list of
  `solarcharger/<instance>/History/Daily/0/Yield` counters and/or the custom
  `pvinverter/<instance>/Ac/Energy/Daily` counters, all in kWh. The latter is an
  extension of [dbus-tasmota-pv](https://github.com/victron-venus/dbus-tasmota-pv),
  sourced from Tasmota `ENERGY.Today`; it is not a standard path on every PV
  inverter. The default list is empty (`unconfigured`). `Yield/User` and lifetime
  `Ac/Energy/Forward` counters are rejected: they are cumulative and cannot
  represent today's generation directly.
- `GATEWAY_ENERGY_ALARM_SOURCES` is an explicit comma-separated list of supported
  `vebus/<instance>/Alarms/<name>` and `battery/<instance>/Alarms/<name>` leaves.
  The default is empty (`unconfigured`). See alarm monitoring below.
- `GATEWAY_ENERGY_MAX_AGE_SECS` is a positive integer, default `120`. Each selected
  topic must have arrived within this age at the gateway.

Select the paths actually published by the installation. Configure only physical
components that belong in the advertised total, and include every required
component. The gateway does not discover missing devices or infer the site's
solar topology. An incomplete configured list can still produce an incomplete
site total. Daily yield covers exactly the selected charger and PV inverter
counters; it is not derived from lifetime counters or power integration. A
charger-only list produces the deliberately narrower report label "Solar charger
generation today". Including a PV inverter daily counter uses "Solar generation
today"; select the full physical topology before calling that a site total.

Configuration rejects duplicate sources, system DC aggregate plus solar charger
components, system AC aggregate plus PV inverter components, multiple system
instances, and total power plus phases from the same component. It cannot detect
two different devices measuring the same physical array. Source order is sorted
for deterministic summation and response serialization.

The solar power value sums the configured Victron readings at their published
measurement points. For example, system DC solar power describes charger output,
while PV inverter power describes AC output. It is not a measurement of battery
charging power or net grid export.

Daily yield uses the device's current-day history counter and its day/reset
semantics. The gateway does not synthesize a midnight reset, choose a timezone, or
maintain an independent energy ledger. Align the selected chargers' day behavior
before describing their sum as a site-wide daily total. When adding Tasmota daily
counters, verify their configured timezone/reset boundaries agree with the
charger counters. Do not silently combine incompatible reporting days.

Example for one DC charger and one AC phase, with charger-only daily energy:

```dotenv
GATEWAY_ENERGY_BATTERY_SOURCE=system/0/Dc/Battery/Soc
GATEWAY_ENERGY_SOLAR_POWER_SOURCES=system/0/Dc/Pv/Power,system/0/Ac/PvOnOutput/L1/Power
GATEWAY_ENERGY_SOLAR_TODAY_SOURCES=solarcharger/290/History/Daily/0/Yield
GATEWAY_ENERGY_MAX_AGE_SECS=120
```

This example is not a topology discovery result. Replace the instance and omit
paths for equipment that does not exist; an absent path is not treated as zero.

## Response contract: schema version 1

```json
{
  "schema_version": 1,
  "generated_at": 1789257600,
  "mqtt_connected": true,
  "metrics": {
    "battery_soc": {
      "value": 77.5,
      "unit": "%",
      "status": "fresh",
      "sources": ["system/0/Dc/Battery/Soc"],
      "age_seconds": 2
    },
    "solar_power": {
      "value": 1550.0,
      "unit": "W",
      "status": "fresh",
      "sources": ["system/0/Ac/PvOnOutput/L1/Power", "system/0/Dc/Pv/Power"],
      "age_seconds": 9
    },
    "solar_today": {
      "value": null,
      "unit": "kWh",
      "status": "unconfigured",
      "sources": [],
      "age_seconds": null
    }
  },
  "alarms": {
    "status": "unconfigured",
    "active": [],
    "sources": [],
    "age_seconds": null
  },
  "reports": {
    "battery": {"text": "Battery charge is 77.5 percent.", "status": "fresh"},
    "solar": {"text": "Solar power is 1.55 kilowatts.", "status": "fresh"},
    "solar_today": {"text": "Solar generation today is not configured.", "status": "unconfigured"},
    "alarms": {"text": "Alarm monitoring is not configured.", "status": "unconfigured"},
    "status": {
      "text": "Battery charge is 77.5 percent. Solar power is 1.55 kilowatts. Solar generation today is not configured. Alarm monitoring is not configured.",
      "status": "unconfigured"
    }
  }
}
```

`generated_at` is the response generation time in Unix seconds, not the source
measurement time. `mqtt_connected` represents the gateway's MQTT connection.
Values retain their numeric precision; reports use one decimal for percent,
whole watts below 1,000 W, two decimals for kilowatts and kWh, with trailing zeros
removed. Extremely large values (at least one billion in the spoken unit) use
spoken scientific notation to keep reports bounded. Reports are plain English
text, not SSML. Exact wording may evolve within
schema version 1; consumers should use the report keys and statuses.

Every metric has all five fields, including explicit JSON `null`:

- `fresh`: every configured source is numeric, valid, and at most the age limit.
  `value` contains the full sum (or single battery value). A real zero is valid.
- `stale`: all configured sources are numeric and valid, but at least one exceeds
  the age limit. `value` is null, so clients cannot accidentally speak an old
  number as current.
- `unavailable`: MQTT is disconnected, a required source is absent/removed/null,
  a value is nonnumeric/negative, battery SoC is outside 0–100, or aggregation
  overflows. `value` is null. Missing/invalid data takes precedence over stale.
- `unconfigured`: no sources were selected; `sources` is empty and `value` is null.

`age_seconds` is the oldest gateway receipt age among the selected sources,
rounded down to whole seconds, and null when any required receipt is absent or
MQTT is disconnected. Expiration compares full monotonic durations, not the
rounded number. Invalid but received numeric/text payloads may have an age;
null/removal deletes the receipt. Changing the host wall clock does not refresh
readings. Unrelated topics cannot refresh a selected source.

A broker disconnect clears readings and receipt times atomically with the
snapshot. A reconnect starts empty and waits for each configured source. Empty
MQTT retained-message removals and JSON null invalidate a leaf; removal at a
device root invalidates all its leaves. The endpoint never returns partial sums.

Freshness is **gateway receipt freshness**, not a guarantee that a physical sensor
sample was taken at that instant. The existing Victron keepalive requests maintain
notifications; there is no additional voice-triggered polling. A broker/device
that republishes an internally frozen number can still produce a fresh receipt.

Each individual report uses its metric's or alarm-monitoring status.
`reports.status` concatenates battery, solar power, daily yield, and alarm reports
and uses this precedence: unavailable, stale, unconfigured, fresh. During an MQTT outage it instead returns one explicit
connection-unavailable report. The status report is an energy summary, not a
claim that the installation is free of alarms or electrically safe.

## Alarm monitoring

The additive `alarms` object contains `status`, `active`, `sources`, and
`age_seconds`. Its status describes telemetry completeness/freshness, not alarm
severity. Each active item contains a configured `source`, a `severity` of
`warning` or `alarm`, and a fixed English `label`. Raw device messages and custom
names never become spoken alarm labels.

The selected leaves must publish numeric `0` (inactive), `1` (warning), or `2`
(alarm). Booleans, numeric strings, fractions, unknown codes, and null are
unavailable. Supported names are:

- VE.Bus: `HighDcCurrent`, `HighDcVoltage`, `LowBattery`, `PhaseRotation`, `Ripple`,
  `TemperatureSensor`, `VoltageSensor`, `GridLost`, `BmsConnectionLost`,
  `HighTemperature`, `Overload`;
  also `L1/HighTemperature`, `L1/LowBattery`, `L1/Overload`, `L1/Ripple`,
  `L1/InverterImbalance`, `L1/MainsImbalance` and their `L2`/`L3` counterparts.
- Battery: `Alarm`, `CommunicationError`, `LowVoltage`, `HighVoltage`, `HighCellVoltage`, `LowCellVoltage`,
  `LowStarterVoltage`, `HighStarterVoltage`, `LowSoc`, `HighChargeCurrent`,
  `HighDischargeCurrent`, `HighCurrent`, `CellImbalance`, `InternalFailure`,
  `HighChargeTemperature`, `LowChargeTemperature`, `LowTemperature`,
  `HighTemperature`, `MidVoltage`, `Contactor`, `BmsCable`,
  `HighInternalTemperature`, `FuseBlown`, `CircuitBreakerTripped`.

Select the actual non-null alarm leaves published by each monitored device, and
include every alarm needed for the intended coverage. Unsupported alarms and
platform notification banners are not silently included. The API is not a
replacement for native protection systems or full installation monitoring.

`active` includes every **confirmed fresh** warning/alarm from the selected
sources, in sorted source order. Stale active values are excluded. If any selected
source is missing, removed, invalid, or stale, the overall status is
`unavailable`/`stale`; any fresh active alarms are still listed, and
`reports.alarms.text` explicitly says alarm coverage is incomplete.

Only a complete, fresh, all-zero selected set produces "No active alarms in the
monitored sources." Empty/unconfigured sources never produce that statement.
Warnings are introduced with "Warning:" and alarm severity with "Alarm:".
Spoken reports list at most five active labels, prioritizing alarm severity in
larger sets, with total warning/alarm counts and an explicit remaining count.
The structured `active` array remains complete. Individual and combined spoken
reports stay below the adapters' 1,200-character limit. An
empty `active` list alone is **not** an all-clear signal; always inspect `status`
or forward the provided report. MQTT disconnect and device removal clear this
list with the rest of the energy receipt cache. `age_seconds` uses the same
oldest-receipt and missing-source rules as metrics.

Example active item:

```json
{"source":"vebus/0/Alarms/L1/Overload","severity":"alarm","label":"Inverter overload on phase L1"}
```

## Verification

1. Start with empty solar lists; confirm those metrics say `unconfigured`.
2. Configure the actual sources and compare fresh values against Venus OS.
3. Check individual battery, solar, daily, alarms, and combined report text.
4. Pause the MQTT publisher beyond the configured age limit while keeping the
   broker connected; verify `stale` with null values.
5. Remove a selected leaf or send `{"value":null}` on a test broker; verify no
   partial sum or zero is substituted. For alarms, test both warning/alarm codes
   and partial coverage; neither stale nor missing alarm data may report all clear.
6. Disconnect the gateway from the test broker; `/v1/energy` must stay HTTP 200
   with unavailable text while `/v1/snapshot` returns HTTP 503.
7. Confirm the read token can GET telemetry and receives HTTP 401 on both GET
   and POST command routes. Never test a production command with the full token
   unless that action is intended.

Unit and router tests exercise these semantics without writing to Cerbo.
Run `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, and
`cargo test` before release.

## Upstream data definitions

The [Venus OS D-Bus documentation](https://github.com/victronenergy/venus/wiki/dbus)
describes the selected system battery, system PV paths, measurement units, and
user-resettable cumulative yield. The
[system calculation source](https://github.com/victronenergy/dbus-systemcalc-py)
defines system AC PV aggregation. Check the actual firmware's published topics
when choosing source identifiers; a listed path is not present on every device.
