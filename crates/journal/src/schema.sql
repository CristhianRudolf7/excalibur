CREATE TABLE IF NOT EXISTS durable_operations(operation_id TEXT PRIMARY KEY NOT NULL,run_id TEXT UNIQUE NOT NULL REFERENCES runs(id),seed_hash TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS durable_schema(version INTEGER PRIMARY KEY,checksum TEXT NOT NULL);
CREATE TRIGGER IF NOT EXISTS durable_journal_no_update BEFORE UPDATE ON journal WHEN EXISTS(SELECT 1 FROM durable_operations WHERE run_id=OLD.run_id) BEGIN SELECT RAISE(ABORT,'durable journal is append-only'); END;
CREATE TRIGGER IF NOT EXISTS durable_journal_no_delete BEFORE DELETE ON journal WHEN EXISTS(SELECT 1 FROM durable_operations WHERE run_id=OLD.run_id) BEGIN SELECT RAISE(ABORT,'durable journal is append-only'); END;
