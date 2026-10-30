CREATE TABLE IF NOT EXISTS scheduler_timers(
    scope TEXT NOT NULL,
    id TEXT NOT NULL,
    definition TEXT NOT NULL CHECK(json_valid(definition)),
    next_at INTEGER,
    PRIMARY KEY(scope,id)
);
CREATE INDEX IF NOT EXISTS scheduler_due ON scheduler_timers(scope,next_at);
CREATE TABLE IF NOT EXISTS scheduler_fires(
    scope TEXT NOT NULL,
    id TEXT NOT NULL,
    timer_id TEXT NOT NULL,
    scheduled_at INTEGER NOT NULL,
    payload TEXT NOT NULL CHECK(json_valid(payload)),
    delivered INTEGER NOT NULL DEFAULT 0 CHECK(delivered IN (0,1)),
    PRIMARY KEY(scope,id)
);
