-- Retention expiry watermark: journal events at or below `through_sequence` were
-- deliberately expired and must not be re-ingested by reconciliation.
CREATE TABLE IF NOT EXISTS telemetry_expired(run_id TEXT NOT NULL,source TEXT NOT NULL,through_sequence INTEGER NOT NULL,expired_at INTEGER NOT NULL,PRIMARY KEY(run_id,source));
