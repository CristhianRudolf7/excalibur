# Excalibur daemon host

Reusable hosting for the shared engine, conversational master, orchestrator and authenticated gateway. `RuntimeSet` separates interactive work, engine tasks and bounded background CPU jobs. `LaneScheduler` reserves provider concurrency/quota for the master. `DaemonHost::open` binds host roots, effective execution security, durable tasks and completion gates; `close` drains components and closes storage.

`HostOptions::project` supplies a single initial project. Embedded hosts can supply several projects within the canonical root ceiling, an optional bounded intake decider and an already-authorized fast-tool catalog. These are trusted host settings; JSON-RPC cannot replace the ceiling. `control_actions` exposes the shared coordination adapter; `recover_task_directives` drains its durable correction outbox.

The synchronous CLI adapter implements foreground `daemon start` and authenticated `daemon stop`; domain behavior stays in shared crates. See [Daemon, master and subagents](../../docs/development/daemon.md) for startup, environment variables, actual contracts, native adapter limits and verification commands. No production inference or external-channel deployment is needed by the tests.
