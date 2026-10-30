# Automatic Agenda runtime

`excalibur_daemon::agenda::AgendaRuntime` is the daemon-owned task that makes reminders fire without an explicit host tick. `excalibur daemon start` spawns it under the existing service supervisor on the work runtime, next to the orchestration and housekeeping services. It adds no RPC, model tool or second delivery path: every admission goes through the generation-guarded `notify_host_guarded` path described in [Explicit reminder dispatch core](agenda-dispatch.md), and the existing housekeeping service delivers the queued notices to the local Master.

## Lifecycle

- **Start.** The first pass after process start applies the missed-fire policy of section 17.4 instead of an ordinary tick. Reminder timers use `RunOnceNow`, so an `every` reminder that missed several intervals while the daemon was down produces one fire at the latest intended time, and the notice keeps that scheduled timestamp. The catch-up mode persists across passes until no reminder timer is overdue, so bounded pages cannot turn the remainder into a replay of every interval. A wake more than 60 seconds after its deadline (suspend, stalled host) is treated the same way.
- **Index.** Each pass rebuilds its view from durable state: a keyset-paginated index of every reminder namespace with its earliest enabled `next_at`, whether it has unadmitted fires, and the newest creator metadata. The runtime holds no binding of its own. It opens a namespace only when that persisted creator binding derives exactly to the sealed namespace (`reminders::namespace_of` with `source=None`); otherwise the namespace is not opened, ticked or admitted, and its rows remain for audit. Dispatch additionally checks each fire's own generation binding against the namespace before using it, so metadata cannot redirect a fire to another principal, scope or conversation, and a missing binding is skipped rather than replaced by a fallback.
- **Sleep.** The runtime sleeps until `min(next_fire_utc)` over all reminder namespaces, bounded by a 60-second idle re-check. The re-check covers authority that becomes valid again, lost process-local hints and monotonic clocks that do not advance during suspend. The clock is injectable (`AgendaClock`); the daemon uses `SystemClock` and tests use `ManualClock`.
- **Wake on mutation.** `excalibur_scheduler::mutation_signal` is a process-local hint keyed by database path. Successful reminder create, snooze, complete and delete operations, `Scheduler::create` and `Scheduler::change_timezone` notify it. A stored permit covers a mutation that lands while a pass is running. The hint is only an optimization; durable timers remain authoritative.
- **Safe mode and startup recovery.** While the recovery ledger reports `recovering` or safe mode, passes still record due fires under the missed-fire policy but admit nothing; the notification writer also rejects guarded admission in that state. The runtime re-checks every 5 seconds, so a manual safe-mode exit or the end of the safe window releases the recorded fires without a restart.
- **Shutdown.** The daemon stop signal cancels the wait immediately. The task result is part of the orderly-shutdown decision that records the clean-exit receipt. A pass cancelled midway is safe to repeat.

## Idempotency

Fires are keyed by scope, timer and scheduled time; timer advancement is a compare-and-set on the stored definition and cursor; notice keys hash the typed fire ID; scheduler acknowledgement happens only after the durable notification receipt exists. Concurrent runtimes, an explicit `Agenda::tick_once` and a restart after SIGKILL therefore record and queue each fire once. Passes inside one runtime are serialized.

## Limits and pending work

This closes the automatic wake, startup recomputation and safe-mode gating for reminder notices only. It does not implement Agenda RPC or model tools, task dispatch from reminders or schedules, `after`/`window` triggers, repetition when the user does not answer, mobile OS scheduled notifications or external channel adapters. Generic scheduler timers outside reminder namespaces are not dispatched by this runtime, and reminder timers have no time-zone change API yet. It does not complete H9 or sections 21.1–21.2.

## Verification

```sh
cargo test -p excalibur-daemon --test agenda_runtime
cargo test -p excalibur-cli --test agenda_runtime
cargo test -p excalibur-scheduler
cargo test -p excalibur-daemon --test agenda_dispatch --test host_notifications --test service_supervision
```

The daemon test runs the runtime against a real host, Orchestrator and Master with an offline provider and a manual clock: startup `RunOnceNow` over ten missed intervals, a wake at the earliest timer, a mutation wake without clock movement, creator-source preservation across snooze, safe-mode recording without admission followed by release after a manual exit, clean stop, database reopening, two racing runtimes and a forged namespace that stays inert. The CLI test seeds reminders, starts the real foreground `excalibur daemon start` process, observes the missed fire before the future reminder and the future reminder at its time through authenticated HTTP, then SIGKILLs and restarts the daemon and checks that nothing is duplicated before a clean stop. Fixtures are local and synthetic.
