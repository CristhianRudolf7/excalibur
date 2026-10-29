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

`pause`/`cancel` record controls without waiting for the run lock. Automatic transitions compare the authoritative durable state atomically: they cannot overwrite a user pause or cancellation. Cancelled/completed runs are terminal and cannot resume or accept uncertainty resolution. Admitted atomic effects may finish and confirm their observed result, including failure; interruption retains the unresolved intention without replacing the user control. New intentions are admitted only while the run is still running in the same writer transaction, including when control arrives during preparation. This is step/batch-boundary control, not the later orchestration-wide cancellation tree. Explicit terminal signals/close stop processes. `directive` persists received/applied facts, replays an interrupted application exactly once, and appends context deltas. `record_check` attaches backend evidence. `load` returns the authoritative reconstructed snapshot. `with_stream_sink` enables streamed notifications; tools still wait for the complete confirmed response. `Observer` exposes non-mutating boundary notifications for verification.

A security runtime can pause for approvals originating from background effects. The driver checks pending/blocked approvals before inference, after a confirmed response and after tool batches. `ToolRuntime::finalization_guard` orders approval creation with terminal completion; retain the guard through the durable compare-and-set. `SecuredTools` supplies the shared per-database/run guard, while runtimes without approval support use the default ready guard. Resumption reuses an already confirmed model response.

## Journal and replay

The journal uses the existing main SQLite writer and durability settings. Its extension tables/triggers are initialized transactionally with a checked schema digest. Registered durable runs reject journal UPDATE/DELETE; event keys are immutable and duplicate payloads return their existing sequence. Run creation and sequence assignment are serialized by the writer. State transitions and the `runs` projection commit together. `Journal::set_state_if` compares the latest journal state inside the writer transaction; it repairs a stale projection and never reactivates a terminal execution. Retransmitting an older transition key validates its immutable payload without rolling the projection back. `append_when_running` conditionally admits an event against that same authoritative state; unconditional result confirmation remains available for already admitted effects. Large events use referenced blobs; consumers can page events without SQLite types in the engine contracts.

Events include `run_started`, `model_request` (full normalized request, hash and layer hashes), `model_response`, `tool_intent`, `tool_result`, directives, state changes and checkpoints. The complete model response commits before any requested effect. Each prepared intention commits before execution, with a stable run/step/call idempotency key and file pre/postconditions. Local preparations bind their canonical root and version; replay into another workspace is rejected. Results commit individually as parallel calls finish. Model-visible history is appended in the original call order, independent of completion order. Snapshots every 20 completed steps bound replay work; checkpoint and continuation state survive reopening.

`ToolRuntime::scheduling` supplies content-independent canonical resource keys through `ToolScheduling`. Discovery must not read contents, bind fingerprints or persist preimages. Adapters without independent metadata use the safe isolated default. The driver groups nonconflicting calls, preserving model order between conflicting batches. It prepares each call only after previous conflicting batches finish and while holding that call's resource locks through intention, effect and result confirmation. A shared/exclusive batch gate preserves command isolation; per-call locks let independent resources progress across runs. Actual prepared resources must fit the declared plan before admission. Later batches have no intention yet: recovery reuses the confirmed response and prepares them against current state. Any already admitted unresolved intention, including entries produced by another compatible writer, still follows its effect reconciliation contract; absence of a result never proves that it did not execute.

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

`LocalTools::new(db, absolute_root, ToolPolicy)` opens an explicit rooted filesystem capability. `Access::{Allow, Ask, Deny}` is enforced before requested effects and checked again where needed. Ask returns a precise approval-required result for the host; it does not grant permission. Defaults allow reads, require approval for writes and deny commands. Canonical paths, rooted operations, process-wide locks and durable write leases prevent traversal, symlink escape and conflicting agent writes. Shell commands require separate explicit authorization and are not confined by the file-reader capability. Authorization and OS isolation use the shared [mandatory security contracts](security.md). Hosts configure `SecuredTools`, the vault redactor and the terminal sandbox explicitly.

