# Agenda reminders user surface

The master's `reminders` fast tool and the authenticated `agenda.reminders` gateway RPC share one core (`excalibur_daemon::agenda_tools::AgendaTools`). Both route through the host `Agenda` API into the scheduler reminder domain (keyed replay, generation CAS, host bindings), and every successful mutation notifies the runtime wake hint (`excalibur_scheduler::mutation_signal`), so the automatic runtime follows without a restart.

## Actions

`create`, `list`, `snooze`, `complete`, `delete` and `change_timezone`. `create` and `snooze` accept exactly one timing: `at` (UTC Unix seconds), `every` (first plus seconds), `local_at` (civil wall time with an IANA zone defaulting to the zone recorded with the accepted source message) or `cron` (numeric five- or six-field expression in the user's zone). `change_timezone` re-expresses every active civil (`LocalAt`/`Cron`) reminder of the namespace in a new IANA zone as a new generation each: the same wall time or cron expression is recomputed, UTC timings are left alone, and a reminder with a due or recorded-but-undelivered fire is skipped rather than losing that fire.

## Authority

Authority never comes from model or client JSON. Every call is bound to the host-authenticated principal plus the conversation, original scope and accepted source message of the request. The orchestrator revalidates that source and the master's scope ceiling inside each writer (`bind_host_notification`). The reminder namespace is sealed from principal, scope and conversation, so another scope or conversation cannot see or mutate these reminders. Calls without an accepted source are refused; there is no fallback namespace. Forged `scope`/`principal` fields in model arguments are refused by the approved tool schema, fabricated or foreign source ids are refused as not authorized, and unauthenticated RPC callers receive `401` before reaching the core.

## Idempotency and validation

Each call carries a durable operation key. The master derives it from the tool call (`master:{conversation}:{source}:{tool_call_id}`); the RPC derives it from the client-supplied `operation`. An exact retry with the same key and arguments returns the original snapshot; the same key with different arguments is refused (`reminder operation key reused with different arguments`). Mutations of an existing reminder need its current generation; a stale generation is refused. Reminder ids default deterministically from the operation key when the caller omits them, so a retried create is an exact replay.

Writer-side text validation runs after the keyed-replay check (AR-10): an exact retry returns its snapshot even if the redactor learned a matching secret since, while a new create whose text the notice writer would refuse is rejected rather than silently redacted, so the stored reminder keeps the user's wording. Secret literals are rejected with a fixed message that never echoes the text.

## Wake on mutation

Successful create, snooze, complete, delete, zone-change and batch zone-change operations notify the process-local mutation hint keyed by database path. The automatic runtime waits on that hint alongside its monotonic re-check, so a mutation wakes an idle runtime without moving the clock. The hint is only an optimization; durable timers remain authoritative.

## Limits and pending work

This surface does not dispatch tasks from reminders, and does not implement `after`/`window` triggers, repetition when the user does not answer, mobile OS notifications or external channel adapters. Fire retention pruning is not implemented. It does not complete H9 or sections 21.1-21.2.

## Verification

```sh
cargo test -p excalibur-daemon --test agenda_tools
cargo test -p excalibur-daemon --test agenda_runtime --test agenda_runtime_isolation --test agenda_dispatch --test host_notifications
cargo test -p excalibur-master
cargo test -p excalibur-scheduler --test reminders
```

`agenda_tools` drives the real daemon host, Orchestrator and Master with scripted/offline local models over three tests: a master-tool create that wakes the idle runtime without moving the clock, an RPC matrix covering exact keyed replay, key reuse with different arguments, generation CAS, scope/source/conversation denial, unauthenticated refusal, secret-literal rejection and the AR-10 replay-after-redactor case, plus completion and deletion; and a civil time-zone change where the runtime delivers nothing at the old instant and the recomputed civil and cron reminders at the new one. Fixtures are local and synthetic; no network or paid APIs are used.
