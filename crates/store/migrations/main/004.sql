ALTER TABLE blobs ADD COLUMN content_generation TEXT NOT NULL DEFAULT '';
CREATE TABLE blob_integrity(hash TEXT PRIMARY KEY, state TEXT NOT NULL CHECK(state IN ('ok','missing','corrupt','metadata_missing','unavailable')), checked_at INTEGER NOT NULL, detail TEXT NOT NULL DEFAULT '');
CREATE TABLE blob_incidents(id TEXT PRIMARY KEY, hash TEXT NOT NULL, state TEXT NOT NULL, detected_at INTEGER NOT NULL, resolved_at INTEGER);
CREATE TABLE storage_recoveries(id TEXT PRIMARY KEY, original_path TEXT NOT NULL, snapshot_path TEXT, snapshot_at INTEGER, recovered_at INTEGER NOT NULL, salvage_path TEXT, salvaged_rows INTEGER NOT NULL, unreadable_tables TEXT NOT NULL, data_loss_possible INTEGER NOT NULL);
CREATE INDEX blob_incidents_open ON blob_incidents(hash,resolved_at);
