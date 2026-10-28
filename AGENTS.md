# Agent instructions

- This repository is an initial scaffold. Do not describe planned capabilities as implemented.
- Keep directory names in English. Maintain English and Spanish documentation as paired files using the `.es.md` suffix, and update both when their shared content changes. This AGENTS.md is maintained in English only.
- Respect the scope of the task and preserve unrelated work. Do not invent credentials, contact information, licensing decisions or test results.
- Add implementation and verification commands here when the corresponding components exist.
- Write commit messages in natural, human language that clearly describes the change.
- Keep agent and provider logic in the shared core, not duplicated in the CLI or HTTP API. See [the architecture overview](docs/architecture/overview.md).
- Rust provider library checks: `cargo fmt --check`, `cargo clippy --workspace --all-targets -- -D warnings`, and `cargo test --workspace`. Tests must use mocks and must not call paid APIs.
