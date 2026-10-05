# Security Policy

## Supported versions

Pre-1.0: only the latest released version receives security fixes.

## Reporting a vulnerability

Please do not open a public GitHub issue for security vulnerabilities,
especially anything involving:

- Credential/secret handling (Slack webhook URLs, API keys in config)
- FFI memory safety (panics crossing the boundary, use-after-free in the
  C ABI layer)
- Injection via signal metadata (e.g. into the SQLite sink or webhook
  payloads)

Instead, use GitHub's private vulnerability reporting (Security tab →
"Report a vulnerability") on this repository, or email the maintainer
directly (add your contact email here before publishing).

## Known accepted risks (v0.1, by design — not vulnerabilities)

- Config-file + environment-variable secret interpolation is the only
  supported mechanism in v0.1. Dedicated secret-manager integration
  (Vault, AWS Secrets Manager) is a named future item, not a gap we
  consider a vulnerability today.
- The SQLite sink stores full signal payloads, including any metadata the
  caller supplies. Do not put secrets in signal metadata/messages — this
  is a documented caller responsibility, not something the SDK sanitizes
  for you in v0.1.
