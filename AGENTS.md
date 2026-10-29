# Agent instructions

- This repository contains provider, storage, minimum durable engine, security, vault and sandbox libraries. Do not describe planned application or interface capabilities as implemented.
- Keep directory names in English. Maintain English and Spanish documentation as paired files using the `.es.md` suffix, and update both when their shared content changes. This AGENTS.md is maintained in English only.
- Respect the scope of the task and preserve unrelated work. Do not invent credentials, contact information, licensing decisions or test results.
- Add implementation and verification commands here when the corresponding components exist.
- Write commit messages in natural, human language that clearly describes the change.
- Keep agent and provider logic in the shared core, not duplicated in the CLI or HTTP API. See [the architecture overview](docs/architecture/overview.md).
- Rust provider library checks: `cargo fmt --check`, `cargo clippy --workspace --all-targets -- -D warnings`, and `cargo test --workspace`. Tests must use mocks and must not call paid APIs.

- Storage verification: `cargo test -p excalibur-store -- --nocapture` exercises concurrency, external locking, real WAL growth and deterministic subprocess interruption tests. See [Base and storage](docs/development/storage.md).

- Durable engine verification: `cargo test -p excalibur-engine --test crash -- --nocapture` covers real process interruption and effect reconciliation; `cargo test -p excalibur-engine --test end_to_end -- --nocapture` runs the shared engine through a mock provider and real local tools. See [Minimum durable engine](docs/development/durable-engine.md).

- Durable control and batch verification: `cargo test -p excalibur-engine --test control_races --test batch_preparation --test batch_crash`, `cargo test -p excalibur-journal --test state_transitions` and `cargo test -p excalibur-tools --test execution_conflicts` cover control races, sequential real file edits, unadmitted batches after SIGKILL, atomic state guards and durable conflict escalation.

- Security verification: `cargo test -p excalibur-security -p excalibur-vault`, `cargo test -p excalibur-tools --test sandbox --test protected_paths` and `cargo test -p excalibur-context --test trust`. Linux sandbox tests require the verified bubblewrap runtime and exercise actual kernel isolation; unsupported profiles fail closed. See [security contracts and limits](docs/development/security.md).
