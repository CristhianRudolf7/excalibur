# Minimum durable engine

[Español](durable-engine.es.md)

The shared Rust libraries implement the minimum durable programming path: journal and effect reconciliation (17.1–17.2), step loop/resource scheduling (7.3–7.4), a normalized OpenAI-compatible gateway (19.2), basic context compilation (11.1), and local file/terminal tools (13.2 and the execution pipeline in 13.4–13.6). The external architecture document remains the scope reference. There is no product CLI, frontend HTTP server or application daemon.

## Libraries and embedding

| Crate | Contract |
|---|---|
| `excalibur-types::runtime` | Provider-neutral messages, calls, schemas, usage, prepared effects and tool outcomes |
| `excalibur-journal` | Confirmed append-only SQLite events, operation deduplication, pages and checkpoints |
| `excalibur-engine` | Durable step driver, budgets, basic completion/loop guards, controls and resource scheduling |
| `excalibur-context` | Deterministic L0–L5 compilation with explicit inputs and per-layer budgets |
| `excalibur-models` | Normalized complete/streamed OpenAI-compatible Chat Completions over the existing provider transport |
| `excalibur-tools` | Capability-rooted file tools, fingerprint edits, execution policy, terminal and post-edit integrations |

The host opens the principal `Database`, initializes `Journal::new`, supplies a `ModelGateway` and `ToolRuntime`, and constructs `Engine::new`. `Engine::create(operation_id, RunSpec)` persists an immutable initial specification and returns a run ID. Repeating an operation with the same specification returns that run; incompatible reuse is rejected. `Engine::run(run_id)` drives or reconstructs it. Independent engine instances share per-run and per-resource locks within the process; existing database ownership prevents a second process opening the same file.

`RunSpec` contains a frozen `ContextInput`, `Budget` and at least one acceptance criterion. Criteria can require a successful invocation, a specific JSON output value, or a host check with evidence. A successful invocation alone does not establish arbitrary task semantics: choose concrete outputs or a backend host check. Mutating intentions/results invalidate host evidence. Final responses go through the deterministic basic gate: missing criteria are returned to the next call at most twice; the engine then reports `CompletedWithPending` rather than accepting the model's claim. This basic gate does not implement the later full project planning/verifier system.

Budgets bound steps, token consumption, wall time and tool calls; tool execution/preparation have timeouts. Near exhaustion/final available step, definitions remain present and tools are disabled. Reported token usage is preserved and retry usage accumulates. Compatible responses missing usage receive an explicitly marked byte-based estimate (`Usage.estimated`), not a fictitious measured zero. Estimates are heuristics, not tokenizer/billing verification. A cost ceiling is rejected when `ModelGateway::cost_is_known()` is false; the supplied gateway has no price calculator and does not claim measured cost. A priced host adapter can provide it. `update_budget` records a trusted host amendment; `resume` continues a paused run without creating a new operation.

`pause`/`cancel` record controls without waiting for the run lock; admitted atomic effects may finish, and subsequent batches are suppressed. This is step/batch-boundary control, not the later orchestration-wide cancellation tree. Explicit terminal signals/close stop processes. `directive` persists received/applied facts, replays an interrupted application exactly once, and appends context deltas. `record_check` attaches backend evidence. `load` returns the authoritative reconstructed snapshot. `with_stream_sink` enables streamed notifications; tools still wait for the complete confirmed response. `Observer` exposes non-mutating boundary notifications for verification.

## Journal and replay

The journal uses the existing main SQLite writer and durability settings. Its extension tables/triggers are initialized transactionally with a checked schema digest. Registered durable runs reject journal UPDATE/DELETE; event keys are immutable and duplicate payloads return their existing sequence. Run creation and sequence assignment are serialized by the writer. State transitions and the `runs` projection commit together. Large events use referenced blobs; consumers can page events without SQLite types in the engine contracts.

Events include `run_started`, `model_request` (full normalized request, hash and layer hashes), `model_response`, `tool_intent`, `tool_result`, directives, state changes and checkpoints. The complete model response commits before any requested effect. Each prepared intention commits before execution, with a stable run/step/call idempotency key and file pre/postconditions. Local preparations bind their canonical root and version; replay into another workspace is rejected. Results commit individually as parallel calls finish. Model-visible history is appended in the original call order, independent of completion order. Snapshots every 20 completed steps bound replay work; checkpoint and continuation state survive reopening.

| Unfinished operation | Reconciliation |
|---|---|
| Model request without complete response | Repeat the identical recorded request; another inference attempt may incur cost |
| Confirmed model response/result | Reuse it; do not repeat that completed operation |
| `pure` | Repeat the read |
| `idempotent` file operation | Recognize postimage as done; retry preimage; any other state is uncertain |
| `keyed` host capability | Resend the same key; the remote service must actually honor its contract |
| Arbitrary `non_idempotent` command | Use a reliable declared probe if available; otherwise block without rerunning |

