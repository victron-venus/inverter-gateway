# Energy API validation

Validation recorded on 2026-09-12 for the 0.3.0 energy API implementation.

## Automated checks

All 46 Rust tests passed, together with formatting, clippy with warnings denied,
GitHub CI, and CodeQL. Tests cover the read-token boundary, offline responses,
independent source freshness, removal and reconnect invalidation, invalid and
incomplete totals, real zero values, alarm severity, and voice response limits.

## Installation and live checks

The existing external-host Kubernetes gateway was upgraded with a pinned image
after its Deployment and Secret were backed up privately. Its existing MQTT
credentials, full API token, host port, service, and tunnel route were preserved.
No additional process was installed on Cerbo GX.

The production checks passed:

- Authenticated public HTTPS health reported an active MQTT connection.
- `/v1/energy` returned schema version 1, `Cache-Control: no-store`, and fresh
  battery, solar power, daily generation, and monitored-alarm reports.
- One battery source, two non-overlapping power sources, and five daily counters
  were cross-checked against the MQTT snapshot. Sequential observations allow
  normal changes between requests.
- All 72 configured, available alarm signals were fresh. The spoken result was
  explicitly limited to the monitored sources.
- Both the original full token and the new read token could read snapshots.
- GET and POST command requests with the read token returned HTTP 401. An invalid
  token was also rejected on the energy endpoint. No device commands were issued.
- The installed Alexa backend fetched all five reports through the existing
  protected HTTPS route using its dedicated read token.

## Limits

Freshness measures receipt of each MQTT source, not the internal sampling health
of every upstream device or third-party driver. A faulty driver that publishes
an invented numeric zero cannot be distinguished from a real zero by this API.
The daily total describes only the explicitly configured counters; it must not
be represented as a meter-verified whole-site total without validating coverage.

Outage and active-alarm behavior were verified with controlled tests. Production
MQTT was not deliberately disconnected and physical alarms were not induced.
Voice account registration, Google linking, and audible speaker tests have
separate requirements documented by the consumer repositories.
