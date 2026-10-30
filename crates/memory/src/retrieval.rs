//! The Retriever (architecture 10.7): deterministic query analysis, lexical, semantic,
//! relational and temporal candidate lists, Reciprocal Rank Fusion, deterministic
//! adjustments, optional reranking, sufficiency notices and budgeted packing.
use crate::{
    BoxFuture, Memory, MemoryGrant, MemoryKind, MemoryStatus, MemoryStore, Result, SourceRef,
    graph::{mentioned_entities, neighbors, scope_entities},
    invalid,
    library::{ChunkRow, load_chunk},
    load_memory, placeholders,
    text::{estimate_tokens, fts_or, fts_phrase, normalize, query_terms},
};
use chrono::{Datelike, Duration, NaiveDate, TimeZone};
use excalibur_store::{Priority, ReadLane, rusqlite};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "mode", content = "at")]
pub enum TimeMode {
    /// Only current records ("vigentes"), with read-time expiry.
    Current,
    /// Include superseded, expired and retired records.
    Historical,
    /// What was true at this instant (valid time).
    ValidAt(i64),
    /// What we believed at this instant (record time).
    KnownAt(i64),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Sources {
    pub memories: bool,
    pub messages: bool,
    pub chunks: bool,
}
impl Default for Sources {
    fn default() -> Self {
        Self {
            memories: true,
            messages: true,
            chunks: false,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RetrievalQuery {
    pub text: String,
    #[serde(default)]
    pub scopes: Vec<String>,
    #[serde(default)]
    pub kinds: Vec<MemoryKind>,
    #[serde(default)]
    pub interval: Option<(i64, i64)>,
    #[serde(default)]
    pub entities: Vec<String>,
    #[serde(default)]
    pub time_mode: Option<TimeMode>,
    #[serde(default = "default_k")]
    pub k: usize,
    #[serde(default = "default_budget")]
    pub token_budget: usize,
    #[serde(default)]
    pub sources: Sources,
    #[serde(default)]
    pub allow_rerank: bool,
}
fn default_k() -> usize {
    10
}
fn default_budget() -> usize {
    2000
}
impl RetrievalQuery {
    pub fn new(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            scopes: Vec::new(),
            kinds: Vec::new(),
            interval: None,
            entities: Vec::new(),
            time_mode: None,
            k: default_k(),
            token_budget: default_budget(),
            sources: Sources::default(),
            allow_rerank: false,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct QueryAnalysis {
    pub terms: Vec<String>,
    pub interval: Option<(i64, i64)>,
    pub time_mode: TimeMode,
    pub entity_seeds: Vec<String>,
    pub required_phrases: Vec<String>,
    pub identifiers: Vec<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ItemKind {
    Memory,
    Message,
    Chunk,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RetrievedItem {
    pub id: String,
    pub kind: ItemKind,
    pub scope: String,
    pub text: String,
    pub status: Option<MemoryStatus>,
    pub valid_from: Option<i64>,
    pub valid_to: Option<i64>,
    pub at: i64,
    pub sources: Vec<SourceRef>,
    pub conversation: Option<String>,
    /// Citation token and resolved location for document chunks.
    pub cite: Option<String>,
    pub location: Option<String>,
    pub score: f64,
    pub lists: Vec<String>,
    pub similarity: Option<f32>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "notice")]
pub enum Notice {
    InsufficientEvidence,
    Contradiction { first: String, second: String },
    Disputed { id: String },
    AmbiguousScopes { scopes: Vec<String> },
    SemanticUnavailable,
    QueryRewritten { query: String },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RetrievalResult {
    pub analysis: QueryAnalysis,
    pub items: Vec<RetrievedItem>,
    pub notices: Vec<Notice>,
    pub packed: String,
    pub semantic_available: bool,
    pub reranked: bool,
}

/// Small local reranker; its order is used directly, not refused with RRF.
pub trait Reranker: Send + Sync {
    fn rerank<'a>(
        &'a self,
        query: &'a str,
        texts: &'a [String],
    ) -> BoxFuture<'a, std::result::Result<Vec<usize>, String>>;
}
/// Single cheap-model rewrite, used only when a query has zero results.
pub trait QueryRewriter: Send + Sync {
    fn rewrite<'a>(&'a self, query: &'a str) -> BoxFuture<'a, std::result::Result<String, String>>;
}

const MONTHS: &[(&str, u32)] = &[
    ("january", 1),
    ("enero", 1),
    ("february", 2),
    ("febrero", 2),
    ("march", 3),
    ("marzo", 3),
    ("april", 4),
    ("abril", 4),
    ("may", 5),
    ("mayo", 5),
    ("june", 6),
    ("junio", 6),
    ("july", 7),
    ("julio", 7),
    ("august", 8),
    ("agosto", 8),
    ("september", 9),
    ("septiembre", 9),
    ("setiembre", 9),
    ("october", 10),
    ("octubre", 10),
    ("november", 11),
    ("noviembre", 11),
    ("december", 12),
    ("diciembre", 12),
];
fn month(name: &str) -> Option<u32> {
    MONTHS.iter().find(|(n, _)| *n == name).map(|(_, m)| *m)
}
fn month_pattern() -> String {
    MONTHS.iter().map(|(n, _)| *n).collect::<Vec<_>>().join("|")
}

fn local_start(tz: chrono_tz::Tz, date: NaiveDate) -> i64 {
    let naive = date.and_hms_opt(0, 0, 0).unwrap();
    tz.from_local_datetime(&naive)
        .earliest()
        .map(|d| d.timestamp())
        .unwrap_or_else(|| naive.and_utc().timestamp())
}
fn day(tz: chrono_tz::Tz, date: NaiveDate) -> (i64, i64) {
    (
        local_start(tz, date),
        local_start(tz, date + Duration::days(1)),
    )
}
fn month_range(tz: chrono_tz::Tz, year: i32, month: u32) -> Option<(i64, i64)> {
    let start = NaiveDate::from_ymd_opt(year, month, 1)?;
    let next = if month == 12 {
        NaiveDate::from_ymd_opt(year + 1, 1, 1)?
    } else {
        NaiveDate::from_ymd_opt(year, month + 1, 1)?
    };
    Some((local_start(tz, start), local_start(tz, next)))
}

/// Deterministic date expressions in English and Spanish, in the user's time zone.
pub fn parse_interval(text: &str, tz: chrono_tz::Tz, now: i64) -> Option<(i64, i64)> {
    let q = normalize(text);
    let today = tz.timestamp_opt(now, 0).single()?.date_naive();
    let infer_year = |m: u32| {
        if m > today.month() {
            today.year() - 1
        } else {
            today.year()
        }
    };
    let re = |p: &str| regex::Regex::new(p).unwrap();
    if let Some(c) = re(r"\b(\d{4})-(\d{2})-(\d{2})\b").captures(&q) {
        let d =
            NaiveDate::from_ymd_opt(c[1].parse().ok()?, c[2].parse().ok()?, c[3].parse().ok()?)?;
        return Some(day(tz, d));
    }
    if let Some(c) = re(r"\b(\d{4})-(\d{2})\b").captures(&q) {
        return month_range(tz, c[1].parse().ok()?, c[2].parse().ok()?);
    }
    let months = month_pattern();
    if let Some(c) = re(&format!(
        r"\b(\d{{1,2}}) (?:de )?({months})(?:,? (?:de )?(\d{{4}}))?\b"
    ))
    .captures(&q)
    {
        let m = month(&c[2])?;
        let y = c
            .get(3)
            .and_then(|y| y.as_str().parse().ok())
            .unwrap_or_else(|| infer_year(m));
        return Some(day(tz, NaiveDate::from_ymd_opt(y, m, c[1].parse().ok()?)?));
    }
    if let Some(c) = re(&format!(
        r"\b({months}) (\d{{1,2}})(?:st|nd|rd|th)?(?:,? (\d{{4}}))?\b"
    ))
    .captures(&q)
    {
        let m = month(&c[1])?;
        let y = c
            .get(3)
            .and_then(|y| y.as_str().parse().ok())
            .unwrap_or_else(|| infer_year(m));
        return Some(day(tz, NaiveDate::from_ymd_opt(y, m, c[2].parse().ok()?)?));
    }
    if let Some(c) = re(&format!(
        r"\b(?:in|en|during|durante) ({months})(?: (?:de |of )?(\d{{4}}))?\b"
    ))
    .captures(&q)
    {
        let m = month(&c[1])?;
        let y = c
            .get(2)
            .and_then(|y| y.as_str().parse().ok())
            .unwrap_or_else(|| infer_year(m));
        return month_range(tz, y, m);
    }
    if let Some(c) = re(r"\b(?:hace (\d+) dias|(\d+) days ago)\b").captures(&q) {
        let n: i64 = c.get(1).or(c.get(2))?.as_str().parse().ok()?;
        return Some(day(tz, today - Duration::days(n)));
    }
    if re(r"\b(today|hoy)\b").is_match(&q) {
        return Some(day(tz, today));
    }
    if re(r"\b(yesterday|ayer)\b").is_match(&q) {
        return Some(day(tz, today - Duration::days(1)));
    }
    let week_start = today - Duration::days(today.weekday().num_days_from_monday() as i64);
    if re(r"\b(last week|la semana pasada|semana pasada)\b").is_match(&q) {
        let start = week_start - Duration::days(7);
        return Some((local_start(tz, start), local_start(tz, week_start)));
    }
    if re(r"\b(this week|esta semana)\b").is_match(&q) {
        return Some((
            local_start(tz, week_start),
            local_start(tz, week_start + Duration::days(7)),
        ));
    }
    if re(r"\b(last month|el mes pasado|mes pasado)\b").is_match(&q) {
        let (y, m) = if today.month() == 1 {
            (today.year() - 1, 12)
        } else {
            (today.year(), today.month() - 1)
        };
        return month_range(tz, y, m);
    }
    if re(r"\b(last year|el ano pasado|ano pasado)\b").is_match(&q) {
        let y = today.year() - 1;
        return Some((
            local_start(tz, NaiveDate::from_ymd_opt(y, 1, 1)?),
            local_start(tz, NaiveDate::from_ymd_opt(y + 1, 1, 1)?),
        ));
    }
    None
}

fn analyze(query: &RetrievalQuery, tz: chrono_tz::Tz, now: i64) -> QueryAnalysis {
    let q = normalize(&query.text);
    let interval = query
        .interval
        .or_else(|| parse_interval(&query.text, tz, now));
    let mut required_phrases = Vec::new();
    let quoted = regex::Regex::new(r#""([^"]{1,200})"|«([^»]{1,200})»|“([^”]{1,200})”"#).unwrap();
    for c in quoted.captures_iter(&query.text) {
        if let Some(m) = c.get(1).or(c.get(2)).or(c.get(3)) {
            required_phrases.push(m.as_str().trim().to_string());
        }
    }
    let identifier = regex::Regex::new(
        r"[A-Za-z0-9]*[A-Za-z][A-Za-z0-9]*(?:[_./:#\-][A-Za-z0-9]+)+|[a-z]+[A-Z][A-Za-z0-9]+",
    )
    .unwrap();
    let identifiers: Vec<String> = identifier
        .find_iter(&query.text)
        .map(|m| m.as_str().to_string())
        .filter(|m| {
            m.chars().count() >= 3 && !required_phrases.iter().any(|p| p.contains(m.as_str()))
        })
        .collect();
    let has = |p: &str| regex::Regex::new(p).unwrap().is_match(&q);
    let point = interval.map(|(_, end)| end - 1);
    let time_mode = query.time_mode.unwrap_or_else(|| {
        if let Some(t) = point.filter(|_| has(r"\b(creiamos|sabiamos|pensabamos|believed|did we know|we knew|did we believe|did we think)\b")) {
            TimeMode::KnownAt(t)
        } else if let Some(t) = point.filter(|_| has(r"\b(era cierto|era verdad|was true|was the case|held true)\b")) {
            TimeMode::ValidAt(t)
        } else if has(r"\b(before|antes|used to|solia|historial|history|previously|anteriormente|que pensaba|what did i think|changed|cambio|cambie|old|antiguo)\b") {
            TimeMode::Historical
        } else {
            TimeMode::Current
        }
    });
    QueryAnalysis {
        terms: query_terms(&query.text),
        interval,
        time_mode,
        entity_seeds: Vec::new(),
        required_phrases,
        identifiers,
    }
}

fn visible(memory: &Memory, grant: &MemoryGrant, mode: TimeMode, now: i64) -> bool {
    if memory.principal != grant.principal || !grant.scopes.contains(&memory.scope) {
        return false;
    }
    // Duplicates merged into another record never surface on their own.
    if memory.status == MemoryStatus::Superseded
        && memory.valid_to.is_none()
        && memory.superseded_by.is_some()
    {
        return false;
    }
    if memory.status == MemoryStatus::Candidate {
        let same_conversation = memory.origin_conversation.is_some()
            && memory.origin_conversation == grant.conversation_id;
        let same_task = memory.origin_task.is_some() && memory.origin_task == grant.task_id;
        return matches!(mode, TimeMode::Current | TimeMode::Historical)
            && (same_conversation || same_task);
    }
    match mode {
        TimeMode::Current => memory.current_at(now),
        TimeMode::Historical => true,
        TimeMode::ValidAt(t) => {
            memory.status != MemoryStatus::Retired
                && memory.valid_from.unwrap_or(memory.recorded_at) <= t
                && memory.valid_to.is_none_or(|end| end > t)
                && memory.expires_at.is_none_or(|e| e > t)
        }
        TimeMode::KnownAt(t) => {
            memory.recorded_at <= t
                && memory.invalidated_at.is_none_or(|end| end > t)
                && memory.expires_at.is_none_or(|e| e > t)
        }
    }
}

fn decay(memory: &Memory, now: i64) -> f64 {
    let reference = memory
        .valid_from
        .unwrap_or(memory.recorded_at)
        .max(memory.recorded_at.min(now));
    let age_days = ((now - reference).max(0) as f64) / 86_400.0;
    0.5f64.powf(age_days / memory.kind.half_life_days())
}

#[derive(Default)]
struct Lists {
    named: BTreeMap<String, Vec<String>>,
}
impl Lists {
    fn push(&mut self, list: &str, key: String) {
        let entries = self.named.entry(list.to_string()).or_default();
        if !entries.contains(&key) {
            entries.push(key);
        }
    }
}

type Loaded = (
    BTreeMap<String, Memory>,
    BTreeMap<String, (MessageRow, String)>,
    BTreeMap<String, ChunkRow>,
);

struct MessageRow {
    id: String,
    scope: String,
    text: String,
    conversation: Option<String>,
    sent_at: i64,
    role: String,
}

fn push_scopes<'a>(params: &mut Vec<&'a dyn rusqlite::ToSql>, scopes: &'a [String]) {
    for scope in scopes {
        params.push(scope);
    }
}

impl MemoryStore {
    /// Combined retrieval inside explicit granted scopes.
    pub async fn retrieve(
        &self,
        grant: &MemoryGrant,
        query: RetrievalQuery,
        now: i64,
    ) -> Result<RetrievalResult> {
        let first = self.retrieve_once(grant, &query, now).await?;
        if !first.items.is_empty() || query.text.trim().is_empty() {
            return Ok(first);
        }
        let Some(rewriter) = self.services.rewriter.clone() else {
            return Ok(first);
        };
        let Ok(rewritten) = rewriter.rewrite(&query.text).await else {
            return Ok(first);
        };
        if rewritten.trim().is_empty() || rewritten.trim() == query.text.trim() {
            return Ok(first);
        }
        let mut second_query = query.clone();
        second_query.text = rewritten.clone();
        let mut second = self.retrieve_once(grant, &second_query, now).await?;
        second
            .notices
            .insert(0, Notice::QueryRewritten { query: rewritten });
        Ok(second)
    }

    async fn retrieve_once(
        &self,
        grant: &MemoryGrant,
        query: &RetrievalQuery,
        now: i64,
    ) -> Result<RetrievalResult> {
        let scopes = grant.select(&query.scopes)?;
        if query.text.len() > 4096 {
            return Err(invalid("query exceeds 4096 bytes"));
        }
        let k = query.k.clamp(1, 50);
        let limit = self.config.list_candidates as i64;
        let mut analysis = analyze(query, grant.timezone, now);
        let semantic_queries = if query.text.trim().is_empty() {
            Vec::new()
        } else {
            self.embeddings.embed_query(&query.text).await
        };
        let has_spaces = !self
            .embeddings
            .spaces()
            .await?
            .iter()
            .all(|s| s.state == "retired");
        let semantic_available = !semantic_queries.is_empty();
        let semantic = self
            .embeddings
            .search_prepared(
                &scopes,
                &semantic_queries,
                self.config.list_candidates,
                None,
            )
            .await?;
        let raw = query.text.clone();
        let (principal, scopes2, analysis2, sources, explicit_entities) = (
            grant.principal.clone(),
            scopes.clone(),
            analysis.clone(),
            query.sources,
            query.entities.clone(),
        );
        let (lists, seeds) = self
            .db
            .read(ReadLane::Interactive, move |c| {
                let mut lists = Lists::default();
                let scope_marks = placeholders(2, scopes2.len());
                let terms = fts_or(&analysis2.terms);
                let lexical = |list: &str, sql: String, q: &str, prefix: &str, lists: &mut Lists| -> rusqlite::Result<()> {
                    let mut s = c.prepare(&sql)?;
                    let mut params: Vec<&dyn rusqlite::ToSql> = vec![&principal];
                    push_scopes(&mut params, &scopes2);
                    params.push(&q);
                    params.push(&limit);
                    for key in s.query_map(params.as_slice(), |r| r.get::<_, String>(0))? {
                        lists.push(list, format!("{prefix}{}", key?));
                    }
                    Ok(())
                };
                let n = scopes2.len();
                let (qi, li) = (n + 2, n + 3);
                let mut lexical_queries: Vec<(String, &str)> = Vec::new();
                if let Some(t) = &terms {
                    lexical_queries.push((t.clone(), "lexical"));
                }
                for phrase in analysis2.required_phrases.iter().chain(analysis2.identifiers.iter()) {
                    if let Some(p) = fts_phrase(phrase) {
                        lexical_queries.push((p, "trigram"));
                    }
                }
                for (q, index) in &lexical_queries {
                    let trigram = *index == "trigram";
                    if sources.memories {
                        let table = if trigram { "memories_trigram" } else { "memories_fts" };
                        lexical(&format!("lexical:{index}:memories"), format!("SELECT m.id FROM {table} f JOIN memories m ON m.rowid=f.rowid WHERE m.principal_id=?1 AND m.scope_id IN ({scope_marks}) AND {table} MATCH ?{qi} ORDER BY bm25({table}) LIMIT ?{li}"), q, "m:", &mut lists)?;
                    }
                    if sources.messages {
                        let table = if trigram { "messages_trigram" } else { "messages_fts" };
                        lexical(&format!("lexical:{index}:messages"), format!("SELECT m.id FROM {table} f JOIN messages m ON m.rowid=f.rowid WHERE m.principal_id=?1 AND m.scope_id IN ({scope_marks}) AND {table} MATCH ?{qi} ORDER BY bm25({table}) LIMIT ?{li}"), q, "msg:", &mut lists)?;
                        if !trigram {
                            lexical("lexical:keys", format!("SELECT k.message_id FROM memory_keys_fts f JOIN memory_keys k ON k.rowid=f.rowid WHERE k.principal_id=?1 AND k.scope_id IN ({scope_marks}) AND memory_keys_fts MATCH ?{qi} ORDER BY bm25(memory_keys_fts) LIMIT ?{li}"), q, "msg:", &mut lists)?;
                        }
                    }
                    if sources.chunks {
                        let table = if trigram { "doc_chunks_trigram" } else { "doc_chunks_fts" };
                        lexical(&format!("lexical:{index}:chunks"), format!("SELECT ch.id FROM {table} f JOIN doc_chunks ch ON ch.rowid=f.rowid JOIN documents d ON d.id=ch.document_id AND d.version=ch.version WHERE ch.principal_id=?1 AND ch.scope_id IN ({scope_marks}) AND {table} MATCH ?{qi} ORDER BY bm25({table}) LIMIT ?{li}"), q, "c:", &mut lists)?;
                    }
                }
                // Relational: entity seeds from the alias lexicon, then 1-2 hops inside scope.
                let entities = scope_entities(c, &principal, &scopes2)?;
                let mut seeds = mentioned_entities(&entities, &raw);
                for e in &explicit_entities {
                    if let Some(found) = entities.iter().find(|x| &x.id == e || normalize(&x.canonical_name) == normalize(e)) {
                        seeds.push(found.id.clone());
                    }
                }
                seeds.sort();
                seeds.dedup();
                if sources.memories && !seeds.is_empty() {
                    let reached = neighbors(c, &principal, &scopes2, &seeds, 2)?;
                    let mut ranked: Vec<(usize, i64, String)> = Vec::new();
                    let mut s = c.prepare(&format!("SELECT m.id,m.recorded_at FROM memory_entity_links l JOIN memories m ON m.id=l.memory_id WHERE l.entity_id=?{} AND m.principal_id=?1 AND m.scope_id IN ({scope_marks})", n + 2))?;
                    for (entity, hop) in &reached {
                        let mut params: Vec<&dyn rusqlite::ToSql> = vec![&principal];
                        push_scopes(&mut params, &scopes2);
                        params.push(entity);
                        for row in s.query_map(params.as_slice(), |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))? {
                            let (id, at) = row?;
                            ranked.push((*hop, -at, id));
                        }
                    }
                    ranked.sort();
                    for (_, _, id) in ranked.into_iter().take(limit as usize) {
                        lists.push("relational", format!("m:{id}"));
                    }
                }
                // Temporal: records and messages inside the interval, newest first.
                if let Some((start, end)) = analysis2.interval {
                    let (si, ei, li) = (n + 2, n + 3, n + 4);
                    if sources.memories {
                        let mut s = c.prepare(&format!("SELECT id FROM memories WHERE principal_id=?1 AND scope_id IN ({scope_marks}) AND COALESCE(valid_from,recorded_at)<?{ei} AND COALESCE(valid_to,COALESCE(valid_from,recorded_at)+1)>?{si} ORDER BY COALESCE(valid_from,recorded_at) DESC LIMIT ?{li}"))?;
                        let mut params: Vec<&dyn rusqlite::ToSql> = vec![&principal];
                        push_scopes(&mut params, &scopes2);
                        params.extend([&start as &dyn rusqlite::ToSql, &end, &limit]);
                        for id in s.query_map(params.as_slice(), |r| r.get::<_, String>(0))? {
                            lists.push("temporal", format!("m:{}", id?));
                        }
                    }
                    if sources.messages {
                        let mut s = c.prepare(&format!("SELECT id FROM messages WHERE principal_id=?1 AND scope_id IN ({scope_marks}) AND sent_at>=?{si} AND sent_at<?{ei} ORDER BY sent_at DESC LIMIT ?{li}"))?;
                        let mut params: Vec<&dyn rusqlite::ToSql> = vec![&principal];
                        push_scopes(&mut params, &scopes2);
                        params.extend([&start as &dyn rusqlite::ToSql, &end, &limit]);
                        for id in s.query_map(params.as_slice(), |r| r.get::<_, String>(0))? {
                            lists.push("temporal", format!("msg:{}", id?));
                        }
                    }
                }
                Ok((lists, seeds))
            })
            .await?;
        analysis.entity_seeds = seeds;
        let mut lists = lists;
        let mut similarity: BTreeMap<String, f32> = BTreeMap::new();
        let floor = self.config.semantic_floor;
        for hit in semantic.hits.iter().filter(|h| h.similarity >= floor) {
            let key = if let Some(id) = hit.object_id.strip_prefix("memory:") {
                sources.memories.then(|| format!("m:{id}"))
            } else if let Some(id) = hit.object_id.strip_prefix("chunk:") {
                sources.chunks.then(|| format!("c:{id}"))
            } else {
                // Enriched keys are resolved to their turns below.
                None
            };
            if let Some(key) = key {
                similarity.entry(key.clone()).or_insert(hit.similarity);
                lists.push("semantic", key);
            }
        }
        // Enriched keys resolve to their original turn.
        let key_hits: Vec<(String, f32)> = semantic
            .hits
            .iter()
            .filter(|h| h.object_id.starts_with("key:") && h.similarity >= floor)
            .map(|h| {
                (
                    h.object_id.trim_start_matches("key:").to_string(),
                    h.similarity,
                )
            })
            .collect();
        if sources.messages && !key_hits.is_empty() {
            let ids: Vec<String> = key_hits.iter().map(|(id, _)| id.clone()).collect();
            let resolved = self
                .db
                .read(ReadLane::Interactive, move |c| {
                    let mut out = Vec::new();
                    let mut s = c.prepare("SELECT message_id FROM memory_keys WHERE id=?1")?;
                    for id in ids {
                        let message: Option<String> = s.query_row([&id], |r| r.get(0)).ok();
                        out.push(message);
                    }
                    Ok(out)
                })
                .await?;
            for ((_, sim), message) in key_hits.into_iter().zip(resolved) {
                if let Some(message) = message {
                    let key = format!("msg:{message}");
                    similarity.entry(key.clone()).or_insert(sim);
                    lists.push("semantic", key);
                }
            }
        }
        // Reciprocal Rank Fusion.
        let mut fused: BTreeMap<String, (f64, Vec<String>)> = BTreeMap::new();
        for (name, keys) in &lists.named {
            for (rank, key) in keys.iter().enumerate() {
                let entry = fused.entry(key.clone()).or_default();
                entry.0 += 1.0 / (self.config.rrf_k + rank as f64 + 1.0);
                entry.1.push(name.clone());
            }
        }
        let keys: Vec<String> = fused.keys().cloned().collect();
        let loaded = self
            .db
            .read(ReadLane::Interactive, move |c| {
                let mut memories = BTreeMap::new();
                let mut messages = BTreeMap::new();
                let mut chunks = BTreeMap::new();
                let mut s = c.prepare("SELECT id,COALESCE(scope_id,''),text,conversation_id,sent_at,role,COALESCE(principal_id,'') FROM messages WHERE id=?1")?;
                for key in keys {
                    if let Some(id) = key.strip_prefix("m:") {
                        if let Some(m) = load_memory(c, id)? {
                            memories.insert(key.clone(), m);
                        }
                    } else if let Some(id) = key.strip_prefix("msg:") {
                        let row = s
                            .query_row([id], |r| {
                                Ok((
                                    MessageRow { id: r.get(0)?, scope: r.get(1)?, text: r.get(2)?, conversation: r.get(3)?, sent_at: r.get(4)?, role: r.get(5)? },
                                    r.get::<_, String>(6)?,
                                ))
                            })
                            .ok();
                        if let Some(row) = row {
                            messages.insert(key.clone(), row);
                        }
                    } else if let Some(id) = key.strip_prefix("c:")
                        && let Some(chunk) = load_chunk(c, id)?
                    {
                        chunks.insert(key.clone(), chunk);
                    }
                }
                Ok((memories, messages, chunks))
            })
            .await?;
        let (memories, messages, chunks): Loaded = loaded;
        let mode = analysis.time_mode;
        let required: Vec<String> = analysis
            .required_phrases
            .iter()
            .chain(analysis.identifiers.iter())
            .map(|p| normalize(p))
            .collect();
        let mut items: Vec<RetrievedItem> = Vec::new();
        for (key, (rrf, list_names)) in fused {
            let mut item = if let Some(m) = memories.get(&key) {
                if !visible(m, grant, mode, now)
                    || (!query.kinds.is_empty() && !query.kinds.contains(&m.kind))
                {
                    continue;
                }
                if mode == TimeMode::Current
                    && let Some((start, end)) = analysis.interval
                    && list_names.iter().all(|l| l != "temporal")
                    && m.valid_from.is_some_and(|v| {
                        let until = m.valid_to.unwrap_or(if m.kind == MemoryKind::Event {
                            v + 1
                        } else {
                            i64::MAX
                        });
                        !(v < end && until > start)
                    })
                {
                    continue;
                }
                let factor = (0.5 + 0.5 * decay(m, now))
                    * (0.8 + 0.4 * m.importance)
                    * m.trust.weight()
                    * (0.8 + 0.2 * m.confidence);
                RetrievedItem {
                    id: m.id.clone(),
                    kind: ItemKind::Memory,
                    scope: m.scope.clone(),
                    text: m.statement.clone(),
                    status: Some(m.status),
                    valid_from: m.valid_from,
                    valid_to: m.valid_to,
                    at: m.recorded_at,
                    sources: m.sources.clone(),
                    conversation: m.origin_conversation.clone(),
                    cite: None,
                    location: None,
                    score: rrf * factor,
                    lists: list_names,
                    similarity: None,
                }
            } else if let Some((row, owner)) = messages.get(&key) {
                if owner != &grant.principal || !scopes.contains(&row.scope) {
                    continue;
                }
                if let TimeMode::ValidAt(t) | TimeMode::KnownAt(t) = mode
                    && row.sent_at > t
                {
                    continue;
                }
                RetrievedItem {
                    id: row.id.clone(),
                    kind: ItemKind::Message,
                    scope: row.scope.clone(),
                    text: format!("{}: {}", row.role, row.text),
                    status: None,
                    valid_from: None,
                    valid_to: None,
                    at: row.sent_at,
                    sources: vec![SourceRef {
                        kind: crate::SourceKind::Message,
                        id: row.id.clone(),
                        context: row.conversation.clone(),
                    }],
                    conversation: row.conversation.clone(),
                    cite: None,
                    location: None,
                    score: rrf,
                    lists: list_names,
                    similarity: None,
                }
            } else if let Some(chunk) = chunks.get(&key) {
                if chunk.principal != grant.principal
                    || !scopes.contains(&chunk.scope)
                    || !chunk.current
                {
                    continue;
                }
                RetrievedItem {
                    id: chunk.id.clone(),
                    kind: ItemKind::Chunk,
                    scope: chunk.scope.clone(),
                    text: chunk.text.clone(),
                    status: None,
                    valid_from: None,
                    valid_to: None,
                    at: chunk.recorded_at,
                    sources: vec![SourceRef {
                        kind: crate::SourceKind::Document,
                        id: chunk.document_id.clone(),
                        context: Some(format!("v{}", chunk.version)),
                    }],
                    conversation: None,
                    cite: Some(chunk.cite_token()),
                    location: Some(chunk.location("en")),
                    score: rrf,
                    lists: list_names,
                    similarity: None,
                }
            } else {
                continue;
            };
            let haystack = normalize(&item.text);
            if !required.iter().all(|p| haystack.contains(p.as_str())) {
                continue;
            }
            item.similarity = similarity.get(&key).copied();
            items.push(item);
        }
        items.sort_by(|a, b| b.score.total_cmp(&a.score).then_with(|| a.id.cmp(&b.id)));
        // Optional rerank when the leaders are very close.
        let mut reranked = false;
        if query.allow_rerank
            && items.len() >= 2
            && let Some(reranker) = self.services.reranker.clone()
            && (items[0].score - items[1].score) / items[0].score.max(f64::EPSILON)
                < self.config.rerank_margin
        {
            let window = items.len().min(20);
            let texts: Vec<String> = items[..window].iter().map(|i| i.text.clone()).collect();
            if let Ok(order) = reranker.rerank(&query.text, &texts).await {
                let mut seen = BTreeSet::new();
                let mut head: Vec<RetrievedItem> = order
                    .into_iter()
                    .filter(|i| *i < window && seen.insert(*i))
                    .map(|i| items[i].clone())
                    .collect();
                for (i, item) in items[..window].iter().enumerate() {
                    if !seen.contains(&i) {
                        head.push(item.clone());
                    }
                }
                head.extend(items.drain(window..));
                items = head;
                reranked = true;
            }
        }
        items.truncate(k);
        let mut notices = Vec::new();
        if !semantic_available && has_spaces {
            notices.push(Notice::SemanticUnavailable);
        }
        let supported = |i: &RetrievedItem| {
            i.lists
                .iter()
                .any(|l| l.starts_with("lexical") || l == "relational" || l == "temporal")
                || i.similarity
                    .is_some_and(|s| s >= self.config.min_similarity)
        };
        if items.first().is_none_or(|i| !supported(i)) {
            notices.push(Notice::InsufficientEvidence);
        }
        // Contradictions between returned current records.
        let current: Vec<&Memory> = items
            .iter()
            .filter(|i| i.kind == ItemKind::Memory)
            .filter_map(|i| memories.get(&format!("m:{}", i.id)))
            .filter(|m| m.current_at(now))
            .collect();
        let mut reported = BTreeSet::new();
        for (i, a) in current.iter().enumerate() {
            for b in current.iter().skip(i + 1) {
                let shared = a.entities.iter().any(|e| b.entities.contains(e));
                let conflict = (a.status == MemoryStatus::Disputed
                    && b.status == MemoryStatus::Disputed
                    && shared)
                    || (a.attribute.is_some()
                        && a.attribute == b.attribute
                        && shared
                        && normalize(&a.statement) != normalize(&b.statement));
                if conflict {
                    reported.insert(a.id.clone());
                    reported.insert(b.id.clone());
                    notices.push(Notice::Contradiction {
                        first: a.id.clone(),
                        second: b.id.clone(),
                    });
                }
            }
        }
        for m in &current {
            if m.status == MemoryStatus::Disputed && !reported.contains(&m.id) {
                notices.push(Notice::Disputed { id: m.id.clone() });
            }
        }
        if scopes.len() > 1
            && items.len() >= 2
            && items[0].scope != items[1].scope
            && items[1].score >= items[0].score * 0.9
        {
            let mut involved: Vec<String> = items.iter().take(3).map(|i| i.scope.clone()).collect();
            involved.sort();
            involved.dedup();
            notices.push(Notice::AmbiguousScopes { scopes: involved });
        }
        let packed = pack(&items, &notices, query.token_budget);
        // Usage tracking feeds decay and archival.
        let used: Vec<String> = items
            .iter()
            .filter(|i| i.kind == ItemKind::Memory)
            .map(|i| i.id.clone())
            .collect();
        if !used.is_empty() {
            self.db
                .write(Priority::Background, move |c| {
                    for id in used {
                        c.execute("UPDATE memories SET usage_count=usage_count+1,last_used_at=?2 WHERE id=?1", rusqlite::params![id, now])?;
                    }
                    Ok(())
                })
                .await?;
        }
        Ok(RetrievalResult {
            analysis,
            items,
            notices,
            packed,
            semantic_available,
            reranked,
        })
    }
}

fn date(at: i64) -> String {
    chrono::DateTime::from_timestamp(at, 0)
        .map(|d| d.format("%Y-%m-%d").to_string())
        .unwrap_or_else(|| at.to_string())
}

/// Group by scope, label every element with id, statement, validity and source.
pub(crate) fn pack(items: &[RetrievedItem], notices: &[Notice], budget: usize) -> String {
    let mut out = String::new();
    let mut used = 0;
    for notice in notices {
        let line = match notice {
            Notice::InsufficientEvidence => "! insufficient evidence\n".to_string(),
            Notice::Contradiction { first, second } => {
                format!("! contradiction between {first} and {second}\n")
            }
            Notice::Disputed { id } => format!("! {id} is disputed\n"),
            Notice::AmbiguousScopes { scopes } => format!(
                "! query is ambiguous between scopes {}\n",
                scopes.join(", ")
            ),
            Notice::SemanticUnavailable => "! semantic search was unavailable\n".to_string(),
            Notice::QueryRewritten { query } => format!("! query rewritten once: {query}\n"),
        };
        used += estimate_tokens(&line);
        out.push_str(&line);
    }
    let mut by_scope: BTreeMap<&str, Vec<&RetrievedItem>> = BTreeMap::new();
    for item in items {
        by_scope.entry(&item.scope).or_default().push(item);
    }
    for (scope, group) in by_scope {
        let header = format!("[scope {scope}]\n");
        if used + estimate_tokens(&header) > budget {
            break;
        }
        used += estimate_tokens(&header);
        out.push_str(&header);
        for item in group {
            let validity = match (item.valid_from, item.valid_to) {
                (Some(a), Some(b)) => format!(" valid {}..{}", date(a), date(b)),
                (Some(a), None) => format!(" valid from {}", date(a)),
                (None, Some(b)) => format!(" valid until {}", date(b)),
                (None, None) => String::new(),
            };
            let status = item
                .status
                .map(|s| format!(" [{}]", s.as_str()))
                .unwrap_or_default();
            let source = if let Some(location) = &item.location {
                format!(
                    " source: {} {}",
                    item.cite.clone().unwrap_or_default(),
                    location
                )
            } else {
                let first = item.sources.first().map(|s| match &s.context {
                    Some(ctx) => format!("{:?} {} ({ctx})", s.kind, s.id),
                    None => format!("{:?} {}", s.kind, s.id),
                });
                first.map(|f| format!(" source: {f}")).unwrap_or_default()
            };
            let line = format!("- {}{status}: {}{validity}{source}\n", item.id, item.text);
            let cost = estimate_tokens(&line);
            if used + cost > budget {
                return out;
            }
            used += cost;
            out.push_str(&line);
        }
    }
    out
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct HistoryQuery {
    #[serde(default)]
    pub text: Option<String>,
    #[serde(default)]
    pub scopes: Vec<String>,
    #[serde(default)]
    pub from: Option<i64>,
    #[serde(default)]
    pub to: Option<i64>,
    #[serde(default)]
    pub conversation: Option<String>,
    #[serde(default)]
    pub role: Option<String>,
    #[serde(default)]
    pub cursor: Option<i64>,
    #[serde(default)]
    pub limit: Option<usize>,
    /// Search past actions (tool calls with their results) by tool name.
    #[serde(default)]
    pub tool: Option<String>,
    /// Action outcome filter: `ok` or `error`.
    #[serde(default)]
    pub status: Option<String>,
    /// `messages` (default), `actions` or `both`.
    #[serde(default)]
    pub include: Option<String>,
    /// Cursor for the action list.
    #[serde(default)]
    pub action_cursor: Option<i64>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct HistoryEntry {
    pub id: String,
    pub scope: String,
    pub conversation: Option<String>,
    pub role: String,
    pub text: String,
    pub sent_at: i64,
}

/// A past tool call with its outcome, from a run the host bound to memory scopes.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ActionEntry {
    pub run_id: String,
    pub sequence: i64,
    pub scope: String,
    pub conversation: Option<String>,
    pub task: Option<String>,
    pub tool: String,
    pub success: bool,
    pub summary: String,
    pub at: i64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct HistoryPage {
    pub entries: Vec<HistoryEntry>,
    pub next_cursor: Option<i64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub actions: Vec<ActionEntry>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_action_cursor: Option<i64>,
}

impl MemoryStore {
    /// Past messages with date, conversation, scope and role filters, newest first.
    pub async fn history_search(
        &self,
        grant: &MemoryGrant,
        query: HistoryQuery,
    ) -> Result<HistoryPage> {
        let include = match query.include.as_deref() {
            None if query.tool.is_some() || query.status.is_some() => "actions",
            None => "messages",
            Some(v @ ("messages" | "actions" | "both")) => v,
            Some(_) => return Err(invalid("include must be messages, actions or both")),
        }
        .to_string();
        if let Some(status) = &query.status
            && !matches!(status.as_str(), "ok" | "error")
        {
            return Err(invalid("status must be ok or error"));
        }
        let mut page = if include == "actions" {
            grant.select(&query.scopes)?;
            HistoryPage {
                entries: Vec::new(),
                next_cursor: None,
                actions: Vec::new(),
                next_action_cursor: None,
            }
        } else {
            self.message_history(grant, query.clone()).await?
        };
        if include != "messages" {
            let (actions, cursor) = self.action_history(grant, &query).await?;
            page.actions = actions;
            page.next_action_cursor = cursor;
        }
        Ok(page)
    }

    async fn action_history(
        &self,
        grant: &MemoryGrant,
        query: &HistoryQuery,
    ) -> Result<(Vec<ActionEntry>, Option<i64>)> {
        let scopes = grant.select(&query.scopes)?;
        let limit = query.limit.unwrap_or(10).clamp(1, 20) as i64;
        let principal = grant.principal.clone();
        let query = query.clone();
        let redactor = self.redactor.clone();
        Ok(self
            .db
            .read(ReadLane::Interactive, move |c| {
                let mut sql = format!("SELECT j.rowid,j.run_id,j.sequence,b.primary_scope,b.conversation_id,b.task_id,COALESCE(json_extract(j.payload,'$.name'),''),COALESCE(json_extract(j.payload,'$.result.success'),0),j.payload,j.created_at FROM journal j JOIN memory_run_bindings b ON b.run_id=j.run_id WHERE j.kind='tool_result' AND j.payload IS NOT NULL AND json_valid(j.payload) AND b.principal_id=?1 AND b.primary_scope IN ({})", placeholders(2, scopes.len()));
                let mut values: Vec<Box<dyn rusqlite::ToSql>> = vec![Box::new(principal)];
                for s in scopes {
                    values.push(Box::new(s));
                }
                let mut add = |clause: &str, value: Box<dyn rusqlite::ToSql>, values: &mut Vec<Box<dyn rusqlite::ToSql>>| {
                    values.push(value);
                    sql.push_str(&clause.replace('?', &format!("?{}", values.len())));
                };
                if let Some(tool) = query.tool.clone() {
                    add(" AND json_extract(j.payload,'$.name')=?", Box::new(tool), &mut values);
                }
                if let Some(status) = query.status.as_deref() {
                    add(" AND COALESCE(json_extract(j.payload,'$.result.success'),0)=?", Box::new(i64::from(status == "ok")), &mut values);
                }
                if let Some(text) = query.text.clone().filter(|t| !t.trim().is_empty()) {
                    add(" AND instr(lower(j.payload),lower(?))>0", Box::new(text), &mut values);
                }
                if let Some(conversation) = query.conversation.clone() {
                    add(" AND b.conversation_id=?", Box::new(conversation), &mut values);
                }
                if let Some(from) = query.from {
                    add(" AND j.created_at>=?", Box::new(from), &mut values);
                }
                if let Some(to) = query.to {
                    add(" AND j.created_at<?", Box::new(to), &mut values);
                }
                if let Some(cursor) = query.action_cursor {
                    add(" AND j.rowid<?", Box::new(cursor), &mut values);
                }
                values.push(Box::new(limit + 1));
                sql.push_str(&format!(" ORDER BY j.rowid DESC LIMIT ?{}", values.len()));
                let mut s = c.prepare(&sql)?;
                let params: Vec<&dyn rusqlite::ToSql> = values.iter().map(|v| v.as_ref()).collect();
                let mut rows = s
                    .query_map(params.as_slice(), |r| {
                        let payload: String = r.get(8)?;
                        let output = serde_json::from_str::<serde_json::Value>(&payload)
                            .map(|v| v["result"]["output"].to_string())
                            .unwrap_or_default();
                        let summary: String = output.chars().take(600).collect();
                        Ok((
                            r.get::<_, i64>(0)?,
                            ActionEntry {
                                run_id: r.get(1)?,
                                sequence: r.get(2)?,
                                scope: r.get(3)?,
                                conversation: r.get(4)?,
                                task: r.get(5)?,
                                tool: r.get(6)?,
                                success: r.get::<_, i64>(7)? != 0,
                                summary,
                                at: r.get(9)?,
                            },
                        ))
                    })?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                let next = if rows.len() as i64 > limit {
                    rows.truncate(limit as usize);
                    rows.last().map(|(rowid, _)| *rowid)
                } else {
                    None
                };
                Ok((
                    rows.into_iter()
                        .map(|(_, mut e)| {
                            e.summary = redactor.redact_text(&e.summary);
                            e
                        })
                        .collect(),
                    next,
                ))
            })
            .await?)
    }

    async fn message_history(
        &self,
        grant: &MemoryGrant,
        query: HistoryQuery,
    ) -> Result<HistoryPage> {
        let scopes = grant.select(&query.scopes)?;
        let limit = query.limit.unwrap_or(10).clamp(1, 20) as i64;
        let principal = grant.principal.clone();
        let fts = query.text.as_deref().and_then(|t| fts_or(&query_terms(t)));
        Ok(self
            .db
            .read(ReadLane::Interactive, move |c| {
                let n = scopes.len();
                let mut sql = format!("SELECT m.rowid,m.id,COALESCE(m.scope_id,''),m.conversation_id,m.role,m.text,m.sent_at FROM messages m WHERE m.principal_id=?1 AND m.scope_id IN ({})", placeholders(2, n));
                let mut values: Vec<Box<dyn rusqlite::ToSql>> = vec![Box::new(principal)];
                for s in scopes {
                    values.push(Box::new(s));
                }
                let mut add = |clause: &str, value: Box<dyn rusqlite::ToSql>, values: &mut Vec<Box<dyn rusqlite::ToSql>>| {
                    values.push(value);
                    sql.push_str(&clause.replace("?", &format!("?{}", values.len())));
                };
                if let Some(q) = fts {
                    add(" AND m.rowid IN (SELECT rowid FROM messages_fts WHERE messages_fts MATCH ?)", Box::new(q), &mut values);
                }
                if let Some(from) = query.from {
                    add(" AND m.sent_at>=?", Box::new(from), &mut values);
                }
                if let Some(to) = query.to {
                    add(" AND m.sent_at<?", Box::new(to), &mut values);
                }
                if let Some(conversation) = query.conversation {
                    add(" AND m.conversation_id=?", Box::new(conversation), &mut values);
                }
                if let Some(role) = query.role {
                    add(" AND m.role=?", Box::new(role), &mut values);
                }
                if let Some(cursor) = query.cursor {
                    add(" AND m.rowid<?", Box::new(cursor), &mut values);
                }
                values.push(Box::new(limit + 1));
                sql.push_str(&format!(" ORDER BY m.rowid DESC LIMIT ?{}", values.len()));
                let mut s = c.prepare(&sql)?;
                let params: Vec<&dyn rusqlite::ToSql> = values.iter().map(|v| v.as_ref()).collect();
                let mut rows = s
                    .query_map(params.as_slice(), |r| {
                        Ok((r.get::<_, i64>(0)?, HistoryEntry { id: r.get(1)?, scope: r.get(2)?, conversation: r.get(3)?, role: r.get(4)?, text: r.get(5)?, sent_at: r.get(6)? }))
                    })?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                let next_cursor = if rows.len() as i64 > limit {
                    rows.truncate(limit as usize);
                    rows.last().map(|(rowid, _)| *rowid)
                } else {
                    None
                };
                Ok(HistoryPage { entries: rows.into_iter().map(|(_, e)| e).collect(), next_cursor, actions: Vec::new(), next_action_cursor: None })
            })
            .await?)
    }
}
