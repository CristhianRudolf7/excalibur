# Daemon, master and subagents

The daemon hosts the existing shared Rust engine. `excalibur-master` owns durable intake and bounded conversational turns; `excalibur-orchestrator` owns agent trees, mailboxes, admission, control and notifications; `excalibur-gateway` adapts those services to authenticated JSON-RPC. `excalibur-daemon` supplies the host lifecycle and the CLI stays a process adapter.

## Hosting and development

```sh
cargo build --workspace
# Supply a private, random bearer token of at least 32 characters through the environment.
excalibur daemon start --project /absolute/project --data-dir /absolute/private-data --listen 127.0.0.1:3791
excalibur daemon stop --data-dir /absolute/private-data --listen 127.0.0.1:3791
```

Both commands require `EXCALIBUR_DAEMON_TOKEN`. The start command runs in the foreground; a service supervisor can own its process. The principal database's exclusive ownership prevents a second owner. Relative data paths resolve once against the starting directory. SIGINT, SIGTERM and authenticated `daemon.stop` initiate shutdown. No installer, operating-system service registration or deployment is implied.

Without a model the daemon still receives durable messages and serves deterministic status/control; natural-language work remains pending with an availability notice. Configure an explicit model with `--model` or `OPENAI_MODEL` and its key through `OPENAI_API_KEY`; `--base-url` selects a compatible provider endpoint. The shared host also accepts a fallback gateway and explicit `MasterConfig`. The master uses the L0–L5 compiler with a 32,768-token ceiling that includes a ten-percent safety margin, 256 KiB and at most sixteen active turns. History is selected by scope before reading text; durable originals remain complete. A source that cannot fit produces `context_blocked`, without calling the provider or skipping it through a later message. The host can invoke `retry_context` after adjusting configuration, or stop that turn. Compilation and tokenization run in the background pool. Secrets are registered with the live redactor and are not included in client events. External model calls are unnecessary for the deterministic backend suite.

Explicit authenticated `/approve [id]` and `/deny [id]` commands use the same durable once-scope decision as `approval.respond`, without a model call. The ID must belong to a run originating in that conversation; omission selects only one unique pending, unexpired request. Ambiguity asks for the exact ID instead of guessing. Selection and its receipt are tied to the operation, so resending a message does not repeat the decision. Only an approval-waiting execution wakes, and user pauses remain paused. No approval-granting tool is exposed to the model.

The standalone adapter binds loopback. Remote hosting requires a trusted authenticated TLS reverse proxy; the shared router accepts host-provided explicit allowed Origins. Browser tokens must be sent in the Authorization header, never in a URL. See [gateway contracts](../../crates/gateway/README.md) for actual methods, blob authority and event cursors. Future memory, skill and automation methods are explicitly unavailable rather than simulated.

## Scheduling and authority

The host owns a two-thread interactive runtime, a separate work runtime and a bounded background CPU pool. Engine runs are Tokio tasks; tools retain their existing sandbox/process-group boundary. A panic at a turn, run or CPU-job boundary becomes a task error. A daemon background-service error initiates controlled shutdown instead of leaving the service silently inactive.

Each provider scheduler has four lanes and continuously replenished request/token buckets. P0 has one reserved concurrency slot and ten percent of the configured minute request/token quota; lower lanes cannot consume that reserve. FIFO order applies within each lane and a lower lane yields to queued higher lanes. Maintenance also yields while the host marks the user active. Reservations use the adapter's declared call bound when available, otherwise a serialized-request estimate. Host ceilings are reduced by numeric `x-ratelimit-limit-requests` and `x-ratelimit-limit-tokens` response headers observed by the compatible adapter; invalid values are ignored. No quota is inferred when headers are absent, and traffic outside this installation is not accounted for. Header names follow the [official OpenAI rate-limit documentation](https://developers.openai.com/api/docs/guides/rate-limits). Priority wrappers preserve observed provider metrics and cache/replay metadata.

