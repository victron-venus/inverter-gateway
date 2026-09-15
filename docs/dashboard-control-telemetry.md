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
