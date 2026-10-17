# Release version inputs

The `versioning` section of `.release-policy.json` owns the exact fields that identify
this application. Committed sources contain a numeric base version. The release
workflow freezes one `.release-plan.json` before any platform build and applies its
version overlay before dependency installation or compilation.

Beta and nightly packages include their candidate identity. Python package fields
use PEP 440 (`X.Y.ZbN` or `.dev...`); SemVer fields use the corresponding hyphenated
suffix. With the `promote-bytes` profile, RC packages contain the final numeric base
version so stable promotion can reuse the exact verified RC bytes. The plan and
release manifest retain the full RC identity.

Local base checks do not allocate versions or create publishable candidates:

```bash
python3 scripts/version_plan.py check-base --root .
```

For a clean checkout at the frozen source commit, apply a plan from the release
workflow before running the existing package command:

```bash
python3 scripts/version_plan.py sync --root . --plan .release-plan.json
```

`RELEASE_VERSION_PLAN` can select a plan path for the local version validator. The
positional package arguments remain the numeric base and channel. A mismatched
source commit, policy, channel, base, or version field fails validation. Independent
dependency versions are never changed by a broad search-and-replace.

Every platform stages only its original declared upload files in a fresh flat
`release-assets/<target>/` directory. Before upload, the staging helper checks the
version inputs again, reads declared package metadata, and produces a receipt with
payload sizes and SHA-256 hashes bound to the plan. Native/source archives include
the frozen plan and input evidence where their packaging format permits it.

The receipt proves which source inputs and bytes were packaged; device, installer,
store, service, and production deployment acceptance remain separate checks.

## Owned files

- `Cargo.lock`
- `Cargo.toml`
- `fuzz/Cargo.lock`

OCI builds set `org.opencontainers.image.version` from the checked package projection
and `org.opencontainers.image.revision` from the source commit. Publication verifies
the version label on every runnable platform image, following the bounded OCI
index, manifest and config digest chain. Local container builds require an available
Docker daemon; registry publication and live deployment are separate actions.

## Exact OCI acceptance

The project-owned `release-build.yml` adapter runs
`scripts/smoke_release_container.py` after BuildKit exports the original OCI archive
and before either container asset is staged. It requires native Linux amd64 on
both the host and the local Docker daemon; an ARM runner or remote daemon cannot
silently substitute emulation. The existing runner-selection variables remain
available, and this amd64 job fails closed on incompatible labels.

The helper validates the OCI digest graph, SBOM/provenance subjects, runtime
configuration and executable before running its normal entrypoint with `--version`.
A temporary Docker archive contains the same config bytes and uncompressed layer
bytes; there is no second build or registry pull. Imported image ID and ordered
rootfs diffIDs must match the original OCI. If that exact image already exists,
it is verified and used without retagging or removing it. Otherwise only a unique
owned temporary tag is loaded and removed, without force or prune. The container
has no network, a read-only rootfs, dropped capabilities and bounded resources.

Success requires the exact created container to exit with integer status 0,
without OOM/runtime errors, expected output and verified owned-container cleanup.
An exclusive intent is persisted before Docker mutations. An interrupted or
unknown outcome fails acceptance; `container-smoke-diagnostics-<attempt>` retains
its identity and cleanup result for investigation. Inspect the recorded unique
name/tag and ownership before manual cleanup; never prune the runner globally.

`inverter-gateway-container-smoke.json` is versioned and staged beside the unchanged
OCI archive. Its source, plan, run/attempt, original OCI SHA-256, config digest,
rootfs diffIDs, binary hash and process result must match candidate evidence.
Stable promotes these same RC bytes. This checks the native loader, entrypoint
and version path only; MQTT, TLS, equipment and deployment acceptance remain
separate. The standalone native archive keeps its own pre-packaging binary check;
its glibc binary is not expected to match the OCI's musl binary hash.
