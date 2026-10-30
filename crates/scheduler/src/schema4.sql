CREATE TABLE IF NOT EXISTS scheduler_reminder_metadata(
 scope TEXT NOT NULL,id TEXT NOT NULL,generation INTEGER NOT NULL,
 body TEXT NOT NULL CHECK(json_valid(body)),PRIMARY KEY(scope,id,generation)
);
CREATE TABLE IF NOT EXISTS scheduler_notification_conditions(
 notification_id TEXT PRIMARY KEY,body TEXT NOT NULL CHECK(json_valid(body))
);