- `read_file`: numbered UTF-8 windows (200 lines by default; files up to 64 MiB), full-content `fp`, durable run/path read set. Binary/vision interpretation depends on the separate external vision capability.
- `write_file`, `edit_file`: existing files require the recorded fingerprint. Preparation stores referenced pre/postimage blobs without changing the target. Atomic rename and directory synchronization precede completion metadata. Matching is exact, typographic-normalized, then trailing-whitespace-insensitive; ambiguous matches return line numbers. Edits without an explicit `fp` use the latest confirmed run/path readset, so consecutive edits in one response observe the preceding postimage. An explicit `fp` remains strict and is never silently rebased. Conflicts at preparation or execution preserve the current content and return the same structured report: expected/current fingerprints and existence, compact diff, affected line ranges, persistent mismatch count and escalation on the third mismatch. Execution compares against the admitted preimage even when a later read refreshed the readset. Successful reads/writes reset the counter; confirmed deletion clears the readset. The additive checksummed tool schema version 2 stores absence conflicts and carries forward existing mismatch counts while retaining version 1.
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
| Parallelism/controls | `engine/tests/scheduling.rs` and engine tests: deterministic barriers, ordered results, shared resource locks across engines/runs, exclusivity, held response + pause, received-only directive and profile mismatch; `control_races.rs`: pause/cancel during success, failure, panic, timeout, final gate and intent admission |
| Ordered preparation/recovery | `engine/tests/batch_preparation.rs`: real consecutive edits with canonical path aliases and confirmed pre/postimage order; `batch_crash.rs`: SIGKILL after first file effect, no intention for the later command, recovery executes it once |
| Journal | `journal/tests/journal.rs`: concurrent sequence assignment/operation deduplication, append-only enforcement, immutable keys and large-blob replay; `state_transitions.rs`: stale keys/projections, terminal immutability, competing transitions and conditional admission |
| Context/gateway | Context process fixtures and model HTTP/SSE mocks: byte identity, stable deltas, schema failures/repair/retry, history translation, usage estimates and complete streaming |
| File pipeline | `tools/tests/files.rs`: permissions, roots/symlinks, fp conflicts/three match levels, partial patches/moves, leases, preimages/history, literal errors/full blobs and configured hooks; `execution_conflicts.rs`: prepare/execute races, persistent escalation, appeared/deleted files, strict explicit fingerprints and schema expansion |
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
cargo test -p excalibur-engine --test control_races --test batch_preparation --test batch_crash
cargo test -p excalibur-journal --test state_transitions
cargo test -p excalibur-tools -- --nocapture
```

Final run: 278 passing tests, zero failures; eight ignored subprocess helpers are executed by their active parent tests. Tests use synthetic data and provider/capability simulators; filesystem/process effects are real inside temporary directories. No credentials, inference services, GPU, deployment or paid benchmark are needed. No real model quality or real remote keyed-service guarantee is claimed.

Built/run evidence applies to this Ubuntu x86-64 VPS, Rust/Cargo 1.99.1. Process killing is not a physical power-loss test. The current terminal sandbox integration was built and exercised on Linux only; Windows/macOS runtime and complete workspace compilation on those targets have not been verified. Full task/project planning, memory/skills, complete verifier, orchestrator, LSP, compaction, agenda and supervisory continuous recovery remain their own scopes. The minimum engine does not represent them as implemented.

Sources: [Temporal event history](https://docs.temporal.io/workflow-execution/event), [DBOS architecture](https://docs.dbos.dev/architecture), [Restate durable concepts](https://docs.restate.dev/foundations/key-concepts), [OpenAI function calling](https://developers.openai.com/api/docs/guides/function-calling), [structured output](https://developers.openai.com/api/docs/guides/structured-outputs), [portable-pty](https://docs.rs/portable-pty/0.9.0/portable_pty/), [Linux parent-death semantics](https://man7.org/linux/man-pages/man2/PR_SET_PDEATHSIG.2const.html), [Windows Job Objects](https://learn.microsoft.com/en-us/windows/win32/procthread/job-objects).
