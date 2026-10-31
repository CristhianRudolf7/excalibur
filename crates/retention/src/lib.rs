//! Shared-core retention and user-requested purge (architecture §4.7).
//!
//! A purge is a keyed durable operation stored in the principal database. Each phase
//! commits its own progress, so a process killed at any boundary resumes with the same
//! key and converges. Derived copies are redacted only in rows that provably belong to
//! the subject: rows of runs linked to it (explicit provenance, `*_id` reference columns
//! next to `run_id`, child runs) and rows that reference one of its member IDs.
mod policy;
mod sweep;

pub use excalibur_telemetry::Telemetry;
pub use policy::{ExpiryReport, HoldReason, RetentionClock, RetentionPolicy, RetentionSweeper};
pub use sweep::{MIN_SEGMENT_NEEDLE, MIN_TEXT_NEEDLE, REDACTION_MARK, VECTOR_PREFIXES};

use excalibur_journal::{Event, Journal};
use excalibur_store::{
    BlobHash, BlobRelease, Database, DatabaseKind, Error as StoreError, Priority, ReadLane,
    rusqlite::{self, Connection, OptionalExtension},
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};
use sweep::{
    DeleteStats, ForeignKeys, Matcher, Redaction, Targets, columns, delete_vectors,
    derive_needles, guarded, json_list, redact_rows, regular_tables, remove_rows, scan_page,
    sweep_tables, table_exists, with_secure_delete,
};

