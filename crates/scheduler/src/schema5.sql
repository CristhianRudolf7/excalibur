CREATE TABLE IF NOT EXISTS scheduler_fire_dead_letters(
 scope TEXT NOT NULL,fire_id TEXT NOT NULL,reason TEXT NOT NULL,recorded_at INTEGER NOT NULL,
 PRIMARY KEY(scope,fire_id)
);
CREATE INDEX IF NOT EXISTS scheduler_fires_undelivered ON scheduler_fires(scope,delivered,scheduled_at,id);
CREATE INDEX IF NOT EXISTS scheduler_fires_timer ON scheduler_fires(scope,timer_id,delivered);
