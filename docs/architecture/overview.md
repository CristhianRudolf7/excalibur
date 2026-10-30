# Architecture overview

[Español](overview.es.md)

This document describes the intended architecture. Rust provider, base/storage and minimum durable engine libraries exist; the complete application and interfaces remain planned. See [provider coverage](provider-implementation.md).

## Shared core and interfaces

The Rust core owns agent and provider logic. The CLI can call the core directly without an HTTP server. A future web interface will communicate through an HTTP API, which calls the same core; application logic must not be duplicated in the CLI or the API.

```text
CLI ──────────────────┐
                      ├── Shared core ── Agent and providers
Web ── HTTP API ──────┘
```

Initially, develop and test through the CLI and shared core. Whether the CLI should also use the API when live sessions need to be shared across interfaces remains an open decision.

See [planned provider support](providers.md) for the target integrations and other modalities.

See [Minimum durable engine](../development/durable-engine.md) for the current contract and backend checks.

The shared task/planning and evidence contracts are described in [Verifiable work](../development/verifiable-work.md).

Context compilation, durable sessions and provider caches are described in [Context and cache](../development/context-cache.md).

Project-domain tools and editing isolation are implemented in the shared libraries; see [Project tools and editing concurrency](../development/project-tools.md).

Scoped memory, retrieval, consolidation and the document library are a shared library not yet connected to the master; see [Memory and documents](../development/memory.md).

Daemon hosting, authenticated client transport, conversational turns and agent trees: [contract](../development/daemon.md).
