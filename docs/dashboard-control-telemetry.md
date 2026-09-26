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
device updates do not refresh the controller's age. The `setpoint_override`
field is the exception to envelope replacement: its authoritative value comes
from the daemon's dedicated acknowledgement topic, as described below.

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
subsequent daemon state as confirmation. There is no raw MQTT passthrough or
arbitrary Home Assistant command forwarding.

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
MQTT does not provide an execution deadline. Tariff edits and the override command
below have correlated daemon acknowledgements; other command success remains
queue acceptance followed by state readback.

## Controller-owned electricity tariff

`inverter-control` owns the tariff plan, validation and durable configuration.
IGW transports `inverter.ui_config.electricity_tariff` and
`inverter.ui_config.electricity_tariff_status` unchanged in snapshots and SSE;
it does not maintain a separate tariff or calculate prices.

`capabilities.electricity_tariff: true` advertises this gateway's command route.
Clients must also require a fresh controller snapshot with
`ui_config.electricity_tariff_status.writable: true` before enabling edits.
Older controllers remain readable but cannot be edited through this route.

`POST /v1/commands/electricity_tariff` requires the write credential and exactly
three fields:

```json
{"request_id":"tariff-1","revision":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","plan":null}
```

The revision must be the current controller-provided SHA-256, encoded as 64
lowercase hexadecimal characters; the example is a placeholder.
`request_id` uses the same nonempty 128-character ASCII identifier format as
setpoint overrides. `plan` is an object, or `null` to clear the configured plan.
The compact UTF-8 JSON envelope must be at most 100,000 bytes. Extra fields,
arbitrary topics, invalid identifiers and other plan types are rejected by IGW.
The controller validates the complete plan schema and checks the revision before
persisting; the gateway deliberately does not duplicate those rules.

The fixed publication is `<INVERTER_TOPIC_PREFIX>/cmd/electricity_tariff`, QoS 0
with no retain flag. It uses the five-second queue deadline and connection
generation guard, and rechecks controller freshness and writability at handoff.
Pending tariff publications are dropped on disconnect rather than replayed on a
new connection. MQTT packet limits allow the bounded command and larger controller
state envelopes (128 KiB outgoing and 1 MiB incoming).

HTTP 200 means queued only. The gateway never fabricates a save acknowledgement.
Clients send once and wait for the controller's subsequent
`ui_config.electricity_tariff_status` with the matching `request_id`:

```json
{"writable":true,"revision":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb","request_id":"tariff-1","error":null}
```

`error: null` confirms the requested plan is durably stored (or already identical).
A non-null error reports rejection and leaves the prior plan authoritative.
After a timeout or interrupted response, refresh status before another user
decision; never automatically retry or switch transports. Controller expiry or
MQTT disconnect invalidates this status with the rest of the controller envelope.

## Correlated setpoint override

`capabilities.setpoint_override: true` advertises the gateway route. Clients must
also require a non-null `inverter.setpoint_override` status before enabling the
editor; a gateway capability alone does not prove daemon support or availability.

`POST /v1/commands/setpoint_override` accepts exactly two fields:

```json
{"value":-250,"request_id":"e29f9247-af9b-4994-b30f-04e727d9c484"}
```

`value` must be a JSON integer in `-2147483648..=2147483647` or `null`.
Booleans, floating-point values and strings are rejected. `request_id` is a
nonempty string of at most 128 ASCII letters, digits, `.`, `_`, `:`, or `-`;
UUIDs are supported. Extra fields and arbitrary topics are rejected. The write
credential is required; read credentials can only inspect the status.

The command is sent once, with QoS 0 and no retain flag, to
`<INVERTER_TOPIC_PREFIX>/cmd/setpoint_override`. It uses the existing five-second
queue deadline and connection-generation guard, and additionally requires a
valid dedicated override status at enqueue and broker-client handoff. The daemon
applies an explicit watt value and maintains it every two seconds until stopped.
`null` stops the override without writing zero. It is independent of DRY mode;
the daemon owns physical writes and their validation.

HTTP 200 acknowledges queueing only. The gateway subscribes to the retained
`<INVERTER_TOPIC_PREFIX>/setpoint_override` topic and places its object in
`snapshot.inverter.setpoint_override` (also in SSE snapshots):

```json
{"value":-250,"last_error":null,"request_id":"e29f9247-af9b-4994-b30f-04e727d9c484"}
```

Clients generate a fresh identifier for each explicit user request, send only
one POST, then wait for a status with that identifier. A non-null `last_error`
means rejection/failure; `value` reports the actual current override intent and
can retain the previous value after a failed edit. Timeout or interrupted HTTP
is not proof of rejection: clients must not automatically retry or switch
transports, and should refresh status before another user decision.

Dedicated acknowledgements override older embedded `inverter/state` status.
Before the first valid acknowledgement, after a broker disconnect, and after
null/empty or malformed acknowledgement payloads, the status is `null` (unknown,
not inactive). A valid status with `value:null` explicitly means no active
override. Status may arrive before its controller envelope and is buffered, but
does not refresh the controller's 120-second liveness deadline. If the controller
expires, its entire `inverter` object becomes null and override controls are
unavailable. An older daemon without the dedicated topic remains unsupported;
the gateway does not substitute potentially stale embedded state.

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
