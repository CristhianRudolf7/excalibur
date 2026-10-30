-- Durable host-only effect provenance; ordinary notice JSON cannot create a guard.
CREATE TABLE IF NOT EXISTS master_notice_guards(
 notice_id TEXT PRIMARY KEY,notification_id TEXT NOT NULL
);