/// Weekly backups are kept four weeks (§4.6); daily and pre-update copies rotate sooner.
pub const MAX_BACKUP_AGE_DAYS: u32 = 28;
pub const BACKUP_NOTICE_CODE: &str = "backups_retain_until_rotation";
const PAGE: u32 = 512;
const BATCH: usize = 64;
const TRIAL_MEMBERS: usize = 16;
const MAX_ORIGINAL_BYTES: u64 = 8 * 1024 * 1024;
const MAX_JOURNAL_BLOB: u64 = 16 * 1024 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("retention storage: {0}")]
    Store(#[from] StoreError),
    #[error("retention journal: {0}")]
    Journal(#[from] excalibur_journal::Error),
    #[error("retention telemetry: {0}")]
    Telemetry(#[from] excalibur_telemetry::Error),
    #[error("retention serialization: {0}")]
    Json(#[from] serde_json::Error),
    #[error("subject not found for this principal and scope")]
    NotFound,
    #[error("operation key already names a different purge in this scope")]
    KeyConflict,
    #[error("a registered purge participant is required: {0}")]
    ParticipantMissing(&'static str),
    /// A run linked to the subject is still active; stop or finish it first.
    #[error("a run linked to the subject is still active: {0}")]
    ActiveRun(String),
    /// Planning validation failed before anything was changed.
    #[error("purge rejected during planning: {0}")]
    Rejected(String),
    #[error("invalid purge request: {0}")]
    Invalid(&'static str),
}
impl From<rusqlite::Error> for Error {
    fn from(error: rusqlite::Error) -> Self {
        Self::Store(error.into())
    }
}
impl Error {
    /// Content-free code recorded on a purge that stopped in a phase.
    fn code(&self) -> String {
        match self {
            Self::ActiveRun(run) => format!("active_run:{run}"),
            Self::Store(error) => format!("storage:{error}"),
            Self::Journal(_) => "journal".into(),
            Self::Telemetry(_) => "telemetry".into(),
            Self::Json(_) => "serialization".into(),
            Self::NotFound => "not_found".into(),
            Self::KeyConflict => "key_conflict".into(),
            Self::ParticipantMissing(name) => format!("participant_missing:{name}"),
            Self::Rejected(reason) => format!("rejected:{reason}"),
            Self::Invalid(reason) => format!("invalid:{reason}"),
        }
    }
}
pub type Result<T> = std::result::Result<T, Error>;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SubjectKind {
    Conversation,
    Document,
    Memory,
}
impl SubjectKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Conversation => "conversation",
            Self::Document => "document",
            Self::Memory => "memory",
        }
    }
    fn parse(text: &str) -> Option<Self> {
        match text {
            "conversation" => Some(Self::Conversation),
            "document" => Some(Self::Document),
            "memory" => Some(Self::Memory),
            _ => None,
        }
    }
}
/// Reason codes only: a tombstone never stores free text.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PurgeReason {
    UserRequest,
    RetentionPolicy,
    Administrative,
}
impl PurgeReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::UserRequest => "user_request",
            Self::RetentionPolicy => "retention_policy",
            Self::Administrative => "administrative",
        }
    }
    fn parse(text: &str) -> Option<Self> {
        match text {
            "user_request" => Some(Self::UserRequest),
            "retention_policy" => Some(Self::RetentionPolicy),
            "administrative" => Some(Self::Administrative),
            _ => None,
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemberKind {
    Conversation,
    Message,
    Document,
    Chunk,
    Memory,
    /// Identifiers added by a participant (for example, extracted memory keys).
    Derived,
}
impl MemberKind {
    fn reference_column(self) -> Option<&'static str> {
        match self {
            Self::Conversation => Some("conversation_id"),
            Self::Message => Some("message_id"),
            Self::Document => Some("document_id"),
            Self::Chunk => Some("chunk_id"),
            Self::Memory => Some("memory_id"),
            Self::Derived => None,
        }
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Member {
    pub kind: MemberKind,
    pub id: String,
    pub scope: String,
}
/// Identifiers of everything the subject owns. Content is never part of a snapshot.
#[derive(Clone, Debug)]
pub struct SubjectSnapshot {
    pub kind: SubjectKind,
    pub id: String,
    pub principal: String,
    pub scope: String,
    pub members: Vec<Member>,
}
impl SubjectSnapshot {
    pub fn members_of(&self, kind: MemberKind) -> impl Iterator<Item = &Member> {
        self.members.iter().filter(move |m| m.kind == kind)
    }
}
/// What a participant adds while the subject still exists.
#[derive(Debug, Default)]
pub struct Collection {
    derived: Vec<Member>,
    texts: Vec<String>,
}
impl Collection {
    pub fn add_derived(&mut self, id: impl Into<String>, scope: impl Into<String>) {
        self.derived.push(Member {
            kind: MemberKind::Derived,
            id: id.into(),
            scope: scope.into(),
        });
    }
    /// Text of a derived copy whose verbatim occurrences must also be found elsewhere.
    pub fn add_text(&mut self, text: impl Into<String>) {
        self.texts.push(text.into());
    }
}
/// A crate that owns additional derived rows (for example, memory keys and embeddings).
/// Both methods run inside short writer transactions on the principal database.
pub trait PurgeParticipant: Send + Sync {
    fn name(&self) -> &'static str;
    fn collect(
        &self,
        c: &Connection,
        subject: &SubjectSnapshot,
        out: &mut Collection,
    ) -> excalibur_store::Result<()>;
    /// Delete owned rows. It may run more than once after an interruption and must be
    /// idempotent. FTS5 secure-delete is active, so index entries leave immediately.
    fn purge(&self, c: &Connection, subject: &SubjectSnapshot) -> excalibur_store::Result<u64>;
}
/// Schemas whose derived rows require a named participant before any purge proceeds.
const REQUIRED_PARTICIPANTS: &[(&str, &str)] = &[("memory_schema", "memory")];

/// Interruption points exercised by crash tests. `BaseDeleteUncommitted` runs inside the
/// open final base-deletion transaction; all others run after their write committed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PurgeBoundary {
    Planned,
    SweepBatch,
    Swept,
    BaseDeleteBatch,
    BaseDeleteUncommitted,
    BaseDeleted,
    TelemetrySwept,
    Finalized,
    BlobsReleased,
    Completed,
}
pub type PurgeObserver = Arc<dyn Fn(PurgeBoundary) + Send + Sync>;
pub type Clock = Arc<dyn Fn() -> i64 + Send + Sync>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    Planned,
    Swept,
    BaseDeleted,
    TelemetrySwept,
    Finalized,
    BlobsReleased,
    Completed,
}
impl Phase {
    fn as_str(self) -> &'static str {
        match self {
            Self::Planned => "planned",
            Self::Swept => "swept",
            Self::BaseDeleted => "base_deleted",
            Self::TelemetrySwept => "telemetry_swept",
            Self::Finalized => "finalized",
            Self::BlobsReleased => "blobs_released",
            Self::Completed => "completed",
        }
    }
    fn parse(text: &str) -> Option<Self> {
        [
            Self::Planned,
            Self::Swept,
            Self::BaseDeleted,
            Self::TelemetrySwept,
            Self::Finalized,
            Self::BlobsReleased,
            Self::Completed,
        ]
        .into_iter()
        .find(|p| p.as_str() == text)
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct PurgeCounts {
    pub runs_linked: u64,
    pub rows_deleted: u64,
    pub rows_nulled: u64,
    pub participant_rows_deleted: u64,
    pub rows_redacted: u64,
    pub journal_events_redacted: u64,
    pub telemetry_rows_redacted: u64,
    pub telemetry_offsets_resealed: u64,
    pub task_hashes_resealed: u64,
    /// Existing hashes that did not match the pre-purge content; left as found.
    pub hash_mismatches: u64,
    /// Matches inside JSON structure (not a string leaf); left as found and flagged.
    pub structured_matches: u64,
    pub vectors_deleted: u64,
    pub edges_deleted: u64,
    pub blobs_removed: u64,
    pub blobs_shared: u64,
    pub base_batches: u64,
    pub runs_reexported: u64,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hygiene {
    /// `PRAGMA secure_delete=ON` and FTS5 `secure-delete` were active for purge writes.
    pub secure_delete: bool,
    /// `VACUUM` is not run; secure_delete already zeroes freed cells and pages.
    pub vacuum: bool,
    pub main_wal_truncated: bool,
    pub telemetry_wal_truncated: Option<bool>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackupNotice {
    pub code: String,
    pub message: String,
    pub max_backup_age_days: u32,
}
impl BackupNotice {
    pub fn standard() -> Self {
        Self {
            code: BACKUP_NOTICE_CODE.into(),
            message: format!(
                "The content was purged from the live stores. Existing backups may still contain it until they rotate out (at most {MAX_BACKUP_AGE_DAYS} days)."
            ),
            max_backup_age_days: MAX_BACKUP_AGE_DAYS,
        }
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tombstone {
    pub id: String,
    pub kind: SubjectKind,
    pub subject_id: String,
    pub scope: String,
    pub reason: PurgeReason,
    pub purged_at: i64,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PurgeReceipt {
    pub operation_key: String,
    pub tombstone: Tombstone,
    pub counts: PurgeCounts,
    pub hygiene: Hygiene,
    /// False when no telemetry handle was supplied: telemetry copies were not swept.
    pub telemetry_covered: bool,
    pub notice: BackupNotice,
}
#[derive(Clone, Debug)]
pub struct PurgeRequest {
    pub operation_key: String,
    pub scope: String,
    pub kind: SubjectKind,
    pub subject_id: String,
    pub reason: PurgeReason,
    pub requested_at: i64,
}
/// Result of resuming one unfinished purge; one failure never blocks the others.
#[derive(Debug)]
pub struct PendingOutcome {
    pub operation_key: String,
    pub result: std::result::Result<PurgeReceipt, String>,
}

#[derive(Clone, Debug)]
struct OpRow {
    key: String,
    tombstone_id: String,
    principal: String,
    scope: String,
    kind: SubjectKind,
    subject_id: String,
    phase: Phase,
    members: Vec<Member>,
    objects: Vec<(String, String)>,
    runs: Vec<String>,
    blob_hashes: Vec<String>,
    counts: PurgeCounts,
    hygiene: Option<Hygiene>,
    telemetry_covered: bool,
}
impl OpRow {
    fn snapshot(&self) -> SubjectSnapshot {
        SubjectSnapshot {
            kind: self.kind,
            id: self.subject_id.clone(),
            principal: self.principal.clone(),
            scope: self.scope.clone(),
            members: self.members.clone(),
        }
    }
    fn runs_json(&self) -> String {
        json_list(self.runs.iter().cloned())
    }
    fn members_json(&self) -> String {
        json_list(self.members.iter().map(|m| m.id.clone()))
    }
}
fn store_json(error: serde_json::Error) -> StoreError {
    StoreError::Configuration(format!("retention state: {error}"))
}
fn load_op(c: &Connection, key: &str) -> excalibur_store::Result<Option<OpRow>> {
    let row = c
        .query_row(
            "SELECT tombstone_id,principal_id,scope_id,subject_kind,subject_id,phase,members,objects,runs,blob_hashes,counts,hygiene,telemetry_covered FROM retention_purges WHERE operation_key=?1",
            [key],
            |r| {
                Ok((
                    [
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                        r.get::<_, String>(3)?,
                        r.get::<_, String>(4)?,
                        r.get::<_, String>(5)?,
                    ],
                    [
                        r.get::<_, String>(6)?,
                        r.get::<_, String>(7)?,
                        r.get::<_, String>(8)?,
                        r.get::<_, String>(9)?,
                        r.get::<_, String>(10)?,
                    ],
                    r.get::<_, Option<String>>(11)?,
                    r.get::<_, bool>(12)?,
                ))
            },
        )
        .optional()?;
    let Some((ids, json, hygiene, telemetry_covered)) = row else {
        return Ok(None);
    };
    let [tombstone_id, principal, scope, kind, subject_id, phase] = ids;
    Ok(Some(OpRow {
        key: key.into(),
        tombstone_id,
        principal,
        scope,
        kind: SubjectKind::parse(&kind).ok_or(StoreError::Invalid("unknown purge kind"))?,
        subject_id,
        phase: Phase::parse(&phase).ok_or(StoreError::Invalid("unknown purge phase"))?,
        members: serde_json::from_str(&json[0]).map_err(store_json)?,
        objects: serde_json::from_str(&json[1]).map_err(store_json)?,
        runs: serde_json::from_str(&json[2]).map_err(store_json)?,
        blob_hashes: serde_json::from_str(&json[3]).map_err(store_json)?,
        counts: serde_json::from_str(&json[4]).map_err(store_json)?,
        hygiene: hygiene
            .map(|h| serde_json::from_str(&h))
            .transpose()
            .map_err(store_json)?,
        telemetry_covered,
    }))
}
fn bump(
    c: &Connection,
    key: &str,
    change: impl FnOnce(&mut PurgeCounts),
) -> excalibur_store::Result<()> {
    let text: String = c.query_row(
        "SELECT counts FROM retention_purges WHERE operation_key=?1",
        [key],
        |r| r.get(0),
    )?;
    let mut counts: PurgeCounts = serde_json::from_str(&text).map_err(store_json)?;
    change(&mut counts);
    c.execute(
        "UPDATE retention_purges SET counts=?2 WHERE operation_key=?1",
        rusqlite::params![key, serde_json::to_string(&counts).map_err(store_json)?],
    )?;
    Ok(())
}
fn flag(c: &Connection, key: &str, code: &str, location: &str) -> excalibur_store::Result<bool> {
    Ok(c.execute(
        "INSERT OR IGNORE INTO retention_purge_flags(operation_key,code,location) VALUES(?1,?2,?3)",
        [key, code, location],
    )? == 1)
}
fn load_needles(c: &Connection, key: &str) -> excalibur_store::Result<Vec<String>> {
    let mut statement = c.prepare(
        "SELECT needle FROM retention_purge_needles WHERE operation_key=?1 ORDER BY ordinal",
    )?;
    let rows = statement
        .query_map([key], |r| r.get(0))?
        .collect::<rusqlite::Result<Vec<String>>>()?;
    Ok(rows)
}
fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
fn merge_json_list(
    c: &Connection,
    key: &str,
    column: &'static str,
    values: impl IntoIterator<Item = String>,
) -> excalibur_store::Result<()> {
    let text: String = c.query_row(
        &format!("SELECT {column} FROM retention_purges WHERE operation_key=?1"),
        [key],
        |r| r.get(0),
    )?;
    let mut set: BTreeSet<String> = serde_json::from_str::<Vec<String>>(&text)
        .map_err(store_json)?
        .into_iter()
        .collect();
    set.extend(values);
    c.execute(
        &format!("UPDATE retention_purges SET {column}=?2 WHERE operation_key=?1"),
        rusqlite::params![key, json_list(set)],
    )?;
    Ok(())
}

/// Latest authoritative state of a durable run, or `None` when the journal has no run.
fn durable_state(c: &Connection, run: &str) -> excalibur_store::Result<Option<String>> {
    if !table_exists(c, "durable_operations")? {
        return Ok(None);
    }
    let known: bool = c.query_row(
        "SELECT EXISTS(SELECT 1 FROM durable_operations WHERE run_id=?1)",
        [run],
        |r| r.get(0),
    )?;
    if !known {
        return Ok(None);
    }
    let latest: Option<Option<String>> = c
        .query_row(
            "SELECT json_extract(payload,'$.state') FROM journal WHERE run_id=?1 AND kind='state_change' ORDER BY sequence DESC LIMIT 1",
            [run],
            |r| r.get(0),
        )
        .optional()?;
    Ok(Some(
        latest.flatten().unwrap_or_else(|| "running".into()),
    ))
}
/// A linked durable run that is not terminal fences the purge.
fn active_run(c: &Connection, runs: &BTreeSet<String>) -> excalibur_store::Result<Option<String>> {
    for run in runs {
        if let Some(state) = durable_state(c, run)?
            && !matches!(state.as_str(), "completed" | "failed" | "cancelled")
        {
            return Ok(Some(run.clone()));
        }
    }
    Ok(None)
}
/// Runs that provably belong to the subject: explicit provenance, tables that carry a
/// `run_id` next to a member reference, memory origins, and their child runs.
fn linked_runs(
    c: &Connection,
    snapshot: &SubjectSnapshot,
) -> excalibur_store::Result<BTreeSet<String>> {
    let mut runs = BTreeSet::new();
    {
        let mut statement = c.prepare("SELECT run_id FROM retention_subject_runs WHERE principal_id=?1 AND subject_kind=?2 AND subject_id=?3")?;
        for run in statement.query_map(
            [
                snapshot.principal.as_str(),
                snapshot.kind.as_str(),
                snapshot.id.as_str(),
            ],
            |r| r.get::<_, String>(0),
        )? {
            runs.insert(run?);
        }
    }
    let mut by_column: BTreeMap<&'static str, Vec<String>> = BTreeMap::new();
    for member in &snapshot.members {
        if let Some(column) = member.kind.reference_column() {
            by_column.entry(column).or_default().push(member.id.clone());
        }
    }
    let mut origin_conversations = Vec::new();
    if snapshot.kind == SubjectKind::Memory {
        let memory_columns = columns(c, "memories")?;
        if memory_columns.iter().any(|c| c == "origin_conversation") {
            let origin: Option<String> = c.query_row(
                "SELECT origin_conversation FROM memories WHERE id=?1",
                [&snapshot.id],
                |r| r.get(0),
            )?;
            origin_conversations.extend(origin);
        }
        if memory_columns.iter().any(|c| c == "origin_task") {
            let task: Option<String> = c.query_row(
                "SELECT origin_task FROM memories WHERE id=?1",
                [&snapshot.id],
                |r| r.get(0),
            )?;
            if let Some(task) = task {
                let mut statement = c.prepare("SELECT id FROM runs WHERE task_id=?1")?;
                for run in statement.query_map([task], |r| r.get::<_, String>(0))? {
                    runs.insert(run?);
                }
            }
        }
    }
    by_column
        .entry("conversation_id")
        .or_default()
        .extend(origin_conversations);
    for table in regular_tables(c)? {
        if table.starts_with("retention_") {
            continue;
        }
        let present = columns(c, &table)?;
        if !present.iter().any(|c| c == "run_id") {
            continue;
        }
        for (column, ids) in &by_column {
            if ids.is_empty() || !present.iter().any(|c| c == column) {
                continue;
            }
            let mut statement = c.prepare(&format!(
                "SELECT DISTINCT run_id FROM {} WHERE {} IN (SELECT value FROM json_each(?1)) AND run_id IS NOT NULL",
                sweep::quote(&table),
                sweep::quote(column)
            ))?;
            for run in statement.query_map([json_list(ids.iter().cloned())], |r| {
                r.get::<_, String>(0)
            })? {
                runs.insert(run?);
            }
        }
    }
    // Child runs, through memory bindings and journal child-budget allocations.
    let bindings = table_exists(c, "memory_run_bindings")?;
    let mut frontier: BTreeSet<String> = runs.clone();
    for _ in 0..8 {
        if frontier.is_empty() {
            break;
        }
        let list = json_list(frontier.iter().cloned());
        let mut next = BTreeSet::new();
        if bindings {
            let mut statement = c.prepare("SELECT run_id FROM memory_run_bindings WHERE parent_run_id IN (SELECT value FROM json_each(?1))")?;
            for run in statement.query_map([&list], |r| r.get::<_, String>(0))? {
                next.insert(run?);
            }
        }
        let mut statement = c.prepare("SELECT json_extract(payload,'$.allocation.child_run') FROM journal WHERE kind='child_budget_reserved' AND run_id IN (SELECT value FROM json_each(?1))")?;
        for run in statement.query_map([&list], |r| r.get::<_, Option<String>>(0))? {
            next.extend(run?);
        }
        next.retain(|run| !runs.contains(run));
        runs.extend(next.iter().cloned());
        frontier = next;
    }
    Ok(runs)
}

/// Record a redacted journal event and re-seal the hashes other crates keep over it,
/// but only when the stored hash still matches the pre-purge content.
fn journal_follow(
    c: &Connection,
    key: &str,
    rowid: i64,
    old_payload: &str,
    new_payload: Value,
) -> excalibur_store::Result<()> {
    let row: Option<(Option<String>, u64, String, Option<String>)> = c
        .query_row(
            "SELECT run_id,sequence,kind,idempotency_key FROM journal WHERE rowid=?1",
            [rowid],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .optional()?;
    let Some((Some(run), sequence, kind, event_key)) = row else {
        return Ok(());
    };
    let event_key = event_key.unwrap_or_default();
    let old_value: Value = match serde_json::from_str(old_payload) {
        Ok(value) => value,
        Err(_) => {
            flag(c, key, "unparseable_journal_payload", &format!("{run}:{sequence}"))?;
            return Ok(());
        }
    };
    let old = Event {
        sequence,
        kind: kind.clone(),
        key: event_key.clone(),
        payload: old_value,
    };
    let new = Event {
        sequence,
        kind,
        key: event_key,
        payload: new_payload,
    };
    let offset = |event: &Event| {
        excalibur_telemetry::journal_offset_hash(event)
            .map_err(|e| StoreError::Configuration(e.to_string()))
    };
    let (old_offset, new_offset) = (offset(&old)?, offset(&new)?);
    c.execute(
        "INSERT INTO retention_purge_events(operation_key,run_id,sequence,old_offset_hash,new_offset_hash) VALUES(?1,?2,?3,?4,?5) ON CONFLICT(operation_key,run_id,sequence) DO UPDATE SET new_offset_hash=excluded.new_offset_hash,old_offset_hash=coalesce(old_offset_hash,excluded.old_offset_hash)",
        rusqlite::params![key, run, sequence, old_offset, new_offset],
    )?;
    if !table_exists(c, "task_observed_events")? {
        return Ok(());
    }
    let stored: Option<String> = c
        .query_row(
            "SELECT payload_hash FROM task_observed_events WHERE run_id=?1 AND sequence=?2",
            rusqlite::params![run, sequence],
            |r| r.get(0),
        )
        .optional()?;
    let Some(stored) = stored else {
        return Ok(());
    };
    let old_hash = sha256_hex(&serde_json::to_vec(&old).map_err(store_json)?);
    let new_hash = sha256_hex(&serde_json::to_vec(&new).map_err(store_json)?);
    if stored == new_hash {
        return Ok(());
    }
    if stored != old_hash {
        if flag(c, key, "task_hash_mismatch", &format!("{run}:{sequence}"))? {
            bump(c, key, |n| n.hash_mismatches += 1)?;
        }
        return Ok(());
    }
    guarded(c, "task_observed_events", "UPDATE", || {
        Ok(c.execute(
            "UPDATE task_observed_events SET payload_hash=?1 WHERE run_id=?2 AND sequence=?3 AND payload_hash=?4",
            rusqlite::params![new_hash, run, sequence, old_hash],
        )?)
    })?;
    bump(c, key, |n| n.task_hashes_resealed += 1)?;
    Ok(())
}
/// Redact targeted rows of one principal-database table and record follow-up work.
fn redact_main(
    c: &Connection,
    key: &str,
    table: &sweep::SweepTable,
    targets: &Targets<'_>,
    rowids: &[i64],
    matcher: &Matcher,
) -> excalibur_store::Result<()> {
    let mut journal_rows = 0u64;
    let journal = table.name.eq_ignore_ascii_case("journal");
    let stats = redact_rows(c, table, targets, rowids, matcher, &mut |c, _, rowid, old| {
        if journal && let Some(previous) = old.get("payload") {
            let current: String =
                c.query_row("SELECT payload FROM journal WHERE rowid=?1", [rowid], |r| {
                    r.get(0)
                })?;
            let value = serde_json::from_str(&current).map_err(store_json)?;
            journal_follow(c, key, rowid, previous, value)?;
            journal_rows += 1;
        }
        Ok(())
    })?;
    if stats.rows > 0 || stats.structured > 0 {
        bump(c, key, |n| {
            n.rows_redacted += stats.rows;
            n.journal_events_redacted += journal_rows;
            n.structured_matches += stats.structured;
        })?;
    }
    Ok(())
}

enum Existing {
    Key(String),
    Conflict,
    Hidden,
    Absent,
}
fn existing_op(
    c: &Connection,
    principal: &str,
    request: &PurgeRequest,
) -> excalibur_store::Result<Existing> {
    let by_key: Option<(String, String, String, String)> = c
        .query_row(
            "SELECT principal_id,scope_id,subject_kind,subject_id FROM retention_purges WHERE operation_key=?1",
            [&request.operation_key],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .optional()?;
    if let Some((p, scope, kind, id)) = by_key {
        // A key of another principal or scope is indistinguishable from a missing subject.
        if p != principal || scope != request.scope {
            return Ok(Existing::Hidden);
        }
        return Ok(
            if kind == request.kind.as_str() && id == request.subject_id {
                Existing::Key(request.operation_key.clone())
            } else {
                Existing::Conflict
            },
        );
    }
    let by_subject: Option<(String, String)> = c
        .query_row(
            "SELECT operation_key,scope_id FROM retention_purges WHERE principal_id=?1 AND subject_kind=?2 AND subject_id=?3",
            rusqlite::params![principal, request.kind.as_str(), request.subject_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    Ok(match by_subject {
        Some((key, scope)) if scope == request.scope => Existing::Key(key),
        Some(_) => Existing::Hidden,
        None => Existing::Absent,
    })
}
/// Authorize and enumerate the subject. `None` hides subjects of other scopes/principals.
fn load_subject(
    c: &Connection,
    principal: &str,
    kind: SubjectKind,
    id: &str,
    scope: &str,
) -> excalibur_store::Result<Option<(SubjectSnapshot, Vec<String>)>> {
    let mut members = Vec::new();
    let mut texts = Vec::new();
    let owner_sql = match kind {
        SubjectKind::Conversation => {
            "SELECT coalesce(principal_id,?2),coalesce(scope_id,active_scope,'legacy:unscoped') FROM conversations WHERE id=?1"
        }
        SubjectKind::Document => {
            "SELECT coalesce(principal_id,?2),coalesce(scope_id,'legacy:unscoped') FROM documents WHERE id=?1"
        }
        SubjectKind::Memory => {
            "SELECT coalesce(principal_id,?2),coalesce(scope_id,'legacy:unscoped') FROM memories WHERE id=?1"
        }
    };
    let owner: Option<(String, String)> = c
        .query_row(owner_sql, [id, principal], |r| Ok((r.get(0)?, r.get(1)?)))
        .optional()?;
    let Some((owner, owner_scope)) = owner else {
        return Ok(None);
    };
    if owner != principal || owner_scope != scope {
        return Ok(None);
    }
    match kind {
        SubjectKind::Conversation => {
            members.push(Member {
                kind: MemberKind::Conversation,
                id: id.into(),
                scope: scope.into(),
            });
            let mut statement = c.prepare(
                "SELECT id,coalesce(scope_id,?2),text FROM messages WHERE conversation_id=?1 AND coalesce(principal_id,?3)=?3 ORDER BY rowid",
            )?;
            let rows = statement
                .query_map([id, scope, principal], |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                    ))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            for (message, message_scope, text) in rows {
                members.push(Member {
                    kind: MemberKind::Message,
                    id: message,
                    scope: message_scope,
                });
                texts.push(text);
            }
        }
        SubjectKind::Document => {
            members.push(Member {
                kind: MemberKind::Document,
                id: id.into(),
                scope: scope.into(),
            });
            if columns(c, "documents")?.iter().any(|c| c == "title") {
                texts.push(c.query_row(
                    "SELECT coalesce(title,'') FROM documents WHERE id=?1",
                    [id],
                    |r| r.get(0),
                )?);
            }
            let context = if columns(c, "doc_chunks")?.iter().any(|c| c == "context") {
                "coalesce(context,'')"
            } else {
                "''"
            };
            let mut statement = c.prepare(&format!(
                "SELECT id,coalesce(scope_id,?2),text,{context} FROM doc_chunks WHERE document_id=?1 ORDER BY rowid"
            ))?;
            let rows = statement
                .query_map([id, scope], |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                        r.get::<_, String>(3)?,
                    ))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            for (chunk, chunk_scope, text, context) in rows {
                members.push(Member {
                    kind: MemberKind::Chunk,
                    id: chunk,
                    scope: chunk_scope,
                });
                texts.push(text);
                texts.push(context);
            }
        }
        SubjectKind::Memory => {
            members.push(Member {
                kind: MemberKind::Memory,
                id: id.into(),
                scope: scope.into(),
            });
            texts.push(c.query_row(
                "SELECT statement FROM memories WHERE id=?1",
                [id],
                |r| r.get(0),
            )?);
        }
    }
    Ok(Some((
        SubjectSnapshot {
            kind,
            id: id.into(),
            principal: principal.into(),
            scope: scope.into(),
            members,
        },
        texts,
    )))
}
fn collect_blob_hashes(
    c: &Connection,
    kind: SubjectKind,
    subject: &str,
    members: &[Member],
) -> excalibur_store::Result<BTreeSet<String>> {
    let ids = json_list(members.iter().map(|m| m.id.clone()));
    let mut hashes = BTreeSet::new();
    let mut statement = c.prepare(
        "SELECT DISTINCT hash FROM blob_refs WHERE owner_id IN (SELECT value FROM json_each(?1))",
    )?;
    for hash in statement.query_map([&ids], |r| r.get::<_, String>(0))? {
        hashes.insert(hash?);
    }
    drop(statement);
    if kind == SubjectKind::Document {
        let direct: Option<String> = c
            .query_row(
                "SELECT blob_hash FROM documents WHERE id=?1",
                [subject],
                |r| r.get(0),
            )
            .optional()?
            .flatten();
        hashes.extend(direct);
        let mut statement = c.prepare(
            "SELECT hash FROM blob_refs WHERE owner_type='document_versions' AND substr(owner_id,1,length(?1)+1)=?1||'@'",
        )?;
        for hash in statement.query_map([subject], |r| r.get::<_, String>(0))? {
            hashes.insert(hash?);
        }
        drop(statement);
        if table_exists(c, "document_versions")? {
            let mut statement =
                c.prepare("SELECT blob_hash FROM document_versions WHERE document_id=?1")?;
            for hash in statement.query_map([subject], |r| r.get::<_, String>(0))? {
                hashes.insert(hash?);
            }
        }
    }
    Ok(hashes)
}
fn base_roots(kind: SubjectKind) -> &'static [&'static str] {
    match kind {
        SubjectKind::Conversation => &["messages", "conversations"],
        SubjectKind::Document => &["doc_chunks", "documents"],
        SubjectKind::Memory => &["memories"],
    }
}
fn member_table(kind: SubjectKind) -> Option<(&'static str, MemberKind)> {
    match kind {
        SubjectKind::Conversation => Some(("messages", MemberKind::Message)),
        SubjectKind::Document => Some(("doc_chunks", MemberKind::Chunk)),
        SubjectKind::Memory => None,
    }
}
fn subject_table(kind: SubjectKind) -> &'static str {
    match kind {
        SubjectKind::Conversation => "conversations",
        SubjectKind::Document => "documents",
        SubjectKind::Memory => "memories",
    }
}
/// Bounded trial deletion inside a rolled-back savepoint, so constraint failures surface
/// at planning time instead of leaving a half-applied purge.
fn trial_delete(c: &Connection, snapshot: &SubjectSnapshot) -> excalibur_store::Result<()> {
    let keys = ForeignKeys::load(c)?;
    keys.validate(base_roots(snapshot.kind))?;
    c.execute_batch("SAVEPOINT retention_trial")?;
    let result = (|| {
        let mut stats = DeleteStats::default();
        if let Some((table, kind)) = member_table(snapshot.kind) {
            let sample: Vec<Value> = snapshot
                .members_of(kind)
                .take(TRIAL_MEMBERS)
                .map(|m| Value::String(m.id.clone()))
                .collect();
            remove_rows(c, &keys, table, "id", &sample, 0, &mut stats)?;
        }
        if snapshot.members.len() <= TRIAL_MEMBERS + 1 {
            remove_rows(
                c,
                &keys,
                subject_table(snapshot.kind),
                "id",
                &[Value::String(snapshot.id.clone())],
                0,
                &mut stats,
            )?;
        }
        Ok(())
    })();
    let rollback = c.execute_batch("ROLLBACK TO retention_trial; RELEASE retention_trial");
    result?;
    rollback?;
    Ok(())
}

static PURGE_SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Shared-core purge service for one principal database (and optionally its telemetry).
#[derive(Clone)]
pub struct PurgeService {
    principal: String,
    main: Database,
    journal: Journal,
    telemetry: Option<Telemetry>,
    participants: Vec<Arc<dyn PurgeParticipant>>,
    observer: Option<PurgeObserver>,
    clock: Clock,
}
impl std::fmt::Debug for PurgeService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PurgeService")
            .field("principal", &self.principal)
            .field("telemetry", &self.telemetry.is_some())
            .finish_non_exhaustive()
    }
}
/// Install the checksummed retention schema on the principal database.
pub(crate) async fn install(db: &Database) -> Result<()> {
    if db.kind() != DatabaseKind::Main {
        return Err(Error::Invalid(
            "retention state lives in the principal database",
        ));
    }
    let sql = include_str!("schema.sql");
    let checksum = sha256_hex(sql.as_bytes());
    db.write(Priority::Journal, move |c| {
        c.execute_batch("CREATE TABLE IF NOT EXISTS retention_schema(version INTEGER PRIMARY KEY,checksum TEXT NOT NULL)")?;
        let known: Option<String> = c
            .query_row(
                "SELECT checksum FROM retention_schema WHERE version=1",
                [],
                |r| r.get(0),
            )
            .optional()?;
        match known {
            Some(known) if known != checksum => Err(StoreError::Configuration(
                "retention schema checksum mismatch".into(),
            )),
            Some(_) => Ok(()),
            None => {
                c.execute_batch(sql)?;
                c.execute(
                    "INSERT INTO retention_schema(version,checksum) VALUES(1,?1)",
                    [checksum],
                )?;
                Ok(())
            }
        }
    })
    .await?;
    Ok(())
}
impl PurgeService {
    /// `principal` owns `main`; telemetry is optional and reported in every receipt.
    pub async fn open(
        principal: impl Into<String>,
        main: Database,
        telemetry: Option<Telemetry>,
    ) -> Result<Self> {
        let principal = principal.into();
        if principal.trim().is_empty() || principal.len() > 512 {
            return Err(Error::Invalid("principal required"));
        }
        install(&main).await?;
        let journal = Journal::new(main.clone()).await?;
        Ok(Self {
            principal,
            main,
            journal,
            telemetry,
            participants: Vec::new(),
            observer: None,
            clock: Arc::new(excalibur_store::unix_seconds),
        })
    }
    pub fn with_participant(mut self, participant: Arc<dyn PurgeParticipant>) -> Self {
        self.participants.push(participant);
        self
    }
    /// Diagnostic hook for interruption tests; never receives content.
    pub fn with_observer(mut self, observer: PurgeObserver) -> Self {
        self.observer = Some(observer);
        self
    }
    pub fn with_clock(mut self, clock: Clock) -> Self {
        self.clock = clock;
        self
    }
    fn notify(&self, boundary: PurgeBoundary) {
        if let Some(observer) = &self.observer {
            observer(boundary);
        }
    }
    /// Declare that `run` derives from a subject (provenance for redaction and fencing).
    pub async fn link_run(&self, kind: SubjectKind, subject_id: &str, run: &str) -> Result<()> {
        if subject_id.trim().is_empty() || run.trim().is_empty() || run.len() > 512 {
            return Err(Error::Invalid("subject and run required"));
        }
        let (principal, id, run, now) = (
            self.principal.clone(),
            subject_id.to_string(),
            run.to_string(),
            (self.clock)(),
        );
        self.main
            .write(Priority::Normal, move |c| {
                c.execute(
                    "INSERT OR IGNORE INTO retention_subject_runs(principal_id,subject_kind,subject_id,run_id,linked_at) VALUES(?1,?2,?3,?4,?5)",
                    rusqlite::params![principal, kind.as_str(), id, run, now],
                )?;
                Ok(())
            })
            .await?;
        Ok(())
    }
    /// Purge one subject. Replaying the same key (or the same subject) returns the
    /// stored receipt; an interrupted operation resumes from its last committed phase.
    pub async fn purge(&self, request: PurgeRequest) -> Result<PurgeReceipt> {
        let valid =
            |s: &str, max: usize| !s.trim().is_empty() && s.len() <= max && !s.contains('\0');
        if !valid(&request.operation_key, 256)
            || !valid(&request.scope, 4096)
            || !valid(&request.subject_id, 512)
        {
            return Err(Error::Invalid(
                "operation key, scope and subject are required",
            ));
        }
        let _serial = PURGE_SERIAL.lock().await;
        let key = self.plan(&request).await?;
        self.drive(&key).await
    }
    /// Resume every unfinished purge of this principal, for example at host startup.
    /// Each purge is driven independently; a failure is reported and the rest continue.
    pub async fn resume_pending(&self) -> Result<Vec<PendingOutcome>> {
        let _serial = PURGE_SERIAL.lock().await;
        let principal = self.principal.clone();
        let keys: Vec<String> = self
            .main
            .read(ReadLane::Interactive, move |c| {
                let mut statement = c.prepare("SELECT operation_key FROM retention_purges WHERE principal_id=?1 AND phase<>'completed' ORDER BY requested_at,operation_key")?;
                let rows = statement
                    .query_map([principal], |r| r.get(0))?
                    .collect::<rusqlite::Result<Vec<String>>>()?;
                Ok(rows)
            })
            .await?;
        let mut outcomes = Vec::new();
        for key in keys {
            let result = self.drive(&key).await.map_err(|e| e.to_string());
            outcomes.push(PendingOutcome {
                operation_key: key,
                result,
            });
        }
        Ok(outcomes)
    }
    /// Resume a purge by key if it belongs to this principal and one of `scopes`.
    /// `Ok(None)` means no such purge is visible to the caller.
    pub async fn resume(&self, key: &str, scopes: &[String]) -> Result<Option<PurgeReceipt>> {
        let _serial = PURGE_SERIAL.lock().await;
        let probe = key.to_string();
        let Some(op) = self
            .main
            .read(ReadLane::Interactive, move |c| {
                if !table_exists(c, "retention_purges")? {
                    return Ok(None);
                }
                load_op(c, &probe)
            })
            .await?
        else {
            return Ok(None);
        };
        if op.principal != self.principal || !scopes.contains(&op.scope) {
            return Ok(None);
        }
        Ok(Some(self.drive(key).await?))
    }
    /// Completed receipt for a key, if any.
    pub async fn receipt(&self, key: &str) -> Result<Option<PurgeReceipt>> {
        let probe = key.to_string();
        let row = self
            .main
            .read(ReadLane::Interactive, move |c| load_op(c, &probe))
            .await?;
        match row {
            Some(op) if op.phase == Phase::Completed && op.principal == self.principal => {
                Ok(Some(self.receipt_of(&op).await?))
            }
            _ => Ok(None),
        }
    }
    /// Content-free tombstone explaining a broken reference, scoped to the caller's scope.
    pub async fn tombstone(
        &self,
        scope: &str,
        kind: SubjectKind,
        subject_id: &str,
    ) -> Result<Option<Tombstone>> {
        let (principal, scope, id) = (
            self.principal.clone(),
            scope.to_string(),
            subject_id.to_string(),
        );
        Ok(self
            .main
            .read(ReadLane::Interactive, move |c| {
                Ok(c.query_row(
                    "SELECT id,reason,purged_at FROM retention_tombstones WHERE principal_id=?1 AND scope_id=?2 AND subject_kind=?3 AND subject_id=?4",
                    rusqlite::params![principal, scope, kind.as_str(), id],
                    |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, i64>(2)?)),
                )
                .optional()?
                .map(|(tid, reason, purged_at)| Tombstone {
                    id: tid,
                    kind,
                    subject_id: id.clone(),
                    scope: scope.clone(),
                    reason: PurgeReason::parse(&reason).unwrap_or(PurgeReason::UserRequest),
                    purged_at,
                }))
            })
            .await?)
    }
    /// Content-free review flags (hash mismatches, structural matches) of a purge.
    pub async fn flags(&self, key: &str) -> Result<Vec<(String, String)>> {
        let key = key.to_string();
        Ok(self
            .main
            .read(ReadLane::Interactive, move |c| {
                let mut statement = c.prepare("SELECT code,location FROM retention_purge_flags WHERE operation_key=?1 ORDER BY code,location")?;
                let rows = statement
                    .query_map([key], |r| Ok((r.get(0)?, r.get(1)?)))?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                Ok(rows)
            })
            .await?)
    }

    async fn load(&self, key: &str) -> Result<OpRow> {
        let key = key.to_string();
        self.main
            .read(ReadLane::Interactive, move |c| load_op(c, &key))
            .await?
            .ok_or(Error::NotFound)
    }
    async fn receipt_of(&self, op: &OpRow) -> Result<PurgeReceipt> {
        let tombstone = self
            .tombstone(&op.scope, op.kind, &op.subject_id)
            .await?
            .ok_or(Error::Invalid("completed purge lacks its tombstone"))?;
        if tombstone.id != op.tombstone_id {
            return Err(Error::Invalid("tombstone does not match its purge"));
        }
        Ok(PurgeReceipt {
            operation_key: op.key.clone(),
            tombstone,
            counts: op.counts.clone(),
            hygiene: op.hygiene.clone().ok_or(Error::Invalid("hygiene missing"))?,
            telemetry_covered: op.telemetry_covered,
            notice: BackupNotice::standard(),
        })
    }
    async fn document_texts(&self, request: &PurgeRequest) -> Result<Vec<String>> {
        if request.kind != SubjectKind::Document {
            return Ok(Vec::new());
        }
        let (id, scope, principal) = (
            request.subject_id.clone(),
            request.scope.clone(),
            self.principal.clone(),
        );
        let originals: Vec<(String, String)> = self
            .main
            .read(ReadLane::Background, move |c| {
                let allowed: bool = c.query_row(
                    "SELECT EXISTS(SELECT 1 FROM documents WHERE id=?1 AND coalesce(scope_id,'legacy:unscoped')=?2 AND coalesce(principal_id,?3)=?3)",
                    [&id, &scope, &principal],
                    |r| r.get(0),
                )?;
                if !allowed {
                    return Ok(Vec::new());
                }
                let mut out = Vec::new();
                let mut statement = c.prepare("SELECT coalesce(kind,''),blob_hash FROM documents WHERE id=?1 AND blob_hash IS NOT NULL")?;
                for row in statement.query_map([&id], |r| Ok((r.get(0)?, r.get(1)?)))? {
                    out.push(row?);
                }
                drop(statement);
                if table_exists(c, "document_versions")? {
                    let mut statement = c.prepare("SELECT kind,blob_hash FROM document_versions WHERE document_id=?1")?;
                    for row in statement.query_map([&id], |r| Ok((r.get(0)?, r.get(1)?)))? {
                        out.push(row?);
                    }
                }
                Ok(out)
            })
            .await?;
        let mut texts = Vec::new();
        let mut seen = BTreeSet::new();
        for (media, hash) in originals {
            if !(media.starts_with("text/") || media == "application/json")
                || !seen.insert(hash.clone())
            {
                continue;
            }
            let Ok(hash) = hash.parse::<BlobHash>() else {
                continue;
            };
            if let Ok(bytes) = self.main.read_blob(hash, MAX_ORIGINAL_BYTES).await
                && let Ok(text) = String::from_utf8(bytes)
            {
                texts.push(text);
            }
        }
        Ok(texts)
    }
    async fn plan(&self, request: &PurgeRequest) -> Result<String> {
        let principal = self.principal.clone();
        let probe = request.clone();
        let existing = self
            .main
            .read(ReadLane::Interactive, move |c| {
                if !table_exists(c, "retention_purges")? {
                    return Ok(Existing::Absent);
                }
                existing_op(c, &principal, &probe)
            })
            .await?;
        match existing {
            Existing::Key(key) => return Ok(key),
            Existing::Conflict => return Err(Error::KeyConflict),
            Existing::Hidden => return Err(Error::NotFound),
            Existing::Absent => {}
        }
        let originals = self.document_texts(request).await?;
        let participants = self.participants.clone();
        let principal = self.principal.clone();
        let request = request.clone();
        enum Planned {
            Key(String, bool),
            Conflict,
            NotFound,
            Missing(&'static str),
            Active(String),
            Rejected(String),
        }
        let planned = self
            .main
            .write(Priority::Normal, move |c| {
                match existing_op(c, &principal, &request)? {
                    Existing::Key(key) => return Ok(Planned::Key(key, false)),
                    Existing::Conflict => return Ok(Planned::Conflict),
                    Existing::Hidden => return Ok(Planned::NotFound),
                    Existing::Absent => {}
                }
                for (schema, name) in REQUIRED_PARTICIPANTS {
                    if table_exists(c, schema)? && !participants.iter().any(|p| p.name() == *name)
                    {
                        return Ok(Planned::Missing(name));
                    }
                }
                let Some((mut snapshot, mut texts)) = load_subject(
                    c,
                    &principal,
                    request.kind,
                    &request.subject_id,
                    &request.scope,
                )?
                else {
                    return Ok(Planned::NotFound);
                };
                let mut collection = Collection::default();
                for participant in &participants {
                    participant.collect(c, &snapshot, &mut collection)?;
                }
                snapshot.members.extend(collection.derived);
                texts.extend(collection.texts);
                texts.extend(originals);
                let runs = linked_runs(c, &snapshot)?;
                if let Some(run) = active_run(c, &runs)? {
                    return Ok(Planned::Active(run));
                }
                if let Err(error) = trial_delete(c, &snapshot) {
                    return Ok(Planned::Rejected(error.to_string()));
                }
                let needles = derive_needles(&texts);
                let mut objects = BTreeSet::new();
                for member in &snapshot.members {
                    objects.insert((member.scope.clone(), member.id.clone()));
                    for prefix in VECTOR_PREFIXES {
                        objects.insert((member.scope.clone(), format!("{prefix}:{}", member.id)));
                    }
                }
                let hashes =
                    collect_blob_hashes(c, request.kind, &request.subject_id, &snapshot.members)?;
                let tombstone = format!("tmb_{}", ulid::Ulid::new());
                let counts = PurgeCounts {
                    runs_linked: runs.len() as u64,
                    ..PurgeCounts::default()
                };
                c.execute(
                    "INSERT INTO retention_purges(operation_key,tombstone_id,principal_id,scope_id,subject_kind,subject_id,reason,phase,requested_at,members,objects,runs,blob_hashes,counts) VALUES(?1,?2,?3,?4,?5,?6,?7,'planned',?8,?9,?10,?11,?12,?13)",
                    rusqlite::params![
                        request.operation_key,
                        tombstone,
                        principal,
                        request.scope,
                        request.kind.as_str(),
                        request.subject_id,
                        request.reason.as_str(),
                        request.requested_at,
                        serde_json::to_string(&snapshot.members).map_err(store_json)?,
                        serde_json::to_string(&objects).map_err(store_json)?,
                        json_list(runs),
                        json_list(hashes),
                        serde_json::to_string(&counts).map_err(store_json)?,
                    ],
                )?;
                for (ordinal, needle) in needles.iter().enumerate() {
                    c.execute(
                        "INSERT INTO retention_purge_needles(operation_key,ordinal,needle) VALUES(?1,?2,?3)",
                        rusqlite::params![request.operation_key, ordinal as i64, needle],
                    )?;
                }
                Ok(Planned::Key(request.operation_key.clone(), true))
            })
            .await?;
        match planned {
            Planned::Key(key, created) => {
                if created {
                    self.notify(PurgeBoundary::Planned);
                }
                Ok(key)
            }
            Planned::Conflict => Err(Error::KeyConflict),
            Planned::NotFound => Err(Error::NotFound),
            Planned::Missing(name) => Err(Error::ParticipantMissing(name)),
            Planned::Active(run) => Err(Error::ActiveRun(run)),
            Planned::Rejected(reason) => Err(Error::Rejected(reason)),
        }
    }
    async fn advance(
        &self,
        key: &str,
        from: Phase,
        to: Phase,
        telemetry: Option<bool>,
    ) -> Result<()> {
        let key = key.to_string();
        self.main
            .write(Priority::Normal, move |c| {
                c.execute(
                    "UPDATE retention_purges SET phase=?3,telemetry_covered=coalesce(?4,telemetry_covered),last_error=NULL WHERE operation_key=?1 AND phase=?2",
                    rusqlite::params![key, from.as_str(), to.as_str(), telemetry],
                )?;
                Ok(())
            })
            .await?;
        Ok(())
    }
    async fn record_error(&self, key: &str, error: &Error) {
        let (key, code) = (key.to_string(), error.code());
        let _ = self
            .main
            .write(Priority::Normal, move |c| {
                c.execute(
                    "UPDATE retention_purges SET last_error=?2 WHERE operation_key=?1",
                    [key, code],
                )?;
                Ok(())
            })
            .await;
    }
    async fn drive(&self, key: &str) -> Result<PurgeReceipt> {
        loop {
            let op = self.load(key).await?;
            let step = match op.phase {
                Phase::Planned => self.sweep_main(&op).await.map(|_| {
                    (
                        Phase::Planned,
                        Phase::Swept,
                        None,
                        Some(PurgeBoundary::Swept),
                    )
                }),
                Phase::Swept => self.delete_base(&op).await.map(|restart| {
                    if restart {
                        (Phase::Swept, Phase::Planned, None, None)
                    } else {
                        (
                            Phase::BaseDeleted,
                            Phase::BaseDeleted,
                            None,
                            Some(PurgeBoundary::BaseDeleted),
                        )
                    }
                }),
                Phase::BaseDeleted => async {
                    self.sweep_main(&op).await?;
                    self.sweep_telemetry(&op).await
                }
                .await
                .map(|covered| {
                    (
                        Phase::BaseDeleted,
                        Phase::TelemetrySwept,
                        Some(covered),
                        Some(PurgeBoundary::TelemetrySwept),
                    )
                }),
                Phase::TelemetrySwept => self.finalize(&op).await.map(|_| {
                    (
                        Phase::Finalized,
                        Phase::Finalized,
                        None,
                        Some(PurgeBoundary::Finalized),
                    )
                }),
                Phase::Finalized => self.release_blobs(&op).await.map(|_| {
                    (
                        Phase::BlobsReleased,
                        Phase::BlobsReleased,
                        None,
                        Some(PurgeBoundary::BlobsReleased),
                    )
                }),
                Phase::BlobsReleased => self.complete(&op).await.map(|_| {
                    (
                        Phase::Completed,
                        Phase::Completed,
                        None,
                        Some(PurgeBoundary::Completed),
                    )
                }),
                Phase::Completed => return self.receipt_of(&op).await,
            };
            match step {
                Ok((from, to, telemetry, boundary)) => {
                    if from != to {
                        self.advance(key, from, to, telemetry).await?;
                    }
                    if let Some(boundary) = boundary {
                        self.notify(boundary);
                    }
                }
                Err(error) => {
                    self.record_error(key, &error).await;
                    return Err(error);
                }
            }
        }
    }
    async fn matcher(&self, key: &str) -> Result<Arc<Matcher>> {
        let key = key.to_string();
        let needles = self
            .main
            .read(ReadLane::Background, move |c| load_needles(c, &key))
            .await?;
        Ok(Arc::new(Matcher::new(&needles)?))
    }
    /// Targeted pass over the principal database: inline journal payloads of linked runs,
    /// their payload blobs, and other rows of linked runs or referencing members.
    async fn sweep_main(&self, op: &OpRow) -> Result<()> {
        let matcher = self.matcher(&op.key).await?;
        if matcher.is_empty() {
            return Ok(());
        }
        let tables = self
            .main
            .read(ReadLane::Background, sweep_tables)
            .await?;
        let (runs, members) = (op.runs_json(), op.members_json());
        for table in tables {
            let mut after = 0;
            loop {
                let (t, m, r, s, p) = (
                    table.clone(),
                    matcher.clone(),
                    runs.clone(),
                    members.clone(),
                    self.principal.clone(),
                );
                let (hits, last) = self
                    .main
                    .read(ReadLane::Background, move |c| {
                        let targets = Targets {
                            runs_json: &r,
                            members_json: &s,
                            principal: &p,
                        };
                        scan_page(c, &t, &targets, after, PAGE, &m)
                    })
                    .await?;
                let Some(last) = last else { break };
                after = last;
                for chunk in hits.chunks(BATCH) {
                    let (t, m, r, s, p, key, rows) = (
                        table.clone(),
                        matcher.clone(),
                        runs.clone(),
                        members.clone(),
                        self.principal.clone(),
                        op.key.clone(),
                        chunk.to_vec(),
                    );
                    self.main
                        .write(Priority::Normal, move |c| {
                            with_secure_delete(c, false, || {
                                let targets = Targets {
                                    runs_json: &r,
                                    members_json: &s,
                                    principal: &p,
                                };
                                redact_main(c, &key, &t, &targets, &rows, &m)
                            })
                        })
                        .await?;
                    self.notify(PurgeBoundary::SweepBatch);
                }
            }
        }
        self.sweep_journal_blobs(op, &matcher).await
    }
    async fn sweep_journal_blobs(&self, op: &OpRow, matcher: &Arc<Matcher>) -> Result<()> {
        let mut after = 0i64;
        loop {
            let runs = op.runs_json();
            let page: Vec<(i64, String, String)> = self
                .main
                .read(ReadLane::Background, move |c| {
                    let mut statement = c.prepare("SELECT rowid,payload_blob,run_id FROM journal WHERE rowid>?1 AND payload_blob IS NOT NULL AND run_id IN (SELECT value FROM json_each(?2)) ORDER BY rowid LIMIT 256")?;
                    let rows = statement
                        .query_map(rusqlite::params![after, runs], |r| {
                            Ok((r.get(0)?, r.get(1)?, r.get(2)?))
                        })?
                        .collect::<rusqlite::Result<Vec<_>>>()?;
                    Ok(rows)
                })
                .await?;
            let Some(last) = page.last().map(|(rowid, _, _)| *rowid) else {
                break;
            };
            after = last;
            for (rowid, old, run) in page {
                let Ok(hash) = old.parse::<BlobHash>() else {
                    continue;
                };
                let bytes = match self.main.read_blob(hash, MAX_JOURNAL_BLOB).await {
                    Ok(bytes) => bytes,
                    Err(StoreError::BlobMissing(_)) => continue,
                    Err(error) => return Err(error.into()),
                };
                if !matcher.is_match(&bytes) {
                    continue;
                }
                let text = String::from_utf8_lossy(&bytes).into_owned();
                let redacted = match matcher.redact_value(&text) {
                    Redaction::Changed(redacted) => redacted,
                    Redaction::Unchanged => continue,
                    Redaction::Structured => {
                        let (key, location) = (op.key.clone(), format!("journal:{rowid}"));
                        self.main
                            .write(Priority::Normal, move |c| {
                                if flag(c, &key, "structured_match", &location)? {
                                    bump(c, &key, |n| n.structured_matches += 1)?;
                                }
                                Ok(())
                            })
                            .await?;
                        continue;
                    }
                };
                let payload: Value = serde_json::from_str(&redacted)?;
                let blobs = self.main.blobs();
                let bytes = redacted.into_bytes();
                let prepared = tokio::task::spawn_blocking(move || blobs.prepare(&bytes))
                    .await
                    .map_err(|_| Error::Invalid("blob preparation interrupted"))??;
                let key = op.key.clone();
                self.main
                    .write_with_blob(prepared, Priority::Normal, move |c, new| {
                        with_secure_delete(c, false, || {
                            let changed = guarded(c, "journal", "UPDATE", || {
                                Ok(c.execute(
                                    "UPDATE journal SET payload_blob=?1 WHERE rowid=?2 AND payload_blob=?3",
                                    rusqlite::params![new.as_str(), rowid, old],
                                )?)
                            })?;
                            if changed == 1 {
                                journal_follow(c, &key, rowid, &text, payload)?;
                                excalibur_journal::record_payload_redirect_in(
                                    c,
                                    &run,
                                    &old,
                                    new.as_str(),
                                )?;
                                merge_json_list(c, &key, "blob_hashes", [old.clone()])?;
                                bump(c, &key, |n| n.journal_events_redacted += 1)?;
                            }
                            Ok(())
                        })
                    })
                    .await?;
                self.notify(PurgeBoundary::SweepBatch);
            }
        }
        Ok(())
    }
    /// Telemetry rows of linked runs, verified re-sealing and re-export. Runs after the
    /// base deletion and the late principal-database pass, so late journal rows are
    /// already recorded. Returns whether telemetry was covered.
    async fn sweep_telemetry(&self, op: &OpRow) -> Result<bool> {
        let Some(telemetry) = &self.telemetry else {
            return Ok(false);
        };
        let tdb = telemetry.database().clone();
        let matcher = self.matcher(&op.key).await?;
        let tables = tdb.read(ReadLane::Background, sweep_tables).await?;
        let (runs, members) = (op.runs_json(), op.members_json());
        if !matcher.is_empty() {
            for table in tables.iter().filter(|t| t.run) {
                let mut after = 0;
                loop {
                    let (t, m, r, s, p) = (
                        table.clone(),
                        matcher.clone(),
                        runs.clone(),
                        members.clone(),
                        self.principal.clone(),
                    );
                    let (hits, last) = tdb
                        .read(ReadLane::Background, move |c| {
                            let targets = Targets {
                                runs_json: &r,
                                members_json: &s,
                                principal: &p,
                            };
                            scan_page(c, &t, &targets, after, PAGE, &m)
                        })
                        .await?;
                    let Some(last) = last else { break };
                    after = last;
                    for chunk in hits.chunks(BATCH) {
                        let (t, m, r, s, p, rows) = (
                            table.clone(),
                            matcher.clone(),
                            runs.clone(),
                            members.clone(),
                            self.principal.clone(),
                            chunk.to_vec(),
                        );
                        let stats = tdb
                            .write(Priority::Normal, move |c| {
                                with_secure_delete(c, false, || {
                                    let targets = Targets {
                                        runs_json: &r,
                                        members_json: &s,
                                        principal: &p,
                                    };
                                    redact_rows(c, &t, &targets, &rows, &m, &mut |_, _, _, _| {
                                        Ok(())
                                    })
                                })
                            })
                            .await?;
                        let key = op.key.clone();
                        self.main
                            .write(Priority::Normal, move |c| {
                                bump(c, &key, |n| {
                                    n.telemetry_rows_redacted += stats.rows;
                                    n.structured_matches += stats.structured;
                                })
                            })
                            .await?;
                    }
                }
            }
        }
        let key = op.key.clone();
        let events: Vec<(String, u64, Option<String>, Option<String>)> = self
            .main
            .read(ReadLane::Background, move |c| {
                let mut statement = c.prepare("SELECT run_id,sequence,old_offset_hash,new_offset_hash FROM retention_purge_events WHERE operation_key=?1 ORDER BY run_id,sequence")?;
                let rows = statement
                    .query_map([key], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                Ok(rows)
            })
            .await?;
        let offsets = tdb
            .read(ReadLane::Interactive, |c| {
                table_exists(c, "telemetry_offsets")
            })
            .await?;
        let (mut resealed, mut mismatched) = (0u64, Vec::new());
        if offsets {
            for (run, sequence, old, new) in events {
                let (Some(old), Some(new)) = (old, new) else {
                    continue;
                };
                let outcome = tdb
                    .write(Priority::Normal, move |c| {
                        let stored: Option<String> = c
                            .query_row(
                                "SELECT payload_hash FROM telemetry_offsets WHERE run_id=?1 AND source='journal' AND sequence=?2",
                                rusqlite::params![run, sequence],
                                |r| r.get(0),
                            )
                            .optional()?;
                        match stored {
                            None => Ok((0u64, None)),
                            Some(stored) if stored == new => Ok((0, None)),
                            Some(stored) if stored != old => Ok((0, Some(format!("{run}:{sequence}")))),
                            Some(_) => guarded(c, "telemetry_offsets", "UPDATE", || {
                                Ok((
                                    c.execute(
                                        "UPDATE telemetry_offsets SET payload_hash=?1 WHERE run_id=?2 AND source='journal' AND sequence=?3 AND payload_hash=?4",
                                        rusqlite::params![new, run, sequence, old],
                                    )? as u64,
                                    None,
                                ))
                            }),
                        }
                    })
                    .await?;
                resealed += outcome.0;
                mismatched.extend(outcome.1);
            }
        }
        let mut exported = 0u64;
        for run in &op.runs {
            let probe = run.clone();
            let registered: bool = tdb
                .read(ReadLane::Interactive, move |c| {
                    Ok(table_exists(c, "telemetry_runs")?
                        && c.query_row(
                            "SELECT EXISTS(SELECT 1 FROM telemetry_runs WHERE run_id=?1)",
                            [probe],
                            |r| r.get(0),
                        )?)
                })
                .await?;
            if registered {
                telemetry.export_run(run).await?;
                exported += 1;
            }
        }
        let key = op.key.clone();
        self.main
            .write(Priority::Normal, move |c| {
                let mut new_flags = 0;
                for location in &mismatched {
                    new_flags += u64::from(flag(c, &key, "telemetry_hash_mismatch", location)?);
                }
                bump(c, &key, |n| {
                    n.telemetry_offsets_resealed += resealed;
                    n.hash_mismatches += new_flags;
                    n.runs_reexported += exported;
                })
            })
            .await?;
        Ok(true)
    }
    /// Bounded base deletion. Returns `true` when newly discovered linked runs require
    /// another sweep before anything is deleted.
    async fn delete_base(&self, op: &OpRow) -> Result<bool> {
        let participants = self.participants.clone();
        let key = op.key.clone();
        // First command: re-check provenance and fences, then participant rows, vectors,
        // edges and blob references.
        let restart = self
            .main
            .write(Priority::Normal, move |c| {
                with_secure_delete(c, true, || {
                    let Some(op) = load_op(c, &key)? else {
                        return Err(StoreError::Invalid("purge disappeared"));
                    };
                    if op.phase != Phase::Swept {
                        return Ok(Ok(false));
                    }
                    let snapshot = op.snapshot();
                    let mut runs: BTreeSet<String> = op.runs.iter().cloned().collect();
                    let fresh = linked_runs(c, &snapshot)?;
                    if let Some(run) = active_run(c, &fresh)? {
                        return Ok(Err(run));
                    }
                    if !fresh.is_subset(&runs) {
                        runs.extend(fresh);
                        merge_json_list(c, &key, "runs", runs.iter().cloned())?;
                        bump(c, &key, |n| n.runs_linked = runs.len() as u64)?;
                        return Ok(Ok(true));
                    }
                    let mut participant_rows = 0;
                    for participant in &participants {
                        participant_rows += participant.purge(c, &snapshot)?;
                    }
                    let objects: BTreeSet<(String, String)> = op.objects.iter().cloned().collect();
                    let vectors = delete_vectors(c, &objects)?;
                    let ids = op.members_json();
                    let edges = if table_exists(c, "edges")? {
                        guarded(c, "edges", "DELETE", || {
                            Ok(c.execute(
                                "DELETE FROM edges WHERE source_id IN (SELECT value FROM json_each(?1)) OR destination_id IN (SELECT value FROM json_each(?1))",
                                [&ids],
                            )? as u64)
                        })?
                    } else {
                        0
                    };
                    let hashes = collect_blob_hashes(c, op.kind, &op.subject_id, &op.members)?;
                    merge_json_list(c, &key, "blob_hashes", hashes)?;
                    c.execute(
                        "DELETE FROM blob_refs WHERE owner_id IN (SELECT value FROM json_each(?1))",
                        [&ids],
                    )?;
                    bump(c, &key, |n| {
                        n.participant_rows_deleted += participant_rows;
                        n.vectors_deleted += vectors;
                        n.edges_deleted += edges;
                    })?;
                    Ok(Ok(false))
                })
            })
            .await?;
        match restart {
            Err(run) => return Err(Error::ActiveRun(run)),
            Ok(true) => return Ok(true),
            Ok(false) => {}
        }
        // Member rows (messages or chunks) in bounded batches.
        if let Some((table, kind)) = member_table(op.kind) {
            loop {
                let (key, ids) = (
                    op.key.clone(),
                    op.members_of_json(kind),
                );
                let deleted = self
                    .main
                    .write(Priority::Normal, move |c| {
                        with_secure_delete(c, true, || {
                            let batch: Vec<Value> = {
                                let mut statement = c.prepare(&format!(
                                    "SELECT id FROM {table} WHERE id IN (SELECT value FROM json_each(?1)) LIMIT {BATCH}"
                                ))?;
                                statement
                                    .query_map([&ids], |r| r.get::<_, String>(0))?
                                    .collect::<rusqlite::Result<Vec<_>>>()?
                                    .into_iter()
                                    .map(Value::String)
                                    .collect()
                            };
                            if batch.is_empty() {
                                return Ok(0);
                            }
                            let keys = ForeignKeys::load(c)?;
                            let mut stats = DeleteStats::default();
                            remove_rows(c, &keys, table, "id", &batch, 0, &mut stats)?;
                            bump(c, &key, |n| {
                                n.rows_deleted += stats.deleted;
                                n.rows_nulled += stats.nulled;
                                n.base_batches += 1;
                            })?;
                            Ok(batch.len())
                        })
                    })
                    .await?;
                if deleted == 0 {
                    break;
                }
                self.notify(PurgeBoundary::BaseDeleteBatch);
            }
        }
        // Final command: the subject row itself, then the phase change.
        let (key, observer, subject) = (op.key.clone(), self.observer.clone(), op.subject_id.clone());
        let kind = op.kind;
        self.main
            .write(Priority::Normal, move |c| {
                with_secure_delete(c, true, || {
                    let keys = ForeignKeys::load(c)?;
                    let mut stats = DeleteStats::default();
                    remove_rows(
                        c,
                        &keys,
                        subject_table(kind),
                        "id",
                        &[Value::String(subject)],
                        0,
                        &mut stats,
                    )?;
                    c.execute(
                        "UPDATE retention_purges SET phase='base_deleted',last_error=NULL WHERE operation_key=?1 AND phase='swept'",
                        [&key],
                    )?;
                    bump(c, &key, |n| {
                        n.rows_deleted += stats.deleted;
                        n.rows_nulled += stats.nulled;
                        n.base_batches += 1;
                    })?;
                    if let Some(observer) = &observer {
                        observer(PurgeBoundary::BaseDeleteUncommitted);
                    }
                    Ok(())
                })
            })
            .await?;
        Ok(false)
    }
    async fn finalize(&self, op: &OpRow) -> Result<()> {
        let key = op.key.clone();
        let now = (self.clock)();
        self.main
            .write(Priority::Normal, move |c| {
                with_secure_delete(c, false, || {
                    c.execute(
                        "INSERT OR IGNORE INTO retention_tombstones(id,principal_id,scope_id,subject_kind,subject_id,reason,purged_at) SELECT tombstone_id,principal_id,scope_id,subject_kind,subject_id,reason,?2 FROM retention_purges WHERE operation_key=?1",
                        rusqlite::params![key, now],
                    )?;
                    c.execute(
                        "DELETE FROM retention_purge_needles WHERE operation_key=?1",
                        [&key],
                    )?;
                    c.execute(
                        "UPDATE retention_purge_events SET old_offset_hash=NULL,new_offset_hash=NULL WHERE operation_key=?1",
                        [&key],
                    )?;
                    c.execute(
                        "UPDATE retention_purges SET phase='finalized',last_error=NULL WHERE operation_key=?1 AND phase='telemetry_swept'",
                        [&key],
                    )?;
                    Ok(())
                })
            })
            .await?;
        Ok(())
    }
    async fn release_blobs(&self, op: &OpRow) -> Result<()> {
        let (mut removed, mut shared) = (0u64, 0u64);
        for hash in &op.blob_hashes {
            let Ok(parsed) = hash.parse::<BlobHash>() else {
                continue;
            };
            match self.main.release_purged_blob(parsed).await? {
                BlobRelease::Removed | BlobRelease::Absent => removed += 1,
                BlobRelease::Shared => shared += 1,
            }
        }
        let key = op.key.clone();
        self.main
            .write(Priority::Normal, move |c| {
                let changed = c.execute(
                    "UPDATE retention_purges SET phase='blobs_released',blob_hashes='[]',last_error=NULL WHERE operation_key=?1 AND phase='finalized'",
                    [&key],
                )?;
                if changed == 1 {
                    bump(c, &key, |n| {
                        n.blobs_removed += removed;
                        n.blobs_shared += shared;
                    })?;
                }
                Ok(())
            })
            .await?;
        Ok(())
    }
    async fn complete(&self, op: &OpRow) -> Result<()> {
        let main_wal_truncated = self.main.checkpoint_truncate().await?;
        let telemetry_wal_truncated = match &self.telemetry {
            Some(telemetry) => Some(telemetry.database().checkpoint_truncate().await?),
            None => None,
        };
        let hygiene = Hygiene {
            secure_delete: true,
            vacuum: false,
            main_wal_truncated,
            telemetry_wal_truncated,
        };
        let text = serde_json::to_string(&hygiene)?;
        let key = op.key.clone();
        let now = (self.clock)();
        self.main
            .write(Priority::Normal, move |c| {
                c.execute(
                    "UPDATE retention_purges SET phase='completed',completed_at=?2,hygiene=?3,notice_code=?4,last_error=NULL WHERE operation_key=?1 AND phase='blobs_released'",
                    rusqlite::params![key, now, text, BACKUP_NOTICE_CODE],
                )?;
                Ok(())
            })
            .await?;
        Ok(())
    }
}
impl OpRow {
    fn members_of_json(&self, kind: MemberKind) -> String {
        json_list(
            self.members
                .iter()
                .filter(|m| m.kind == kind)
                .map(|m| m.id.clone()),
        )
    }
}
