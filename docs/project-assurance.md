# Architecture, security evidence and roadmap

## Getting started

Use [installation and configuration](../README.md) and [transport deployment](transport-security.md). The documented [contribution checks](../CONTRIBUTING.md) are the local validation entry point. [Release ownership](../RELEASING.md) separates automated qualification from acceptance on a target deployment.

## Architecture and boundaries

`config.rs` validates startup settings. `mqtt_bridge.rs` owns the broker connection and feeds parsed updates into `state.rs`; snapshots and event streams expose that state through `http.rs`. `whitelist.rs` admits only supported commands, and freshness/availability checks run before guarded publication. `transport.rs` loads TLS material and binds listeners before background MQTT tasks start. The container and service deployment files are part of the supported delivery paths.

## Security argument and operating assumptions

HTTP bearer credentials have separate read-only and control scopes; comparison uses the `subtle` constant-time primitive. Missing control credentials fail startup unless the explicit insecure local-testing switch is set. TLS uses rustls 0.23 with its ring provider, TLS 1.2/1.3 and standard authenticated cipher suites; no custom cipher or signature implementation exists. MQTT TLS validates the broker chain and DNS name using configured or native roots before MQTT credentials are exchanged. Operator-provided server certificates/keys are loaded by rustls/ring. Existing plaintext HTTP/MQTT compatibility paths remain supported and must be isolated or protected by the documented tunnel; they prevent a claim that every network path is secure by default. The project does not generate operator tokens or maintain external-user password hashes.

`src/http.rs` tests read/control authorization; `src/transport.rs` tests real trusted/untrusted TLS listeners and startup failures; `mqtt_bridge.rs` exercises transport selection and malformed telemetry. `scripts/ci.sh` runs formatting, strict Clippy and all Cargo targets. Locked dependency auditing and immutable-container smoke checks run in CI. RC-only libFuzzer targets exercise actual energy configuration and command-payload validation, 60 seconds each. [FUZZING.md](../FUZZING.md) distinguishes mutation campaigns from ordinary fixed-corpus replay.

These controls limit identified risks; they do not constitute an exhaustive independent security assessment. A passing test or a dependency scan does not establish absence of defects. Keep operator credentials outside source archives and public issue reports. Follow [SECURITY.md](../SECURITY.md) for confidential reports.

## Quality and maintenance policy

Major functionality requires automated tests, bug fixes require focused regressions, and changed interfaces require documentation and release-note updates under [CONTRIBUTING.md](../CONTRIBUTING.md). Review must keep validation enabled and explain narrow exceptions. Use the repository lockfiles and the documented common build tools. Dependabot/dependency review and required source-analysis jobs provide recurring feedback; the maintainer investigates actual findings rather than equating a green job with an empty alert list.

## Roadmap and acknowledged gaps

Measure full maintained Rust/Python tooling coverage and review network backpressure, reconnect and lifecycle fault injection. Document an independent access recovery arrangement. Prove source-plus-container release authentication before claiming Silver signing. Audit remaining topic/configuration/input boundaries; the existing whitelist and parser tests are not proof that all inputs are validated.

The [changelog](../CHANGELOG.md) records delivered behavior and upgrade impact; issues and pull requests record decisions. This roadmap is a prioritized maintenance direction, not a release-date promise. Current automated test counts and scans belong to their exact source revision and CI run. No whole-project 80% coverage, complete security assurance case, bit-for-bit reproducible build or comprehensive accessibility certification is claimed by this document.

## OpenSSF assessment

[The evidence index](openssf-evidence.md) connects the public practices to the official assessment. The live assessment is authoritative for the awarded level. Unverified criteria remain unknown or unmet, and Silver continuity remains unmet as documented in [governance](../GOVERNANCE.md).

## Exact TLS key-strength boundary

`src/tls_policy.rs` checks X.509 public-key metadata using the maintained x509-parser library, after rustls's ordinary chain/name verification for MQTT peers. RSA moduli must contain at least 2048 actual bits (a provider's byte-rounded 2047-bit acceptance is insufficient); permitted curves are P-256/P-384/P-521 or Ed25519. Explicit MQTT roots and native HTTPS chains are checked before starting; weak operating-system roots are omitted. HTTPS checks and loads the same certificate bytes, without a file reread race. No cryptographic primitive or trust assertion is implemented by the metadata check.

Tests prove that the ordinary verifier accepts a trusted 2047-bit test leaf and the wrapper rejects it, check 1024/2047/2048-bit boundaries, root rejection and native HTTPS failure before binding. Existing live MQTT tests retain successful trusted connections and rejection of untrusted/wrong-name peers before CONNECT credentials. These synthetic certificates contain no deployment private keys. TLS 1.2/1.3 suites and key exchange are supplied by rustls/ring.