Keyed external effects are an extension contract, exercised with a durable simulator; no external capability service is pretended to exist. Timed-out read-only calls become failed results. Interrupted retry-safe calls pause with their intention retained; genuine uncertainty blocks. A trusted consumer may call `resolve_uncertain` only with an observed result and reconciliation evidence, not a blind retry request. The engine never infers that an arbitrary shell command is safe to repeat. Exact project/host-declared test/build/lint command strings may be admitted as repeatable; prefix extensions are excluded.

Three identical unsuccessful tool steps pause the run. Three consecutive file fingerprint conflicts expose an escalation request and pause for host/parent intervention. Full progress/semantic loop analysis, the parent hierarchy and supervisory recovery are later capabilities.

## Context and model gateway

L0 holds core instructions, security rules and name-sorted tool definitions; L1 identity/catalog; L2 fixed scope inputs; L3 literal task inputs; L4 appended activity/deltas; L5 explicit time, budget, continuation and transient notices. The same input yields byte-identical requests and layer hashes. Time/counters never enter stable layers. Updates append L4 messages rather than editing earlier messages. Suggested skills are bounded to three; actual retrieval/skill selection comes from the host. Per-layer estimates reject oversized compilation rather than silently dropping mandatory inputs. Memory retrieval, masking, compaction and tokenizer calibration are outside this basic compiler.

`OpenAiGateway` reuses `excalibur-providers::ProviderClient` for authentication, HTTPS/loopback transport and secret redaction. It translates normalized messages/tool-result history, native function calls and strict textual JSON fallback. Arguments and structured output are validated against JSON Schema with external HTTP/file schema loading disabled; minor JSON repairs are bounded and schema-validation failures permit a single corrective retry. Role effort is fixed by `GatewayConfig`. The profile fingerprint pins provider/endpoint/model/role/effort/options across restart and excludes credential values, so credential rotation does not change it. Other configurable host gateways must override their identity accordingly.

Master streaming emits provisional text/tool deltas and a complete response; truncation/filtering is an error, never permission to run partial tool calls. Worker/expert paths deliver complete responses. The supplied normalization adapter is Chat Completions-compatible; existing other provider protocols remain in their prior library, without claiming equivalent normalized adapters for all of them.

## Files and execution pipeline

`LocalTools::new(db, absolute_root, ToolPolicy)` opens an explicit rooted filesystem capability. `Access::{Allow, Ask, Deny}` is enforced before requested effects and checked again where needed. Ask returns a precise approval-required result for the host; it does not grant permission. Defaults allow reads, require approval for writes and deny commands. Canonical paths, rooted operations, process-wide locks and durable write leases prevent traversal, symlink escape and conflicting agent writes. Shell commands require separate explicit authorization and are not confined by the file-reader capability. OS sandbox provisioning/security approvals belong to host integrations and the later security capability; no OS sandbox is claimed here.

- `read_file`: numbered UTF-8 windows (200 lines by default; files up to 64 MiB), full-content `fp`, durable run/path read set. Binary/vision interpretation depends on the separate external vision capability.
- `write_file`, `edit_file`: existing files require the recorded fingerprint. Preparation stores referenced pre/postimage blobs without changing the target. Atomic rename and directory synchronization precede completion metadata. Matching is exact, typographic-normalized, then trailing-whitespace-insensitive; ambiguous matches return line numbers. Conflicts preserve the user's content, return a compact diff and escalate on the third mismatch.
- `apply_patch`: Codex-style add/delete/update/move blocks and context hunks. A partial multi-file interruption reconciles each pre/postimage, without overwriting an unrelated state. This is recoverable multi-file application, not a cross-filesystem atomic transaction.
- `grep`, `glob`: bounded regex/file summaries and gitignore-aware traversal.
- `leave_continuation_note`: save/clear a brief note for later calls/restarts. `get_tool_output`: verified byte windows of full stored output; base64 preserves arbitrary byte boundaries, with text only when the window is valid UTF-8.

Arguments are validated before preparation, including minor JSON repair. Post-edit formatting is a host transformation of prepared bytes; diagnostics/evidence invalidation are explicit configured callbacks, exercised through backend integrations. Without them results say `not_configured`/`pending_host`, and a durable changed-file queue is retained. LSP/source indexing itself is not implemented by those hooks. File versions record pre/postimages and the call/run; owned write tasks finish their atomic publication/history even when the caller is cancelled.

Completed outputs have full blob references. For a live session, the reference contains exactly the observed prefix through `archived_through_cursor`; subsequent calls archive later prefixes and eventually the complete output. Previews retain head/tail, literal errors and critical session metadata. Terminal output is redacted before spooling/archiving using host-supplied secret values; secrets are never discovered automatically. `Database::read_blob_window` allocates at most 1 MiB of selected output plus a 64 KiB buffer/8 MiB decoder window and verifies the complete registered size/hash before returning. It costs O(total blob content) per window, not random access; large-output reads run outside the writer. Integrity errors retain typed storage incidents. No 16 MiB total-output restriction is imposed by that retrieval API.

## Terminal lifecycle

