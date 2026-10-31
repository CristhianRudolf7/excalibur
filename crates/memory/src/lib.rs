//! Scoped long-term memory, retrieval, consolidation, embeddings, identity documents and the
//! document library with deterministic citations. This crate is a library: hosts supply the
//! model-backed roles (extractor, decider, reranker, vision, transcription) through traits and
//! decide when its entry points run. No model, network or daemon is started here.
pub mod admission;
pub mod consolidation;
pub mod context;
pub mod curation;
pub mod embeddings;
pub mod graph;
pub mod identity;
pub mod library;
pub mod maintenance;
pub mod model_roles;
pub mod purge;
pub mod remote_embeddings;
pub mod retrieval;
pub mod runtime;
pub mod testing;
mod text;
pub mod tools;

pub use admission::*;
pub use consolidation::*;
pub use context::*;
pub use curation::*;
pub use embeddings::*;
pub use graph::*;
pub use identity::*;
pub use library::*;
pub use maintenance::*;
pub use model_roles::*;
pub use purge::MemoryPurgeParticipant;
pub use remote_embeddings::*;
pub use retrieval::*;
pub use runtime::*;
pub use tools::*;

use excalibur_store::{
    Database, Priority,
    rusqlite::{self, OptionalExtension},
};
use excalibur_vault::Redactor;
use serde::{Deserialize, Serialize};
use std::{collections::BTreeSet, sync::Arc};

pub use chrono_tz;
pub use excalibur_types::runtime::BoxFuture;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("memory storage: {0}")]
    Store(#[from] excalibur_store::Error),
    #[error("memory serialization failed")]
    Json(#[from] serde_json::Error),
    #[error("scope denied: {0}")]
    Scope(String),
    #[error("{0}")]
    Invalid(String),
    #[error("not found: {0}")]
    NotFound(String),
    #[error("unsupported: {0}")]
    Unsupported(String),
    #[error("unavailable: {0}")]
    Unavailable(String),
    #[error("rejected by ingestion guard: {0}")]
    Rejected(String),
}
impl From<rusqlite::Error> for Error {
    fn from(error: rusqlite::Error) -> Self {
        Self::Store(error.into())
    }
}
pub type Result<T> = std::result::Result<T, Error>;

pub(crate) fn invalid(message: impl Into<String>) -> Error {
    Error::Invalid(message.into())
}
pub(crate) fn new_id(prefix: &str) -> String {
    format!("{prefix}_{}", ulid::Ulid::new())
}
pub(crate) fn digest(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    format!("{:x}", Sha256::digest(bytes))
}

/// Memory record kinds (architecture 10.3).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryKind {
    Fact,
    Preference,
    Event,
    Commitment,
    Belief,
    Relation,
    Procedure,
}
impl MemoryKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Fact => "fact",
            Self::Preference => "preference",
            Self::Event => "event",
            Self::Commitment => "commitment",
            Self::Belief => "belief",
            Self::Relation => "relation",
            Self::Procedure => "procedure",
        }
    }
    pub fn parse(value: &str) -> Result<Self> {
        Ok(match value {
            "fact" => Self::Fact,
            "preference" => Self::Preference,
            "event" => Self::Event,
            "commitment" => Self::Commitment,
            "belief" => Self::Belief,
            "relation" => Self::Relation,
            "procedure" => Self::Procedure,
            _ => return Err(invalid(format!("unknown memory kind {value}"))),
        })
    }
    /// Initial decay half-lives in days (Appendix B starting values).
    pub fn half_life_days(self) -> f64 {
        match self {
            Self::Event => 7.0,
            Self::Preference | Self::Commitment => 90.0,
            Self::Fact | Self::Belief | Self::Relation | Self::Procedure => 365.0,
        }
    }
}

/// Lifecycle states. `Active` is the architecture's "vigente".
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryStatus {
    Candidate,
    Active,
    Expired,
    Superseded,
    Disputed,
    Retired,
}
impl MemoryStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Candidate => "candidate",
            Self::Active => "active",
            Self::Expired => "expired",
            Self::Superseded => "superseded",
            Self::Disputed => "disputed",
            Self::Retired => "retired",
        }
    }
    pub fn parse(value: &str) -> Result<Self> {
        Ok(match value {
            "candidate" => Self::Candidate,
            "active" => Self::Active,
            "expired" => Self::Expired,
            "superseded" => Self::Superseded,
            "disputed" => Self::Disputed,
            "retired" => Self::Retired,
            _ => return Err(invalid(format!("unknown memory status {value}"))),
        })
    }
}

