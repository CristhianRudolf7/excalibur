# Agent orchestration

`excalibur-orchestrator` is the principal-local shared backend for agent trees, execution admission, project resolution, durable communication and notification routing. Protocol servers and the CLI call this library.

Open it with the same principal `Database` and `Arc<Engine>`. Attach an `OrchestratorTools` adapter and `OrchestrationGate` before serving requests, install a host `RunAuthority`, and select the work runtime with `with_runtime`. `tick` reaps completed tasks, delivers mail, enforces directive deadlines and admits eligible work. `shutdown` pauses active work and awaits its durable stopping boundary; it does not discard an in-flight effect future.

## Delegation and execution

Host-registered `CustomProfile` records contain a bounded name, icon and identity/style literal over an existing built-in base role. Registration is immutable and idempotent; a changed profile receives a new ID. `Agent.profile` retains the enforced base role and `profile_id` identifies the customization. The identity enters frozen L1 context without conversation history. Profiles cannot supply extra permissions or tools.

A single persisted master has depth zero. It creates level-one coordinators; coordinators may create at most four active level-two workers by default. Workers cannot delegate, both in their model catalog and in the execution API. Profiles constrain tool visibility and preparation; read-only profiles cannot receive writable roots. The host binds the actual security token before dispatch, independently of the catalog. Child capability roots, network, secrets, scopes and tool-call budget must be subsets of the parent. The engine reserves the child's budget in its durable journal and settles terminal consumption.

`Delegation` includes an operation ID, goal, deliverable, scope, task, origin conversation, inputs, mandatory literal instructions, suggested skills, profile, token and run specification. Repeating an operation returns its original agent; different content is rejected. A child receives the parent's selected mandatory instruction literals. Conversation/reasoning history is omitted unless `fork_context` is explicitly selected.

When a host installs `with_tasks(TaskStore)`, root and child runs bind to real scoped task records. A missing delegated task receives an immutable request card plus an implicit requirement and final-response criterion; an existing root request card is preserved. Agents and run ownership are linked into the foundational tables. The optional task completion gate can therefore use existing planning/evidence contracts.

Ready and interrupted running executions are admitted through a bounded queue, with project round robin within priority. Waiting parents do not retain an admission slot. The model-callable `subagent` tool accepts a compact goal/deliverable/profile contract, derives a bounded capability and budget subset, and durably freezes that derivation for replay. It handles delegation, child controls and `wait`; `agent_message` exposes reports, questions, answers, directives and mailbox acknowledgment. Waiting is applied at the next safe step boundary after the tool result is confirmed. Child mail wakes a waiting parent without polling a model. `OrchestrationGate` prevents finalization while any child is nonterminal or its budget is unsettled and can retain an existing task/evidence completion gate.

## Control and communication

`control_keyed` persists an immutable control intent, serializes commands and retains a replayable result. Pause cascades; resume affects the target and only descendants marked by that pause. Interrupt affects only the target. Cancellation requires confirmation and runs from leaves to root; terminal state cannot be reopened. Redirect carries the literal instruction and resumes its target. Effects retain the engine's cancellation class and reconciliation contract; the returned pending list identifies effects still reaching their stopping boundary.

A model-callable `agent_message` question is blocking by default (`payload.blocking: false` opts out). Its durable user wait is admitted at the next step boundary only after the exact successful tool receipt; an unconfirmed communication effect is reconciled first. A parent answer clears the matching user wait (`payload.question_id`, or the oldest outstanding question when omitted) and wakes the execution. A `blocked` report similarly stops at the safe boundary with its reason. A directive or explicit redirect clears these pending control requests. Re-entering a waiting or blocked execution makes no additional model call.

Mail is persisted before waking recipients. Message IDs and sender/client IDs deduplicate retries; acknowledgments are durable. Agents communicate with their parent or direct children, not siblings. Reports and answers enter the engine as context data without changing the mandatory-instruction revision. A normal directive leaves a user-paused execution paused. An interrupt directive stops the current execution at its supported boundary. Automatic pauses and resumes use journal revision guards; a later user pause cannot be overwritten while waiting for an effect boundary. Applying a directive to the journal is distinct from acknowledgment: a later model request must have included its context before automatic acknowledgment. Scope corrections propagate deterministically down every affected branch. Unacknowledged directives escalate after two steps or 120 running seconds when not waiting inside a long tool.

`status` derives state, step, queue position, outstanding tool intents, live host output, last confirmed progress, narrative report, plan and usage from backend records. A report reminder becomes due after 15 steps or ten minutes without an update. It cannot expose a model's internal generation or a tool's unreported internal progress.

The trusted daemon calls `rebind_master_at_startup` before dispatch when reprovisioning startup capabilities. It preserves the master identity and every existing descendant token; `register_master` remains strict. Removed startup scopes receive no new runtime grant. Durable registered roots remain privacy boundaries even when their runtime is not provisioned. A rejected host binding emits an urgent durable stop notice and a parent completion message before scheduling continues.

## Projects and resources

Projects retain aliases, description, canonical roots, lifecycle state, VCS and host settings. Path resolution returns every containing project and selects execution project, then mentioned project, then deepest root. Scope reads require an explicit nonempty authorized set.

The host feeds RSS once per second to `resource_sample`. Above the configured threshold, admission stops, idle services are unloaded, lower-priority runs are paused, and severe pressure may request termination of the largest tool through the host hook. Admission resumes after 60 seconds continuously below 80 percent. Resource-owned pauses resume one at a time by priority; a user pause takes ownership and is never resumed by the resource watcher. Native RSS measurement and OS process-priority adapters belong to the daemon host.

## Notifications

Notifications retain the origin conversation, class, stable ID and acknowledgment. Reaping classifies user/approval waits, approval pauses, budget stops, failures and blockers as urgent. User/resource pauses and ordinary terminal reports are normal; waits for children or tools emit no stop notice. Urgent notifications are immediately eligible; normal ones wait for their five-second grouping window; progress is opt-in except for the N3 task default. Omitted/null `progress` inherits that default; explicit false disables it. Valid task policy fields override the conversation policy. Quiet hours use an explicit UTC offset, with urgent bypass configurable. Disconnected origins can additionally route to a configured preferred channel; urgent messages can link into another recently active conversation. `pending_deliveries` persists channel attempts. Adapters must send idempotently using the notification ID and call `confirm_delivery` only after acceptance. One read acknowledgment suppresses remaining channel delivery. External channel transport is supplied by the host; this crate never claims an unconfirmed network delivery.

## Verification

```
cargo test -p excalibur-orchestrator
cargo clippy -p excalibur-orchestrator --all-targets -- -D warnings
```

Tests run the real SQLite writer and durable engine with deterministic synthetic model responses. They exercise hierarchy, capability and budget rejection, operation replay, scoped projects and symlink roots, cascades and pause ownership, child waiting/wakeup, model-callable delegation, completion guards, durable mail and notification policies. No model service or credential is required. See the paired daemon documentation for native runtime and network verification.
