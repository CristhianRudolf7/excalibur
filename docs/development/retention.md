# Retention and purge on request

`excalibur-retention` is the shared-core service for architecture §4.7: user-requested purge of a conversation, a document or a memory, plus the time-based retention classes. It is a library. No CLI subcommand, HTTP route or confirmation dialog (§20.5) is added here.

`MemoryStore::purge_memory` delegates to this service; the gateway's existing `memory.purge` call uses it. The daemon host gives the memory store its telemetry handle and calls `resume_pending_purges` at startup.

## Purge as a keyed durable operation

`PurgeService::open(principal, main, telemetry)` installs a checksummed additive schema (`retention_schema` version 1) in the principal database. The journal gains an additive `journal_payload_redirects` table (`durable_schema` version 2), and telemetry gains `telemetry_expired` (`telemetry_schema` version 2). No existing table is altered.

`purge(PurgeRequest)` takes an operation key, scope, subject kind and ID, reason code and request time.

**Planning.** Planning is one writer command and mutates nothing until every check passes:
1. It authorizes the subject and enumerates its members: the conversation's messages, the chunks of every document version, and IDs that participants derive.
2. It finds the subject's **linked runs** (see below).
3. It refuses the purge with `ActiveRun` while any linked durable run is not `completed`, `failed` or `cancelled`.
4. It validates the foreign-key graph statically and runs a bounded trial deletion (up to 16 members plus the subject row) inside a savepoint that it rolls back. A failure returns `Rejected` and records nothing.
5. Only then does it record needles, members, vector object IDs, linked runs and blob hashes.

**Phases.** Each phase commits its own progress in `retention_purges`:

1. **swept.** Rows of the principal database that belong to the subject are redacted in bounded writer batches. This includes inline journal payloads, journal payload blobs, and other rows of linked runs or rows that reference a member.
2. **base_deleted.** The first command re-discovers linked runs and fences active ones. If new terminal runs appear, the purge returns to the sweep. That same command then runs the participant deletions and removes vectors (by key lookup), edges and blob references. Members are then deleted in commands of at most 64 rows, and a final command deletes the subject row. Each deletion also removes the rows that cannot exist without the deleted rows. Nullable references from unrelated rows are set to NULL.
3. **telemetry_swept.** A second pass over the principal database covers rows written or changed after the first sweep. Then telemetry rows of linked runs are redacted, journal-sourced offsets are re-sealed, and the runs are exported again.
4. **finalized.** The tombstone is written, and the needles and expected-hash records are deleted.
5. **blobs_released.** Unreferenced blob files and their metadata are removed immediately. Content that another owner still references is kept (`blobs_shared`).
6. **completed.** `wal_checkpoint(TRUNCATE)` runs on both databases.

**Replay and resume.** Replaying the same key, or the same subject under another key in the same scope, returns the stored receipt. A key that names a different subject in the same scope fails with `KeyConflict`. A key of another scope or principal answers `NotFound`, the same as a missing subject.

A phase that fails records a content-free `last_error` and leaves the purge resumable. `resume_pending()` drives every unfinished purge independently and returns one `PendingOutcome` per key, so one stuck purge never blocks the others. `resume(key, scopes)` resumes a single visible purge; `purge_memory` uses it after a crash past the memory row's deletion.

## What belongs to a subject

Redaction never matches content across the whole database. A row is redacted only when it provably belongs to the subject:

- **Linked runs.** A run is linked to the subject in any of these cases:
  - it was declared through `PurgeService::link_run(kind, id, run)`;
  - a table carries a `run_id` next to a member reference (`conversation_id`, `message_id`, `document_id`, `chunk_id`, `memory_id`), for example `memory_run_bindings` or `chunk_retrievals`;
  - it is bound to a memory's `origin_conversation` or is a run of its `origin_task`;
  - it is a child of a linked run, through `memory_run_bindings.parent_run_id` or a journal child-budget allocation.

  Their journal rows, journal payload blobs, telemetry rows and other rows that carry their `run_id` are candidates.