/// Source trust. External content never becomes a preference or identity.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Trust {
    UntrustedExternal,
    Agent,
    User,
}
impl Trust {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Agent => "agent",
            Self::UntrustedExternal => "untrusted_external",
        }
    }
    pub fn parse(value: &str) -> Result<Self> {
        Ok(match value {
            "user" => Self::User,
            "agent" => Self::Agent,
            "untrusted_external" => Self::UntrustedExternal,
            _ => return Err(invalid(format!("unknown trust {value}"))),
        })
    }
    pub(crate) fn weight(self) -> f64 {
        match self {
            Self::User => 1.0,
            Self::Agent => 0.9,
            Self::UntrustedExternal => 0.75,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceKind {
    Message,
    ToolCall,
    Document,
    Page,
    Memory,
}
impl SourceKind {
    /// Content that originates outside the principal and the agent.
    pub fn external(self) -> bool {
        matches!(self, Self::Page)
    }
}

/// Provenance pointer kept with every memory.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct SourceRef {
    pub kind: SourceKind,
    pub id: String,
    /// Conversation for messages, or document version for documents, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<String>,
}
impl SourceRef {
    pub fn message(id: impl Into<String>) -> Self {
        Self {
            kind: SourceKind::Message,
            id: id.into(),
            context: None,
        }
    }
    pub fn page(id: impl Into<String>) -> Self {
        Self {
            kind: SourceKind::Page,
            id: id.into(),
            context: None,
        }
    }
}

/// Host-issued authority for one caller. Scopes are mandatory and explicit; nothing in this
/// crate widens them. `max_trust` caps what a caller may assert about its own writes.
#[derive(Clone, Debug)]
pub struct MemoryGrant {
    pub principal: String,
    pub scopes: BTreeSet<String>,
    pub run_id: Option<String>,
    pub conversation_id: Option<String>,
    pub task_id: Option<String>,
    pub timezone: chrono_tz::Tz,
    pub max_trust: Trust,
}
impl MemoryGrant {
    pub fn new(
        principal: impl Into<String>,
        scopes: impl IntoIterator<Item = impl Into<String>>,
    ) -> Self {
        Self {
            principal: principal.into(),
            scopes: scopes.into_iter().map(Into::into).collect(),
            run_id: None,
            conversation_id: None,
            task_id: None,
            timezone: chrono_tz::UTC,
            max_trust: Trust::Agent,
        }
    }
    pub fn with_run(mut self, run: impl Into<String>) -> Self {
        self.run_id = Some(run.into());
        self
    }
    pub fn with_conversation(mut self, conversation: impl Into<String>) -> Self {
        self.conversation_id = Some(conversation.into());
        self
    }
    pub fn with_task(mut self, task: impl Into<String>) -> Self {
        self.task_id = Some(task.into());
        self
    }
    pub fn with_timezone(mut self, timezone: chrono_tz::Tz) -> Self {
        self.timezone = timezone;
        self
    }
    pub fn with_max_trust(mut self, trust: Trust) -> Self {
        self.max_trust = trust;
        self
    }
    pub(crate) fn validate(&self) -> Result<()> {
        if self.principal.trim().is_empty() || self.principal.len() > 256 {
            return Err(Error::Scope("principal required".into()));
        }
        if self.scopes.is_empty() || self.scopes.len() > 64 {
            return Err(Error::Scope(
                "between 1 and 64 explicit scopes required".into(),
            ));
        }
        if self
            .scopes
            .iter()
            .any(|s| s.trim().is_empty() || s.len() > 512 || s.contains('\0'))
        {
            return Err(Error::Scope("invalid scope".into()));
        }
        Ok(())
    }
    /// Every requested scope must be granted. An empty request means all granted scopes.
    pub fn select(&self, requested: &[String]) -> Result<Vec<String>> {
        self.validate()?;
        if requested.is_empty() {
            return Ok(self.scopes.iter().cloned().collect());
        }
        let mut out = BTreeSet::new();
        for scope in requested {
            if !self.scopes.contains(scope) {
                return Err(Error::Scope(format!("{scope} is not granted")));
            }
            out.insert(scope.clone());
        }
        Ok(out.into_iter().collect())
    }
    pub fn require(&self, scope: &str) -> Result<()> {
        self.validate()?;
        if self.scopes.contains(scope) {
            Ok(())
        } else {
            Err(Error::Scope(format!("{scope} is not granted")))
        }
    }
}

/// Initial numeric parameters (Appendix B). All are host-overridable.
#[derive(Clone, Debug)]
pub struct MemoryConfig {
    pub duplicate_similarity: f32,
    pub ambiguous_similarity: f32,
    pub list_candidates: usize,
    pub rrf_k: f64,
    pub min_similarity: f32,
    /// Semantic neighbours below this similarity do not enter the candidate list.
    pub semantic_floor: f32,
    pub rerank_margin: f64,
    pub embedding_dimensions: u32,
    pub external_index_threshold: u64,
    pub segment_idle_seconds: i64,
    pub relation_proposal_min_facts: u32,
    pub relation_proposal_window_seconds: i64,
    pub relation_activation_uses: u32,
    pub profile_confirmation_evidence: u32,
    pub compaction_max_removed: f64,
    pub doc_edit_escalation_mismatches: u32,
}
impl Default for MemoryConfig {
    fn default() -> Self {
        Self {
            duplicate_similarity: 0.95,
            ambiguous_similarity: 0.80,
            list_candidates: 50,
            rrf_k: 60.0,
            min_similarity: 0.35,
            semantic_floor: 0.2,
            rerank_margin: 0.05,
            embedding_dimensions: 512,
            external_index_threshold: 500_000,
            segment_idle_seconds: 30 * 60,
            relation_proposal_min_facts: 5,
            relation_proposal_window_seconds: 28 * 86_400,
            relation_activation_uses: 5,
            profile_confirmation_evidence: 2,
            compaction_max_removed: 0.25,
            doc_edit_escalation_mismatches: 2,
        }
    }
}

/// Optional model-backed roles. Absent roles degrade deterministically.
#[derive(Clone, Default)]
pub struct MemoryServices {
    pub decider: Option<Arc<dyn AdmissionClassifier>>,
    pub cheap_classifier: Option<Arc<dyn AdmissionClassifier>>,
    pub reranker: Option<Arc<dyn Reranker>>,
    pub rewriter: Option<Arc<dyn QueryRewriter>>,
}

#[derive(Clone)]
pub struct MemoryStore {
    pub(crate) db: Database,
    pub(crate) redactor: Redactor,
    pub(crate) config: Arc<MemoryConfig>,
    pub(crate) services: MemoryServices,
    pub(crate) embeddings: EmbeddingRegistry,
    pub(crate) admission_lock: Arc<tokio::sync::Mutex<()>>,
    pub(crate) library: LibraryServices,
}
impl std::fmt::Debug for MemoryStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MemoryStore").finish_non_exhaustive()
    }
}

