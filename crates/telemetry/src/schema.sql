CREATE TABLE IF NOT EXISTS telemetry_schema(version INTEGER PRIMARY KEY,checksum TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS telemetry_runs(run_id TEXT PRIMARY KEY,metadata TEXT NOT NULL CHECK(json_valid(metadata)),snapshot TEXT CHECK(snapshot IS NULL OR json_valid(snapshot)),finalized INTEGER NOT NULL DEFAULT 0,ended_at_ms INTEGER);
CREATE TABLE IF NOT EXISTS telemetry_offsets(cursor INTEGER PRIMARY KEY AUTOINCREMENT,run_id TEXT NOT NULL,source TEXT NOT NULL,sequence INTEGER NOT NULL,event_id TEXT NOT NULL UNIQUE REFERENCES events(id),event_key TEXT NOT NULL,timestamp_ms INTEGER NOT NULL,recovered INTEGER NOT NULL,payload_hash TEXT NOT NULL,UNIQUE(run_id,source,sequence));
CREATE INDEX IF NOT EXISTS telemetry_run_sequence ON telemetry_offsets(run_id,source,sequence);
CREATE TABLE IF NOT EXISTS telemetry_exports(run_id TEXT PRIMARY KEY,last_cursor INTEGER NOT NULL,manifest TEXT NOT NULL CHECK(json_valid(manifest)));
CREATE TRIGGER IF NOT EXISTS telemetry_events_no_update BEFORE UPDATE ON events BEGIN SELECT RAISE(ABORT,'telemetry events are append-only'); END;
CREATE TRIGGER IF NOT EXISTS telemetry_events_no_delete BEFORE DELETE ON events BEGIN SELECT RAISE(ABORT,'telemetry events are append-only'); END;
CREATE TABLE IF NOT EXISTS telemetry_settings(name TEXT PRIMARY KEY,value TEXT NOT NULL);

CREATE TRIGGER IF NOT EXISTS telemetry_offsets_no_update BEFORE UPDATE ON telemetry_offsets BEGIN SELECT RAISE(ABORT,'telemetry offsets are append-only'); END;
CREATE TRIGGER IF NOT EXISTS telemetry_offsets_no_delete BEFORE DELETE ON telemetry_offsets BEGIN SELECT RAISE(ABORT,'telemetry offsets are append-only'); END;
