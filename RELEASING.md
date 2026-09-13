<!-- Generated from venus-os-ci-toolkit/templates/release-strategy.md and .release-policy.json. -->
# Release strategy

This document defines the release policy for **victron-venus/inverter-gateway**. The
[release runbook](docs/release-workflow.md) contains commands, prerequisites and
recovery steps. The [machine-readable policy](.release-policy.json) declares the
actual validation workflows, version source and project-specific blockers.

**Release readiness:** channels are implemented; activation and every required gate must pass before publication.

## Development and versioning

Changes use short-lived branches and reviewed pull requests into
`main`. The default branch is the integration and release source;
there is no separate permanent beta, RC or release branch. Branch/tag pushes do
not publish stable releases. Every repository versions and releases independently.

The committed base version comes from `Cargo.toml`. A release preparation PR
updates that version and any required native/package companion versions together.
Version numbers are explicit; a commit message does not automatically select one.

Use [Semantic Versioning](https://semver.org/): patch for compatible fixes, minor
for compatible functionality and major for incompatible changes. State changes to
configuration, APIs, protocols and stored data in the release notes. For `0.x`,
document breaking changes explicitly; do not imply a stable compatibility contract.
The `v` prefix belongs to the Git tag, not the numeric base version.

## Release channels

- **Nightly** is a daily integration build from the default branch. It runs the
  declared checks and complete build matrix. When candidate publication is enabled,
  its tag is `vX.Y.Z-nightly.<UTC timestamp>.<run>.<attempt>`. It is for development
  testing and provides no production support promise.
- **Beta** is a preview of the next version, built after a default-branch push or
  explicit request. Tags are `vX.Y.Z-beta.N`. Betas are intended for broader testing;
  features and behavior may still change before the RC.
- **Release candidate (RC)** is an explicit maintainer request for a version ready
  for acceptance testing. Tags are `vX.Y.Z-rc.N`. Each RC reruns the required checks
  and build matrix. A code, dependency or packaging change requires a new RC.
- **Stable** is a separate manual promotion of one accepted RC to `vX.Y.Z`.
  Promotion copies the verified RC payloads byte for byte; it never rebuilds them.
  Stable publication must not be inferred from a successful branch build.

Nightly and beta are previews; neither is directly promotable to stable. An RC
may be requested without a previous beta. Channel order is a workflow policy,
not a comparison of tag strings. Native binaries/packages retain the committed
base version; the release tag and manifest identify their channel and source.

## Required validation and evidence

1. The aggregate **CI gate** requires every workflow declared in the policy to
   succeed. Failed, missing, canceled or skipped required checks block the gate.
2. Candidate publication additionally requires the complete platform build matrix
   and **Release gate**. Artifacts from a partially successful matrix are not a
   releasable candidate.
3. The candidate manifest binds the source commit, source policy, run and attempt,
   complete asset inventory and SHA-256 checksums. A separate Actions artifact
   preserves the promotion evidence for 90 days.
4. Stable promotion verifies the original RC run, default-branch ancestry,
   qualification at the RC's source revision, retained evidence and every asset.
   Missing or expired evidence requires a new RC.

There is no force-publish, replace-tag or skip-checks option. A later policy edit
cannot retroactively qualify an older RC. Build artifact retention is declared by
each build adapter (currently 14 or 30 days across the fleet);
published candidate assets are not automatically deleted by this workflow.

## Release ownership and acceptance

The maintainer requesting the RC owns its release notes and acceptance evidence.
Before requesting stable, the maintainer must:

- Review the changes, compatibility impact, known issues and upgrade/recovery
  instructions. Update the existing changelog when present; otherwise include a
  human-readable summary in the release preparation PR. The categories in
  [Keep a Changelog](https://keepachangelog.com/en/1.1.0/) are a useful format.
- Test the exact RC assets on the intended targets, including applicable hardware,
  operating systems, integrations and configuration/data migrations. Record the
  tested RC tag, source SHA, results and limitations in the PR or linked issue.
- Confirm the original RC run succeeded and that no unresolved release blocker
  applies. Changing a binary after acceptance invalidates that acceptance.
- Request the exact RC's stable promotion and approve the pending release job.
  Publish user-facing notes alongside the generated provenance without replacing
  any payload or manifest.

The current application adapter uses the `release` environment's required reviewer
on public repositories, where that capability is available without a paid private
repository security plan. A single maintainer may request and approve a release;
this is not an independently enforced two-person review policy. Human acceptance
and release-note quality remain maintainer responsibilities, not inferred CI results.

## Publication and deployment

Candidate publication is initially disabled. The maintainer enables
`RELEASE_CHANNELS_ENABLED=true` only after the workflows pass, release approval is
configured and legacy deployment hooks have been migrated. Until then automatic
builds retain Actions artifacts and manual beta/RC/stable publication fails closed.

GitHub release publication, registry/package import and production deployment are
separate operations. Where configured, import the verified stable OCI or Python
assets with the local publication tool, then deploy immutable image digests or
versioned packages. Prereleases, tag pushes and generic CI completion must not
trigger production deployment. See the runbook for this repository's adapters.

## Hotfixes, rollback and support

A hotfix follows the same reviewed change, checks, RC acceptance and stable
promotion path with a new patch version. There is no emergency bypass. The current
pipeline releases from the default branch only; backport release branches require
an explicitly reviewed extension of the policy and are not implicitly supported.

For rollback, redeploy a previously accepted immutable artifact using the project's
deployment procedure. Check data/configuration compatibility first. Preserve the
failed version, tags and evidence for diagnosis; do not overwrite them. A corrected
build receives a new version and a new RC.

This policy does not promise an LTS branch or support duration. Prerelease feedback
does not substitute for acceptance of the selected RC.

## Project-specific limits

- Container is exported as an OCI archive; registry/deployment promotion must consume it without rebuilding.
- Existing Linux amd64 target is retained; live MQTT/hardware validation remains separate.

Local checks run all commands in `local_checks`; missing toolchains, container
runtimes or credentials are failures, not successful skips. They help reproduce
CI but do not create promotion evidence or certify untested physical targets.



## Maintaining this policy

Changes to the lifecycle, required gates or version/asset mappings go through PR
review together with the corresponding workflow changes. Update the toolkit
template and `.release-policy.json`, then regenerate and review this document and
the runbook. The README links here rather than duplicating the release procedure.
