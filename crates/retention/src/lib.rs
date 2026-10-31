//! Shared-core retention and user-requested purge (architecture §4.7).
//!
//! A purge is a keyed durable operation stored in the principal database. Each phase
//! commits its own progress, so a process killed at any boundary resumes with the same
//! key and converges. Content needed to find derived copies is kept only until the base
//! rows are deleted; afterwards only identifiers, counters and a tombstone remain.
mod policy;
mod sweep;

pub use policy::{
    ExpiryReport, HoldReason, RetentionClock, RetentionPolicy, RetentionSweeper,
};
pub use sweep::{MIN_SEGMENT_NEEDLE, MIN_TEXT_NEEDLE, REDACTION_MARK};

use excalibur_journal::{Event, Journal};
use excalibur_store::{
    BlobHash, BlobRelease, Database, DatabaseKind, Error as StoreError, Priority, ReadLane,
    rusqlite::{self, Connection, OptionalExtension},
};
use excalibur_telemetry::Telemetry;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};
use sweep::{
    DeleteStats, Filter, ForeignKeys, Matcher, columns, delete_vectors, derive_needles, guarded,
    max_rowid, rebuild_fts, redact_rows, remove_rows, scan_page, sweep_tables, table_exists,
    with_secure_delete,
};

/// Weekly backups are kept four weeks (§4.6); daily and pre-update copies rotate sooner.
pub const MAX_BACKUP_AGE_DAYS: u32 = 28;
pub const BACKUP_NOTICE_CODE: &str = "backups_retain_until_rotation";
const PAGE: u32 = 512;
const BATCH: usize = 64;
const MAX_ORIGINAL_BYTES: u64 = 8 * 1024 * 1024;

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
    #[error("operation key already names a different purge")]
    KeyConflict,
    #[error("a registered purge participant is required: {0}")]
    ParticipantMissing(&'static str),
    #[error("invalid purge request: {0}")]
    Invalid(&'static str),
}
impl From<rusqlite::Error> for Error {
    fn from(error: rusqlite::Error) -> Self {
        Self::Store(error.into())
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
    /// Delete owned rows; called once, in the same transaction as the base deletion.
    /// Tables named in `touched` get their external-content FTS5 indexes rebuilt.
    fn purge(
        &self,
        c: &Connection,
        subject: &SubjectSnapshot,
        touched: &mut Vec<String>,
    ) -> excalibur_store::Result<u64>;
}
/// Schemas whose derived rows require a named participant before any purge proceeds.
const REQUIRED_PARTICIPANTS: &[(&str, &str)] = &[("memory_schema", "memory")];

/// Interruption points exercised by crash tests. `BaseDeleteUncommitted` runs inside the
/// open base-deletion transaction; all others run after their phase committed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PurgeBoundary {
    Planned,
    MainSweepBatch,
    MainSwept,
    JournalBlobsSwept,
    TelemetrySwept,
    BaseDeleteUncommitted,
    BaseDeleted,
    BlobsReleased,
    Completed,
}
pub type PurgeObserver = Arc<dyn Fn(PurgeBoundary) + Send + Sync>;
pub type Clock = Arc<dyn Fn() -> i64 + Send + Sync>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    Planned,
    MainSwept,
    JournalBlobsSwept,
    TelemetrySwept,
    BaseDeleted,
    BlobsReleased,
    Completed,
}
impl Phase {
    fn as_str(self) -> &'static str {
        match self {
            Self::Planned => "planned",
            Self::MainSwept => "main_swept",
            Self::JournalBlobsSwept => "journal_blobs_swept",
            Self::TelemetrySwept => "telemetry_swept",
            Self::BaseDeleted => "base_deleted",
            Self::BlobsReleased => "blobs_released",
            Self::Completed => "completed",
        }
    }
    fn parse(text: &str) -> Option<Self> {
        [
            Self::Planned,
            Self::MainSwept,
            Self::JournalBlobsSwept,
            Self::TelemetrySwept,
            Self::BaseDeleted,
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
    pub rows_deleted: u64,
    pub rows_nulled: u64,
    pub participant_rows_deleted: u64,
    pub rows_redacted: u64,
    pub journal_events_redacted: u64,
    pub telemetry_rows_redacted: u64,
    pub telemetry_offsets_resealed: u64,
    pub vectors_deleted: u64,
    pub edges_deleted: u64,
    pub blobs_removed: u64,
    pub blobs_shared: u64,
    pub fts_rebuilt: u64,
    pub runs_reexported: u64,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hygiene {
    /// `PRAGMA secure_delete=ON` was active for every purge write.
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

#[derive(Clone, Debug)]
struct OpRow {
    key: String,
    tombstone_id: String,
    scope: String,
    kind: SubjectKind,
    subject_id: String,
    phase: Phase,
    members: Vec<Member>,
    scopes: Vec<String>,
    watermarks: BTreeMap<String, i64>,
    blob_hashes: Vec<String>,
    runs: Vec<String>,
    counts: PurgeCounts,
    hygiene: Option<Hygiene>,
    telemetry_covered: bool,
}
impl OpRow {
    fn snapshot(&self, principal: &str) -> SubjectSnapshot {
        SubjectSnapshot {
            kind: self.kind,
            id: self.subject_id.clone(),
            principal: principal.into(),
            scope: self.scope.clone(),
            members: self.members.clone(),
        }
    }
    fn scopes_json(&self) -> String {
        serde_json::to_string(&self.scopes).unwrap_or_else(|_| "[]".into())
    }
}
fn store_json(error: serde_json::Error) -> StoreError {
    StoreError::Configuration(format!("retention state: {error}"))
}
fn load_op(c: &Connection, key: &str) -> excalibur_store::Result<Option<OpRow>> {
    let row = c
        .query_row(
            "SELECT tombstone_id,scope_id,subject_kind,subject_id,phase,members,scopes,watermarks,blob_hashes,runs,counts,hygiene,telemetry_covered FROM retention_purges WHERE operation_key=?1",
            [key],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, String>(4)?,
                    [
                        r.get::<_, String>(5)?,
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
    let Some((tombstone_id, scope, kind, subject_id, phase, json, hygiene, telemetry_covered)) = row
    else {
        return Ok(None);
    };
    Ok(Some(OpRow {
        key: key.into(),
        tombstone_id,
        scope,
        kind: SubjectKind::parse(&kind).ok_or(StoreError::Invalid("unknown purge kind"))?,
        subject_id,
        phase: Phase::parse(&phase).ok_or(StoreError::Invalid("unknown purge phase"))?,
        members: serde_json::from_str(&json[0]).map_err(store_json)?,
        scopes: serde_json::from_str(&json[1]).map_err(store_json)?,
        watermarks: serde_json::from_str(&json[2]).map_err(store_json)?,
        blob_hashes: serde_json::from_str(&json[3]).map_err(store_json)?,
        runs: serde_json::from_str(&json[4]).map_err(store_json)?,
        counts: serde_json::from_str(&json[5]).map_err(store_json)?,
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
fn id_list(ids: impl IntoIterator<Item = String>) -> String {
    Value::Array(ids.into_iter().map(Value::String).collect()).to_string()
}

/// Record a redacted journal event and re-seal hashes that other crates keep over it.
fn journal_follow(
    c: &Connection,
    key: &str,
    rowid: i64,
    payload: Option<Value>,
) -> excalibur_store::Result<()> {
    let row: Option<(Option<String>, u64, String, Option<String>, Option<String>)> = c
        .query_row(
            "SELECT run_id,sequence,kind,idempotency_key,payload FROM journal WHERE rowid=?1",
            [rowid],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
        )
        .optional()?;
    let Some((Some(run), sequence, kind, event_key, inline)) = row else {
        return Ok(());
    };
    c.execute(
        "INSERT OR IGNORE INTO retention_purge_events(operation_key,run_id,sequence) VALUES(?1,?2,?3)",
        rusqlite::params![key, run, sequence],
    )?;
    if !table_exists(c, "task_observed_events")? {
        return Ok(());
    }
    let payload = match (payload, inline) {
        (Some(payload), _) => payload,
        (None, Some(text)) => serde_json::from_str(&text).map_err(store_json)?,
        (None, None) => return Ok(()),
    };
    let event = Event {
        sequence,
        kind,
        key: event_key.unwrap_or_default(),
        payload,
    };
    let hash = sha256_hex(&serde_json::to_vec(&event).map_err(store_json)?);
    guarded(c, "task_observed_events", "UPDATE", || {
        Ok(c.execute(
            "UPDATE task_observed_events SET payload_hash=?1 WHERE run_id=?2 AND sequence=?3",
            rusqlite::params![hash, run, sequence],
        )?)
    })?;
    Ok(())
}
/// Redact rows of one principal-database table and record follow-up work.
fn redact_main(
    c: &Connection,
    key: &str,
    table: &sweep::SweepTable,
    filter: &Filter<'_>,
    rowids: &[i64],
    matcher: &Matcher,
) -> excalibur_store::Result<()> {
    let mut journal_rows = 0u64;
    let changed = redact_rows(c, table, filter, rowids, matcher, &mut |c, name, rowid, cols| {
        c.execute(
            "INSERT OR IGNORE INTO retention_purge_tables(operation_key,table_name) VALUES(?1,?2)",
            [key, &name.to_ascii_lowercase()],
        )?;
        if name.eq_ignore_ascii_case("journal") && cols.iter().any(|c| c == "payload") {
            journal_follow(c, key, rowid, None)?;
            journal_rows += 1;
        }
        Ok(())
    })?;
    if changed > 0 {
        bump(c, key, |n| {
            n.rows_redacted += changed;
            n.journal_events_redacted += journal_rows;
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
        return Ok(
            if p == principal
                && scope == request.scope
                && kind == request.kind.as_str()
                && id == request.subject_id
            {
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
    let owner: Option<(String, String)> = match kind {
        SubjectKind::Conversation => c
            .query_row(
                "SELECT coalesce(principal_id,?2),coalesce(scope_id,active_scope,'legacy:unscoped') FROM conversations WHERE id=?1",
                [id, principal],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?,
        SubjectKind::Document => c
            .query_row(
                "SELECT coalesce(principal_id,?2),coalesce(scope_id,'legacy:unscoped') FROM documents WHERE id=?1",
                [id, principal],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?,
        SubjectKind::Memory => c
            .query_row(
                "SELECT coalesce(principal_id,?2),coalesce(scope_id,'legacy:unscoped') FROM memories WHERE id=?1",
                [id, principal],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?,
    };
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
                    Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?))
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
            let document_columns = columns(c, "documents")?;
            if document_columns.iter().any(|c| c == "title") {
                texts.push(c.query_row(
                    "SELECT coalesce(title,'') FROM documents WHERE id=?1",
                    [id],
                    |r| r.get(0),
                )?);
            }
            let chunk_columns = columns(c, "doc_chunks")?;
            let context = if chunk_columns.iter().any(|c| c == "context") {
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
    let ids = id_list(members.iter().map(|m| m.id.clone()));
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
        rusqlite::params![key, id_list(set)],
    )?;
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
        return Err(Error::Invalid("retention state lives in the principal database"));
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
    /// Purge one subject. Replaying the same key (or the same subject) returns the
    /// stored receipt; an interrupted operation resumes from its last committed phase.
    pub async fn purge(&self, request: PurgeRequest) -> Result<PurgeReceipt> {
        let valid = |s: &str, max: usize| !s.trim().is_empty() && s.len() <= max && !s.contains('\0');
        if !valid(&request.operation_key, 256)
            || !valid(&request.scope, 4096)
            || !valid(&request.subject_id, 512)
        {
            return Err(Error::Invalid("operation key, scope and subject are required"));
        }
        let _serial = PURGE_SERIAL.lock().await;
        let key = self.plan(&request).await?;
        self.drive(&key).await
    }
    /// Resume every unfinished purge of this principal, for example at host startup.
    pub async fn resume_pending(&self) -> Result<Vec<PurgeReceipt>> {
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
        let mut receipts = Vec::new();
        for key in keys {
            receipts.push(self.drive(&key).await?);
        }
        Ok(receipts)
    }
    /// Completed receipt for a key, if any.
    pub async fn receipt(&self, key: &str) -> Result<Option<PurgeReceipt>> {
        let key = key.to_string();
        let row = self
            .main
            .read(ReadLane::Interactive, {
                let key = key.clone();
                move |c| load_op(c, &key)
            })
            .await?;
        match row {
            Some(op) if op.phase == Phase::Completed => Ok(Some(self.receipt_of(&op).await?)),
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
            if !(media.starts_with("text/") || media == "application/json") || !seen.insert(hash.clone())
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
                    if table_exists(c, schema)? && !participants.iter().any(|p| p.name() == *name) {
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
                let needles = derive_needles(&texts);
                let scopes: BTreeSet<String> = snapshot
                    .members
                    .iter()
                    .map(|m| m.scope.clone())
                    .chain([request.scope.clone()])
                    .collect();
                let mut watermarks = BTreeMap::new();
                for table in sweep_tables(c, true)? {
                    watermarks.insert(table.name.clone(), max_rowid(c, &table.name)?);
                }
                let hashes =
                    collect_blob_hashes(c, request.kind, &request.subject_id, &snapshot.members)?;
                let tombstone = format!("tmb_{}", ulid::Ulid::new());
                c.execute(
                    "INSERT INTO retention_purges(operation_key,tombstone_id,principal_id,scope_id,subject_kind,subject_id,reason,phase,requested_at,members,scopes,watermarks,blob_hashes) VALUES(?1,?2,?3,?4,?5,?6,?7,'planned',?8,?9,?10,?11,?12)",
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
                        id_list(scopes),
                        serde_json::to_string(&watermarks).map_err(store_json)?,
                        id_list(hashes),
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
        }
    }
    async fn advance(&self, key: &str, from: Phase, to: Phase, telemetry: Option<bool>) -> Result<()> {
        let key = key.to_string();
        self.main
            .write(Priority::Normal, move |c| {
                c.execute(
                    "UPDATE retention_purges SET phase=?3,telemetry_covered=coalesce(?4,telemetry_covered) WHERE operation_key=?1 AND phase=?2",
                    rusqlite::params![key, from.as_str(), to.as_str(), telemetry],
                )?;
                Ok(())
            })
            .await?;
        Ok(())
    }
    async fn drive(&self, key: &str) -> Result<PurgeReceipt> {
        loop {
            let op = self.load(key).await?;
            match op.phase {
                Phase::Planned => {
                    self.sweep_main(&op).await?;
                    self.advance(key, Phase::Planned, Phase::MainSwept, None)
                        .await?;
                    self.notify(PurgeBoundary::MainSwept);
                }
                Phase::MainSwept => {
                    self.sweep_journal_blobs(&op).await?;
                    self.advance(key, Phase::MainSwept, Phase::JournalBlobsSwept, None)
                        .await?;
                    self.notify(PurgeBoundary::JournalBlobsSwept);
                }
                Phase::JournalBlobsSwept => {
                    let covered = self.sweep_telemetry(&op).await?;
                    self.advance(
                        key,
                        Phase::JournalBlobsSwept,
                        Phase::TelemetrySwept,
                        Some(covered),
                    )
                    .await?;
                    self.notify(PurgeBoundary::TelemetrySwept);
                }
                Phase::TelemetrySwept => {
                    self.delete_base(&op).await?;
                    self.notify(PurgeBoundary::BaseDeleted);
                }
                Phase::BaseDeleted => {
                    self.release_blobs(&op).await?;
                    self.notify(PurgeBoundary::BlobsReleased);
                }
                Phase::BlobsReleased => {
                    self.complete(&op).await?;
                    self.notify(PurgeBoundary::Completed);
                }
                Phase::Completed => return self.receipt_of(&op).await,
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
    async fn sweep_main(&self, op: &OpRow) -> Result<()> {
        let matcher = self.matcher(&op.key).await?;
        if matcher.is_empty() {
            return Ok(());
        }
        let tables = self
            .main
            .read(ReadLane::Background, |c| sweep_tables(c, true))
            .await?;
        let scopes = op.scopes_json();
        for table in tables {
            let mut after = 0;
            loop {
                let (t, m, s, p) = (
                    table.clone(),
                    matcher.clone(),
                    scopes.clone(),
                    self.principal.clone(),
                );
                let (hits, last) = self
                    .main
                    .read(ReadLane::Background, move |c| {
                        let filter = Filter {
                            scopes_json: &s,
                            principal: &p,
                        };
                        scan_page(c, &t, &filter, after, PAGE, &m)
                    })
                    .await?;
                let Some(last) = last else { break };
                after = last;
                for chunk in hits.chunks(BATCH) {
                    let (t, m, s, p, key, rows) = (
                        table.clone(),
                        matcher.clone(),
                        scopes.clone(),
                        self.principal.clone(),
                        op.key.clone(),
                        chunk.to_vec(),
                    );
                    self.main
                        .write(Priority::Normal, move |c| {
                            with_secure_delete(c, || {
                                let filter = Filter {
                                    scopes_json: &s,
                                    principal: &p,
                                };
                                redact_main(c, &key, &t, &filter, &rows, &m)
                            })
                        })
                        .await?;
                    self.notify(PurgeBoundary::MainSweepBatch);
                }
            }
        }
        Ok(())
    }
    async fn sweep_journal_blobs(&self, op: &OpRow) -> Result<()> {
        let matcher = self.matcher(&op.key).await?;
        if matcher.is_empty() {
            return Ok(());
        }
        let mut after = 0i64;
        loop {
            let (scopes, principal) = (op.scopes_json(), self.principal.clone());
            let page: Vec<(i64, String)> = self
                .main
                .read(ReadLane::Background, move |c| {
                    let mut statement = c.prepare("SELECT rowid,payload_blob FROM journal WHERE rowid>?1 AND payload_blob IS NOT NULL AND (scope_id IS NULL OR scope_id IN (SELECT value FROM json_each(?2))) AND (principal_id IS NULL OR principal_id=?3) ORDER BY rowid LIMIT 256")?;
                    let rows = statement
                        .query_map(rusqlite::params![after, scopes, principal], |r| {
                            Ok((r.get(0)?, r.get(1)?))
                        })?
                        .collect::<rusqlite::Result<Vec<_>>>()?;
                    Ok(rows)
                })
                .await?;
            let Some(last) = page.last().map(|(rowid, _)| *rowid) else {
                break;
            };
            after = last;
            for (rowid, old) in page {
                let Ok(hash) = old.parse::<BlobHash>() else {
                    continue;
                };
                let bytes = match self.main.read_blob(hash, 16 * 1024 * 1024).await {
                    Ok(bytes) => bytes,
                    Err(StoreError::BlobMissing(_)) => continue,
                    Err(error) => return Err(error.into()),
                };
                if !matcher.is_match(&bytes) {
                    continue;
                }
                let text = String::from_utf8_lossy(&bytes).into_owned();
                let redacted = matcher
                    .redact_text(&text, REDACTION_MARK)
                    .unwrap_or(text);
                let payload: Value = serde_json::from_str(&redacted)?;
                let blobs = self.main.blobs();
                let bytes = redacted.into_bytes();
                let prepared = tokio::task::spawn_blocking(move || blobs.prepare(&bytes))
                    .await
                    .map_err(|_| Error::Invalid("blob preparation interrupted"))??;
                let key = op.key.clone();
                self.main
                    .write_with_blob(prepared, Priority::Normal, move |c, new| {
                        with_secure_delete(c, || {
                            let changed = guarded(c, "journal", "UPDATE", || {
                                Ok(c.execute(
                                    "UPDATE journal SET payload_blob=?1 WHERE rowid=?2 AND payload_blob=?3",
                                    rusqlite::params![new.as_str(), rowid, old],
                                )?)
                            })?;
                            if changed == 1 {
                                journal_follow(c, &key, rowid, Some(payload))?;
                                merge_json_list(c, &key, "blob_hashes", [old.clone()])?;
                                c.execute("INSERT OR IGNORE INTO retention_purge_tables(operation_key,table_name) VALUES(?1,'journal')", [&key])?;
                                bump(c, &key, |n| n.journal_events_redacted += 1)?;
                            }
                            Ok(())
                        })
                    })
                    .await?;
            }
        }
        Ok(())
    }
    /// Returns whether telemetry was covered.
    async fn sweep_telemetry(&self, op: &OpRow) -> Result<bool> {
        let Some(telemetry) = &self.telemetry else {
            return Ok(false);
        };
        let tdb = telemetry.database().clone();
        let matcher = self.matcher(&op.key).await?;
        let tables = tdb
            .read(ReadLane::Background, |c| sweep_tables(c, false))
            .await?;
        let scopes = op.scopes_json();
        // Pass 1 persists affected runs before any change, so re-export survives a crash.
        let mut runs: BTreeSet<String> = op.runs.iter().cloned().collect();
        if !matcher.is_empty() {
            for table in tables.iter().filter(|t| t.columns.iter().any(|c| c == "run_id")) {
                let mut after = 0;
                loop {
                    let (t, m, s, p) = (
                        table.clone(),
                        matcher.clone(),
                        scopes.clone(),
                        self.principal.clone(),
                    );
                    let (found, last) = tdb
                        .read(ReadLane::Background, move |c| {
                            let filter = Filter {
                                scopes_json: &s,
                                principal: &p,
                            };
                            let (hits, last) = scan_page(c, &t, &filter, after, PAGE, &m)?;
                            let mut found = Vec::new();
                            for rowid in hits {
                                let run: Option<String> = c.query_row(
                                    &format!("SELECT run_id FROM {} WHERE rowid=?1", sweep::quote(&t.name)),
                                    [rowid],
                                    |r| r.get(0),
                                )?;
                                found.extend(run);
                            }
                            Ok((found, last))
                        })
                        .await?;
                    runs.extend(found);
                    let Some(last) = last else { break };
                    after = last;
                }
            }
        }
        let key = op.key.clone();
        let journal_runs: Vec<(String, u64)> = self
            .main
            .read(ReadLane::Background, move |c| {
                let mut statement = c.prepare("SELECT run_id,sequence FROM retention_purge_events WHERE operation_key=?1 ORDER BY run_id,sequence")?;
                let rows = statement
                    .query_map([key], |r| Ok((r.get(0)?, r.get(1)?)))?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                Ok(rows)
            })
            .await?;
        runs.extend(journal_runs.iter().map(|(run, _)| run.clone()));
        let (key, list) = (op.key.clone(), runs.clone());
        self.main
            .write(Priority::Normal, move |c| merge_json_list(c, &key, "runs", list))
            .await?;
        // Pass 2 rewrites matching telemetry rows.
        if !matcher.is_empty() {
            for table in &tables {
                let mut after = 0;
                loop {
                    let (t, m, s, p) = (
                        table.clone(),
                        matcher.clone(),
                        scopes.clone(),
                        self.principal.clone(),
                    );
                    let (hits, last) = tdb
                        .read(ReadLane::Background, move |c| {
                            let filter = Filter {
                                scopes_json: &s,
                                principal: &p,
                            };
                            scan_page(c, &t, &filter, after, PAGE, &m)
                        })
                        .await?;
                    let Some(last) = last else { break };
                    after = last;
                    for chunk in hits.chunks(BATCH) {
                        let (t, m, s, p, rows) = (
                            table.clone(),
                            matcher.clone(),
                            scopes.clone(),
                            self.principal.clone(),
                            chunk.to_vec(),
                        );
                        let changed = tdb
                            .write(Priority::Normal, move |c| {
                                with_secure_delete(c, || {
                                    let filter = Filter {
                                        scopes_json: &s,
                                        principal: &p,
                                    };
                                    redact_rows(c, &t, &filter, &rows, &m, &mut |_, _, _, _| Ok(()))
                                })
                            })
                            .await?;
                        let key = op.key.clone();
                        self.main
                            .write(Priority::Normal, move |c| {
                                bump(c, &key, |n| n.telemetry_rows_redacted += changed)
                            })
                            .await?;
                    }
                }
            }
        }
        // Re-seal journal-sourced offsets so reconciling the redacted journal is idempotent.
        let offsets = tdb
            .read(ReadLane::Interactive, |c| table_exists(c, "telemetry_offsets"))
            .await?;
        let mut resealed = 0u64;
        if offsets {
            for (run, sequence) in journal_runs {
                let Some(event) = self
                    .journal
                    .entries(&run, sequence.saturating_sub(1), 1)
                    .await?
                    .into_iter()
                    .find(|e| e.sequence == sequence)
                else {
                    continue;
                };
                let hash = excalibur_telemetry::journal_offset_hash(&event)?;
                resealed += tdb
                    .write(Priority::Normal, move |c| {
                        guarded(c, "telemetry_offsets", "UPDATE", || {
                            Ok(c.execute(
                                "UPDATE telemetry_offsets SET payload_hash=?1 WHERE run_id=?2 AND source='journal' AND sequence=?3 AND payload_hash<>?1",
                                rusqlite::params![hash, run, sequence],
                            )? as u64)
                        })
                    })
                    .await?;
            }
        }
        // Regenerate exported run artifacts from the redacted database.
        let mut exported = 0u64;
        for run in runs {
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
                telemetry.export_run(&run).await?;
                exported += 1;
            }
        }
        let key = op.key.clone();
        self.main
            .write(Priority::Normal, move |c| {
                bump(c, &key, |n| {
                    n.telemetry_offsets_resealed += resealed;
                    n.runs_reexported += exported;
                })
            })
            .await?;
        Ok(true)
    }
    async fn delete_base(&self, op: &OpRow) -> Result<()> {
        let key = op.key.clone();
        let principal = self.principal.clone();
        let participants = self.participants.clone();
        let observer = self.observer.clone();
        let now = (self.clock)();
        self.main
            .write(Priority::Normal, move |c| {
                with_secure_delete(c, || {
                    let Some(op) = load_op(c, &key)? else {
                        return Err(StoreError::Invalid("purge disappeared"));
                    };
                    if op.phase != Phase::TelemetrySwept {
                        return Ok(());
                    }
                    let scopes = op.scopes_json();
                    let filter = Filter {
                        scopes_json: &scopes,
                        principal: &principal,
                    };
                    // Copies written after planning are caught in the same transaction.
                    let matcher = Matcher::new(&load_needles(c, &key)?)?;
                    if !matcher.is_empty() {
                        for table in sweep_tables(c, true)? {
                            let mut after = op.watermarks.get(&table.name).copied().unwrap_or(0);
                            loop {
                                let (hits, last) = scan_page(c, &table, &filter, after, PAGE, &matcher)?;
                                let Some(last) = last else { break };
                                after = last;
                                redact_main(c, &key, &table, &filter, &hits, &matcher)?;
                            }
                        }
                    }
                    let snapshot = op.snapshot(&principal);
                    let mut participant_rows = 0;
                    let mut touched = Vec::new();
                    for participant in &participants {
                        participant_rows += participant.purge(c, &snapshot, &mut touched)?;
                    }
                    let ids: BTreeSet<String> = op.members.iter().map(|m| m.id.clone()).collect();
                    let vectors = delete_vectors(c, &ids, &op.scopes)?;
                    let id_json = id_list(ids.iter().cloned());
                    let edges = if table_exists(c, "edges")? {
                        guarded(c, "edges", "DELETE", || {
                            Ok(c.execute(
                                "DELETE FROM edges WHERE (source_id IN (SELECT value FROM json_each(?1)) OR destination_id IN (SELECT value FROM json_each(?1))) AND (scope_id IS NULL OR scope_id IN (SELECT value FROM json_each(?2)))",
                                [&id_json, &scopes],
                            )? as u64)
                        })?
                    } else {
                        0
                    };
                    let mut hashes: BTreeSet<String> = op.blob_hashes.iter().cloned().collect();
                    hashes.extend(collect_blob_hashes(c, op.kind, &op.subject_id, &op.members)?);
                    c.execute(
                        "DELETE FROM blob_refs WHERE owner_id IN (SELECT value FROM json_each(?1))",
                        [&id_json],
                    )?;
                    let keys = ForeignKeys::load(c)?;
                    let mut stats = DeleteStats::default();
                    let of = |kind: MemberKind| -> Vec<Value> {
                        op.members
                            .iter()
                            .filter(|m| m.kind == kind)
                            .map(|m| Value::String(m.id.clone()))
                            .collect()
                    };
                    let subject = vec![Value::String(op.subject_id.clone())];
                    match op.kind {
                        SubjectKind::Conversation => {
                            remove_rows(c, &keys, "messages", "id", &of(MemberKind::Message), 0, &mut stats)?;
                            remove_rows(c, &keys, "conversations", "id", &subject, 0, &mut stats)?;
                        }
                        SubjectKind::Document => {
                            remove_rows(c, &keys, "doc_chunks", "id", &of(MemberKind::Chunk), 0, &mut stats)?;
                            remove_rows(c, &keys, "documents", "id", &subject, 0, &mut stats)?;
                        }
                        SubjectKind::Memory => {
                            remove_rows(c, &keys, "memories", "id", &subject, 0, &mut stats)?;
                        }
                    }
                    let mut modified: BTreeSet<String> = stats
                        .tables
                        .iter()
                        .chain(touched.iter())
                        .map(|t| t.to_ascii_lowercase())
                        .collect();
                    {
                        let mut statement = c.prepare("SELECT table_name FROM retention_purge_tables WHERE operation_key=?1")?;
                        for table in statement.query_map([&key], |r| r.get::<_, String>(0))? {
                            modified.insert(table?);
                        }
                    }
                    let fts = rebuild_fts(c, &modified)?;
                    c.execute(
                        "INSERT OR IGNORE INTO retention_tombstones(id,principal_id,scope_id,subject_kind,subject_id,reason,purged_at) SELECT tombstone_id,principal_id,scope_id,subject_kind,subject_id,reason,?2 FROM retention_purges WHERE operation_key=?1",
                        rusqlite::params![key, now],
                    )?;
                    c.execute("DELETE FROM retention_purge_needles WHERE operation_key=?1", [&key])?;
                    c.execute(
                        "UPDATE retention_purges SET phase='base_deleted',blob_hashes=?2 WHERE operation_key=?1",
                        rusqlite::params![key, id_list(hashes)],
                    )?;
                    bump(c, &key, |n| {
                        n.rows_deleted += stats.deleted;
                        n.rows_nulled += stats.nulled;
                        n.participant_rows_deleted += participant_rows;
                        n.vectors_deleted += vectors;
                        n.edges_deleted += edges;
                        n.fts_rebuilt += fts;
                    })?;
                    if let Some(observer) = &observer {
                        observer(PurgeBoundary::BaseDeleteUncommitted);
                    }
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
                    "UPDATE retention_purges SET phase='blobs_released',blob_hashes='[]' WHERE operation_key=?1 AND phase='base_deleted'",
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
                    "UPDATE retention_purges SET phase='completed',completed_at=?2,hygiene=?3,notice_code=?4,watermarks='{}' WHERE operation_key=?1 AND phase='blobs_released'",
                    rusqlite::params![key, now, text, BACKUP_NOTICE_CODE],
                )?;
                Ok(())
            })
            .await?;
        Ok(())
    }
}