`exec` supports pipes and real `portable-pty` sessions; `exec_io` provides list, cursor reads, text/named keys, pattern-or-exit `wait_for`, signals and close. Yield defaults to 10 seconds within 250–30000 ms. Complete spools preserve output beyond the resident head/tail buffer (1 MiB). Streaming regex matching can find output across chunks and outside resident previews without loading the spool. Status tails are at most 20 lines/2 KiB.

Session ceilings are 16 per run and 64 per process across manager instances. Least-recently-used eviction protects eight recent sessions where possible and notifies the owner of a terminated command. `TerminalConfig` allows stricter limits, shell and redaction; `LocalTools::with_terminal` injects a configured manager. Environment detection supplies OS/architecture, available shells, package managers and common commands to stable scope input. Pipes use `PAGER=cat`, `GIT_PAGER=cat`, `TERM=dumb` and color suppression.

Unix commands use process groups; Linux adds parent-death handling and an EOF guardian to terminate the owned process group on daemon death; a real shell-and-child test verifies it. The spawning/monitor thread stays alive for the process lifetime. Cancellation sends TERM then KILL after two seconds. Windows has guarded Job Object/kill-on-close adaptation; runtime verification is listed separately. Shutdown stops owned sessions, joins completed monitors and explicitly releases their spool leases. Completed archived spools are acknowledged only after blob publication; retained descriptors support cursor reads after unlinking. Unclaimed diagnostic spools use leases and explicit age-bounded cleanup. Sessions from a dead manager are reported lost and arbitrary commands are not reconstructed/restarted automatically.

## Backend verification

| Requirement | Executable evidence |
|---|---|
| Complete task | `engine/tests/end_to_end.rs`: mock compatible HTTP → durable requests/responses → real write/read/terminal check → concrete acceptance outputs → completion; duplicate delivery does not repeat it |
| Effect reconciliation | `engine/tests/crash.rs`: actual SIGKILL at eight boundaries × four effect classes, model-response reuse, same keyed effect once, uncertain command blocked, trusted reconciliation; real file postimage and external-edit protection |
| Checkpoints/notes/budgets/gate | `engine/tests/engine.rs`: reopening, 20-step checkpoint, note persistence, explicit budget expansion, disabled tools, missing/stale evidence and bounded retries |
| Parallelism/controls | `engine/tests/scheduling.rs` and engine tests: deterministic barriers, ordered results, shared resource locks across engines/runs, exclusivity, held response + pause, received-only directive and profile mismatch |
| Journal | `journal/tests/journal.rs`: concurrent sequence assignment/operation deduplication, append-only enforcement, immutable keys and large-blob replay |
| Context/gateway | Context process fixtures and model HTTP/SSE mocks: byte identity, stable deltas, schema failures/repair/retry, history translation, usage estimates and complete streaming |
| File pipeline | `tools/tests/files.rs`: permissions, roots/symlinks, fp conflicts/three match levels, partial patches/moves, leases, preimages/history, literal errors/full blobs and configured hooks |
| Processes | `tools/tests/terminal.rs`: real pipes/PTY/keys, signals, timeout, parent death/descendants, global limits, spools, redaction and streaming pattern waits |
| Bounded full output | `store/tests/blob_window.rs`: 20 MiB stream, start/middle/end/EOF windows, corruption beyond requested bytes, typed incidents and registered-size checks |

Paths in the table are relative to `crates/`. From the repository root:

```sh
cargo fmt --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo build --workspace
cargo test -p excalibur-engine --test crash -- --nocapture
cargo test -p excalibur-engine --test end_to_end -- --nocapture
cargo test -p excalibur-tools -- --nocapture
```

Final run: 179 passing tests, zero failures; ignored subprocess helpers are executed by their active parent tests. Tests use synthetic data and provider/capability simulators; filesystem/process effects are real inside temporary directories. No credentials, inference services, GPU, deployment or paid benchmark are needed. No real model quality or real remote keyed-service guarantee is claimed.

Built/run evidence applies to this Ubuntu x86-64 VPS, Rust/Cargo 1.99.1. Process killing is not a physical power-loss test. The exact isolated `terminal.rs` module was additionally type-checked with `cargo check` for `x86_64-pc-windows-gnu` and `aarch64-apple-darwin` through a temporary manifest outside the product, without SQLite C. This checks adapter types, not the complete workspace build or Windows/macOS runtime. Full task/project planning, memory/skills, complete verifier, orchestrator, OS sandbox installation, LSP, compaction, agenda and supervisory continuous recovery remain their own scopes. The minimum engine does not represent them as implemented.

Sources: [Temporal event history](https://docs.temporal.io/workflow-execution/event), [DBOS architecture](https://docs.dbos.dev/architecture), [Restate durable concepts](https://docs.restate.dev/foundations/key-concepts), [OpenAI function calling](https://developers.openai.com/api/docs/guides/function-calling), [structured output](https://developers.openai.com/api/docs/guides/structured-outputs), [portable-pty](https://docs.rs/portable-pty/0.9.0/portable_pty/), [Linux parent-death semantics](https://man7.org/linux/man-pages/man2/PR_SET_PDEATHSIG.2const.html), [Windows Job Objects](https://learn.microsoft.com/en-us/windows/win32/procthread/job-objects).
