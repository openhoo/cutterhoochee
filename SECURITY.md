# Security policy

## Supported versions

Security fixes target the latest released version and the `main` branch.

## Reporting a vulnerability

Use [GitHub private vulnerability reporting](https://github.com/openhoo/cutterhoochee/security/advisories/new). Do not disclose a suspected vulnerability in a public issue.

Include the affected version, operating system, reproduction steps, impact, and any known mitigations. Use generated media and disposable projects whenever possible. Never attach provider credentials, OAuth tokens, private recordings, or an unredacted application-data directory.

The maintainers coordinate validation, remediation, and disclosure. Vulnerabilities affecting project-file parsing, native IPC, provider evidence consent, external-tool permissions, or bundled media processing are security-relevant even when exploitation requires opening a local file.

## Distribution boundary

The Git repository contains application source and documentation, not personal editing projects or generated native sidecars. Releases must retain the bundled components' license notices and source provenance. The project's Apache-2.0 license does not replace third-party licenses.
