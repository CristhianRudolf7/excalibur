# Agent instructions

- This repository contains provider, storage, minimum durable engine, security, vault, sandbox, telemetry and evaluation libraries, plus the noninteractive exec command. Do not describe planned application or interface capabilities as implemented.
- Keep directory names in English. Maintain English and Spanish documentation as paired files using the `.es.md` suffix, and update both when their shared content changes. This AGENTS.md is maintained in English only.
- Respect the scope of the task and preserve unrelated work. Do not invent credentials, contact information, licensing decisions or test results.
- Add implementation and verification commands here when the corresponding components exist.
- Write commit messages in natural, human language that clearly describes the change.
- Keep agent and provider logic in the shared core, not duplicated in the CLI or HTTP API. See [the architecture overview](docs/architecture/overview.md).
- Rust provider library checks: `cargo fmt --check`, `cargo clippy --workspace --all-targets -- -D warnings`, and `cargo test --workspace`. Tests must use mocks and must not call paid APIs.

- Storage verification: `cargo test -p excalibur-store -- --nocapture` exercises concurrency, external locking, real WAL growth and deterministic subprocess interruption tests. See [Base and storage](docs/development/storage.md).

- Durable engine verification: `cargo test -p excalibur-engine --test crash -- --nocapture` covers real process interruption and effect reconciliation; `cargo test -p excalibur-engine --test end_to_end -- --nocapture` runs the shared engine through a mock provider and real local tools. See [Minimum durable engine](docs/development/durable-engine.md).

- Durable control and batch verification: `cargo test -p excalibur-engine --test control_races --test batch_preparation --test batch_crash`, `cargo test -p excalibur-journal --test state_transitions` and `cargo test -p excalibur-tools --test execution_conflicts` cover control races, sequential real file edits, unadmitted batches after SIGKILL, atomic state guards and durable conflict escalation.

- Security verification: build the internal native adapter with `cargo build -p excalibur-sandbox --bin excalibur-network-bridge` before isolated network tests. Run `cargo test -p excalibur-sandbox -p excalibur-security -p excalibur-vault`, `cargo test -p excalibur-tools --test sandbox --test sandbox_adversarial --test protected_paths --test file_authority` and `cargo test -p excalibur-context --test trust`. Full workspace tests build the adapter through integration fixtures. `cargo test -p excalibur-engine --test out_of_band_approval` and security finalization tests exercise background approval ordering. Linux tests use verified bubblewrap, real C/Rust builds and isolated local HTTP/TLS/SOCKS fixtures; unavailable confinement fails closed. See [security contracts and limits](docs/development/security.md).

- Noninteractive verification: `cargo test -p excalibur-cli -p excalibur-eval -p excalibur-telemetry -p excalibur-models` and `cargo test -p excalibur-engine --test event_projection` exercise CLI exit codes, denied approvals, isolated trial policy, incremental artifacts, replay, usage and resource accounting. See [Noninteractive execution and evaluation](docs/development/noninteractive.md); official offline reference validation is documented in [telemetry](docs/development/telemetry.md). Tests remain local and synthetic.