- **Member references.** Rows whose member-reference column holds one of the subject's member IDs are also candidates.
- **Never candidates.** Rows of other runs, legacy rows with no link, and rows of other scopes are never touched, even when they contain the same text. Scoped tables additionally require an exact scope match (scope names are never substring-matched; legacy NULL scopes stay with unscoped subjects).

Within candidate rows, only text columns that are not identifiers, keys, hashes, enumerations, timestamps or part of any primary key, index or foreign key are rewritten. The following are never rewritten:
- independent subject content: other messages, memories, documents, chunks and identity documents;
- content tables of external-content FTS5 indexes;
- structural metadata, including the hash-chained `audit` table, `durable_operations` and `task_observed_events`.

**Matching.** Matching uses exact subject texts of at least 12 characters, sentences or lines of at least 24 characters, and their JSON-escaped spellings. It is ASCII-case-insensitive.

**JSON.** Values that parse as JSON are redacted only in their string leaves (replacement `[purged]`), so the document stays valid JSON. A match that lies in JSON structure (an object, keys or numbers) is left in place, counted in `structured_matches` and listed by `PurgeService::flags(key)`. Text that looks like JSON but does not parse is never rewritten. Plain text is replaced directly.

## Journal, telemetry and other stores

- **Journal.** Payloads are redacted in place. Sequence, kind, idempotency key, state and row count do not change, and the append-only triggers are lifted only inside the purge transaction. A redacted payload blob gets a new content address; `journal_payload_redirects` maps the old hash, so `Journal::context_observation(run, old_hash)` still resolves archived context observations.
- **Re-sealing.** Hashes kept over journal events are re-sealed only when the stored hash still matches the pre-purge content. Otherwise the hash is left as found, `hash_mismatches` is counted and a `task_hash_mismatch` or `telemetry_hash_mismatch` flag is recorded. This covers `task_observed_events.payload_hash` and `telemetry_offsets.payload_hash` (via `excalibur_telemetry::journal_offset_hash`).
- **Telemetry.** Rows of linked runs in `telemetry.db` are redacted, and every linked run with telemetry is exported again, so `runs/<id>/` artifacts are regenerated.
- **FTS5.** Purge deletions run with FTS5 `secure-delete` enabled on every external-content index. Entries leave the index immediately, without a full rebuild, and the setting is restored afterwards. This requires SQLite 3.42 or later, which is the bundled library.
- **Vectors.** `vec0` rows are deleted by key for each member ID and `<prefix>:<id>` (`memory`, `message`, `msg`, `chunk`, `key`, `document`, `conversation`), and sqlite-vec 0.1.9 zeroes the deleted bytes.
- **Memory participant.** `excalibur_memory::MemoryPurgeParticipant` handles memory-owned rows:
  - it removes extracted keys and their embeddings, admissions, transitions, entity links, relation needs, curation history and chunk retrieval records;
  - it closes open questions.

  A purge is refused with `ParticipantMissing("memory")` when the memory schema is installed but this participant is not registered.

## Tombstone and backup notice

`retention_tombstones` holds only `id, principal_id, scope_id, subject_kind, subject_id, reason, purged_at`. `reason` is a code (`user_request`, `retention_policy`, `administrative`). `PurgeService::tombstone(scope, kind, id)` answers only within the caller's scope.

Every receipt carries `BackupNotice { code: "backups_retain_until_rotation", max_backup_age_days: 28 }`: existing backups may keep the data until they rotate out (§4.6). Backups are not rewritten.

## Retention classes (§4.7)

`RetentionSweeper::open(main, telemetry, RetentionPolicy::default(), clock)` takes an injected clock in Unix seconds. The defaults are a 30-day full journal, 30-day tool outputs, 90-day detailed telemetry and 7-day shadow snapshots. `run_once()` does the following:
- It deletes detailed telemetry older than 90 days: events with their offsets, traces and metrics. It keeps `metric_daily` and exports the touched runs again. It also records a per-run `telemetry_expired` watermark, so `Telemetry::reconcile` does not re-ingest expired journal events.
- It reports terminal runs whose journal is older than 30 days (`journal_compaction_due`), except runs held through `hold_run` (`journal_held`).
- It counts unreferenced journal payload blobs of those runs (`tool_output_blobs_due`).

