//! Memory writes and admission (architecture 10.5). Updates never destroy: a refinement adds
//! a record and supersedes the previous one; undecidable conflicts stay disputed with a question.
use crate::{
    BoxFuture, Error, IndexOutcome, Memory, MemoryGrant, MemoryKind, MemoryStatus, MemoryStore,
    Result, SourceRef, Trust,
    embeddings::{EmbeddingRegistry, cosine_i8},
    graph::{mentioned_entities, resolve_entity, scope_entities},
    invalid, load_memory, new_id, placeholders,
    text::{fts_or, normalize, query_terms},
    transition,
};
use excalibur_store::{Priority, ReadLane, rusqlite};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::BTreeSet;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MemoryInput {
    pub scope: String,
    pub kind: MemoryKind,
    pub statement: String,
    #[serde(default)]
    pub entities: Vec<String>,
    #[serde(default)]
    pub attribute: Option<String>,
    #[serde(default)]
    pub valid_from: Option<i64>,
    #[serde(default)]
    pub valid_to: Option<i64>,
    #[serde(default)]
    pub expires_at: Option<i64>,
    #[serde(default)]
    pub confidence: Option<f64>,
    #[serde(default)]
    pub importance: Option<f64>,
    #[serde(default)]
    pub sources: Vec<SourceRef>,
    #[serde(default = "default_trust")]
    pub trust: Trust,
    /// Explicit temporal change ("now I live in..."); detected from wording when not set.
    #[serde(default)]
    pub temporal_change: bool,
}
fn default_trust() -> Trust {
    Trust::Agent
}
impl MemoryInput {
    pub fn new(scope: impl Into<String>, kind: MemoryKind, statement: impl Into<String>) -> Self {
        Self {
            scope: scope.into(),
            kind,
            statement: statement.into(),
            entities: Vec::new(),
            attribute: None,
            valid_from: None,
            valid_to: None,
            expires_at: None,
            confidence: None,
            importance: None,
            sources: Vec::new(),
            trust: Trust::Agent,
            temporal_change: false,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Classification {
    Duplicate,
    Refinement,
    Contradiction { temporal_change: bool },
    New,
}

/// Decider or cheap-model classifier for the ambiguous similarity band.
pub trait AdmissionClassifier: Send + Sync {
    fn name(&self) -> String;
    fn classify<'a>(
        &'a self,
        candidate: &'a Memory,
        existing: &'a Memory,
        similarity: f32,
    ) -> BoxFuture<'a, std::result::Result<Classification, String>>;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AdmissionDecision {
    Duplicate,
    Refinement,
    Contradiction,
    New,
    /// Ambiguous with no classifier available: the record stays a candidate.
    Deferred,
}
impl AdmissionDecision {
    fn as_str(self) -> &'static str {
        match self {
            Self::Duplicate => "duplicate",
            Self::Refinement => "refinement",
            Self::Contradiction => "contradiction",
            Self::New => "new",
            Self::Deferred => "deferred",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AdmissionResult {
    /// The record that now carries the fact (the existing one for duplicates).
    pub memory_id: String,
    pub candidate_id: String,
    pub decision: AdmissionDecision,
    pub status: MemoryStatus,
    pub similarity: Option<f32>,
    pub against: Option<String>,
    pub classifier: String,
    pub question_id: Option<String>,
    pub downgraded_from_preference: bool,
    pub semantic_pending: bool,
}

fn temporal_cue(text: &str) -> bool {
    static CUE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    CUE.get_or_init(|| {
        regex::Regex::new(r"\b(now|currently|no longer|anymore|moved|since|ahora|actualmente|ya no|me mude|desde|nowadays|hoy en dia)\b").unwrap()
    })
    .is_match(&normalize(text))
}

struct Neighbor {
    memory: Memory,
    similarity: f32,
}

impl MemoryStore {
    /// Explicit, immediate write ("remember that..."): admission runs now.
    pub async fn memory_write(
        &self,
        grant: &MemoryGrant,
        input: MemoryInput,
        now: i64,
    ) -> Result<AdmissionResult> {
        let (id, downgraded, index) = self.insert_candidate(grant, input, false, now).await?;
        let mut result = self.admit(&grant.principal, &id, now).await?;
        result.downgraded_from_preference = downgraded;
        result.semantic_pending |= !index.queued.is_empty() && index.indexed.is_empty();
        Ok(result)
    }
    /// Proposal during work: retrievable only in the originating conversation or task until
    /// consolidation admits it.
    pub async fn memory_propose(
        &self,
        grant: &MemoryGrant,
        input: MemoryInput,
        now: i64,
    ) -> Result<String> {
        if grant.conversation_id.is_none() && grant.task_id.is_none() {
            return Err(invalid(
                "a proposal needs an originating conversation or task",
            ));
        }
        if input.sources.is_empty() {
            return Err(invalid("a proposal needs its source"));
        }
        Ok(self.insert_candidate(grant, input, true, now).await?.0)
    }

    pub(crate) async fn insert_candidate(
        &self,
        grant: &MemoryGrant,
        input: MemoryInput,
        bind_origin: bool,
        now: i64,
    ) -> Result<(String, bool, IndexOutcome)> {
        grant.require(&input.scope)?;
        let statement = self.redactor.redact_text(input.statement.trim());
        if statement.is_empty() || statement.len() > 8192 {
            return Err(invalid("statement must contain 1 to 8192 bytes"));
        }
        let mut trust = input.trust.min(grant.max_trust);
        if input.sources.iter().any(|s| s.kind.external()) {
            trust = Trust::UntrustedExternal;
        }
        let mut kind = input.kind;
        let downgraded = trust == Trust::UntrustedExternal && kind == MemoryKind::Preference;
        if downgraded {
            kind = MemoryKind::Fact;
        }
        let clamp = |v: Option<f64>| v.unwrap_or(0.5).clamp(0.0, 1.0);
        self.ensure_principal(&grant.principal).await?;
        let id = new_id("mem");
        let temporal = input.temporal_change || temporal_cue(&statement);
        let (principal, id2, scope, statement2) = (
            grant.principal.clone(),
            id.clone(),
            input.scope.clone(),
            statement.clone(),
        );
        let (conversation, task) = if bind_origin {
            (grant.conversation_id.clone(), grant.task_id.clone())
        } else {
            (None, None)
        };
        let input2 = input.clone();
        self.db
            .write(Priority::Normal, move |c| {
                let mut entities = BTreeSet::new();
                for reference in &input2.entities {
                    if !reference.trim().is_empty() {
                        entities.insert(resolve_entity(c, &principal, &scope, reference, now)?);
                    }
                }
                let known = scope_entities(c, &principal, std::slice::from_ref(&scope))?;
                entities.extend(mentioned_entities(&known, &statement2));
                let entity_json = serde_json::to_string(&entities).unwrap();
                let sources = serde_json::to_string(&input2.sources).unwrap();
                c.execute(
                    "INSERT INTO memories(id,kind,statement,entities,valid_from,valid_to,recorded_at,expires_at,state,confidence,importance,sources,source_confidence,principal_id,scope_id,trust,attribute,origin_conversation,origin_task,normalized,created_at,updated_at) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,'candidate',?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?7,?7)",
                    rusqlite::params![id2, kind.as_str(), statement2, entity_json, input2.valid_from, input2.valid_to, now, input2.expires_at, clamp(input2.confidence), clamp(input2.importance), sources, trust.weight(), principal, scope, trust.as_str(), input2.attribute.as_deref().map(normalize), conversation, task, normalize(&statement2)],
                )?;
                for entity in &entities {
                    c.execute("INSERT OR IGNORE INTO memory_entity_links(memory_id,entity_id) VALUES(?1,?2)", rusqlite::params![id2, entity])?;
                }
                c.execute("INSERT INTO memory_transitions(memory_id,from_state,to_state,reason,at) VALUES(?1,NULL,'candidate',?2,?3)", rusqlite::params![id2, if temporal { "recorded; temporal change" } else { "recorded" }, now])?;
                Ok(())
            })
            .await?;
        let index = self
            .embeddings
            .index(
                &grant.principal,
                &input.scope,
                &format!("memory:{id}"),
                &statement,
                now,
            )
            .await?;
        if !index.queued.is_empty() || index.indexed.is_empty() {
            let id3 = id.clone();
            self.db
                .write(Priority::Normal, move |c| {
                    c.execute(
                        "UPDATE memories SET similarity_pending=1 WHERE id=?1",
                        [&id3],
                    )?;
                    Ok(())
                })
                .await?;
        }
        Ok((id, downgraded, index))
    }

    async fn neighbors(&self, candidate: &Memory) -> Result<(Vec<Neighbor>, bool)> {
        let c2 = candidate.clone();
        let (pool, own) = self
            .db
            .read(ReadLane::Interactive, move |c| {
                let mut ids = BTreeSet::new();
                if !c2.entities.is_empty() {
                    let sql = format!("SELECT DISTINCT l.memory_id FROM memory_entity_links l JOIN memories m ON m.id=l.memory_id WHERE m.principal_id=?1 AND m.scope_id=?2 AND m.state IN ('active','disputed') AND m.id!=?3 AND l.entity_id IN ({}) LIMIT 50", placeholders(4, c2.entities.len()));
                    let mut s = c.prepare(&sql)?;
                    let mut params: Vec<&dyn rusqlite::ToSql> = vec![&c2.principal, &c2.scope, &c2.id];
                    for e in &c2.entities {
                        params.push(e);
                    }
                    for id in s.query_map(params.as_slice(), |r| r.get::<_, String>(0))? {
                        ids.insert(id?);
                    }
                }
                if let Some(q) = fts_or(&query_terms(&c2.statement)) {
                    let mut s = c.prepare("SELECT m.id FROM memories_fts f JOIN memories m ON m.rowid=f.rowid WHERE memories_fts MATCH ?1 AND m.principal_id=?2 AND m.scope_id=?3 AND m.state IN ('active','disputed') AND m.id!=?4 ORDER BY bm25(memories_fts) LIMIT 20")?;
                    for id in s.query_map(rusqlite::params![q, c2.principal, c2.scope, c2.id], |r| r.get::<_, String>(0))? {
                        ids.insert(id?);
                    }
                }
                if let Some(attribute) = &c2.attribute {
                    let mut s = c.prepare("SELECT id FROM memories WHERE principal_id=?1 AND scope_id=?2 AND attribute=?3 AND state IN ('active','disputed') AND id!=?4 LIMIT 50")?;
                    for id in s.query_map(rusqlite::params![c2.principal, c2.scope, attribute, c2.id], |r| r.get::<_, String>(0))? {
                        ids.insert(id?);
                    }
                }
                let own = EmbeddingRegistry::stored_vector(c, &c2.scope, &format!("memory:{}", c2.id))?;
                let mut pool = Vec::new();
                for id in ids {
                    if let Some(m) = load_memory(c, &id)? {
                        pool.push(m);
                    }
                }
                Ok((pool, own))
            })
            .await?;
        let mut neighbors: Vec<Neighbor> = Vec::new();
        let mut semantic = false;
        let mut ids: BTreeSet<String> = pool.iter().map(|m| m.id.clone()).collect();
        let mut pool = pool;
        if let Some((space, vector)) = own {
            semantic = true;
            let spaces = self.embeddings.spaces().await?;
            if let Some(space) = spaces.into_iter().find(|s| s.id == space) {
                let found = self
                    .embeddings
                    .search_prepared(
                        std::slice::from_ref(&candidate.scope),
                        &[(space, vector.clone())],
                        20,
                        Some("memory:"),
                    )
                    .await?;
                for hit in found.hits {
                    let id = hit.object_id.trim_start_matches("memory:").to_string();
                    if id != candidate.id && ids.insert(id.clone()) {
                        let id2 = id.clone();
                        if let Some(m) = self
                            .db
                            .read(ReadLane::Interactive, move |c| Ok(load_memory(c, &id2)?))
                            .await?
                            && m.principal == candidate.principal
                            && matches!(m.status, MemoryStatus::Active | MemoryStatus::Disputed)
                        {
                            pool.push(m);
                        }
                    }
                }
            }
            for m in pool.drain(..) {
                let (scope, object) = (m.scope.clone(), format!("memory:{}", m.id));
                let other = self
                    .db
                    .read(ReadLane::Interactive, move |c| {
                        Ok(EmbeddingRegistry::stored_vector(c, &scope, &object)?)
                    })
                    .await?;
                let similarity = other.map(|(_, v)| cosine_i8(&vector, &v)).unwrap_or(0.0);
                neighbors.push(Neighbor {
                    memory: m,
                    similarity,
                });
            }
        } else {
            for m in pool {
                let similarity = if m.statement.trim() == candidate.statement.trim()
                    || normalize(&m.statement) == normalize(&candidate.statement)
                {
                    1.0
                } else {
                    0.0
                };
                neighbors.push(Neighbor {
                    memory: m,
                    similarity,
                });
            }
        }
        neighbors.sort_by(|a, b| {
            b.similarity
                .total_cmp(&a.similarity)
                .then_with(|| a.memory.id.cmp(&b.memory.id))
        });
        Ok((neighbors, semantic))
    }

    async fn classify(
        &self,
        candidate: &Memory,
        existing: &Memory,
        similarity: f32,
    ) -> Option<(Classification, String)> {
        for classifier in [&self.services.decider, &self.services.cheap_classifier]
            .into_iter()
            .flatten()
        {
            if let Ok(class) = classifier.classify(candidate, existing, similarity).await {
                return Some((class, classifier.name()));
            }
        }
        None
    }

    /// Admit a stored candidate: duplicate, refinement, contradiction, new, or deferred.
    pub async fn admit(
        &self,
        principal: &str,
        candidate_id: &str,
        now: i64,
    ) -> Result<AdmissionResult> {
        let _guard = self.admission_lock.lock().await;
        let id = candidate_id.to_string();
        let candidate = self
            .db
            .read(ReadLane::Interactive, move |c| Ok(load_memory(c, &id)?))
            .await?
            .filter(|m| m.principal == principal)
            .ok_or_else(|| Error::NotFound(candidate_id.into()))?;
        if candidate.status != MemoryStatus::Candidate {
            return Err(invalid("only candidates can be admitted"));
        }
        let temporal = {
            let id = candidate.id.clone();
            self.db
                .read(ReadLane::Interactive, move |c| {
                    Ok(c.query_row("SELECT EXISTS(SELECT 1 FROM memory_transitions WHERE memory_id=?1 AND reason LIKE '%temporal%')", [&id], |r| r.get::<_, bool>(0))?)
                })
                .await?
        };
        let (neighbors, semantic) = self.neighbors(&candidate).await?;
        let shares = |m: &Memory| {
            let a: BTreeSet<_> = candidate.entities.iter().collect();
            let b: BTreeSet<_> = m.entities.iter().collect();
            (a.is_empty() && b.is_empty()) || a.intersection(&b).next().is_some()
        };
        let config = self.config.clone();
        let mut decision = AdmissionDecision::New;
        let mut against: Option<&Memory> = None;
        let mut similarity = neighbors.first().map(|n| n.similarity);
        let mut classifier = "deterministic".to_string();
        let mut temporal_change = temporal;
        if let Some(top) = neighbors.first() {
            if top.similarity >= config.duplicate_similarity && shares(&top.memory) {
                decision = AdmissionDecision::Duplicate;
                against = Some(&top.memory);
            } else if top.similarity >= config.ambiguous_similarity {
                against = Some(&top.memory);
                match self.classify(&candidate, &top.memory, top.similarity).await {
                    Some((class, name)) => {
                        classifier = name;
                        decision = match class {
                            Classification::Duplicate => AdmissionDecision::Duplicate,
                            Classification::Refinement => AdmissionDecision::Refinement,
                            Classification::Contradiction { temporal_change: t } => {
                                temporal_change |= t;
                                AdmissionDecision::Contradiction
                            }
                            Classification::New => {
                                against = None;
                                AdmissionDecision::New
                            }
                        };
                    }
                    None => {
                        classifier = "unavailable".into();
                        decision = AdmissionDecision::Deferred;
                    }
                }
            }
        }
        // Same entity and attribute with a different statement is a deterministic conflict.
        if decision == AdmissionDecision::New
            && let Some(attribute) = &candidate.attribute
            && let Some(n) = neighbors.iter().find(|n| {
                n.memory.attribute.as_ref() == Some(attribute)
                    && shares(&n.memory)
                    && normalize(&n.memory.statement) != normalize(&candidate.statement)
            })
        {
            decision = AdmissionDecision::Contradiction;
            against = Some(&n.memory);
            similarity = Some(n.similarity);
        }
        // A lower-trust assertion cannot replace or invalidate a user assertion merely
        // by claiming that a temporal change happened. Keep it pending for review.
        if against.is_some_and(|other| candidate.trust < other.trust)
            && matches!(
                decision,
                AdmissionDecision::Refinement | AdmissionDecision::Contradiction
            )
        {
            decision = AdmissionDecision::Deferred;
            classifier = "trust_ceiling".into();
        }
        let against = against.cloned();
        let mut question_id = None;
        let candidate2 = candidate.clone();
        let against2 = against.clone();
        let detail = json!({"semantic": semantic, "temporal_change": temporal_change, "neighbors": neighbors.iter().take(5).map(|n| json!({"id": n.memory.id, "similarity": n.similarity})).collect::<Vec<_>>()});
        let classifier2 = classifier.clone();
        let (memory_id, status, qid) = self
            .db
            .write(Priority::Normal, move |c| {
                let candidate = candidate2;
                let current = load_memory(c, &candidate.id)?;
                if current.map(|m| m.status) != Some(MemoryStatus::Candidate) {
                    return Err(excalibur_store::Error::Invalid("candidate changed during admission"));
                }
                if let Some(other) = &against2 {
                    let fresh = load_memory(c, &other.id)?;
                    if !fresh.is_some_and(|m| matches!(m.status, MemoryStatus::Active | MemoryStatus::Disputed)) {
                        return Err(excalibur_store::Error::Invalid("neighbor changed during admission"));
                    }
                }
                let mut question = None;
                let (memory_id, status) = match (decision, &against2) {
                    (AdmissionDecision::Duplicate, Some(other)) => {
                        let mut sources: BTreeSet<SourceRef> = other.sources.iter().cloned().collect();
                        sources.extend(candidate.sources.iter().cloned());
                        c.execute(
                            "UPDATE memories SET sources=?2,confidence=max(COALESCE(confidence,0),?3),updated_at=?4 WHERE id=?1",
                            rusqlite::params![other.id, serde_json::to_string(&sources).unwrap(), candidate.confidence, now],
                        )?;
                        c.execute("UPDATE memories SET superseded_by=?2,invalidated_at=?3 WHERE id=?1", rusqlite::params![candidate.id, other.id, now])?;
                        transition(c, &candidate.id, Some(MemoryStatus::Candidate), MemoryStatus::Superseded, "duplicate merged into existing record", now)?;
                        (other.id.clone(), other.status)
                    }
                    (AdmissionDecision::Refinement, Some(other))
                    | (AdmissionDecision::Contradiction, Some(other)) if decision == AdmissionDecision::Refinement || temporal_change => {
                        transition(c, &candidate.id, Some(MemoryStatus::Candidate), MemoryStatus::Active, decision.as_str(), now)?;
                        let end = candidate.valid_from.unwrap_or(now);
                        c.execute("UPDATE memories SET superseded_by=?2,valid_to=COALESCE(valid_to,?3),invalidated_at=?4 WHERE id=?1", rusqlite::params![other.id, candidate.id, end, now])?;
                        transition(c, &other.id, Some(other.status), MemoryStatus::Superseded, &format!("superseded by {}", candidate.id), now)?;
                        (candidate.id.clone(), MemoryStatus::Active)
                    }
                    (AdmissionDecision::Contradiction, Some(other)) => {
                        transition(c, &candidate.id, Some(MemoryStatus::Candidate), MemoryStatus::Disputed, "contradiction without temporal change", now)?;
                        if other.status != MemoryStatus::Disputed {
                            transition(c, &other.id, Some(other.status), MemoryStatus::Disputed, &format!("disputed by {}", candidate.id), now)?;
                        }
                        let qid = new_id("mq");
                        c.execute(
                            "INSERT INTO memory_questions(id,principal_id,scope_id,memory_ids,question,priority,state,created_at) VALUES(?1,?2,?3,?4,?5,'low','open',?6)",
                            rusqlite::params![qid, candidate.principal, candidate.scope, json!([other.id, candidate.id]).to_string(), format!("Which is current: «{}» or «{}»?", other.statement, candidate.statement), now],
                        )?;
                        question = Some(qid);
                        (candidate.id.clone(), MemoryStatus::Disputed)
                    }
                    (AdmissionDecision::Deferred, _) => (candidate.id.clone(), MemoryStatus::Candidate),
                    _ => {
                        transition(c, &candidate.id, Some(MemoryStatus::Candidate), MemoryStatus::Active, "new", now)?;
                        (candidate.id.clone(), MemoryStatus::Active)
                    }
                };
                if semantic {
                    c.execute("UPDATE memories SET similarity_pending=0 WHERE id=?1", [&candidate.id])?;
                }
                c.execute(
                    "INSERT INTO memory_admissions(id,principal_id,scope_id,memory_id,decision,similarity,against_id,classifier,detail,at) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",
                    rusqlite::params![new_id("adm"), candidate.principal, candidate.scope, candidate.id, decision.as_str(), similarity, against2.as_ref().map(|m| m.id.clone()), classifier2, detail.to_string(), now],
                )?;
                Ok((memory_id, status, question))
            })
            .await?;
        question_id = question_id.or(qid);
        Ok(AdmissionResult {
            memory_id,
            candidate_id: candidate.id,
            decision,
            status,
            similarity,
            against: against.map(|m| m.id),
            classifier,
            question_id,
            downgraded_from_preference: false,
            semantic_pending: !semantic,
        })
    }

    /// Admission decisions recorded for a memory.
    pub async fn admissions(
        &self,
        grant: &MemoryGrant,
        memory_id: &str,
    ) -> Result<Vec<serde_json::Value>> {
        if self.memory(grant, memory_id).await?.is_none() {
            return Err(Error::NotFound(memory_id.into()));
        }
        let id = memory_id.to_string();
        Ok(self
            .db
            .read(ReadLane::Interactive, move |c| {
                let mut s = c.prepare("SELECT decision,similarity,against_id,classifier,detail,at FROM memory_admissions WHERE memory_id=?1 ORDER BY at,id")?;
                let rows = s
                    .query_map([&id], |r| {
                        let detail: String = r.get(4)?;
                        Ok(json!({"decision": r.get::<_, String>(0)?, "similarity": r.get::<_, Option<f64>>(1)?, "against": r.get::<_, Option<String>>(2)?, "classifier": r.get::<_, String>(3)?, "detail": serde_json::from_str::<serde_json::Value>(&detail).unwrap_or_default(), "at": r.get::<_, i64>(5)?}))
                    })?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                Ok(rows)
            })
            .await?)
    }

    /// Enriched key: an extracted fact indexed as an extra key pointing to its turn.
    pub(crate) async fn add_message_key(
        &self,
        principal: &str,
        scope: &str,
        message_id: &str,
        memory_id: Option<&str>,
        key: &str,
        now: i64,
    ) -> Result<()> {
        let id = format!(
            "key_{}",
            &crate::digest(format!("{message_id}\0{}", normalize(key)).as_bytes())[..32]
        );
        let (principal2, scope2, message, memory, key2, id2) = (
            principal.to_string(),
            scope.to_string(),
            message_id.to_string(),
            memory_id.map(str::to_string),
            self.redactor.redact_text(key),
            id.clone(),
        );
        let inserted = self
            .db
            .write(Priority::Normal, move |c| {
                Ok(c.execute(
                    "INSERT OR IGNORE INTO memory_keys(id,principal_id,scope_id,message_id,memory_id,key_text) VALUES(?1,?2,?3,?4,?5,?6)",
                    rusqlite::params![id2, principal2, scope2, message, memory, key2],
                )? == 1)
            })
            .await?;
        if inserted {
            self.embeddings
                .index(principal, scope, &format!("key:{id}"), key, now)
                .await?;
        }
        Ok(())
    }

    /// User-requested deletion: the only operation that destroys memory content.
    /// Runs the shared durable purge (tombstone, derived-copy sweep, FTS/vector/blob
    /// hygiene) keyed by the memory ID. Telemetry is swept only by hosts that open the
    /// purge service with their telemetry handle.
    pub async fn purge_memory(&self, grant: &MemoryGrant, id: &str, now: i64) -> Result<()> {
        let memory = self
            .memory(grant, id)
            .await?
            .ok_or_else(|| Error::NotFound(id.into()))?;
        let service = excalibur_retention::PurgeService::open(&grant.principal, self.db.clone(), None)
            .await
            .map_err(purge_error)?
            .with_participant(std::sync::Arc::new(crate::MemoryPurgeParticipant));
        service
            .purge(excalibur_retention::PurgeRequest {
                operation_key: format!("memory:{id}"),
                scope: memory.scope.clone(),
                kind: excalibur_retention::SubjectKind::Memory,
                subject_id: id.to_string(),
                reason: excalibur_retention::PurgeReason::UserRequest,
                requested_at: now,
            })
            .await
            .map_err(purge_error)?;
        Ok(())
    }
}
fn purge_error(error: excalibur_retention::Error) -> Error {
    match error {
        excalibur_retention::Error::Store(error) => Error::Store(error),
        excalibur_retention::Error::NotFound => Error::NotFound("memory".into()),
        other => Error::Invalid(other.to_string()),
    }
}
