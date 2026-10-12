# Changelog

## [0.4.2] - Development line

### Changes

- Reuse fresh telemetry when deciding command availability, avoiding unnecessary state copies.
- Replace the unmaintained PEM parser and refresh compatible dependencies.
- Stop packaging on a binary-version mismatch, preserve reviewed release notes, and read release API status from HTTP headers.
- Refresh shared CI actions, Sonar coverage for release helpers, and the optional runner reserve; GitHub-hosted runners remain the default.

### Upgrade

No configuration or data migration is introduced. Retain the existing MQTT and HTTPS credentials. This candidate still requires independent validation against the intended equipment before production use.

### Security

Replace the unmaintained PEM parser and retain pinned CI dependencies. MQTT and HTTPS authentication requirements remain unchanged; keep credentials outside published artifacts and maintain network isolation and independent equipment safeguards.

## [0.4.1] - Development line

### Release overview

Exposes authenticated HTTPS/SSE energy snapshots and scoped commands backed by MQTT. The existing README documents configuration and external interfaces for this development line.

### Maintenance

- Publish reviewed release notes from the exact source commit used to build each candidate, preserving build provenance.
- Document contribution checks, confidential security reporting and the project-specific trust boundaries.

### Upgrade

These maintenance changes do not introduce a configuration or data migration. Retain local configuration and credentials when using the documented update procedure. Validate the candidate on an isolated system before production use; automated checks do not establish hardware acceptance.

### Security

Private vulnerability reporting and response policy are documented in SECURITY.md. This maintenance update strengthens release evidence and review instructions; it does not replace deployment authentication, network isolation or independent equipment safeguards. No new project CVE is announced by these changes.