## Hygiene and honest limits

- **Secure delete, no VACUUM.** Every purge write runs with `PRAGMA secure_delete=ON`. `VACUUM` is not applied: it would renumber implicit rowids that external-content FTS5 indexes depend on, and it needs exclusive access. Free space that earlier updates left inside pages without secure_delete can still hold old bytes. The purge test prints raw database and WAL hits rather than asserting on them; in the recorded run there were none, but that is not guaranteed.
- **Outside SQLite.** File-system blocks, SSD remapping, old copies left by atomic file replacement and backups are outside SQLite's reach.
- **What matching misses.**
  - Unicode case folding and normalization (for example composed or decomposed accents), whitespace or line-wrap changes, truncated snippets and encodings such as base64 are not detected.
  - Texts under 12 characters are deleted from their base rows but not searched for elsewhere.
  - Paraphrases and model-derived facts are not detected, and independent subjects are not cascaded.
- **What linkage misses.** Copies in runs that have no provenance link to the subject are not redacted, by design (RP-1): the purge cannot prove they belong to the subject. Hosts should link the runs they start on behalf of a subject.
- **Structure is reported, not rewritten.** Matches inside JSON structure stay in place and are reported for review.
- **Not covered:**
  - the hash-chained `audit` table, `durable_operations.seed_hash`, and evidence-output, file-version and tool-snapshot blobs;
  - the separate decider ledger database and `codegraph.db`;
  - project purge, a §4.7 subject that is not implemented.
- **Fail-closed side effects.** Resending an original journal payload under a redacted key fails as a key conflict, and re-registering original telemetry run metadata is rejected. Reconciling telemetry while a purge is between its journal and telemetry phases reports an identity conflict until the purge completes.
- **Not executed.** Journal compaction, the 30-day tool-output deletion and shadow-snapshot expiry are reported, not executed.
- **Platform.** Verification covers Linux only.

## Verification

`cargo test -p excalibur-retention` runs the following:

- **`purge`.** It plants unique markers in base tables, FTS5 indexes (including trigram), `vec0` probe vectors, attachment, document and journal blobs, an archived context observation, inline and upper-case journal copies, notifications, evidence, memory keys, a foreign-key child table, telemetry events and run metadata, and export files. Each subject is linked to its run by real provenance. After the conversation, document and memory are purged, an exhaustive search of every table, shadow table and raw or zstd-decoded file finds nothing. It also checks FTS MATCH, zeroed vector bytes, tombstones, the notice, replay, re-sealed hashes, the context-observation redirect and the backup. The suite also has these regression tests:
  - `rp1`: scope B's and an unlinked legacy run's journal, telemetry, evidence and notifications are byte-identical despite shared sentences;
  - `rp2`: a JSON-shaped message and a message that is a run ID keep every payload valid and every ID intact;
  - `rp3`: `resume_pending` isolates a failing purge;
  - `rp4`: bounded base-deletion commands with incremental FTS deletes;
  - `rp6`: the active-run fence and late rows;
  - `rp8`: `purge_memory` resumes after a crash and covers telemetry;
  - `rp9`: tampered hashes are flagged, not re-sealed.
- **`crash`.** It sends SIGKILL at ten boundaries, including inside the uncommitted final base-deletion transaction, then resumes with the same keys and checks convergence.
- **`policy`.** It drives the expiry classes with a manual clock and checks that expired telemetry is not re-ingested.

`cargo test -p excalibur-memory`, `cargo test -p excalibur-gateway --test shared_backend` and `cargo test -p excalibur-daemon --test memory_host` cover the delegated memory purge and the host wiring. Fixtures are synthetic and local.
