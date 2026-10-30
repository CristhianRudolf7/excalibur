# Durable run-stall watchdog

This document describes the stall watchdog for durable runs (architecture 17.6 "Heartbeats and supervision" and the stuck-run watchdog of 22.5). It covers the shared engine heartbeat and the orchestrator watchdog only. It is not the operating-system `sd_notify` watchdog, and it is not a RAM, disk, WAL, provider-error or cache-drift watchdog. Those remain separate requirements.

## Heartbeat

At the start of every driver admission and at every step boundary of an active run, the engine renews `runs.heartbeat_at`. This column already existed in the base schema and is a projection, not a journal fact, so replay, revisions and event projection are unchanged. Only an execution that is authoritatively `running` is renewed, so a stale driver cannot refresh a paused, blocked or terminal run.

The watchdog reference time is the latest of that heartbeat, a confirmed driver progress fact (`run_started`, `model_request`, `model_dispatch`, `model_response`, `model_failure`, `model_interrupted`, `tool_intent`, `tool_result`, `step_completed`, `checkpoint`, `compaction`, `directive_applied`, `wake_event`) and a confirmed transition to `running`. Directives received, agent messages and host recovery notes are external writes and do not count, so they cannot hide a hung driver.

## Detection

`Orchestrator::watchdog_runs(at)` takes an explicit UTC clock, and `Orchestrator::tick` calls it with the host clock. It only examines runs whose driver task is live in this orchestrator and whose state is `running`. Waiting states, including `waiting(tool)`, children, user and approval waits, are never examined. A run is stalled when `at - reference >= 600` seconds.

A registered pending tool, meaning a confirmed `tool_intent` without a result, counts as a declared long wait until its intention time plus the run's tool timeout. The ten-minute clock restarts at that deadline.

Before the interruption, a host intent row is written to `orchestration_run_watchdog`, keyed by run and observed journal revision, in a transaction that rechecks the revision and heartbeat. The engine then performs an exact-revision CAS through `Engine::interrupt_stalled_revision`, without taking the run lock a hung driver may hold. A newer journal fact, a user control or a terminal transition makes the CAS fail. Pending effects are evaluated at that same revision:

- with an unresolved non-pure intention, the run becomes `blocked` with reason `watchdog: unresolved effect requires reconciliation`;
- otherwise it becomes `paused` with reason `watchdog_stalled`.

The host may also interrupt a managed tool on a best-effort basis.

## Interruption, reconciliation and decision

A live driver normally observes the durable control and stops by itself. A hung model call records its interruption and conservative accounting. If the exact driver task observed at detection is still alive 5 seconds later on the watchdog clock, it is aborted; a newer driver of the same agent is never aborted. The abort is recorded durably before cancelling, and the reaper ignores a cancelled driver only for a run with such an open aborted incident. Any other cancellation still fails the run. If it still has not stopped 60 seconds later (for example, it is stuck in synchronous code that cannot be cancelled), the incident is reported without an automatic resume.

Once the old driver is gone, the decision reuses the 17.3 startup assessment (`assess_resume`, shared with `recover_startup`). An automatic resume requires all of the following:

- host and frozen run autonomy at least autonomous;
- an interruption shorter than six hours. This is the time since detection and, when the incident predates the current daemon boot, also the boot downtime measured by startup recovery. The rule is shared with startup recovery through `assess_resume`;
- sufficient remaining budget;
- no unresolved non-pure intention;
- no pending approval;
- no recorded file edits;
- no model request without a durable response;
- no outstanding child reservation;
- no active startup recovery or safe mode;
- no quarantine;
- no earlier watchdog incident that already resumed this run.

An eligible run is moved from `paused(watchdog_stalled)` to `ready` with the keyed CAS `watchdog-resume:<revision>`, and only then receives a `recovery_note` for its L5 context. The ordinary scheduler then admits it again, and the engine reconciles its pending pure tools as in 17.2. Any other case keeps the pause, with the blockers recorded as the reason.

A blocked run keeps its block until the existing explicit `resolve_uncertain` reconciliation. The watchdog never executes or retries an effect. If a later user control or engine transition owns the run, the incident is recorded as `superseded`.

