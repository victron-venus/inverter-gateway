# Dashboard control and EV telemetry

IGW transports the retained `<INVERTER_TOPIC_PREFIX>/state` message alongside
native Cerbo leaf values. `INVERTER_TOPIC_PREFIX` defaults to `inverter` and is
independent of `VICTRON_PORTAL_ID`. It must be a literal MQTT topic root without
wildcards. No Home Assistant connection is needed for inverter-control flags.

`GET /v1/snapshot` and SSE snapshots include an `inverter` object with the
daemon's state, including `booleans`, `ess_mode`, `dry_run`, and
`ui_config.header_toggles`. Each publication replaces this object. Its value is
`null` before the first publication, after retained-message removal, or after
120 seconds without a controller publication. A broker disconnect invalidates
the entire snapshot; reads return 503 until telemetry is received again. Native
device updates do not refresh the controller's age.

The native `ev` and `evcharger` buckets retain `Soc`, `Connected`, names, VIN,
power, status and mode. Keys retain the actual instance number, for example
`ev["22/Soc"]` and `evcharger["40/Ac/Power"]`; consumers discover and select
instances rather than assuming those example numbers. Numeric zero remains a
valid reading. Null leaves and removed device roots retain the existing
invalidation semantics.

The gateway subscribes to only these ESS setting leaves:

- `N/<portal>/settings/+/Settings/CGwacs/Hub4Mode`
- `N/<portal>/settings/+/Settings/CGwacs/BatteryLife/State`

These populate the `settings` bucket for clients that calculate native ESS
status. `/Settings/InverterControl/*` mirrors are not used to infer daemon flags:
the daemon's `booleans` object is authoritative.

## Authenticated header commands

The existing write credential is required. Read-only credentials cannot invoke
commands. Controller commands also require a current controller snapshot and a
connected broker. Their MQTT publications are never retained.

- `POST /v1/commands/toggle` accepts exactly `entity` and explicit `state`.
  Entities are the seven documented inverter-control flags, optionally with the
  historical `input_boolean.` prefix. Values normalize to canonical `on`/`off`.
  Arbitrary HA entities, topics, missing states and extra fields are rejected.
- `POST /v1/commands/dry_run` accepts only `{"value":true}` or `{"value":false}`.
- `POST /v1/commands/ess_mode` accepts only `{}` and invokes the daemon's existing
  ESS toggle action. It uses MQTT QoS 0 to avoid requesting duplicate delivery of
  this non-idempotent legacy action. Clients must not automatically retry it.

The explicit flag and dry-run setters use QoS 1. A successful response means the
command was queued, not that physical equipment changed; clients display the
subsequent daemon state as confirmation. There is no raw MQTT passthrough, new
setpoint API or arbitrary Home Assistant command forwarding.

Tests cover native leaf retention, controller replacement/removal/expiry,
reconnect invalidation, subscription delivery with a one-item MQTT queue, and
authenticated command validation. These tests use a local broker fixture and
an in-memory command queue; they do not switch a physical inverter.

Controller commands waiting in IGW's command queue for more than five seconds are
discarded before handoff to rumqttc. Each request
is bound to the current MQTT connection generation and requires fresh controller
state when the bridge hands it to the broker client. A disconnect drops pending
controller publications before reconnecting; legacy native alarm commands retain
their existing behavior. HTTP acceptance acknowledges queueing, not execution by
the physical controller. The five-second limit ends at broker-client handoff;
MQTT and the daemon do not provide an execution deadline or an execution receipt.

## Native water telemetry and commands

Snapshots advertise `"capabilities":{"water_mode":true}`. Clients must keep
water controls unavailable against an older gateway that omits this capability.
The `pump` bucket retains `State`, `Mode`, `Connected`, `Status` and device names;
the `tank` bucket retains `Level`, `Connected`, `Status` and names. `Level` is
already a percentage: a value of `0.5` means 0.5%, not 50%. Unknown/null values
and disconnected devices must not be rendered as measured zero or stopped.

`POST /v1/commands/water_mode` accepts exactly
`{"instance":7,"mode":1}`. Instance is an integer in `0..=4294967295`, resolved
by the client from its selected pump or valve; mode is the integer `0` (auto),
`1` (always on), or `2` (always off). Strings, booleans, floats, extra fields and
arbitrary topics are rejected. This command requires the write credential and
does not depend on inverter-control or a dashboard Home Assistant connection.

IGW requires a currently connected broker and a valid observed `Mode` for that
exact pump instance; an explicitly disconnected or unknown `Connected` value
rejects the command. Devices that do not publish `Connected` remain supported.
The queued request is bound to the connection generation, expires after five
seconds, and rechecks the target before nonblocking broker-client handoff.
Disconnects discard pending water publications, preventing reconnect replay.

The only publication is `W/<configured portal>/pump/<instance>/Mode` with
`{"value":<mode>}`, QoS 0 and no retain flag. Clients must not automatically
retry. Acceptance confirms queueing only; subsequent native `Mode` and `State`
readback is authoritative. `dbus-pump` owns automation and physical actions.
The queue deadline ends at broker-client handoff, not physical execution.