/// Ordered, checksummed additive migrations. Installed versions are never rewritten.
const SCHEMA: &[(i64, &str)] = &[
    (1, include_str!("schema.sql")),
    (2, include_str!("schema2.sql")),
    (3, include_str!("schema3.sql")),
];

impl MemoryStore {
    /// Install the checksummed additive schema on the principal database.
    pub async fn open(db: Database) -> Result<Self> {
        db.write(Priority::Journal, move |c| {
            c.execute_batch("CREATE TABLE IF NOT EXISTS memory_schema(version INTEGER PRIMARY KEY,checksum TEXT NOT NULL)")?;
            for (version, sql) in SCHEMA {
                let hash = digest(sql.as_bytes());
                let old: Option<String> = c
                    .query_row(
                        "SELECT checksum FROM memory_schema WHERE version=?1",
                        [version],
                        |r| r.get(0),
                    )
                    .optional()?;
                match old {
                    Some(old) if old != hash => {
                        return Err(excalibur_store::Error::Invalid(
                            "memory schema checksum mismatch",
                        ));
                    }
                    Some(_) => {}
                    None => {
                        // The writer runs every command inside one savepoint.
                        c.execute_batch(sql)?;
                        c.execute(
                            "INSERT INTO memory_schema(version,checksum) VALUES(?1,?2)",
                            rusqlite::params![version, hash],
                        )?;
                    }
                }
            }
            Ok(())
        })
        .await?;
        Ok(Self {
            embeddings: EmbeddingRegistry::new(db.clone()),
            db,
            redactor: Redactor::default(),
            config: Arc::new(MemoryConfig::default()),
            services: MemoryServices::default(),
            admission_lock: Arc::new(tokio::sync::Mutex::new(())),
            library: LibraryServices::default(),
        })
    }
    pub fn with_config(mut self, config: MemoryConfig) -> Self {
        self.config = Arc::new(config);
        self
    }
    pub fn with_services(mut self, services: MemoryServices) -> Self {
        self.services = services;
        self
    }
    pub fn with_redactor(mut self, redactor: Redactor) -> Self {
        self.redactor = redactor;
        self
    }
    pub fn database(&self) -> &Database {
        &self.db
    }
    pub fn config(&self) -> &MemoryConfig {
        &self.config
    }
    pub fn embeddings(&self) -> &EmbeddingRegistry {
        &self.embeddings
    }
    pub(crate) async fn ensure_principal(&self, principal: &str) -> Result<()> {
        let principal = principal.to_string();
        self.db
            .write(Priority::Normal, move |c| {
                c.execute(
                    "INSERT OR IGNORE INTO principals(id,name) VALUES(?1,?1)",
                    [&principal],
                )?;
                Ok(())
            })
            .await?;
        Ok(())
    }
}