## Alerts and idempotency

Every incident with an interruption produces one alert in the ordinary `orchestration_notifications` table. Its content passes the same secret-literal and 64 KiB checks as `notify`; reasons are bounded to 1,024 characters, and reasons owned by other controls are never copied (a superseded incident only names the new state). The alert is committed in the same transaction as the incident's `notified` receipt. The master delivery pipeline routes it to the agent's original conversation. The payload has `kind: "run_watchdog"`, the run, revision, action, decision, reason, scope and conversation. A blocked or paused outcome is `urgent`, and a resumed outcome is `normal`. The ordinary stop notice that the existing reaper emits for a cooperatively stopped driver remains separate.

Passes in one process are serialized. Passes from other processes or reopened databases are idempotent through:

- the keyed intent row;
- the exact-revision CAS keys `watchdog-stalled:<n>` and `watchdog-blocked:<n>`;
- the keyed resume `watchdog-resume:<n>`;
- `UPDATE ... WHERE decision IS NULL`;
- deterministic notification IDs.

A failure while handling one run is reported and does not stop other runs, the rest of the tick or admission. A failed heartbeat renewal never fails the driver. A process that stops after the CAS resumes the incident from the confirmed journal key on the next pass. Startup recovery preserves `paused(watchdog_stalled)` as an existing control, and the watchdog settles it with the downtime rule above. An intent row whose CAS never applied is recorded as `not_applied` and produces no alert. Alerts require both the host row and the confirmed state-change key at revision `n + 1`.

Schema: v1 (`run_watchdog.sql`, from the unverified checkpoint `d203232`) is kept unchanged. Additive v2 (`run_watchdog2.sql`) adds `action`, `decision`, `reason` and `aborted` once and is checksum-verified. The v1 cursor table is not used.

## Verification

```sh
cargo test -p excalibur-orchestrator --test run_watchdog
cargo test -p excalibur-orchestrator --test startup_recovery
cargo test -p excalibur-engine --test crash --test end_to_end
```

The suite drives real engine drivers with local mock models and tools, a real temporary SQLite database and an injected watchdog clock. No test waits ten real minutes or calls a paid API. It covers:

- the 599/600-second boundary and heartbeat renewal;
- at most one automatic resume per run (a second stall stays paused);
- a restart between the interruption and its settlement, with short downtime (resumed), downtime over six hours (paused) and supervised host autonomy (paused);
- an intent row whose interruption was never confirmed (`not_applied`, no alert);
- cooperative interruption of a hung model call, with an urgent alert;
- concurrent passes, and reopening the database without a duplicate interruption or alert;
- abort of a driver hung outside any control point, followed by a single resume that completes from the recorded model response;
- the exemption for a registered pending tool until its timeout;
- blocking an uncertain non-idempotent effect without executing it again;
- user control and a stale revision winning over the watchdog;
- the additive, checksum-verified schema.

## Limits

- The heartbeat is renewed per step, as the architecture specifies. A healthy model call that streams for more than ten minutes, or a run waiting on the process-wide tool batch lock behind another run's long tool, can therefore be declared stalled. The interrupted model call then reserves its unknown charge.
- Detection uses the wall clock (SQLite `unixepoch()` and the host clock). A system suspend/resume or a forward NTP jump of ten minutes or more can make every running run look stalled at once.
- "Concurrent passes" in the tests are serialized by the in-process lock. Separate processes rely on the durable keys and CAS described above; no multi-process test exists.
- Detection covers orchestrator-dispatched runs whose driver task lives in the current process. Conversational master turns and runs that are `running` but only queued for admission are not watched.
- A driver blocked in synchronous code cannot be cancelled by Tokio. It is reported but remains in the process.
- Interrupting a tool relies on the existing tool lifecycle and host adapter. Orphaned operating-system processes are not inventoried here.
- Automatic resume is conservative: runs with recorded file edits stay paused for explicit recovery. An interrupted model call reserves its unknown provider charge conservatively. Without a declared call bound, this can exhaust the token budget and leave the run paused, as in the hung-model test.
- Alerts go to the master and are not external channels. Health watchdogs other than this one remain unimplemented, and H9 is not complete.
