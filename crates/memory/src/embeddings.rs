//! Versioned embedding spaces, Matryoshka truncation, int8 storage, an indexing queue for
//! provider outages and dual-read reindexing when the model changes (architecture 10.8).
use crate::{BoxFuture, Result, digest, invalid};
use excalibur_store::{
    Database, Priority, ReadLane,
    rusqlite::{self, OptionalExtension},
    schema::{self, VectorEncoding},
};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex, RwLock},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderLocation {
    Local,
    Remote,
}
impl ProviderLocation {
    fn as_str(self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::Remote => "remote",
        }
    }
}

/// Common provider interface for local (ONNX/candle) or remote (API) embedders.
pub trait EmbeddingProvider: Send + Sync {
    fn model(&self) -> String;
    fn source_dimensions(&self) -> u32;
    fn location(&self) -> ProviderLocation;
    fn embed<'a>(
        &'a self,
        texts: &'a [String],
    ) -> BoxFuture<'a, std::result::Result<Vec<Vec<f32>>, String>>;
}

/// Default deployment profiles. Descriptors only: the local runtimes are not linked here.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EmbeddingProfile {
    DesktopLimitedMemory,
    ServerOrRoomyDesktop,
    MobileOrMinimal,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProfileDescriptor {
    pub model: &'static str,
    pub location: ProviderLocation,
    pub weights: &'static str,
    pub approximate_mib: u32,
    pub license: &'static str,
    pub license_requires_acceptance: bool,
    pub source_dimensions: u32,
}
impl EmbeddingProfile {
    pub fn descriptor(self) -> ProfileDescriptor {
        match self {
            Self::DesktopLimitedMemory => ProfileDescriptor {
                model: "EmbeddingGemma-300M",
                location: ProviderLocation::Local,
                weights: "4-bit",
                approximate_mib: 200,
                license: "Gemma terms",
                license_requires_acceptance: true,
                source_dimensions: 768,
            },
            Self::ServerOrRoomyDesktop => ProfileDescriptor {
                model: "Qwen3-Embedding-0.6B",
                location: ProviderLocation::Local,
                weights: "8-bit",
                approximate_mib: 620,
                license: "Apache-2.0",
                license_requires_acceptance: false,
                source_dimensions: 1024,
            },
            Self::MobileOrMinimal => ProfileDescriptor {
                model: "remote-api",
                location: ProviderLocation::Remote,
                weights: "remote",
                approximate_mib: 0,
                license: "provider terms",
                license_requires_acceptance: false,
                source_dimensions: 0,
            },
        }
    }
}

