# Excalibur

[Español](README.es.md)

Excalibur is a planned personal AI agent that will converse with users, help plan and execute tasks, work across projects, write and modify code, run tools and tests, search for information, remember preferences and decisions, and retrieve relevant conversation context. It is intended to delegate work to specialized subagents and extend its capabilities through skills.

## Status

The repository currently contains an initial structure and documentation only. The agent, CLI, web interface, store and other functionality have not yet been implemented.

## Planned organization

- `apps/`: CLI and web applications.
- `crates/`: Rust backend and agent components.
- `packages/`: shared frontend packages.
- `store/`: planned catalog, installation and examples for tools and workflows.
- `docs/`: architecture, development, user guides, store and research documentation.
- `tests/`: integration tests and benchmarks.
- `scripts/`: project automation scripts.

See [ROADMAP.md](ROADMAP.md), [AGENTS.md](AGENTS.md), [CHANGELOG.md](CHANGELOG.md) and [SECURITY.md](SECURITY.md). Excalibur is source-available under a license with commercial restrictions; see [LICENSE](LICENSE).
