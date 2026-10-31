-- Retention and purge state. Rows hold identifiers, phases, counters and error codes;
-- subject content is held only transiently in retention_purge_needles and deleted when
-- the purge finalizes.
CREATE TABLE IF NOT EXISTS retention_purges(
  operation_key TEXT PRIMARY KEY NOT NULL CHECK(length(operation_key) BETWEEN 1 AND 256),
  tombstone_id TEXT NOT NULL UNIQUE,
  principal_id TEXT NOT NULL,
  scope_id TEXT NOT NULL,
  subject_kind TEXT NOT NULL CHECK(subject_kind IN ('conversation','document','memory')),
  subject_id TEXT NOT NULL,
  reason TEXT NOT NULL CHECK(reason IN ('user_request','retention_policy','administrative')),
  phase TEXT NOT NULL CHECK(phase IN ('planned','swept','base_deleted','telemetry_swept','finalized','blobs_released','completed')),
  requested_at INTEGER NOT NULL,
  completed_at INTEGER,
  members TEXT NOT NULL DEFAULT '[]' CHECK(json_valid(members)),
  objects TEXT NOT NULL DEFAULT '[]' CHECK(json_valid(objects)),
  runs TEXT NOT NULL DEFAULT '[]' CHECK(json_valid(runs)),
  blob_hashes TEXT NOT NULL DEFAULT '[]' CHECK(json_valid(blob_hashes)),
  counts TEXT NOT NULL DEFAULT '{}' CHECK(json_valid(counts)),
  hygiene TEXT CHECK(hygiene IS NULL OR json_valid(hygiene)),
  telemetry_covered INTEGER NOT NULL DEFAULT 0 CHECK(telemetry_covered IN (0,1)),
  notice_code TEXT,
  last_error TEXT,
  UNIQUE(principal_id,subject_kind,subject_id)
);
CREATE TABLE IF NOT EXISTS retention_purge_needles(
  operation_key TEXT NOT NULL REFERENCES retention_purges(operation_key),
  ordinal INTEGER NOT NULL,
  needle TEXT NOT NULL,
  PRIMARY KEY(operation_key,ordinal)
);
-- Journal events redacted by a purge, with the telemetry offset hash expected before
-- and after, so re-sealing never hides an earlier mismatch.
CREATE TABLE IF NOT EXISTS retention_purge_events(
  operation_key TEXT NOT NULL REFERENCES retention_purges(operation_key),
  run_id TEXT NOT NULL,
  sequence INTEGER NOT NULL,
  old_offset_hash TEXT,
  new_offset_hash TEXT,
  PRIMARY KEY(operation_key,run_id,sequence)
);
CREATE TABLE IF NOT EXISTS retention_purge_flags(
  operation_key TEXT NOT NULL REFERENCES retention_purges(operation_key),
  code TEXT NOT NULL,
  location TEXT NOT NULL,
  PRIMARY KEY(operation_key,code,location)
);
-- Explicit provenance: runs that a host or engine declares as derived from a subject.
CREATE TABLE IF NOT EXISTS retention_subject_runs(
  principal_id TEXT NOT NULL,
  subject_kind TEXT NOT NULL CHECK(subject_kind IN ('conversation','document','memory')),
  subject_id TEXT NOT NULL,
  run_id TEXT NOT NULL,
  linked_at INTEGER NOT NULL,
  PRIMARY KEY(principal_id,subject_kind,subject_id,run_id)
);
CREATE TABLE IF NOT EXISTS retention_tombstones(
  id TEXT PRIMARY KEY NOT NULL,
  principal_id TEXT NOT NULL,
  scope_id TEXT NOT NULL,
  subject_kind TEXT NOT NULL CHECK(subject_kind IN ('conversation','document','memory')),
  subject_id TEXT NOT NULL,
  reason TEXT NOT NULL,
  purged_at INTEGER NOT NULL,
  UNIQUE(principal_id,subject_kind,subject_id)
);
CREATE TABLE IF NOT EXISTS retention_holds(
  run_id TEXT PRIMARY KEY NOT NULL,
  reason TEXT NOT NULL CHECK(reason IN ('evaluation','replay','skill_candidate','skill_evaluation')),
  held_at INTEGER NOT NULL
);
