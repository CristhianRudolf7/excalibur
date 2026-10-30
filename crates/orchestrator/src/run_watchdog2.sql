ALTER TABLE orchestration_run_watchdog ADD COLUMN action TEXT;
ALTER TABLE orchestration_run_watchdog ADD COLUMN decision TEXT;
ALTER TABLE orchestration_run_watchdog ADD COLUMN reason TEXT;
ALTER TABLE orchestration_run_watchdog ADD COLUMN aborted INTEGER NOT NULL DEFAULT 0;
CREATE INDEX IF NOT EXISTS orchestration_run_watchdog_open ON orchestration_run_watchdog(notified,decision);