/// Stored memory row.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Memory {
    pub id: String,
    pub principal: String,
    pub scope: String,
    pub kind: MemoryKind,
    pub statement: String,
    pub entities: Vec<String>,
    pub attribute: Option<String>,
    pub valid_from: Option<i64>,
    pub valid_to: Option<i64>,
    pub recorded_at: i64,
    pub invalidated_at: Option<i64>,
    pub expires_at: Option<i64>,
    pub status: MemoryStatus,
    pub superseded_by: Option<String>,
    pub confidence: f64,
    pub importance: f64,
    pub sources: Vec<SourceRef>,
    pub trust: Trust,
    pub last_used_at: Option<i64>,
    pub use_count: i64,
    pub origin_conversation: Option<String>,
    pub origin_task: Option<String>,
}
impl Memory {
    /// Read-time validity: expiry is enforced even before consolidation records it.
    pub fn current_at(&self, now: i64) -> bool {
        matches!(self.status, MemoryStatus::Active | MemoryStatus::Disputed)
            && self.expires_at.is_none_or(|e| e > now)
    }
}

pub(crate) const MEMORY_COLUMNS: &str = "id,COALESCE(principal_id,''),COALESCE(scope_id,''),COALESCE(kind,'fact'),statement,entities,attribute,valid_from,valid_to,recorded_at,invalidated_at,expires_at,state,superseded_by,COALESCE(confidence,0.5),COALESCE(importance,0.5),sources,trust,last_used_at,usage_count,origin_conversation,origin_task";

pub(crate) fn memory_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Memory> {
    let kind: String = row.get(3)?;
    let state: String = row.get(12)?;
    let trust: String = row.get(17)?;
    let entities: String = row.get(5)?;
    let sources: String = row.get(16)?;
    Ok(Memory {
        id: row.get(0)?,
        principal: row.get(1)?,
        scope: row.get(2)?,
        kind: MemoryKind::parse(&kind).unwrap_or(MemoryKind::Fact),
        statement: row.get(4)?,
        entities: serde_json::from_str(&entities).unwrap_or_default(),
        attribute: row.get(6)?,
        valid_from: row.get(7)?,
        valid_to: row.get(8)?,
        recorded_at: row.get(9)?,
        invalidated_at: row.get(10)?,
        expires_at: row.get(11)?,
        status: MemoryStatus::parse(&state).unwrap_or(MemoryStatus::Retired),
        superseded_by: row.get(13)?,
        confidence: row.get(14)?,
        importance: row.get(15)?,
        sources: serde_json::from_str(&sources).unwrap_or_default(),
        trust: Trust::parse(&trust).unwrap_or(Trust::UntrustedExternal),
        last_used_at: row.get(18)?,
        use_count: row.get(19)?,
        origin_conversation: row.get(20)?,
        origin_task: row.get(21)?,
    })
}

