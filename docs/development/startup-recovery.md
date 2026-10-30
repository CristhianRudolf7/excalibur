# Daemon startup and crash-loop admission

The shared host owns a durable boot ledger in the principal database. `DaemonHost::open` opens the additive checksum-verified recovery schema and records a new incarnation before initializing host services. Storage already performs its integrity check at database open. A prior boot without a clean-exit receipt is recorded as an unclean restart; provider errors and ordinary provider retries do not create boot or crash entries.

`DaemonHost::close` drains the master/orchestrator and native resources before recording a clean boundary. The foreground command supplies its service outcome: a failed supervisor or cleanup does not receive a clean receipt. The marker is the last durable application write before database shutdown, rather than proof that every operating-system teardown succeeded.

Three unclean restart detections within 600 seconds enter safe mode until 1,800 seconds after the latest detected crash. Another crash during that period extends it. Crash times are detection times at the next boot, because SIGKILL cannot write its own exit record. Interactive housekeeping updates the boot heartbeat independently of work tasks. A clock rollback prevents automatic recovery admission.

## Actual admission and control

`Orchestrator::tick` keeps reaping and servicing mailboxes and controls, but does not dispatch workers while startup recovery or safe mode is active. A second durable check runs before runtime binding and records the boot/run association before spawning the engine task. Quarantine is checked at that same admission boundary. Conversation intake, master turns, authenticated status, pause/cancel and daemon shutdown remain available. New work received in safe mode remains durably ready in the queue.

A crashed boot retains the runs actually admitted by that incarnation. On the next boot, their authoritative run states and confirmed `step_completed` entries determine which step was active. Three crashes with the same active run and step quarantine that run. Safe-mode exit does not clear quarantine. No sampling of a provider error or merely queued run establishes crash attribution.

Authenticated host RPC methods:

- `daemon.recovery.status`: boot, recovery phase, safe-mode deadline, recent crash count, downtime, quarantine IDs and the first 100 current-boot decisions. Complete decisions and aggregate counts remain durable in SQLite.
- `daemon.recovery.exit_safe_mode`, with `operation`: exits the global mode. The same operation cannot exit a later mode generation on replay.
- `daemon.recovery.release_quarantine`, with `operation` and `run`: releases that quarantine and leaves the blocked run paused for recovery. An explicit ordinary run-resume control is still needed. These methods are not model tools or capability grants.

Safe-mode expiry and manual exit only release pauses owned by safe mode. The host rechecks pending approvals, remaining budget, recorded edits and the six-hour interruption bound before automatically resuming an eligible pause. User pauses retain ownership. A run that no longer qualifies remains paused for recovery. Admission order remains the existing priority/weighted project schedule.

## Conservative startup decisions

Automatic recovery requires both the current host autonomy and the run's immutable security autonomy to be autonomous or higher, downtime below six hours, more than one remaining step, positive remaining tokens and more than 30 remaining wall seconds. Unknown frozen autonomy is supervised. User/parent/resource controls and approval waits are preserved. Unresolved non-pure tool intentions block before inference or tool dispatch. Outstanding child reservations, recorded file edits and model requests without a durable response pause for explicit recovery rather than being blindly repeated.

The existing engine still owns effect reconciliation. This block deliberately does not claim complete startup world reconciliation: replaying durable external-capability tasks, comprehensive filesystem comparison after recorded edits, terminal-session inventories, and receipts for every possible in-flight provider dispatch remain further recovery work. In particular, a recorded file edit prevents automatic startup admission until the host/user reconciles it. An approval is neither granted nor consumed by startup.

Every nonterminal registered agent receives a durable recovery decision and host recovery fact. The shared engine replays the latest fact into L5 continuation context without overwriting the worker's continuation note or turning it into a user instruction. Existing controls are not woken by that fact. Startup enqueues a real urgent notification for each newly blocked decision, binding boot, run, reason, original conversation and scope. Separate `recovery_summary` notifications report counts and bounded run/decision/reason details for the conversation's active scope, once per conversation and boot. The existing master delivery pipeline handles urgency and quiet-hour policy; cross-conversation urgent routing exposes only a link, not the decision details. Additive schema v2 commits each receipt with its notification, so reopening and retrying publication cannot lose or duplicate the warning. Complete decision rows remain in storage; summaries never substitute for individual urgent warnings.

## Backend verification and remaining H9 scope

```sh
cargo test -p excalibur-orchestrator --test startup_recovery
cargo test -p excalibur-cli --test startup_recovery
cargo test -p excalibur-daemon --test service_supervision
```

The orchestrator suite uses real temporary SQLite databases, deterministic clock samples and mock model/tools. It covers the boot threshold, exact-step quarantine and release, the real dispatch gate, expiry/manual controls, replay, autonomy/six-hour policy, preserved approval facts, unresolved tool intentions and scoped urgent notification deduplication after a real database reopen. The Linux CLI fixture starts the real foreground daemon, kills it with SIGKILL three times, queries authenticated HTTP, exercises master conversation intake during safe mode, verifies one durable summary, exits safe mode and checks a clean restart. It uses no provider model configuration or paid API.

This is a connected startup/admission foundation, not H9 completion. Long-running project sessions, the ten-minute stuck-run watchdog, full world reconciliation, scheduled-fire dispatch, retention/purge, installers and operating-system supervisors, signed updates/drain/rollback and native platform execution remain separate unfinished requirements. No OS service or update path is implied by the foreground boot ledger.
