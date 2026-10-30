---
name: common-pitfalls
description: "Checklist of frequent mistakes to review before delivery: skipped tests, out-of-scope changes, hardcoded paths or credentials, leftover generated files, stale docs, unchecked outputs."
metadata:
  excalibur.group: verification
  excalibur.version: "1.0.0"
  excalibur.platforms: "linux,macos,windows"
  excalibur.risk: low
  excalibur.display: "{\"en\": {\"name\": \"Common pitfalls\", \"description\": \"Checklist of frequent mistakes before delivery\"}, \"es\": {\"name\": \"Errores frecuentes\", \"description\": \"Lista de errores frecuentes antes de entregar\"}}"
---
# Common pitfalls

Review this list before delivering any change.

- **Tests**: none skipped, disabled, marked ignored or weakened to pass; new behavior has a test.
- **Scope**: no changes outside the requested scope; unrelated work in the tree is preserved.
- **Hardcoding**: no absolute machine paths, usernames, hostnames, credentials, tokens or secrets in code, tests or docs.
- **Leftovers**: no debug prints, temporary files, generated artifacts or build outputs committed by mistake.
- **Documentation**: docs, examples and changelogs reflect the change; paired translations are updated together.
- **Outputs**: every claimed result was actually observed; command outputs were read, not assumed.
- **Errors**: failures are reported, not hidden; error messages are actionable.
- **Formatting and lint**: the project's formatter and linters pass.