The Linux host reads daemon and descendant RSS from `/proc` each second. The shared resource controller stops admission, requests idle-resource unloading, pauses low-priority runs for resources, and can stop a tool as the final action. Admission resumes after sixty seconds below eighty percent of the configured limit; user pauses remain paused. Native RSS adapters on other platforms are explicitly unavailable in this adapter. Linux tools use nice 10 and best-effort I/O priority 7; an existing delegated cgroup remains host-owned. The tests distinguish observed priorities from unverified cgroup deployment.

`HostOptions` supplies canonical root authority and registered projects. Each project root must lie inside that authority. `DaemonActions` narrows a project's worker paths and data scopes; `Security::register_run` checks every actual child token against its parent before dispatch. The master role exposes only bounded coordination/status/question/time operations. File and terminal work becomes a level-one run. Workers at level two cannot create children, even by calling a hidden tool directly. Profile catalogs are frozen in each run's durable context; runtime authorization remains independent of tool visibility.

The `general` scope excludes registered project files, including in the verifier. Select the initial project using `scope: "project:default"` in `conversation.send` or `/project default`. Registering project metadata does not widen the permission ceiling: executable scopes and authorized roots are provisioned in `HostOptions`. Registered custom profiles retain a mandatory base role; names, icons and identity text cannot add permissions.

The host can configure `GitOptions` with a real identity and `IntegrationChecks`; no defaults are invented. The parent creates the registered worktree and a child delegation carries `worktree-owner:<owner>` in `inputs`. The host checks parent task, scope, canonical root and unique ownership, fixes the effective token and routes files/processes to that tree. Child checkpoint and Git queue operations use their specific authority; integration remains the parent's operation. A2 can require an actual Git approval; A3 needs explicit `full_acknowledged`. The CLI does not invent an identity or integration checks. `ScopedPlanning` reuses `EpisodeTools` transactions and receipts; each run's completion gate and independent verifier use its effective root.

## Durable coordination

Intake persists before acknowledging and deduplicates by client identity and `client_msg_id`; a conflicting reuse is rejected. A client's outbox may resend an unacknowledged message and resume events using the last confirmed cursor. Conversations run independently; each conversation serializes its turns, limits them to eight model steps and injects queued messages at safe boundaries. Early unshown generation can restart with the combined messages. Provider failure retains the inbox and keeps status/control available; availability retries are bounded and an explicit health signal can reset the circuit.

Structured delegation carries goal, deliverable, scope, paths through the capability token, input references, literal instructions, skills, budget and optional explicit context fork. A child normally receives no parent reasoning/history. Every task retains a durable request card with literal source messages and their IDs, interpretation, instructions, assumptions and an initial observable criterion. Child budget reservations are enforced by the existing durable engine. `task_db` and `task.batch` share a durable outbox that propagates only confirmed corrections to affected branches; task revisions invalidate their evidence. Waiting parents release execution slots, allowing children of several trees to advance under the global running limit. Model-callable coordination tools and the completion gate use the same orchestrator as the API.

Mailbox messages have durable IDs, at-least-once delivery and explicit acknowledgments. Directives persist before delivery, propagate only to affected branches/scopes and retain their instruction revision. The engine and tool cancellation contracts determine when interrupting is safe. The status projection reads durable state, outstanding model/tool intents, host live output, usage, queue position and narrative reports; it cannot reveal a model's private generation or progress inside a silent tool.

Control commands are serialized and durable. Pause cascades with an operation-specific `paused_by`; resume restores only those descendants. Interrupt preserves unrelated descendants. Automatic pause/resume checks ownership reason and journal revision with CAS to preserve user pauses. Cancellation is terminal and requires authenticated confirmation; a model-generated boolean is not user confirmation in the master adapter. Redirect carries the new instruction and can resume the addressed paused run. Reversibility continues to use file history and shadow snapshots; a control command does not undo an already-sent external effect.

