# OpenSSF Best Practices evidence: inverter-gateway

This index supports review against the [OpenSSF Passing criteria](https://www.bestpractices.dev/en/criteria/0). It is not an awarded badge, a security guarantee or a completed self-attestation. Initial source inventory: `7fe41df969094574325ebd26df574c2646ea19db`. Re-check the final merged commit and its CI before submitting an assessment.

## Project and contribution process

Exposes authenticated HTTPS/SSE energy snapshots and scoped commands backed by MQTT.

- [Public repository and history](https://github.com/victron-venus/inverter-gateway) provide source, commits and interim changes.
- [README](../README.md) describes installation, configuration and usage.
- [Contribution process](../CONTRIBUTING.md) documents reports, review, style and the policy to add automated tests for major changes.
- [Issues](https://github.com/victron-venus/inverter-gateway/issues) and [pull requests](https://github.com/victron-venus/inverter-gateway/pulls) provide searchable public discussion and change review.
- [Security policy](../SECURITY.md) documents confidential reporting, response goals, trust boundaries and delivery practices.

The root [LICENSE](../LICENSE) records the project license. Third-party components retain their own notices.

## Implementation and interfaces

- [src/config.rs](../src/config.rs)
- [src/http.rs](../src/http.rs)
- [src/mqtt_bridge.rs](../src/mqtt_bridge.rs)
- [src/whitelist.rs](../src/whitelist.rs)
- [docs/transport-security.md](../docs/transport-security.md)

Interface documentation must explain accepted configuration and inputs, outputs, failure handling and relevant permission boundaries. Verify it against the implementation when changing behavior; source links alone do not establish that every interface is documented.

## Build, test and analysis evidence

The local validation entry point is [scripts/ci.sh](../scripts/ci.sh). Its actual test and compiler/linter commands, not the presence of a workflow name, define the available coverage.

[GitHub Actions](https://github.com/victron-venus/inverter-gateway/actions) provides run logs and results. The checked-in workflow definitions are:

- [auto-approve.yml](../.github/workflows/auto-approve.yml)
- [auto-merge.yml](../.github/workflows/auto-merge.yml)
- [ci.yml](../.github/workflows/ci.yml)
- [codeql.yml](../.github/workflows/codeql.yml)
- [coderabbit-autofix.yml](../.github/workflows/coderabbit-autofix.yml)
- [coderabbit-review.yml](../.github/workflows/coderabbit-review.yml)
- [dependency-review.yml](../.github/workflows/dependency-review.yml)
- [quality-gate.yml](../.github/workflows/quality-gate.yml)
- [release-build.yml](../.github/workflows/release-build.yml)
- [release-pipeline.yml](../.github/workflows/release-pipeline.yml)
- [scorecards.yml](../.github/workflows/scorecards.yml)

Do not equate a green metadata or release job with successful application tests. Record actual test results, coverage limitations and security-analysis results for the submitted revision. Test execution does not establish physical-device behavior.

## Items requiring explicit verification before submission

- Confirm the private reporting channel works and examine issue/advisory history. Historical response-time claims require actual reports and responses, including any reports outside GitHub.
- Obtain primary-developer attestations about secure-design and vulnerability-prevention knowledge; repository text cannot establish a person's knowledge.
- Verify every user-facing release has useful release notes and upgrade impact, and includes any assigned vulnerability identifiers for fixes.
- Review dependency, code-scanning and secret-scanning findings and their age. A workflow success result is not proof that all findings are resolved.
- Review the actual cryptographic libraries, protocols, key lengths, randomness, certificate checks and password storage applicable to this project. Do not copy another project's answers.
- Verify build reproducibility from source, test policy adherence in recent substantive changes, dynamic analysis and any manual-memory-code checks.
- Link only this project's real awarded badge once the assessment is accepted.

The live assessment, when created, is the source of truth for the badge level. Unverified criteria remain open.

## Release-note source

[CHANGELOG.md](../CHANGELOG.md) contains the current development line's change summary, upgrade impact and security notes. The release policy opts into commit-pinned notes; publication rejects missing or incomplete current-version sections. Historical release bodies still require a separate audit before claiming complete coverage.

The October 2026 maintenance imports the release identity and metadata parser
from `venus-os-ci-toolkit` revision `9dd211a`, including bounded TOML parsing and
source-bound release-note validation. Consumer release-contract suites verify
the imported engine. Workflow-validator refactoring preserves this repository's
existing policy; it does not imply all newer toolkit workflow guarantees are enabled.

Release guidance parsing also imports toolkit revision `ec99f37`: Markdown code
fences cannot provide or split the required version, Upgrade or Security headings.
The release contract suite tests matching fence markers and lengths, unclosed
examples, CRLF input and rejection before any remote publication writes.

Release guidance and validation helpers also import reviewed toolkit revision `cab6d07`: comments and empty code fences cannot satisfy upgrade/security guidance, while visible literal examples remain valid. Receipt size limits are enforced before parsing. The existing consumer workflow policy is retained.

[Toolkit revision `6f3bac8`](https://github.com/victron-venus/venus-os-ci-toolkit/commit/6f3bac867a101441e029ee7c20c5aa92541eb308) also makes same-level and higher-level ATX headings end release sections, including empty headings, while headings inside code or comments remain inert.

Release structure uses ATX headings (`#`, `##`, `###`), with up to three leading spaces. Setext-style or ambiguous text/comment/underline structures in the selected version are rejected; use ATX headings or a blank line before a thematic `---` separator. Literal fenced/indented examples and comments do not define sections.

Toolkit revision `3533a43322cb351433ca8e90b786f14edcea7725` validates staging inputs before creating output and removes only a newly created target after a failed copy, artifact check or receipt write. Regression tests cover retry and preservation of existing operator files.

Toolkit revision `4895f852f87ca1f27e2bc75e24c5c792e4f95aa6` includes verified staging cleanup and rejects release guidance made only of headings or separators. Regression tests cover retry, preservation of existing operator files, and visible guidance requirements.