/// Truncate (Matryoshka), L2-normalize and quantize to int8.
pub fn prepare_vector(vector: &[f32], dimensions: u32) -> Vec<i8> {
    let take = (dimensions as usize).min(vector.len());
    let slice = &vector[..take];
    let norm = slice.iter().map(|x| x * x).sum::<f32>().sqrt();
    slice
        .iter()
        .map(|x| {
            let v = if norm > 0.0 { x / norm } else { 0.0 };
            (v * 127.0).round().clamp(-127.0, 127.0) as i8
        })
        .collect()
}
pub fn cosine_i8(a: &[i8], b: &[i8]) -> f32 {
    let n = a.len().min(b.len());
    let (mut dot, mut na, mut nb) = (0f64, 0f64, 0f64);
    for i in 0..n {
        let (x, y) = (a[i] as f64, b[i] as f64);
        dot += x * y;
        na += x * x;
        nb += y * y;
    }
    if na == 0.0 || nb == 0.0 {
        0.0
    } else {
        (dot / (na.sqrt() * nb.sqrt())) as f32
    }
}
fn to_json(vector: &[i8]) -> String {
    serde_json::to_string(vector).unwrap_or_else(|_| "[]".into())
}
fn to_blob(vector: &[i8]) -> Vec<u8> {
    vector.iter().map(|v| *v as u8).collect()
}
fn from_blob(blob: &[u8]) -> Vec<i8> {
    blob.iter().map(|v| *v as i8).collect()
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EmbeddingSpace {
    pub id: String,
    pub model: String,
    pub dimensions: u32,
    pub source_dimensions: u32,
    pub quantization: String,
    pub state: String,
    pub migrated_from: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexOutcome {
    pub indexed: Vec<String>,
    pub queued: Vec<String>,
}
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueueReport {
    pub indexed: usize,
    pub failed: usize,
    pub remaining: usize,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SemanticHit {
    pub scope: String,
    pub object_id: String,
    pub similarity: f32,
}
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct SemanticSearch {
    pub available: bool,
    pub spaces: Vec<String>,
    pub hits: Vec<SemanticHit>,
}

fn space_id(model: &str, dimensions: u32) -> String {
    let base: String = model
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .take(40)
        .collect();
    format!(
        "emb_{base}_{dimensions}_{}",
        &digest(format!("{model}\0{dimensions}").as_bytes())[..8]
    )
}

fn load_spaces(c: &rusqlite::Connection) -> rusqlite::Result<Vec<EmbeddingSpace>> {
    let mut s = c.prepare("SELECT id,model,dimensions,COALESCE(source_dimensions,dimensions),quantization,state,migrated_from FROM embedding_spaces ORDER BY created_at,id")?;
    s.query_map([], |r| {
        Ok(EmbeddingSpace {
            id: r.get(0)?,
            model: r.get(1)?,
            dimensions: r.get(2)?,
            source_dimensions: r.get(3)?,
            quantization: r.get(4)?,
            state: r.get(5)?,
            migrated_from: r.get(6)?,
        })
    })?
    .collect()
}

/// Text authoritative for a vector object; the original text is always retained.
pub(crate) fn object_text(
    c: &rusqlite::Connection,
    object: &str,
) -> rusqlite::Result<Option<String>> {
    if let Some(id) = object.strip_prefix("memory:") {
        return c
            .query_row("SELECT statement FROM memories WHERE id=?1", [id], |r| {
                r.get(0)
            })
            .optional();
    }
    if let Some(id) = object.strip_prefix("chunk:") {
        return c
            .query_row(
                "SELECT CASE WHEN context IS NULL OR context='' THEN text ELSE context||' '||text END FROM doc_chunks WHERE id=?1",
                [id],
                |r| r.get(0),
            )
            .optional();
    }
    if let Some(id) = object.strip_prefix("key:") {
        return c
            .query_row("SELECT key_text FROM memory_keys WHERE id=?1", [id], |r| {
                r.get(0)
            })
            .optional();
    }
    Ok(None)
}

/// Registry of attached providers and their durable spaces.
#[derive(Clone)]
pub struct EmbeddingRegistry {
    db: Database,
    providers: Arc<RwLock<BTreeMap<String, Arc<dyn EmbeddingProvider>>>>,
}
impl EmbeddingRegistry {
    pub(crate) fn new(db: Database) -> Self {
        Self {
            db,
            providers: Arc::default(),
        }
    }
    fn provider(&self, space: &str) -> Option<Arc<dyn EmbeddingProvider>> {
        self.providers.read().unwrap().get(space).cloned()
    }
    pub async fn spaces(&self) -> Result<Vec<EmbeddingSpace>> {
        Ok(self
            .db
            .read(ReadLane::Interactive, |c| Ok(load_spaces(c)?))
            .await?)
    }
    async fn live_spaces(&self) -> Result<Vec<EmbeddingSpace>> {
        Ok(self
            .spaces()
            .await?
            .into_iter()
            .filter(|s| s.state != "retired")
            .collect())
    }
    async fn create_space(
        &self,
        provider: &Arc<dyn EmbeddingProvider>,
        dimensions: u32,
        state: &'static str,
        from: Option<String>,
        now: i64,
    ) -> Result<EmbeddingSpace> {
        let model = provider.model();
        let source = provider.source_dimensions();
        let dims = if source == 0 {
            dimensions
        } else {
            dimensions.min(source)
        };
        let id = space_id(&model, dims);
        let location = provider.location().as_str();
        let (id2, model2, from2) = (id.clone(), model.clone(), from.clone());
        self.db
            .write(Priority::Normal, move |c| {
                schema::create_quantized_vector_space(c, &id2, dims, VectorEncoding::Int8)?;
                c.execute(
                    "INSERT OR IGNORE INTO embedding_spaces(id,model,dimensions,quantization,state,provider,source_dimensions,migrated_from,activated_at) VALUES(?1,?2,?3,'int8',?4,?5,?6,?7,?8)",
                    rusqlite::params![id2, model2, dims, state, location, source, from2, (state == "active").then_some(now)],
                )?;
                Ok(())
            })
            .await?;
        self.providers
            .write()
            .unwrap()
            .insert(id.clone(), provider.clone());
        Ok(EmbeddingSpace {
            id,
            model,
            dimensions: dims,
            source_dimensions: source,
            quantization: "int8".into(),
            state: state.into(),
            migrated_from: from,
        })
    }
    /// Attach a provider to its existing space, or create the first active space.
    pub async fn attach(
        &self,
        provider: Arc<dyn EmbeddingProvider>,
        dimensions: u32,
        now: i64,
    ) -> Result<EmbeddingSpace> {
        let model = provider.model();
        let spaces = self.live_spaces().await?;
        if let Some(space) = spaces.iter().find(|s| s.model == model) {
            self.providers
                .write()
                .unwrap()
                .insert(space.id.clone(), provider);
            return Ok(space.clone());
        }
        if spaces.iter().any(|s| s.state == "active") {
            return Err(invalid(
                "another embedding space is active; change models with begin_migration",
            ));
        }
        self.create_space(&provider, dimensions, "active", None, now)
            .await
    }
    /// Detach a provider (for example, after a local model was unloaded or an API failed).
    pub fn detach(&self, space: &str) {
        self.providers.write().unwrap().remove(space);
    }
    /// Start a background reindex into a new space. Reads use both spaces until it finishes.
    pub async fn begin_migration(
        &self,
        provider: Arc<dyn EmbeddingProvider>,
        dimensions: u32,
        now: i64,
    ) -> Result<EmbeddingSpace> {
        let active = self
            .live_spaces()
            .await?
            .into_iter()
            .find(|s| s.state == "active")
            .ok_or_else(|| invalid("no active embedding space to migrate from"))?;
        if active.model == provider.model() {
            return Err(invalid("migration requires a different model"));
        }
        let space = self
            .create_space(
                &provider,
                dimensions,
                "migrating",
                Some(active.id.clone()),
                now,
            )
            .await?;
        let (old, new) = (active.id, space.id.clone());
        self.db
            .write(Priority::Background, move |c| {
                let mut s = c.prepare("SELECT scope_id,object_id,principal_id FROM memory_vector_objects WHERE space_id=?1 UNION SELECT scope_id,object_id,principal_id FROM memory_index_queue WHERE space_id=?1")?;
                let rows = s
                    .query_map([&old], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?)))?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                for (scope, object, principal) in rows {
                    if let Some(text) = object_text(c, &object)? {
                        c.execute("INSERT OR IGNORE INTO memory_index_queue(space_id,principal_id,scope_id,object_id,text,enqueued_at) VALUES(?1,?2,?3,?4,?5,?6)", rusqlite::params![new, principal, scope, object, text, now])?;
                    }
                }
                Ok(())
            })
            .await?;
        Ok(space)
    }
    /// Promote the new space only when every object of the old one is present.
    pub async fn finish_migration(&self, space: &str, now: i64) -> Result<()> {
        let space = space.to_string();
        let outcome = self
            .db
            .write(Priority::Normal, move |c| {
                let from: Option<String> = c
                    .query_row(
                        "SELECT migrated_from FROM embedding_spaces WHERE id=?1 AND state='migrating'",
                        [&space],
                        |r| r.get(0),
                    )
                    .optional()?
                    .flatten();
                let Some(from) = from else {
                    return Ok(Err("space is not migrating".to_string()));
                };
                let missing: i64 = c.query_row(
                    "SELECT count(*) FROM memory_vector_objects o WHERE o.space_id=?1 AND NOT EXISTS(SELECT 1 FROM memory_vector_objects n WHERE n.space_id=?2 AND n.scope_id=o.scope_id AND n.object_id=o.object_id)",
                    rusqlite::params![from, space],
                    |r| r.get(0),
                )?;
                let queued: i64 = c.query_row(
                    "SELECT count(*) FROM memory_index_queue WHERE space_id=?1",
                    [&space],
                    |r| r.get(0),
                )?;
                if missing > 0 || queued > 0 {
                    return Ok(Err(format!(
                        "migration incomplete: {missing} missing, {queued} queued"
                    )));
                }
                c.execute("UPDATE embedding_spaces SET state='retired',updated_at=?2 WHERE id=?1", rusqlite::params![from, now])?;
                c.execute("UPDATE embedding_spaces SET state='active',activated_at=?2,updated_at=?2 WHERE id=?1", rusqlite::params![space, now])?;
                c.execute("DELETE FROM memory_index_queue WHERE space_id=?1", [&from])?;
                Ok(Ok(from))
            })
            .await?;
        match outcome {
            Ok(from) => {
                self.detach(&from);
                Ok(())
            }
            Err(message) => Err(invalid(message)),
        }
    }
    #[allow(clippy::too_many_arguments)]
    async fn write_vector(
        &self,
        space: &EmbeddingSpace,
        principal: &str,
        scope: &str,
        object: &str,
        text: &str,
        vector: Vec<i8>,
        now: i64,
    ) -> Result<()> {
        let (space, principal, scope, object, hash) = (
            space.id.clone(),
            principal.to_string(),
            scope.to_string(),
            object.to_string(),
            digest(text.as_bytes()),
        );
        self.db
            .write(Priority::Normal, move |c| {
                schema::put_vector(c, &space, &scope, &object, &to_json(&vector))?;
                c.execute("INSERT INTO memory_vector_objects(space_id,scope_id,object_id,principal_id,text_hash,vector,indexed_at) VALUES(?1,?2,?3,?4,?5,?6,?7) ON CONFLICT(space_id,scope_id,object_id) DO UPDATE SET text_hash=excluded.text_hash,vector=excluded.vector,indexed_at=excluded.indexed_at", rusqlite::params![space, scope, object, principal, hash, to_blob(&vector), now])?;
                c.execute("DELETE FROM memory_index_queue WHERE space_id=?1 AND scope_id=?2 AND object_id=?3", rusqlite::params![space, scope, object])?;
                Ok(())
            })
            .await?;
        Ok(())
    }
    #[allow(clippy::too_many_arguments)]
    async fn enqueue(
        &self,
        space: &str,
        principal: &str,
        scope: &str,
        object: &str,
        text: &str,
        error: &str,
        now: i64,
    ) -> Result<()> {
        let (space, principal, scope, object, text, error) = (
            space.to_string(),
            principal.to_string(),
            scope.to_string(),
            object.to_string(),
            text.to_string(),
            error.to_string(),
        );
        self.db
            .write(Priority::Normal, move |c| {
                c.execute("INSERT INTO memory_index_queue(space_id,principal_id,scope_id,object_id,text,enqueued_at,attempts,last_error) VALUES(?1,?2,?3,?4,?5,?6,1,?7) ON CONFLICT(space_id,scope_id,object_id) DO UPDATE SET text=excluded.text,attempts=attempts+1,last_error=excluded.last_error", rusqlite::params![space, principal, scope, object, text, now, error])?;
                Ok(())
            })
            .await?;
        Ok(())
    }
    /// Embed `text` into every live space; unavailable providers enqueue the object.
    pub async fn index(
        &self,
        principal: &str,
        scope: &str,
        object: &str,
        text: &str,
        now: i64,
    ) -> Result<IndexOutcome> {
        let mut outcome = IndexOutcome::default();
        let owned = vec![text.to_string()];
        for space in self.live_spaces().await? {
            let embedded = match self.provider(&space.id) {
                Some(provider) => provider
                    .embed(&owned)
                    .await
                    .and_then(|mut v| v.pop().ok_or_else(|| "empty embedding response".into())),
                None => Err("provider not attached".into()),
            };
            match embedded {
                Ok(vector) => {
                    let vector = prepare_vector(&vector, space.dimensions);
                    self.write_vector(&space, principal, scope, object, text, vector, now)
                        .await?;
                    outcome.indexed.push(space.id);
                }
                Err(error) => {
                    self.enqueue(&space.id, principal, scope, object, text, &error, now)
                        .await?;
                    outcome.queued.push(space.id);
                }
            }
        }
        Ok(outcome)
    }
    /// Drain the queue for spaces whose provider is attached.
    pub async fn process_queue(&self, limit: usize, now: i64) -> Result<QueueReport> {
        let mut report = QueueReport::default();
        for space in self.live_spaces().await? {
            let Some(provider) = self.provider(&space.id) else {
                continue;
            };
            let (id, limit2) = (space.id.clone(), limit as i64);
            let batch = self
                .db
                .read(ReadLane::Background, move |c| {
                    let mut s = c.prepare("SELECT principal_id,scope_id,object_id,text FROM memory_index_queue WHERE space_id=?1 ORDER BY enqueued_at,object_id LIMIT ?2")?;
                    let rows = s
                        .query_map(rusqlite::params![id, limit2], |r| {
                            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?, r.get::<_, String>(3)?))
                        })?
                        .collect::<rusqlite::Result<Vec<_>>>()?;
                    Ok(rows)
                })
                .await?;
            if batch.is_empty() {
                continue;
            }
            let texts: Vec<String> = batch.iter().map(|b| b.3.clone()).collect();
            match provider.embed(&texts).await {
                Ok(vectors) if vectors.len() == texts.len() => {
                    for ((principal, scope, object, text), vector) in batch.into_iter().zip(vectors)
                    {
                        let vector = prepare_vector(&vector, space.dimensions);
                        self.write_vector(&space, &principal, &scope, &object, &text, vector, now)
                            .await?;
                        report.indexed += 1;
                    }
                }
                Ok(_) | Err(_) => report.failed += batch.len(),
            }
        }
        report.remaining = self
            .db
            .read(ReadLane::Background, |c| {
                Ok(c.query_row("SELECT count(*) FROM memory_index_queue q JOIN embedding_spaces s ON s.id=q.space_id WHERE s.state!='retired'", [], |r| r.get::<_, i64>(0))? as usize)
            })
            .await?;
        Ok(report)
    }
    pub async fn queued(&self) -> Result<usize> {
        Ok(self
            .db
            .read(ReadLane::Interactive, |c| {
                Ok(
                    c.query_row("SELECT count(*) FROM memory_index_queue", [], |r| {
                        r.get::<_, i64>(0)
                    })? as usize,
                )
            })
            .await?)
    }
    /// Remove an object from every space (purge on user request).
    pub(crate) fn remove_in(
        c: &rusqlite::Connection,
        scope: &str,
        object: &str,
    ) -> rusqlite::Result<()> {
        for space in load_spaces(c)? {
            schema::delete_vector(c, &space.id, scope, object)?;
        }
        c.execute(
            "DELETE FROM memory_vector_objects WHERE scope_id=?1 AND object_id=?2",
            rusqlite::params![scope, object],
        )?;
        c.execute(
            "DELETE FROM memory_index_queue WHERE scope_id=?1 AND object_id=?2",
            rusqlite::params![scope, object],
        )?;
        Ok(())
    }
    /// Stored quantized vector of an object in the active space, if indexed.
    pub(crate) fn stored_vector(
        c: &rusqlite::Connection,
        scope: &str,
        object: &str,
    ) -> rusqlite::Result<Option<(String, Vec<i8>)>> {
        c.query_row(
            "SELECT o.space_id,o.vector FROM memory_vector_objects o JOIN embedding_spaces s ON s.id=o.space_id WHERE o.scope_id=?1 AND o.object_id=?2 AND s.state!='retired' ORDER BY s.state='active' DESC LIMIT 1",
            rusqlite::params![scope, object],
            |r| Ok((r.get::<_, String>(0)?, from_blob(&r.get::<_, Vec<u8>>(1)?))),
        )
        .optional()
    }
    /// Embed a text with every live attached provider. Missing spaces mean degradation.
    pub(crate) async fn embed_query(&self, text: &str) -> Vec<(EmbeddingSpace, Vec<i8>)> {
        let mut out = Vec::new();
        let Ok(spaces) = self.live_spaces().await else {
            return out;
        };
        let owned = vec![text.to_string()];
        for space in spaces {
            if let Some(provider) = self.provider(&space.id)
                && let Ok(mut vectors) = provider.embed(&owned).await
                && let Some(vector) = vectors.pop()
            {
                let prepared = prepare_vector(&vector, space.dimensions);
                out.push((space, prepared));
            }
        }
        out
    }
    /// KNN inside explicit scopes across live spaces; results merge by best similarity.
    pub async fn search(&self, scopes: &[String], text: &str, k: usize) -> Result<SemanticSearch> {
        let queries = self.embed_query(text).await;
        self.search_prepared(scopes, &queries, k, None).await
    }
    pub(crate) async fn search_prepared(
        &self,
        scopes: &[String],
        queries: &[(EmbeddingSpace, Vec<i8>)],
        k: usize,
        prefix: Option<&'static str>,
    ) -> Result<SemanticSearch> {
        let mut result = SemanticSearch {
            available: !queries.is_empty(),
            ..Default::default()
        };
        if queries.is_empty() || scopes.is_empty() {
            return Ok(result);
        }
        let mut best: BTreeMap<(String, String), f32> = BTreeMap::new();
        for (space, query) in queries {
            let (id, scopes2, query2) = (space.id.clone(), scopes.to_vec(), query.clone());
            // Over-fetch when filtering by object kind so the filtered list still fills k.
            let fetch = if prefix.is_some() {
                (k * 4).min(4096)
            } else {
                k.min(4096)
            } as u32;
            let hits = self
                .db
                .read(ReadLane::Interactive, move |c| {
                    let refs: Vec<&str> = scopes2.iter().map(String::as_str).collect();
                    let hits = schema::query_vectors(c, &id, &refs, &to_json(&query2), fetch.max(1))?;
                    let mut out = Vec::new();
                    for hit in hits {
                        let vector: Option<Vec<u8>> = c
                            .query_row(
                                "SELECT vector FROM memory_vector_objects WHERE space_id=?1 AND scope_id=?2 AND object_id=?3",
                                rusqlite::params![id, hit.scope_id, hit.object_id],
                                |r| r.get(0),
                            )
                            .optional()?;
                        if let Some(vector) = vector {
                            out.push((hit.scope_id, hit.object_id, cosine_i8(&query2, &from_blob(&vector))));
                        }
                    }
                    Ok(out)
                })
                .await?;
            result.spaces.push(space.id.clone());
            for (scope, object, similarity) in hits {
                if prefix.is_some_and(|p| !object.starts_with(p)) {
                    continue;
                }
                let entry = best.entry((scope, object)).or_insert(f32::MIN);
                *entry = entry.max(similarity);
            }
        }
        let mut hits: Vec<SemanticHit> = best
            .into_iter()
            .map(|((scope, object_id), similarity)| SemanticHit {
                scope,
                object_id,
                similarity,
            })
            .collect();
        hits.sort_by(|a, b| {
            b.similarity
                .total_cmp(&a.similarity)
                .then_with(|| a.object_id.cmp(&b.object_id))
        });
        hits.truncate(k);
        result.hits = hits;
        Ok(result)
    }
    /// Indexed vectors of one scope in the active space.
    pub async fn vector_count(&self, scope: &str) -> Result<u64> {
        let scope = scope.to_string();
        Ok(self
            .db
            .read(ReadLane::Background, move |c| {
                Ok(c.query_row("SELECT count(*) FROM memory_vector_objects o JOIN embedding_spaces s ON s.id=o.space_id AND s.state='active' WHERE o.scope_id=?1", [&scope], |r| r.get::<_, i64>(0))? as u64)
            })
            .await?)
    }
}

