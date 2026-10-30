CREATE TABLE IF NOT EXISTS orchestration_run_watchdog_schema(version INTEGER PRIMARY KEY,checksum TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS orchestration_run_watchdog(run_id TEXT NOT NULL REFERENCES runs(id),sequence INTEGER NOT NULL,agent_id TEXT NOT NULL REFERENCES orchestration_agents(id),observed_at INTEGER NOT NULL,notified INTEGER NOT NULL DEFAULT 0,PRIMARY KEY(run_id,sequence));
CREATE TABLE IF NOT EXISTS orchestration_run_watchdog_cursor(id INTEGER PRIMARY KEY CHECK(id=1),agent TEXT NOT NULL);
INSERT OR IGNORE INTO orchestration_run_watchdog_cursor VALUES(1,'');
