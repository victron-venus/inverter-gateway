# Release candidate fuzzing

See the [illustrated RC fuzzing process and verified rollout results](https://github.com/victron-venus/venus-os-ci-toolkit/blob/main/docs/rc-fuzzing-results.md)
for the shared release flow, evidence requirements and a dated cross-project
snapshot. The project-specific scope and reproduction guidance below still apply.

inverter-gateway runs bounded `cargo-fuzz`/libFuzzer campaigns through the reusable
`victron-venus/venus-os-ci-toolkit` release pipeline. This is not enrollment in
Google OSS-Fuzz and is not continuous fuzzing.

## Trigger, release gate, and badge

`.github/fuzz.json` lists the exact preparation commands, targets, seed paths,
and budgets. The release policy opts into the shared workflow. It checks that
the repository is **public** and the resolved channel is **release candidate**
before allocating a fuzzing runner. Campaigns do not run for PRs, ordinary
pushes, nightlies, betas, schedules, or stable promotion. Ordinary CI replays
the small checked-in corpus as deterministic unit tests without generating new
inputs or executing the libFuzzer engine.

The README's **Fuzzing: RC only** badge describes this policy and links here. It
is not a result badge. The RC workflow's fuzzing job and retained source-bound
report give the actual result for a particular candidate. Every configured
target must succeed before that RC is published. A panic, oracle failure,
crash, timeout, or resource-limit failure blocks it. Fix and test the cause,
then issue a new candidate. Stable promotion reuses evidence only for the
matching accepted RC source and artifacts; it does not repeat the campaign.

## Production code and oracles

- `energy-sources` calls the actual `EnergyConfig` parser with mutated source
  lists, instance numbers, and freshness limits. Accepted settings retain
  canonical numeric instances, unique sources, a positive age limit, and one
  system instance for power-flow inputs. Aggregate/phase overlap cannot bypass
  the production double-count safeguards. Re-parsing normalized lists preserves
  the source selection.
- `command-payload` calls the actual controller whitelist normalizer for flags,
  ESS selection, tariff changes, and setpoint overrides. Accepted commands remain
  stable under revalidation; unknown fields and unknown commands are rejected.
  Normalized toggle values remain on/off and setpoint values remain signed
  32-bit integers or null. Nothing is queued or published.
- `certificate-policy` exercises the production DER and PEM key-strength
  parsers through feature-gated, test-only entry points. It checks complete DER
  consumption, truncation/trailing-byte rejection, and chains of zero through
  eighteen certificates around the sixteen-certificate limit. Synthetic seeds
  cover RSA 1024/2047/2048-bit keys, allowed P-256 and Ed25519 keys, and disallowed
  P-224 keys. A chain containing a weak key must fail, even after strong keys.
  Named seed acceptance is asserted in ordinary CI; malformed seeds are also
  replayed. These checks do not establish certificate trust: production rustls
  still verifies chains, signatures and names before applying this policy.

The binary and fuzz harness use the same library modules. `src/main.rs` remains
responsible for process startup; the pure `controller_payload` entry point only
validates data. The harness never calls startup, `from_env`, transport binding,
MQTT clients, command execution, or background tasks. This campaign does not
exercise real authentication traffic, reconnection timing, TLS handshakes,
network backpressure, or hardware behavior.

The target processes have no network access in the shared runner. Preparation
may download public dependencies, then the instrumented binaries run in an
isolated network namespace. Inputs and fixtures are synthetic; no credentials,
private URLs, captured production payloads, or device data belong in the corpus.

## Budgets and retained evidence

Three targets run sequentially for **60 seconds each** (180 seconds of mutation
budget). The shared runner additionally bounds per-input time, memory, and the
whole job. Compilation is separate from mutation time. Rust is pinned to
`nightly-2026-10-12`; `fuzz/Cargo.lock` starts from the production lockfile and
adds the fuzz tooling. The shared workflow pins its cargo-fuzz and action
versions. Its report identifies the source, manifest hash, target inventory, duration,
and result. The source manifest records the commands.

The runner copies `fuzz/corpus/<executable>/` seeds to its writable diagnostics
area. It retains logs, the report, evolved corpus, and failure inputs as run
artifacts without mutating the checkout. Reports from another source revision
or expired artifacts are not substitutes for the current RC gate. Fuzzing is
bounded evidence, not proof of correctness or complete input coverage.

## Local reproduction

Follow the [official cargo-fuzz guide](https://rust-fuzz.github.io/book/cargo-fuzz.html)
and install the cargo-fuzz version pinned by the shared workflow. From the
repository root:

```sh
rustup toolchain install nightly-2026-10-12 --profile minimal --component rust-src
cargo fetch --locked --manifest-path fuzz/Cargo.toml
cargo test --locked --manifest-path fuzz/Cargo.toml --lib
cargo +nightly-2026-10-12 fuzz build --fuzz-dir fuzz
mkdir -p /tmp/gateway-fuzz-corpus
cp fuzz/corpus/energy_sources/* /tmp/gateway-fuzz-corpus/
cargo +nightly-2026-10-12 fuzz run energy_sources /tmp/gateway-fuzz-corpus -- \
  -max_total_time=60 -timeout=10 -rss_limit_mb=2048
```

For the certificate target, copy `fuzz/corpus/certificate_policy/` to a separate
scratch directory and use `certificate_policy` in the same command. The `fuzzing`
Cargo feature is enabled only by the unpublished fuzz crate; ordinary builds do
not expose these test entry points. No private certificate keys are committed.

Use a disposable corpus because libFuzzer adds useful mutations there. The
manifest maps the documented target names to executable names (underscores
instead of hyphens). The shared Linux runner uses instrumented
`x86_64-unknown-linux-gnu` binaries directly. Local macOS results do not replace
the Linux RC gate.

Reproduce a retained failure on its exact source and toolchain, then minimize:

```sh
cargo +nightly-2026-10-12 fuzz run energy_sources /path/to/crash-input
cargo +nightly-2026-10-12 fuzz tmin energy_sources /path/to/crash-input
```

Distinguish a production bug from an incorrect oracle or environmental failure.
Do not suppress the assertion or delete a failing seed just to release. Add a
focused ordinary regression test for a fixed bug and a minimized synthetic seed
when safe. Report sensitive findings under [SECURITY.md](SECURITY.md) before
publishing a reproducer.

## Release workflow retries

Fresh release inputs are selected by the exact GitHub workflow run ID and run
attempt.

Before publication starts, use **Re-run all jobs** only if the allocator and
release policy permit reuse of that run's reservation. Recreate every required
input in the same attempt; failed-only or individual retries can leave inputs
missing, and older artifacts must not fill those gaps.

For a fresh candidate or final build, once publication has started
(`begin_publication`), or its external publication result is partial or uncertain,
inspect the existing release, tags and assets before retrying. A consumed build
number requires a new workflow run and reservation authorized by the release
policy; an existing immutable tag may also require a new base version. Never
reset the release ledger, overwrite assets or retag to force a retry. Historical
byte promotion of an accepted RC keeps its reserved number and follows the
existing promotion policy.

Artifacts from older attempts may remain available for diagnostics; they are not
inputs to the new attempt and do not need to be deleted. Filtering artifact names
alone does not prove package completeness or receipt identity. Complete package
inventory and receipt identity checks remain separate release requirements.

See the shared [package identity and retry contract](https://github.com/victron-venus/venus-os-ci-toolkit/blob/main/docs/VERSIONING.md#package-identity-across-reruns).

### Package receipt identity

Fresh publication verifies each staged package receipt against the independently
supplied repository, source commit, workflow run and attempt. Receipts created
before this migration, or offline without an Actions identity, cannot qualify a
fresh publication. Run all required producers in one complete workflow attempt
after migration; an earlier attempt cannot fill missing inputs.

Historical promotion and retained-evidence validation keep their existing checks.
Receipt coverage validates the staged files; the expected packages and platforms
still require a separate completeness check. Existing release approval and device
acceptance policies continue to apply.

See the shared [receipt migration guide](https://github.com/victron-venus/venus-os-ci-toolkit/blob/main/docs/release-workflow.md#migrate-package-receipt-identity-without-replacing-the-release-engine).
