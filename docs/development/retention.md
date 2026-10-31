# Retention and purge on request

`excalibur-retention` is the shared-core service for architecture §4.7: user-requested purge of a conversation, a document or a memory, plus the time-based retention classes. It is a library. No CLI subcommand, HTTP route or confirmation dialog (§20.5) is added here; `MemoryStore::purge_memory`, which the gateway's existing `memory.purge` call uses, now delegates to this service.

## Purge as a keyed durable operation

`PurgeService::open(principal, main, telemetry)` installs a checksummed additive schema (`retention_schema` version 1) in the principal database. No existing table is altered. `purge(PurgeRequest)` takes an operation key, scope, subject kind and ID, reason code and request time. It runs these phases, and each one commits its own progress in `retention_purges`:

1. **planned.** The subject is authorized, and its member IDs, needles, scope set, per-table rowid watermarks and blob hashes are recorded. Messages of the conversation count as members, as do chunks of every document version and IDs that participants derive.
2. **main_swept.** Derived copies are redacted in the principal database.
3. **journal_blobs_swept.** Journal payloads larger than 64 KiB that live in blobs are rewritten.
4. **telemetry_swept.** The telemetry database is swept, offsets are re-sealed and affected run artifacts are exported again.
5. **base_deleted.** One writer transaction does several things. It sweeps rows written after the watermarks, calls the participant deletions, deletes vectors, edges and blob references, removes the base rows together with every row that cannot exist without them, rebuilds the affected FTS5 indexes, writes the tombstone and deletes the needles.
6. **blobs_released.** Unreferenced blob files and their metadata are removed right away, without the 24-hour garbage-collection grace period. Content that another owner still references is kept and counted as `blobs_shared`.
7. **completed.** A `wal_checkpoint(TRUNCATE)` runs on both databases, then the hygiene and backup-notice code are recorded.

Replaying the same key, or the same subject under another key, returns the stored `PurgeReceipt`. Reusing a key for a different subject fails with `KeyConflict`. `resume_pending()` finishes interrupted operations, for example at host startup. Each phase is idempotent: rows that were already redacted no longer match, blob release accepts files that are already gone, and the base phase checks its phase inside the same transaction.

Content needed to find copies (the needles) is stored only in `retention_purge_needles` until the base rows are deleted, in the same transaction. A crash before that point resumes with the same needles. A crash after it no longer needs them.

## What is removed and where

- **Base rows.** Conversations, their messages, documents with their versions and chunks, and memories. Deletion follows the real foreign keys from `pragma_foreign_key_list`. Rows that cannot exist without the subject are deleted, for example `client_messages` and master inbox/turn tables. Nullable references from unrelated rows are cleared instead. For example, a bystander message's `reply_to` becomes NULL, and the tombstone explains the broken reference.
- **Derived copies.** Every regular table in the principal and telemetry databases is scanned, except the following:
  - independent subject content: other messages, memories, documents, chunks and identity documents;
  - structural metadata (blob, schema, migration, vector-index and short-code tables);
  - the hash-chained `audit` table;
  - virtual tables and their shadow tables.

  This covers the journal, notifications, approvals, memory keys and queues, telemetry events, run metadata and any table that other crates add. Matching uses exact subject texts of at least 12 characters, sentences or lines of at least 24 characters, and their JSON-escaped spellings. JSON values keep their structure: only string values (and keys) change, and the replacement is `[purged]`. If a redacted value collides with a unique key, a row-specific marker is used instead.
- **Journal.** Payloads are redacted in place. Sequence, kind, idempotency key, state and row count do not change. The append-only triggers are dropped and recreated only inside the purge transaction. Hashes kept over journal events are re-sealed to the redacted payload, so `Telemetry::reconcile` and task re-observation stay idempotent: `telemetry_offsets.payload_hash` uses the new `excalibur_telemetry::journal_offset_hash`, and `task_observed_events.payload_hash` is updated when that table exists.
- **Telemetry.** Rows in `telemetry.db` are swept the same way, and every affected run is exported again, so the `runs/<id>/` artifacts are regenerated from the redacted data.
- **Indexes and vectors.** External-content FTS5 indexes over modified tables are rebuilt. This covers messages, memories, chunks and memory keys, including the trigram indexes, so deleted terms leave the segments and not just their delete markers. `vec0` rows whose object ID is a member ID or `<prefix>:<member id>` are deleted, and sqlite-vec 0.1.9 zeroes the deleted vector bytes in its chunk storage. The memory participant also removes `memory_vector_objects` and `memory_index_queue` entries.
- **Memory participant.** `excalibur_memory::MemoryPurgeParticipant` handles memory-owned rows. It removes extracted keys (and their embeddings) for purged messages and memories, admissions, transitions, entity links, relation needs and curation history, closes open questions, and removes chunk retrieval records. A purge is refused with `ParticipantMissing("memory")` when the memory schema is installed but this participant is not registered.

## Tombstone and backup notice

`retention_tombstones` holds only `id, principal_id, scope_id, subject_kind, subject_id, reason, purged_at`. `reason` is a code (`user_request`, `retention_policy`, `administrative`), never free text. `PurgeService::tombstone(scope, kind, id)` answers only within the caller's scope.

