# Security policy

## Reporting a vulnerability

Send vulnerability details privately using [GitHub's Report a vulnerability form](https://github.com/victron-venus/inverter-gateway/security/advisories/new). Include the affected commit or release, reproduction steps, impact and a suggested mitigation if known. Remove live secrets and personal information. Do not open a public issue with exploit details. If private reporting is temporarily unavailable, open a public issue requesting a confidential contact without disclosing the vulnerability.

Maintainers aim to acknowledge reports within 14 days, investigate promptly and coordinate disclosure with the reporter. Critical confirmed vulnerabilities receive priority. Confirmed exploitable medium-or-higher issues should be corrected within 60 days of public disclosure; where a fix needs longer, publish the limitation and available mitigation. Release notes should identify any assigned CVE or equivalent advisory identifier for fixes. These are maintenance policies, not claims about historical response times.

## Supported code

Security fixes are developed on the current default branch and released through the repository's normal delivery process. Older releases are not guaranteed backports; reproduce against the current code where practical and include the original affected version in the report. A prerelease or development build is not a promise of production or hardware acceptance.

## Trust boundaries

HTTP bearer tokens, Cloudflare Access and the MQTT broker form separate trust boundaries. Keep read-only and command credentials scoped, require certificate validation, and restrict allowed MQTT topics and commands. Tokens belong in server-side configuration, never public browser assets.

## Secure development and delivery

Validate external values at trust boundaries, reject unsupported or malformed commands, avoid shell interpolation, preserve certificate verification, and use maintained cryptographic libraries rather than custom cryptography. Apply least privilege to service accounts, repository tokens and filesystem permissions. Follow [CONTRIBUTING.md](CONTRIBUTING.md) for validation and review.

Obtain source and published artifacts through the repository's HTTPS URLs. Verify published checksums or provenance when provided, over an authenticated channel. Keep local configuration, credentials and private keys out of source control and logs. Report suspected exposure through the private channel so credentials can be revoked and replaced; deleting a file alone does not revoke it.

[OpenSSF evidence and remaining verification](docs/openssf-evidence.md) is maintained separately from this policy.
