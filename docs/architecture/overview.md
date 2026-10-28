# Architecture overview

[Español](overview.es.md)

This document describes the intended architecture. A partial Rust provider library exists; the agent and interfaces are not yet implemented. See [provider coverage](provider-implementation.md).

## Shared core and interfaces

The Rust core owns agent and provider logic. The CLI can call the core directly without an HTTP server. A future web interface will communicate through an HTTP API, which calls the same core; application logic must not be duplicated in the CLI or the API.

```text
CLI ──────────────────┐
                      ├── Shared core ── Agent and providers
Web ── HTTP API ──────┘
```

Initially, develop and test through the CLI and shared core. Whether the CLI should also use the API when live sessions need to be shared across interfaces remains an open decision.

See [planned provider support](providers.md) for the target integrations and other modalities.
