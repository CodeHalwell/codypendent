# Security Policy

Codypendent is an agentic developer environment that can execute commands, modify repositories, call external services, and process untrusted content. Security reports are treated as high priority.

## Report privately

Do not open a public issue for:

- command or sandbox escape;
- secret exposure;
- plugin signature bypass;
- cross-scope data leakage;
- approval bypass;
- GitHub write without authorization;
- client authentication bypass;
- malicious event replay;
- unsafe default capability.

Use the repository's private vulnerability reporting feature or the security contact configured by the project.

## Security expectations

- tools run with least privilege;
- plugins are untrusted by default;
- MCP compatibility does not imply safety;
- model output is untrusted;
- repository and document content may contain prompt injection;
- secrets are brokered and redacted;
- remote model eligibility follows data classification;
- all sensitive actions are auditable;
- updates that expand permissions require explicit review.

## Supported versions

Until the first stable release, only the latest minor release receives security fixes.

## Disclosure

The project aims to acknowledge a valid report promptly, provide a remediation plan, and coordinate disclosure after a fix is available.