Notifications retain origin, class, delivery attempts and acknowledgment. Urgent notices bypass normal grouping; normal notices wait five seconds and honor quiet hours. Origin conversations always retain their notice, urgent notices can link another active conversation, and preferred external-channel attempts remain pending until a host channel adapter confirms an idempotent send. The standalone host publishes conversation notices; it does not claim a Telegram/mobile channel is installed. Acknowledgment in one channel suppresses subsequent attempts elsewhere.

## Verification

```sh
cargo test -p excalibur-master
cargo test -p excalibur-orchestrator
cargo test -p excalibur-gateway
cargo test -p excalibur-daemon
cargo test -p excalibur-cli --test daemon
cargo test -p excalibur-tools --test process_priority
cargo fmt --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo build --workspace
```

Tests use synthetic model responses, isolated temporary databases, native local sockets/processes and explicit time samples or paused Tokio time. Availability, steering, control and notification fixtures are backend tests. Platform execution evidence and any inherited pending items belong to the separate delivery report; a successful Linux test does not certify another operating system.

Worker journal progress shares the durable conversation event cursor. Entries expose only run, agent, scope, journal sequence and event kind; detailed state stays behind authenticated status queries. Replayed projections deduplicate by run and journal sequence. Each worker uses the same telemetry projection as embedded execution and exports per-run ATIF trajectories and metrics under `daemon-logs/runs/<run_id>`. Master attempts retain actual primary/fallback metadata, known cost/cache usage and pending unknowns.


## Explicit project actions

A master delegation can name `scope`; `scopes` records an explicitly transversal task. The primary scope must occur in that set. An override requires an unambiguous project mention in the frozen literal source messages and must fit the host token. The durable authorization receipt retains the original source IDs and text, so a later selector change cannot retarget replay. Separate project jobs receive separate tasks. A transversal job keeps one primary task scope and the exact granted project scopes and roots in its capability; it cannot acquire an unmentioned project. Git worktrees remain project-specific.

Master `task_db` operations may explicitly name different authorized scopes. The adapter validates every group before the first effect, executes one idempotent transaction per scope and returns labeled `scopes` results. This is not a cross-scope atomic transaction; retry uses the same group receipts. Unknown or ambiguous project mentions require clarification rather than a guessed write.

For write authorization, short ASCII aliases (one or two characters) use the explicit `project:<id>` spelling; an ordinary article is not a project grant.


Transversal workers bind a positive filesystem boundary to the exact registered root union. File reads/edits, searches, shadow captures and confined processes share that boundary; the independent verifier keeps it read-only. Ungranted nested projects and future files outside the union remain inaccessible. Historical captures with paths outside the current boundary cannot be listed or diffed through that run. `ToolRuntime::filesystem_confinement` is a trusted host attestation, absent by default: generic runtimes still require permission for the whole workspace. Every attested root is checked against the current token; model arguments cannot create an attestation.


Informational reports addressed to the master enter the same notification routing as other notices. `NotificationPolicy.progress` may be omitted/null (enabled by default for N3 tasks), true (enabled) or false (disabled). A valid task `notification_policy` overrides the conversation fields; malformed overrides preserve the conversation policy. Quiet hours and acknowledgments still apply. Required human answers, approval pauses and budget stops are urgent; ordinary child/tool waits do not generate repeated notices.


At a stopped-host startup, `DaemonHost::open` uses `Orchestrator::rebind_master_at_startup` to apply the trusted root ceiling from `HostOptions`, retaining the master ID and every existing descendant's frozen token. Ordinary `register_master` remains strict. Factories and grants use the current provisioned projects; all durable registered roots, including omitted projects, remain privacy denials for General and overlapping scopes. A run whose scope is no longer provisioned blocks before inference or tool effects and emits an urgent notice and parent message. The backend does not perform hot root or scope grants.