Every receipt carries `BackupNotice { code: "backups_retain_until_rotation", max_backup_age_days: 28 }`. Its message says that existing backups may still contain the data until they rotate out. The weekly copies of §4.6 are kept four weeks, and daily and pre-update copies rotate sooner. Backups are not rewritten.

## Scope and principal

The service is opened for one principal, which matches the physical one-file-per-principal layout. A subject of another scope or principal answers `NotFound`, the same as a missing one. Sweeps only touch rows whose `scope_id` is NULL or one of the subject's own scopes, and whose `principal_id` is NULL or the service principal. Scoped copies that belong to other scopes are never changed.

## Retention classes (§4.7)

`RetentionSweeper::open(main, telemetry, RetentionPolicy::default(), clock)` takes an injected clock in Unix seconds. The defaults are a 30-day full journal, 30-day tool outputs, 90-day detailed telemetry and 7-day shadow snapshots. `run_once()` does the following:

- It deletes detailed telemetry older than 90 days, which means events with their offsets, traces and metrics. It keeps `metric_daily` and exports the touched runs again.
- It reports terminal runs whose journal is older than 30 days as `journal_compaction_due`. Runs held through `hold_run(run, Evaluation | Replay | SkillCandidate | SkillEvaluation)` are reported as `journal_held` instead.
- It counts journal payload blobs of due runs that no evidence, document, memory or message references (`tool_output_blobs_due`).

## Hygiene and honest limits

- Every purge write runs with `PRAGMA secure_delete=ON`, so freed cells and pages are zeroed. `VACUUM` is **not** applied. It would renumber implicit rowids that the external-content FTS5 indexes depend on, and it needs exclusive access. A `wal_checkpoint(TRUNCATE)` resets both WAL files when no reader holds a snapshot, and the receipt reports `main_wal_truncated` and `telemetry_wal_truncated`.
- The guarantee covers ordinary queries over every table, every shadow table and the blob files. Free space that was left inside pages *before* the purge, by earlier updates made without secure_delete, can still hold old bytes. The purge test prints the raw database and WAL bytes it finds instead of asserting on them. In the recorded run they held no marker either, but that is a result of these fixtures and is not guaranteed. File-system blocks, SSD remapping and earlier copies left by atomic file replacement (such as old export files) are outside SQLite's reach.
- Backups keep the content until they rotate. A test proves that the pre-purge backup still contains it.
- Subject texts shorter than 12 characters are deleted from their base rows but not searched for in derived stores. This avoids rewriting short structural values. Paraphrases, summaries and model-derived facts that are not verbatim copies are not detected. Independent subjects (such as another memory that states the same fact) are not cascaded.
- Unscoped derived rows (most journal rows and telemetry events carry no scope) are matched by content. A long sentence that is shared verbatim with another scope's subject is therefore also redacted in those unscoped copies. Scoped rows of other scopes are never touched.
- The hash-chained `audit` table is not rewritten. It stores events that are already redacted.
- `durable_operations.seed_hash`, file-version blobs, evidence output blobs and other non-journal blobs are not rewritten. A retransmission of an original journal payload under a redacted key fails as a key conflict, and re-registering original telemetry run metadata is rejected. Both fail closed.
- `MemoryStore::purge_memory` opens the service without telemetry (`telemetry_covered=false`). Hosts that hold the telemetry handle must use `PurgeService` directly to cover telemetry.
- Rows written into telemetry between its sweep and the base deletion are not swept again. Copies that running agents still hold in memory can be written after the purge.
- Journal compaction, the 30-day tool-output deletion and shadow-snapshot expiry are reported, not executed. The compaction format and its owner are not implemented here.
- Verification covers Linux only. macOS, Windows and mobile behaviour of secure_delete, file removal and checkpoints has not been exercised.

## Verification

`cargo test -p excalibur-retention` runs the following:

- **`purge`.** It plants unique markers in base tables, FTS5 indexes (including trigram), `vec0` probe vectors, attachment, document and journal blobs, inline journal events, notifications, memory keys, a foreign-key child table, telemetry events and run metadata, and telemetry export files. It checks that every category is present, then purges the conversation, the document and the memory. An exhaustive search then reads every table and shadow table of both databases, plus raw and zstd-decoded blob and export files. It finds nothing. FTS MATCH finds nothing, the probe vector bytes are gone, the tombstones and the notice are present, and replay is idempotent. Journal sequences, kinds and keys are unchanged, reconcile and task hashes are re-sealed, the backup still contains the data, and scope and principal isolation hold.
- **`crash`.** It sends SIGKILL to a subprocess at each of nine boundaries: planned, after a sweep batch, main swept, journal blobs swept, telemetry swept, inside the uncommitted base-deletion transaction, base deleted, blobs released and completed. It then resumes with the same keys and checks convergence: no markers, three tombstones, `quick_check=ok` and restored append-only triggers.
- **`policy`.** It drives the expiry classes with a manual clock.

`cargo test -p excalibur-memory` keeps the memory suites green with the delegated purge. Fixtures are synthetic and local.