/// Loader for a local model; loading may fail (for example, insufficient RAM).
pub trait LocalModelLoader: Send + Sync {
    fn model(&self) -> String;
    fn source_dimensions(&self) -> u32;
    fn load(&self) -> std::result::Result<Arc<dyn LocalEmbeddingModel>, String>;
}
pub trait LocalEmbeddingModel: Send + Sync {
    fn embed(&self, texts: &[String]) -> std::result::Result<Vec<Vec<f32>>, String>;
}
type Clock = Arc<dyn Fn() -> i64 + Send + Sync>;

/// Lazily loads a local model on first use and releases it after an idle period.
pub struct LazyLocalEmbedder {
    loader: Arc<dyn LocalModelLoader>,
    state: Mutex<Option<(Arc<dyn LocalEmbeddingModel>, i64)>>,
    idle_seconds: i64,
    clock: Clock,
}
impl LazyLocalEmbedder {
    pub fn new(loader: Arc<dyn LocalModelLoader>, clock: Clock) -> Self {
        Self {
            loader,
            state: Mutex::new(None),
            idle_seconds: 600,
            clock,
        }
    }
    pub fn with_idle_seconds(mut self, seconds: i64) -> Self {
        self.idle_seconds = seconds;
        self
    }
    pub fn loaded(&self) -> bool {
        self.state.lock().unwrap().is_some()
    }
    /// Release the model when idle; returns whether it was unloaded.
    pub fn unload_if_idle(&self) -> bool {
        let now = (self.clock)();
        let mut state = self.state.lock().unwrap();
        if state
            .as_ref()
            .is_some_and(|(_, used)| now - used >= self.idle_seconds)
        {
            *state = None;
            return true;
        }
        false
    }
}
impl EmbeddingProvider for LazyLocalEmbedder {
    fn model(&self) -> String {
        self.loader.model()
    }
    fn source_dimensions(&self) -> u32 {
        self.loader.source_dimensions()
    }
    fn location(&self) -> ProviderLocation {
        ProviderLocation::Local
    }
    fn embed<'a>(
        &'a self,
        texts: &'a [String],
    ) -> BoxFuture<'a, std::result::Result<Vec<Vec<f32>>, String>> {
        Box::pin(async move {
            let model = {
                let mut state = self.state.lock().unwrap();
                let now = (self.clock)();
                match state.as_mut() {
                    Some((model, used)) => {
                        *used = now;
                        model.clone()
                    }
                    None => {
                        let model = self.loader.load()?;
                        *state = Some((model.clone(), now));
                        model
                    }
                }
            };
            model.embed(texts)
        })
    }
}
