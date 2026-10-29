# Excalibur

[Español](README.es.md)

Excalibur is a planned personal AI agent that will converse with users, help plan and execute tasks, work across projects, write and modify code, run tools and tests, search for information, remember preferences and decisions, and retrieve relevant conversation context. It is intended to delegate work to specialized subagents and extend its capabilities through skills.

## Status

The repository contains documentation and shared Rust provider, storage, minimum durable engine, durable task/planning and evidence, context/cache, security, vault, Linux sandbox, telemetry and evaluation libraries. The complete personal agent, interactive CLI, web interface and marketplace remain planned. See [provider coverage and limits](docs/architecture/provider-implementation.md).

See [Base and storage](docs/development/storage.md) for implementation and verification evidence. See [Minimum durable engine](docs/development/durable-engine.md) for the embedded step loop, effect recovery, context, gateway and local tools.

For toolchain setup and available checks, see the [development environment](docs/development/environment.md).

See [Mandatory security](docs/development/security.md) for backend authorization, secret handling and sandbox contracts.

## Planned organization

- `apps/`: CLI and web applications.
- `crates/`: Rust backend and agent components.
- `packages/`: shared frontend packages.
- `store/`: planned catalog, installation and examples for tools and workflows.
- `docs/`: architecture, development, user guides, store and research documentation.
- `tests/`: integration tests and benchmarks.
- `scripts/`: project automation scripts.

See [ROADMAP.md](ROADMAP.md), [AGENTS.md](AGENTS.md), [CHANGELOG.md](CHANGELOG.md) and [SECURITY.md](SECURITY.md). Excalibur is source-available under a license with commercial restrictions; see [LICENSE](LICENSE).

The `excalibur exec` command, evaluation profile, metrics/ATIF and replay are documented in [Noninteractive execution and evaluation](docs/development/noninteractive.md). The concrete Harbor adapter is supplied by the user.

See [Verifiable work](docs/development/verifiable-work.md) for durable tasks, plans, fresh evidence, the completion gate and independent verification.

See [Context and cache](docs/development/context-cache.md) for durable context views, compaction, literal instructions and provider cache contracts.
