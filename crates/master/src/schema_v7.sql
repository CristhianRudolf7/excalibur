-- Fair bounded notice scans survive restarts without changing delivery receipts.
CREATE TABLE IF NOT EXISTS master_notice_group_cursors(
 principal_id TEXT PRIMARY KEY,due_at INTEGER NOT NULL,
 conversation_id TEXT NOT NULL,scope_id TEXT NOT NULL,
 round_due_at INTEGER NOT NULL,round_conversation_id TEXT NOT NULL,
 round_scope_id TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS master_notice_member_cursors(
 principal_id TEXT NOT NULL,conversation_id TEXT NOT NULL,scope_id TEXT NOT NULL,
 due_at INTEGER NOT NULL,notice_id TEXT NOT NULL,round_notice_id TEXT NOT NULL,
 PRIMARY KEY(principal_id,conversation_id,scope_id,due_at)
);
CREATE INDEX IF NOT EXISTS master_notice_pending_groups
 ON master_notice_outbox(flushed,due_at,conversation_id,scope_id,notice_id);
CREATE INDEX IF NOT EXISTS master_notice_pending_members
 ON master_notice_outbox(conversation_id,scope_id,due_at,flushed,notice_id);
