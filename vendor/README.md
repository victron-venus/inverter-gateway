# Temporary rumqttc TLS dependency repair

`rumqttc-0.25.1/` comes from the published crates.io rumqttc 0.25.1 archive.
The unused upstream development Cargo.lock is omitted so there is only one
resolved dependency graph. The unused certs/generate.sh development helper is
also omitted: the application generates its own test certificates and does not
need an upstream network/bootstrap script. The unused `examples/tls.rs` sample
and its Cargo example targets are omitted because they embed demonstration
credentials; the application supplies operator-configured credentials. The
Apache-2.0 LICENSE is added from its exact upstream source commit f1e9e8d558783f942993046679cdf3c8c3a3d36b.
Original archive SHA256:
`0feff8d882bff0b2fddaf99355a10336d43dd3ed44204f85ece28cf9626ab519`.

Besides the omitted development files and example targets, the Cargo manifests
only raise the `rustls-webpki` requirement from `0.102.8` to `0.103.13`. All retained
Rust source, including the complete runtime library, is unchanged. Cargo.lock
resolves the patched dependency to 0.103.15.
The 0.102.x dependency is vulnerable to RUSTSEC-2026-0049, RUSTSEC-2026-0098,
RUSTSEC-2026-0099 and RUSTSEC-2026-0104; the current upstream release still
requires it. See upstream issue https://github.com/bytebeamio/rumqtt/issues/1067
and proposed manifest fix https://github.com/bytebeamio/rumqtt/pull/1037.

The application keeps its existing MQTT protocol, Rustls verification and
transport behavior. An unrelated maintained MQTT fork would require a larger
protocol/API and supply-chain review. This local patch is temporary: remove
`[patch.crates-io]` and this directory when an upstream rumqttc release accepts
rustls-webpki >=0.103.13, update Cargo.lock, and rerun the locked TLS tests,
Clippy and cargo audit. Do not add advisory ignores or weaken TLS verification.
