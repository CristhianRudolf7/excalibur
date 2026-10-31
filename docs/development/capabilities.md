# Capability contracts

[Español](capabilities.es.md)

`excalibur-capabilities` is the shared library for architecture §14.1, §14.8 and §14.11. It covers the capability manifest contract, the durable long-operation task lifecycle (`working`, `input_required`, `completed`, `failed`, `cancelled`), keyed effects, cooperative cancellation that is re-polled, restart-safe polling with backoff driven by an injected clock, the §14.8 application state deltas with acceptance gates, the §14.11 outbound disclosure filter and a deterministic simulated provider. It performs no network I/O and is not wired into the daemon; MCP/ACP transports and concrete capabilities remain pending.

## Manifest

`CapabilityManifest::validate` checks bounded identity and version, 1–512 uniquely named operations with object schemas, idempotency keys for every operation with effects, bounded platforms and deployment profiles, `max_concurrency` 1–1024, positive session limits, unique states including `working` and at least one terminal state, and a 1 MiB definition bound. Registering the same identity and version twice is idempotent only for a byte-identical definition.

## Durable tasks

`CapabilityTasks` is scoped by authenticated host authority, never by model or capability content. The durable task id is `cap_<32 hex>` over scope and operation key, so resubmitting the same key returns the same task and a different key or scope never collides with it.

Each drive follows claim, remote call and guarded apply:

1. a claim transaction picks the next action (start, deliver input, cancel or poll), counts start attempts and leases the task;
2. the provider is called outside any transaction;
3. an apply transaction records the outcome only when the claim row version still matches, so a stale driver cannot overwrite newer state.

A crash between claim and apply leaves a visible claim. `recover_interrupted` releases it at host startup and the next drive resends the same idempotency key (`<task>:start`, `<task>:input:<seq>`, `<task>:cancel`). The simulation applies each key once and counts both receipts and applications, so tests prove at-least-once delivery with at-most-once effects.

Polling, input and cancellation behave as follows:

- `next_poll_at` follows the remote `poll_interval_ms` clamped to the harness policy; transient errors use exponential backoff (`backoff_base_ms << (n-1)`, capped at `backoff_max_ms`) and fail after `max_transient_failures` retries. A pre-start deadline or explicit cancel moves the task to `cancelled` without contacting the remote. A confirmed start that keeps failing is marked `effect_uncertain` so a host never silently resubmits under a new key.
- `input_required` surfaces the request as untrusted `ExternalOutput`, records the host answer durably first and delivers it with the per-request key. A rejected answer asks again with a new sequence; the answer bytes are never written to the audit log.
- cancellation is cooperative: `request_cancel` is idempotent, the keyed cancel is delivered once and polling continues. An ignored cancel that later completes sets `cancel_unhonored`; a late `cancelled` still wins. Every transition, dispatch, transient error, rejection, deadline, concurrency deferral, lease expiry and stale-outcome discard is appended to `capability_task_events`.

Concurrency and isolation are enforced in the claim: one SQLite writer serializes drivers, concurrent drivers send one start, stale outcomes are discarded by claim sequence, per-capability `max_concurrency` defers extra starts and other scopes can neither read nor drive a task.

## Disclosure and application state

`filter_outbound` enforces the §14.11 policy per contact and purpose. The default is deny: anything not explicitly allowed is withheld and requires user permission. Credentials are never shared whatever the policy says, and commitments (payment, contract, appointment) need explicit authorization. Withheld segments keep only their index, category and reason; their text is never echoed.

`AppSnapshot`, `delta`, `gate_allows` and `gate_blockers` cover §14.8. The host keeps one snapshot per `app:<id>` conversation, includes only the delta in L3 when the state changes and treats manual and agent edits through the same `rev + 1` step. `delta` rejects cross-instance and backwards revisions and reports changed and removed top-level keys with a compact summary. Application rules arrive as `AcceptanceCheck` values; the gate blocks completion while any required check fails. Optional checks never block.

## Simulation

`SimulatedCapability` plays the remote side against its own JSON file, separate from the harness database, so it survives harness restarts. Scripts reveal one step per poll, `CancelBehavior` covers honor, ignore and honor-after-N-polls, and hooks for `fail_next` and `lose_next_responses` reproduce outages and post-delivery timeouts. Receipt and effect counters prove each keyed start, input and cancel is applied once no matter how often it is received.

## Verification

```sh
cargo test -p excalibur-capabilities --test lifecycle --test properties
cargo fmt --check
cargo clippy -p excalibur-capabilities --all-targets -- -D warnings
cargo check --workspace --all-targets
cargo build -p excalibur-capabilities
```

`lifecycle` covers the §14.12 rows in scope: full lifecycle through every state with a daemon restart mid-task, keyed resend applied once after lost responses, unhonoured cancellation re-polled to completion, transient backoff across restart with uncertain-effect marking, pre-start and deadline cancels without extra effects, intents recorded during flight with stale outcomes dropped, single-start concurrency with per-capability limits, undeclared states with wrong-sequence answers and scope isolation, and rejected input with schema checksum enforcement. `properties` sweeps deterministic fixed-seed inputs over durable id stability, backoff shape, manifest idempotency rules, disclosure deny-by-default with credential exclusion and commitment authorization, and application delta and gate behavior. Tests use temporary SQLite databases and the deterministic simulation; they make no model or network calls.

## Limits

- Library only: no daemon, RPC, tool or Agenda wiring; no MCP/ACP transport; no concrete browser, search, vision, voice, device, document, messaging, workspace, meeting, agent-contact or node capability.
- Browser routines and assertions, `web_read`/`source_search` source events, hidden-instruction marking, credential-by-reference plumbing, meeting consent, node sandboxing, document conversion, GPU training, store workflows and the remaining §14.12 rows are not covered here.
- Timeouts and budgets are per-task deadlines and retry caps only; cost accounting and provider failover live elsewhere.
- File modes and SQLite behavior are verified on Linux only. Other platforms have not been built or run.
