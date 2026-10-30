CREATE TABLE IF NOT EXISTS scheduler_reminders(
 scope TEXT NOT NULL, id TEXT NOT NULL, generation INTEGER NOT NULL,
 record TEXT NOT NULL CHECK(json_valid(record)), PRIMARY KEY(scope,id)
);
CREATE TABLE IF NOT EXISTS scheduler_reminder_timers(
 scope TEXT NOT NULL, timer_id TEXT NOT NULL, reminder_id TEXT NOT NULL,
 generation INTEGER NOT NULL, PRIMARY KEY(scope,timer_id)
);
CREATE TABLE IF NOT EXISTS scheduler_reminder_receipts(
 scope TEXT NOT NULL, operation_key TEXT NOT NULL, request TEXT NOT NULL,
 response TEXT NOT NULL CHECK(json_valid(response)), PRIMARY KEY(scope,operation_key)
);
CREATE TABLE IF NOT EXISTS scheduler_cancelled_fires(
 scope TEXT NOT NULL, fire_id TEXT NOT NULL, operation_key TEXT NOT NULL,
 PRIMARY KEY(scope,fire_id)
);
