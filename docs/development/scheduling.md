# Durable timer foundation

`excalibur-scheduler` provides scoped UTC one-shot and fixed-interval timers over the shared storage writer. It atomically records a fire in a delivery outbox and advances the timer. The fire ID encodes scope, timer ID and intended UTC time; retries and competing ticks reuse that ID. Timer IDs are immutable: recreating the exact definition succeeds, while changing it requires a new ID.

Recovery ticks apply `RunAll`, `RunOnceNow` or `Skip`. A bounded `RunAll` tick leaves its remaining backlog for later ticks. `RunOnceNow` coalesces missed intervals into the latest intended time, and `Skip` advances to the next future time. A fire due exactly at recovery time is delivered. Regular ticks record every due occurrence, subject to the page limit.

Pending deliveries survive storage shutdown and reopening. Acknowledgment is scoped and idempotent. Consumers must use the fire ID as an external idempotency key: the outbox alone cannot guarantee exactly one external effect if a consumer crashes between sending and acknowledging. The authenticated host supplies scope; schedule payloads cannot widen it.

Verification: `cargo test -p excalibur-scheduler` covers recovery policies, bounded backlog, reopening, concurrent ticks, definition conflicts, scope isolation and timestamp overflow. Fixtures are local and deterministic.

This is preparation for H9, with no daemon dispatcher connected yet. IANA local-time/cron calculation, timezone-change notices, dependency/activity windows, reminder lifecycle and channel delivery, startup safe mode, quarantine, retention and signed updates remain pending. H9 is not complete.
