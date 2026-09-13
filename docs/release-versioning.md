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

OCI builds set `org.opencontainers.image.version` from the checked package projection
and `org.opencontainers.image.revision` from the source commit. Publication verifies
the version label on every runnable platform image, following the bounded OCI
index, manifest and config digest chain. Local container builds require an available
Docker daemon; registry publication and live deployment are separate actions.
