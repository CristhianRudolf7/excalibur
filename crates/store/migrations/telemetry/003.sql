-- Independent schema history remains in lockstep; telemetry has no retrieval indexes.
CREATE TABLE storage_metadata(key TEXT PRIMARY KEY NOT NULL,value TEXT NOT NULL);
