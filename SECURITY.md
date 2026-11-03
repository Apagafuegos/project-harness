# Security policy

project-harness runs model-requested tools on your machine, so sandbox escapes,
authorization bypasses, secret leaks and journal tampering are in scope. The
intended boundaries are described in
[`docs/contracts/execution-security.md`](docs/contracts/execution-security.md).

## Reporting a vulnerability

Please report privately through GitHub:
[Report a vulnerability](https://github.com/Apagafuegos/project-harness/security/advisories/new).
Do not open a public issue for a suspected vulnerability.

Include the harness version (`harness --version`), the output of
`harness doctor`, and the steps or session needed to reproduce it.

## Supported versions

Before 1.0, only the latest release receives security fixes.