pub(crate) fn load_memory(c: &rusqlite::Connection, id: &str) -> rusqlite::Result<Option<Memory>> {
    c.query_row(
        &format!("SELECT {MEMORY_COLUMNS} FROM memories WHERE id=?1"),
        [id],
        memory_from_row,
    )
    .optional()
}

pub(crate) fn transition(
    c: &rusqlite::Connection,
    id: &str,
    from: Option<MemoryStatus>,
    to: MemoryStatus,
    reason: &str,
    now: i64,
) -> rusqlite::Result<()> {
    c.execute(
        "UPDATE memories SET state=?2,updated_at=?3 WHERE id=?1",
        rusqlite::params![id, to.as_str(), now],
    )?;
    c.execute(
        "INSERT INTO memory_transitions(memory_id,from_state,to_state,reason,at) VALUES(?1,?2,?3,?4,?5)",
        rusqlite::params![id, from.map(|s| s.as_str()), to.as_str(), reason, now],
    )?;
    Ok(())
}

pub(crate) fn placeholders(start: usize, count: usize) -> String {
    (start..start + count)
        .map(|i| format!("?{i}"))
        .collect::<Vec<_>>()
        .join(",")
}

impl MemoryStore {
    /// Read one memory inside the caller's grant.
    pub async fn memory(&self, grant: &MemoryGrant, id: &str) -> Result<Option<Memory>> {
        grant.validate()?;
        let id = id.to_string();
        let memory = self
            .db
            .read(excalibur_store::ReadLane::Interactive, move |c| {
                Ok(load_memory(c, &id)?)
            })
            .await?;
        Ok(memory.filter(|m| m.principal == grant.principal && grant.scopes.contains(&m.scope)))
    }
    /// Full status history, oldest first.
    pub async fn transitions(
        &self,
        grant: &MemoryGrant,
        id: &str,
    ) -> Result<Vec<(Option<String>, String, String, i64)>> {
        if self.memory(grant, id).await?.is_none() {
            return Err(Error::NotFound(id.into()));
        }
        let id = id.to_string();
        Ok(self
            .db
            .read(excalibur_store::ReadLane::Interactive, move |c| {
                let mut s = c.prepare("SELECT from_state,to_state,reason,at FROM memory_transitions WHERE memory_id=?1 ORDER BY id")?;
                let rows = s
                    .query_map([&id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                Ok(rows)
            })
            .await?)
    }
    /// Pending low-priority questions for the user (disputes, undecidable contradictions).
    pub async fn open_questions(&self, grant: &MemoryGrant) -> Result<Vec<MemoryQuestion>> {
        let scopes = grant.select(&[])?;
        let principal = grant.principal.clone();
        Ok(self
            .db
            .read(excalibur_store::ReadLane::Interactive, move |c| {
                let sql = format!("SELECT id,scope_id,memory_ids,question,priority FROM memory_questions WHERE principal_id=?1 AND state='open' AND scope_id IN ({}) ORDER BY created_at,id", placeholders(2, scopes.len()));
                let mut s = c.prepare(&sql)?;
                let mut params: Vec<&dyn rusqlite::ToSql> = vec![&principal];
                for scope in &scopes {
                    params.push(scope);
                }
                let rows = s
                    .query_map(params.as_slice(), |r| {
                        let ids: String = r.get(2)?;
                        Ok(MemoryQuestion {
                            id: r.get(0)?,
                            scope: r.get(1)?,
                            memory_ids: serde_json::from_str(&ids).unwrap_or_default(),
                            question: r.get(3)?,
                            priority: r.get(4)?,
                        })
                    })?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                Ok(rows)
            })
            .await?)
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MemoryQuestion {
    pub id: String,
    pub scope: String,
    pub memory_ids: Vec<String>,
    pub question: String,
    pub priority: String,
}
