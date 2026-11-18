# Documentation

IGW, the gateway that serves an energy API and relays dashboard telemetry for a
Victron installation, with its transport policy and validation record.

## Interfaces

- [Energy API and voice reports](energy-api.md) — the versioned read-only `GET /v1/energy` contract.
- [Dashboard control and EV telemetry](dashboard-control-telemetry.md) — the retained `state` message IGW transports.
- [Transport security and migration](transport-security.md) — what IGW connects to and what it never dials out to.
- [Energy API validation](validation.md) — the 2026-09-12 record for the 0.3.0 implementation.

## Assurance

- [Architecture, security evidence and roadmap](project-assurance.md) — how the pieces fit and what the contribution checks cover.
- [OpenSSF Best Practices evidence: inverter-gateway](openssf-evidence.md) — the index behind the assessment.

## Releases

- [Release version inputs](release-versioning.md) — the `versioning` fields that identify a release.
- [CI and release operations](release-workflow.md) — `.release-policy.json` and the required status.
- [CI orchestration](CI-ORCHESTRATION.md) — how the quality gate and release pipeline divide the events.
- [Version plans and manifest synchronization](VERSIONING.md) — the versioning opt-in.
- [Fuzzing release candidates](rc-fuzzing.md) — the reusable bounded fuzz workflow.
- [Release read recovery and rerun validation](release-read-safety.md) — the two fixes carried from the shared toolkit.
- [Early final-build runner check](runner-preflight.md) — why the check runs before the final build.
