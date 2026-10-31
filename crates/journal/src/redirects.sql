-- Purge redaction gives a redacted journal payload blob a new content address; old
-- references held by context sessions resolve through this per-run mapping.
CREATE TABLE IF NOT EXISTS journal_payload_redirects(run_id TEXT NOT NULL,old_hash TEXT NOT NULL,new_hash TEXT NOT NULL,PRIMARY KEY(run_id,old_hash));
