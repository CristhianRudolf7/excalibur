//! Host-registered domain tools, deterministic retrieval and secured delegation.
use excalibur_types::runtime::*;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, Mutex, RwLock},
};
const SCHEMA_URI: &str = "https://json-schema.org/draft/2020-12/schema";
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CallerRole {
    Master,
    Worker,
    Expert,
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ToolEffect {
    ReadOnly,
    WriteLocal,
    Exec,
    WriteExternal,
    Communication,
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CancellationClass {
    Immediate,
    Graceful,
    AfterCompletion,
    Cooperative,
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ToolRisk {
    Low,
    Medium,
    High,
    Unknown,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DryRunBehavior {
    ReadOnly,
    Adapter,
    Unsupported,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ToolEmbedding {
    pub model: String,
    pub values: Vec<f32>,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct CatalogTool {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
    pub output_schema: Value,
    pub tags: Vec<String>,
    pub effect: ToolEffect,
    pub idempotency: Effect,
    pub resources_description: String,
    pub risk: ToolRisk,
    pub timeout_ms: u64,
    pub output_budget_bytes: usize,
    pub platforms: Vec<String>,
    pub inverse: Option<String>,
    pub probe: Option<String>,
    pub dry_run: DryRunBehavior,
    pub fast: bool,
    pub cancel: CancellationClass,
    #[serde(skip)]
    pub embedding: Option<ToolEmbedding>,
}
impl CatalogTool {
    /// Defaults are appropriate for a host-confirmed read-only adapter. A host
    /// registering writes must declare its real effect and recovery contract.
    pub fn from_definition(definition: ToolDefinition) -> Self {
        Self{name:definition.name,description:definition.description,input_schema:definition.input_schema,output_schema:json!({"$schema":SCHEMA_URI}),tags:Vec::new(),effect:ToolEffect::ReadOnly,idempotency:Effect::Pure,resources_description:"The registered runtime resolves canonical resources during scheduling and preparation.".into(),risk:ToolRisk::Unknown,timeout_ms:30_000,output_budget_bytes:50*1024,platforms:vec!["host_adapter".into()],inverse:None,probe:None,dry_run:DryRunBehavior::ReadOnly,fast:false,cancel:CancellationClass::Immediate,embedding:None}
    }
    pub fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.name.clone(),
            description: self.description.clone(),
            input_schema: self.input_schema.clone(),
        }
    }
    pub fn digest(&self) -> String {
        digest(&serde_json::to_vec(self).expect("serializable tool metadata"))
    }
}
pub trait ToolEmbedder: Send + Sync {
    fn model(&self) -> &str;
    fn dimensions(&self) -> usize;
    fn embed<'a>(&'a self, query: &'a str) -> BoxFuture<'a, Result<Vec<f32>, String>>;
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ToolReference {
    Name(String),
    Mcp { server: String, tool: String },
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct RetrievalScore {
    pub name: String,
    pub bm25: f64,
    pub cosine: Option<f64>,
    pub fused: f64,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ToolSearch {
    pub catalog_revision: u64,
    pub catalog_hash: String,
    pub tools: Vec<CatalogTool>,
    pub scores: Vec<RetrievalScore>,
    pub vector_status: String,
}
#[derive(Clone)]
pub struct ToolCatalog {
    inner: Arc<RwLock<Arc<CatalogSnapshot>>>,
}
struct CatalogSnapshot {
    revision: u64,
    hash: String,
    tools: BTreeMap<String, CatalogTool>,
    terms: BTreeMap<String, BTreeMap<String, usize>>,
    df: BTreeMap<String, usize>,
    average_length: f64,
}
impl ToolCatalog {
    pub fn new(tools: Vec<CatalogTool>) -> Result<Self, String> {
        Ok(Self {
            inner: Arc::new(RwLock::new(Arc::new(CatalogSnapshot::build(1, tools)?))),
        })
    }
    pub fn replace(&self, tools: Vec<CatalogTool>) -> Result<(), String> {
        let previous = self.snapshot();
        let next = CatalogSnapshot::build(previous.revision.saturating_add(1), tools)?;
        let mut guard = self.inner.write().map_err(|_| "catalog lock poisoned")?;
        if guard.revision != previous.revision {
            return Err(
                "catalog changed while preparing replacement; retry the host update".into(),
            );
        }
        *guard = Arc::new(next);
        Ok(())
    }
    fn snapshot(&self) -> Arc<CatalogSnapshot> {
        self.inner.read().unwrap_or_else(|e| e.into_inner()).clone()
    }
    pub fn names(&self) -> BTreeSet<String> {
        self.snapshot().tools.keys().cloned().collect()
    }
    pub fn get(&self, name: &str) -> Option<CatalogTool> {
        self.snapshot().tools.get(name).cloned()
    }
    pub fn revision(&self) -> u64 {
        self.snapshot().revision
    }
    pub fn hash(&self) -> String {
        self.snapshot().hash.clone()
    }
    pub fn resolve_skill_tools(&self, names: &[String]) -> Result<Vec<CatalogTool>, String> {
        self.resolve_references(
            &names
                .iter()
                .cloned()
                .map(ToolReference::Name)
                .collect::<Vec<_>>(),
        )
    }
    pub fn resolve_references(&self, refs: &[ToolReference]) -> Result<Vec<CatalogTool>, String> {
        let snapshot = self.snapshot();
        let mut names = BTreeSet::new();
        for reference in refs {
            names.insert(match reference {
                ToolReference::Name(name) => name.clone(),
                ToolReference::Mcp { server, tool } => mcp_name(server, tool)?,
            });
        }
        names
            .into_iter()
            .map(|name| {
                snapshot
                    .tools
                    .get(&name)
                    .cloned()
                    .ok_or_else(|| format!("unavailable skill tool: {name}"))
            })
            .collect()
    }
    pub fn search(
        &self,
        query: &str,
        role: CallerRole,
        vector: Option<(&str, &[f32])>,
    ) -> Result<ToolSearch, String> {
        self.snapshot().search(query, role, vector)
    }
}
#[derive(Clone)]
pub struct DeferredTools {
    delegate: Arc<dyn ToolRuntime>,
    catalog: Arc<ToolCatalog>,
    role: CallerRole,
    core: Vec<ToolDefinition>,
    embedder: Option<Arc<dyn ToolEmbedder>>,
    embedding_timeout_ms: u64,
    native_pins: Arc<Mutex<BTreeMap<String, Vec<ToolDefinition>>>>,
}
impl DeferredTools {
    /// The delegate must already enforce host permissions (for example
    /// SecuredTools). Every resolved domain call enters that same boundary.
    pub fn new(
        delegate: Arc<dyn ToolRuntime>,
        catalog: Arc<ToolCatalog>,
        role: CallerRole,
    ) -> Result<Self, String> {
        let snapshot = catalog.snapshot();
        let definitions = delegate.definitions();
        let names = snapshot.tools.keys().cloned().collect::<BTreeSet<_>>();
        let mut core = definitions
            .iter()
            .filter(|d| {
                !names.contains(&d.name) && !matches!(d.name.as_str(), "search_tools" | "call_tool")
            })
            .cloned()
            .collect::<Vec<_>>();
        core.sort_by(|a, b| a.name.cmp(&b.name));
        for definition in &core {
            validate_name(&definition.name)?;
            schema(&definition.input_schema)?;
        }
        if core.windows(2).any(|v| v[0].name == v[1].name) {
            return Err("duplicate core tool".into());
        }
        let wrapper = Self {
            delegate,
            catalog,
            role,
            core,
            embedder: None,
            embedding_timeout_ms: 2000,
            native_pins: Arc::new(Mutex::new(BTreeMap::new())),
        };
        wrapper.validate_snapshot(&snapshot)?;
        Ok(wrapper)
    }
    pub fn with_embedder(mut self, embedder: Arc<dyn ToolEmbedder>) -> Result<Self, String> {
        if embedder.model().is_empty() || !(1..=4096).contains(&embedder.dimensions()) {
            return Err("invalid host embedding configuration".into());
        }
        self.embedder = Some(embedder);
        Ok(self)
    }
    pub fn with_embedding_timeout_ms(mut self, timeout_ms: u64) -> Result<Self, String> {
        if !(1..=30_000).contains(&timeout_ms) {
            return Err("embedding deadline must be 1..30000 ms".into());
        }
        self.embedding_timeout_ms = timeout_ms;
        Ok(self)
    }
    pub fn catalog(&self) -> Arc<ToolCatalog> {
        self.catalog.clone()
    }
    pub fn resolve_skill_tools(&self, names: &[String]) -> Result<Vec<CatalogTool>, String> {
        let tools = self.catalog.resolve_skill_tools(names)?;
        if self.role == CallerRole::Master && tools.iter().any(|t| !t.fast) {
            return Err("master cannot load a non-fast domain tool".into());
        }
        for tool in &tools {
            self.sanitize_metadata(tool)?;
        }
        Ok(tools)
    }
    fn sanitize_metadata(&self, tool: &CatalogTool) -> Result<(), String> {
        let raw = serde_json::to_value(tool).map_err(|_| "invalid tool metadata")?;
        if self.delegate.sanitize_durable(&raw)? != raw {
            return Err("sensitive content in tool metadata".into());
        }
        Ok(())
    }
    fn validate_snapshot(&self, snapshot: &CatalogSnapshot) -> Result<(), String> {
        let definitions = self.delegate.definitions();
        for entry in snapshot.tools.values() {
            if self.core.iter().any(|t| t.name == entry.name) {
                return Err("deferred registration collides with frozen core".into());
            }
            if !definitions
                .iter()
                .any(|d| d.name == entry.name && d.input_schema == entry.input_schema)
            {
                return Err(format!(
                    "registered tool has no matching runtime adapter: {}",
                    entry.name
                ));
            }
            self.sanitize_metadata(entry)?;
        }
        Ok(())
    }
}
fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
impl CatalogSnapshot {
    fn build(revision: u64, entries: Vec<CatalogTool>) -> Result<Self, String> {
        if entries.len() > 8192 {
            return Err("catalog exceeds 8192 tools".into());
        }
        let mut tools = BTreeMap::new();
        let mut terms = BTreeMap::new();
        let mut df = BTreeMap::new();
        let mut total_length = 0;
        let mut total_bytes = 0usize;
        let mut fingerprint = Vec::new();
        for tool in entries {
            validate_name(&tool.name)?;
            if matches!(tool.name.as_str(), "search_tools" | "call_tool")
                || tool.description.is_empty()
                || tool.description.len() > 8192
                || tool.tags.len() > 64
                || tool.tags.iter().any(|s| s.len() > 128)
                || !(1..=86_400_000).contains(&tool.timeout_ms)
                || !(256..=1_048_576).contains(&tool.output_budget_bytes)
                || tool.platforms.is_empty()
            {
                return Err("invalid bounded tool metadata".into());
            }
            schema(&tool.input_schema)?;
            schema(&tool.output_schema)?;
            if !tool.input_schema.is_object() {
                return Err("tool input schema must be an object".into());
            }
            if let Some(vector) = &tool.embedding {
                check_vector(&vector.values)?;
                if vector.model.is_empty() || vector.model.len() > 256 {
                    return Err("invalid embedding model identity".into());
                }
            }
            let bytes = serde_json::to_vec(&(&tool, &tool.embedding))
                .map_err(|_| "invalid tool metadata")?;
            total_bytes = total_bytes.saturating_add(bytes.len());
            if total_bytes > 64 * 1024 * 1024 {
                return Err("catalog exceeds 64 MiB".into());
            }
            let mut words = tokenize(&tool.description);
            for tag in &tool.tags {
                words.extend(tokenize(tag));
                words.extend(tokenize(tag));
            }
            for _ in 0..3 {
                words.extend(tokenize(&tool.name));
            }
            let mut counts = BTreeMap::new();
            for word in words {
                *counts.entry(word).or_insert(0usize) += 1;
            }
            total_length += counts.values().sum::<usize>();
            for word in counts.keys() {
                *df.entry(word.clone()).or_insert(0usize) += 1;
            }
            fingerprint.push((tool.name.clone(), bytes));
            terms.insert(tool.name.clone(), counts);
            if tools.insert(tool.name.clone(), tool).is_some() {
                return Err("duplicate catalog name".into());
            }
        }
        fingerprint.sort_by(|a, b| a.0.cmp(&b.0));
        let bytes = serde_json::to_vec(&fingerprint).map_err(|_| "invalid catalog fingerprint")?;
        let average_length = total_length as f64 / tools.len().max(1) as f64;
        Ok(Self {
            revision,
            hash: digest(&bytes),
            tools,
            terms,
            df,
            average_length,
        })
    }
    fn search(
        &self,
        query: &str,
        role: CallerRole,
        vector: Option<(&str, &[f32])>,
    ) -> Result<ToolSearch, String> {
        if query.is_empty() || query.len() > 8192 {
            return Err("search query requires 1..8192 bytes".into());
        }
        let query_terms = tokenize(query).into_iter().collect::<BTreeSet<_>>();
        if query_terms.is_empty() {
            return Err("search query requires a word".into());
        }
        if let Some((_, v)) = vector {
            check_vector(v)?;
        }
        let eligible = |t: &CatalogTool| role != CallerRole::Master || t.fast;
        let mut lexical = Vec::new();
        let mut semantic = Vec::new();
        let n = self.tools.len() as f64;
        for (name, tool) in self.tools.iter().filter(|(_, t)| eligible(t)) {
            let terms = &self.terms[name];
            let length = terms.values().sum::<usize>() as f64;
            let mut score = 0.;
            for term in &query_terms {
                let tf = *terms.get(term).unwrap_or(&0) as f64;
                if tf == 0. {
                    continue;
                }
                let df = *self.df.get(term).unwrap_or(&0) as f64;
                let idf = (1. + (n - df + 0.5) / (df + 0.5)).ln();
                score += idf * tf * 2.2
                    / (tf + 1.2 * (0.25 + 0.75 * length / self.average_length.max(1.)));
            }
            if name == query {
                score += 10.;
            }
            if score > 0. {
                lexical.push((name.clone(), score));
            }
            if let Some((model, query_vector)) = vector
                && let Some(index) = &tool.embedding
                && index.model == model
                && index.values.len() == query_vector.len()
            {
                let similarity = cosine(query_vector, &index.values);
                if similarity > 0. {
                    semantic.push((name.clone(), similarity));
                }
            }
        }
        let rank = |items: &mut Vec<(String, f64)>| {
            items.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)))
        };
        rank(&mut lexical);
        rank(&mut semantic);
        let mut fused: BTreeMap<String, RetrievalScore> = BTreeMap::new();
        for (index, (name, value)) in lexical.iter().enumerate() {
            let entry = fused.entry(name.clone()).or_insert(RetrievalScore {
                name: name.clone(),
                bm25: 0.,
                cosine: None,
                fused: 0.,
            });
            entry.bm25 = *value;
            entry.fused += 1. / (61. + index as f64);
        }
        for (index, (name, value)) in semantic.iter().enumerate() {
            let entry = fused.entry(name.clone()).or_insert(RetrievalScore {
                name: name.clone(),
                bm25: 0.,
                cosine: None,
                fused: 0.,
            });
            entry.cosine = Some(*value);
            entry.fused += 1. / (61. + index as f64);
        }
        let mut scores = fused.into_values().collect::<Vec<_>>();
        scores.sort_by(|a, b| {
            b.fused
                .total_cmp(&a.fused)
                .then_with(|| b.bm25.total_cmp(&a.bm25))
                .then_with(|| a.name.cmp(&b.name))
        });
        scores.truncate(5);
        let tools = scores.iter().map(|s| self.tools[&s.name].clone()).collect();
        Ok(ToolSearch {
            catalog_revision: self.revision,
            catalog_hash: self.hash.clone(),
            tools,
            scores,
            vector_status: if vector.is_some() {
                "host_vectors"
            } else {
                "not_configured"
            }
            .into(),
        })
    }
}
fn tokenize(text: &str) -> Vec<String> {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_lowercase())
        .collect()
}
fn check_vector(vector: &[f32]) -> Result<(), String> {
    if vector.is_empty()
        || vector.len() > 4096
        || vector.iter().any(|x| !x.is_finite())
        || vector.iter().all(|x| *x == 0.)
    {
        Err("embedding must contain 1..4096 finite nonzero coordinates".into())
    } else {
        Ok(())
    }
}
fn cosine(a: &[f32], b: &[f32]) -> f64 {
    let dot = a
        .iter()
        .zip(b)
        .map(|(a, b)| f64::from(*a) * f64::from(*b))
        .sum::<f64>();
    let norm = |v: &[f32]| v.iter().map(|v| f64::from(*v).powi(2)).sum::<f64>().sqrt();
    dot / (norm(a) * norm(b))
}
pub fn validate_name(name: &str) -> Result<(), String> {
    if name.is_empty()
        || name.len() > 64
        || !name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
        || name.as_bytes()[0].is_ascii_digit()
    {
        Err("tool name must be snake_case ASCII, at most 64 bytes and start with a letter or underscore".into())
    } else {
        Ok(())
    }
}
pub fn mcp_name(server: &str, tool: &str) -> Result<String, String> {
    if [server, tool]
        .iter()
        .any(|s| s.is_empty() || s.len() > 1024 || s.chars().any(char::is_control))
    {
        return Err("MCP source names require bounded printable identifiers".into());
    }
    let normalize = |s: &str| {
        s.chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() {
                    c.to_ascii_lowercase()
                } else {
                    '_'
                }
            })
            .collect::<String>()
    };
    let raw = format!("mcp__{}__{}", normalize(server), normalize(tool));
    // A leading or trailing underscore in one part merges with the `__`
    // separators, so `("a_","b")` and `("a","_b")` would collide without a
    // hash; treat those the same as any other lossy normalization.
    let changed = [server, tool].iter().any(|s| {
        s.contains("__")
            || s.starts_with('_')
            || s.ends_with('_')
            || s.bytes()
                .any(|b| !b.is_ascii_lowercase() && !b.is_ascii_digit() && b != b'_')
    });
    let name = if raw.len() > 64 || changed {
        let identity = serde_json::to_vec(&(server, tool)).map_err(|_| "invalid MCP identity")?;
        format!("{}_{}", &raw[..raw.len().min(47)], &digest(&identity)[..16])
    } else {
        raw
    };
    validate_name(&name)?;
    Ok(name)
}
fn external_ref(value: &Value) -> bool {
    match value {
        Value::Object(map) => map.iter().any(|(key, value)| {
            matches!(key.as_str(), "$ref" | "$dynamicRef")
                && value.as_str().is_some_and(|v| !v.starts_with('#'))
                || external_ref(value)
        }),
        Value::Array(a) => a.iter().any(external_ref),
        _ => false,
    }
}
fn schema(value: &Value) -> Result<jsonschema::Validator, String> {
    if value
        .get("$schema")
        .and_then(Value::as_str)
        .is_some_and(|uri| uri != SCHEMA_URI)
    {
        return Err("schema must use JSON Schema 2020-12".into());
    }
    if external_ref(value)
        || serde_json::to_vec(value)
            .map_err(|_| "invalid schema")?
            .len()
            > 256 * 1024
    {
        return Err("schema external references or size are not supported".into());
    }
    jsonschema::options()
        .with_draft(jsonschema::Draft::Draft202012)
        .build(value)
        .map_err(|_| "invalid JSON Schema 2020-12".into())
}
fn check_args(schema_value: &Value, args: &Value) -> Result<(), String> {
    let validator = schema(schema_value)?;
    if let Some(error) = validator.iter_errors(args).next() {
        return Err(format!(
            "schema validation failed at {}",
            error.instance_path()
        ));
    }
    Ok(())
}
impl DeferredTools {
    fn arguments(&self, call: &ToolCall) -> Result<Value, String> {
        let args: Value = serde_json::from_str(&call.arguments)
            .or_else(|_| json5::from_str(&call.arguments))
            .map_err(|_| "invalid tool JSON")?;
        if self.delegate.sanitize_durable(&args)? != args {
            return Err("sensitive tool arguments require a secret reference".into());
        }
        Ok(args)
    }
    fn target(&self, call: &ToolCall) -> Result<(ToolCall, Option<CatalogTool>), String> {
        let snapshot = self.catalog.snapshot();
        self.validate_snapshot(&snapshot)?;
        let args = self.arguments(call)?;
        let target = if call.name == "call_tool" {
            check_args(&call_schema(), &args)?;
            let name = args["name"].as_str().unwrap();
            let arguments = args.get("args").or_else(|| args.get("arguments")).unwrap();
            ToolCall {
                id: call.id.clone(),
                name: name.into(),
                arguments: arguments.to_string(),
            }
        } else {
            ToolCall {
                id: call.id.clone(),
                name: call.name.clone(),
                arguments: args.to_string(),
            }
        };
        let entry = snapshot.tools.get(&target.name).cloned();
        if let Some(entry) = &entry {
            if self.role == CallerRole::Master && !entry.fast {
                return Err("master cannot invoke a non-fast domain tool".into());
            }
            let arguments: Value = serde_json::from_str(&target.arguments)
                .map_err(|_| "invalid resolved arguments")?;
            check_args(&entry.input_schema, &arguments)?;
        } else if let Some(core) = self.core.iter().find(|t| t.name == target.name) {
            let args: Value = serde_json::from_str(&target.arguments)
                .map_err(|_| "invalid resolved arguments")?;
            check_args(&core.input_schema, &args)?;
        } else {
            return Err("tool is unavailable in the host catalog".into());
        }
        Ok((target, entry))
    }
    fn unwrap(
        &self,
        run: &str,
        tool: &PreparedTool,
    ) -> Result<(PreparedTool, Option<CatalogTool>), String> {
        if tool.data["deferred_version"] != 1 || tool.data["run"] != run {
            return Err("invalid prepared deferred tool or run owner".into());
        }
        let (target, entry) = self.target(&tool.call)?;
        let inner: PreparedTool = serde_json::from_value(tool.data["inner"].clone())
            .map_err(|_| "invalid delegated preparation")?;
        let cancellation = cancellation_for(&inner, entry.as_ref());
        if tool.data["cancellation"] != serde_json::to_value(cancellation).unwrap()
            || inner.call != target
            || inner.effect != tool.effect
            || inner.resources != tool.resources
            || inner.exclusive != tool.exclusive
            || entry.as_ref().map(CatalogTool::digest)
                != tool.data["entry_hash"].as_str().map(String::from)
        {
            return Err("prepared tool or catalog definition changed; prepare again".into());
        }
        Ok((inner, entry))
    }
    async fn search(&self, query: &str) -> Result<ToolSearch, String> {
        let snapshot = self.catalog.snapshot();
        self.validate_snapshot(&snapshot)?;
        let mut status = "not_configured";
        let vector = if let Some(embedder) = &self.embedder {
            if snapshot.tools.values().any(|tool| {
                (self.role != CallerRole::Master || tool.fast)
                    && tool.embedding.as_ref().is_some_and(|v| {
                        v.model == embedder.model() && v.values.len() == embedder.dimensions()
                    })
            }) {
                match tokio::time::timeout(
                    std::time::Duration::from_millis(self.embedding_timeout_ms),
                    embedder.embed(query),
                )
                .await
                {
                    Ok(Ok(v)) if v.len() == embedder.dimensions() && check_vector(&v).is_ok() => {
                        status = "host_vectors";
                        Some((embedder.model().to_string(), v))
                    }
                    _ => {
                        status = "unavailable";
                        None
                    }
                }
            } else {
                status = "no_matching_index";
                None
            }
        } else {
            None
        };
        let mut found = snapshot.search(
            query,
            self.role,
            vector.as_ref().map(|(m, v)| (m.as_str(), v.as_slice())),
        )?;
        found.vector_status = status.into();
        for tool in &found.tools {
            self.sanitize_metadata(tool)?;
        }
        Ok(found)
    }
    fn checked_output(&self, entry: Option<&CatalogTool>, mut result: ToolResult) -> ToolResult {
        if result.success
            && let Some(entry) = entry
            && check_args(&entry.output_schema, &result.output).is_err()
        {
            let blob = result.full_output_blob.take();
            let mut failure = ToolResult::error(
                "tool output failed its registered schema; the admitted effect may have completed",
            );
            failure.output["code"] = json!("output_schema_validation_failed");
            failure.output["effect_may_have_completed"] = json!(entry.idempotency != Effect::Pure);
            failure.full_output_blob = blob;
            return failure;
        }
        match self.delegate.sanitize_durable(&result.output) {
            Ok(value) => result.output = value,
            Err(_) => return ToolResult::error("tool output could not pass the host data guard"),
        };
        result
    }
}
fn cancellation_for(inner: &PreparedTool, entry: Option<&CatalogTool>) -> CancellationClass {
    entry
        .map(|t| t.cancel)
        .unwrap_or_else(|| match inner.call.name.as_str() {
            "write_file" | "edit_file" | "apply_patch" | "revert_edit" => {
                CancellationClass::AfterCompletion
            }
            "exec" => CancellationClass::Graceful,
            "exec_io" if inner.effect != Effect::Pure => CancellationClass::Graceful,
            _ => CancellationClass::Immediate,
        })
}
fn search_schema() -> Value {
    json!({"$schema":SCHEMA_URI,"type":"object","properties":{"query":{"type":"string","minLength":1,"maxLength":8192}},"required":["query"],"additionalProperties":false})
}
fn call_schema() -> Value {
    json!({"$schema":SCHEMA_URI,"type":"object","properties":{"name":{"type":"string","pattern":"^[a-z_][a-z0-9_]{0,63}$"},"args":{"type":"object"},"arguments":{"type":"object"}},"required":["name"],"oneOf":[{"required":["args"],"not":{"required":["arguments"]}},{"required":["arguments"],"not":{"required":["args"]}}],"additionalProperties":false})
}
impl ToolRuntime for DeferredTools {
    fn overrides_builtin<'a>(&'a self, run: &'a str, name: &'a str) -> BoxFuture<'a, bool> {
        self.delegate.overrides_builtin(run, name)
    }

    fn run_definitions<'a>(
        &'a self,
        run: &'a str,
    ) -> BoxFuture<'a, Result<Option<Vec<ToolDefinition>>, String>> {
        Box::pin(async move {
            let Some(mut definitions) = self.delegate.run_definitions(run).await? else {
                return Ok(None);
            };
            let names = self.catalog.names();
            definitions.retain(|definition| {
                !names.contains(&definition.name)
                    && !matches!(definition.name.as_str(), "search_tools" | "call_tool")
            });
            definitions.extend(self.definitions().into_iter().filter(|definition| {
                matches!(definition.name.as_str(), "search_tools" | "call_tool")
            }));
            definitions.sort_by(|a, b| a.name.cmp(&b.name));
            Ok(Some(definitions))
        })
    }

    fn deferred_definitions<'a>(
        &'a self,
        run: &'a str,
    ) -> BoxFuture<'a, Result<Vec<ToolDefinition>, String>> {
        Box::pin(async move {
            if let Some(definitions) = self
                .native_pins
                .lock()
                .map_err(|_| "native catalog pin lock poisoned")?
                .get(run)
                .cloned()
            {
                return Ok(definitions);
            }
            let snapshot = self.catalog.snapshot();
            self.validate_snapshot(&snapshot)?;
            let definitions = snapshot
                .tools
                .values()
                .filter(|t| self.role != CallerRole::Master || t.fast)
                .map(CatalogTool::definition)
                .collect::<Vec<_>>();
            let mut pins = self
                .native_pins
                .lock()
                .map_err(|_| "native catalog pin lock poisoned")?;
            Ok(pins.entry(run.into()).or_insert(definitions).clone())
        })
    }
    fn live_status(&self, run: &str) -> Vec<Value> {
        self.delegate.live_status(run)
    }
    fn definitions(&self) -> Vec<ToolDefinition> {
        let mut definitions = self.core.clone();
        definitions.push(ToolDefinition{name:"search_tools".into(),description:"Find the five best available domain tools by name, description and tags; return their full schemas without changing the core catalog.".into(),input_schema:search_schema()});
        definitions.push(ToolDefinition{name:"call_tool".into(),description:"Invoke a registered domain tool through the same argument, resource and permission checks as a direct call.".into(),input_schema:call_schema()});
        definitions.sort_by(|a, b| a.name.cmp(&b.name));
        definitions
    }
    fn scheduling<'a>(
        &'a self,
        run: &'a str,
        call: &'a ToolCall,
    ) -> BoxFuture<'a, Option<ToolScheduling>> {
        Box::pin(async move {
            if call.name == "search_tools" {
                return Some(ToolScheduling {
                    resources: vec![],
                    exclusive: false,
                });
            }
            let (target, _) = self.target(call).ok()?;
            self.delegate.scheduling(run, &target).await
        })
    }
    fn prepare<'a>(
        &'a self,
        run: &'a str,
        call: ToolCall,
    ) -> BoxFuture<'a, Result<PreparedTool, String>> {
        Box::pin(async move {
            if call.name == "search_tools" {
                let args = self.arguments(&call)?;
                check_args(&search_schema(), &args)?;
                self.delegate.audit_builtin(run, &call, None).await?;
                return Ok(PreparedTool {
                    call,
                    effect: Effect::Pure,
                    resources: vec![],
                    exclusive: false,
                    data: json!({"deferred_version":1,"run":run,"search":args["query"],"cancellation":"immediate"}),
                });
            }
            let (target, entry) = self.target(&call)?;
            let inner = self.delegate.prepare(run, target).await?;
            if let Some(entry) = &entry
                && inner.effect != entry.idempotency
                && entry.idempotency != Effect::NonIdempotent
                && inner.effect != Effect::Pure
            {
                return Err("runtime effect does not match registered idempotency".into());
            }
            let cancellation = cancellation_for(&inner, entry.as_ref());
            Ok(PreparedTool {
                call,
                effect: inner.effect,
                resources: inner.resources.clone(),
                exclusive: inner.exclusive,
                data: json!({"deferred_version":1,"run":run,"entry_hash":entry.map(|t|t.digest()),"cancellation":cancellation,"inner":inner}),
            })
        })
    }
    fn execute<'a>(
        &'a self,
        run: &'a str,
        key: &'a str,
        tool: &'a PreparedTool,
    ) -> BoxFuture<'a, ToolResult> {
        Box::pin(async move {
            if tool.call.name == "search_tools" {
                if tool.data["run"] != run || tool.data["deferred_version"] != 1 {
                    return ToolResult::error("invalid search preparation owner");
                }
                let args = match self.arguments(&tool.call) {
                    Ok(v) => v,
                    Err(e) => return ToolResult::error(e),
                };
                if args["query"] != tool.data["search"]
                    || check_args(&search_schema(), &args).is_err()
                {
                    return ToolResult::error("search preparation changed");
                }
                let result = match self.search(args["query"].as_str().unwrap()).await {
                    Ok(found) => ToolResult::ok(serde_json::to_value(found).unwrap()),
                    Err(e) => ToolResult::error(e),
                };
                if let Err(e) = self
                    .delegate
                    .audit_builtin(run, &tool.call, Some(&result))
                    .await
                {
                    return ToolResult::error(e);
                }
                return result;
            }
            let (inner, entry) = match self.unwrap(run, tool) {
                Ok(v) => v,
                Err(e) => return ToolResult::error(e),
            };
            let result = if let Some(entry) = &entry
                && matches!(
                    entry.cancel,
                    CancellationClass::Immediate | CancellationClass::Graceful
                ) {
                match tokio::time::timeout(
                    std::time::Duration::from_millis(entry.timeout_ms),
                    self.delegate.execute(run, key, &inner),
                )
                .await
                {
                    Ok(r) => r,
                    Err(_) => {
                        let interruption = self
                            .delegate
                            .interrupt_execution(run, false, Some(&inner))
                            .await;
                        let mut result = ToolResult::error(
                            "tool deadline exceeded; reconcile the admitted operation before retrying",
                        );
                        result.output["code"] = json!("tool_timeout");
                        result.output["interruption_confirmed"] =
                            json!(matches!(interruption, Ok(true)));
                        result
                    }
                }
            } else {
                self.delegate.execute(run, key, &inner).await
            };
            self.checked_output(entry.as_ref(), result)
        })
    }
    fn reconcile<'a>(
        &'a self,
        run: &'a str,
        key: &'a str,
        tool: &'a PreparedTool,
    ) -> BoxFuture<'a, Reconcile> {
        Box::pin(async move {
            if tool.call.name == "search_tools" {
                return Reconcile::Retry;
            }
            let (inner, entry) = match self.unwrap(run, tool) {
                Ok(v) => v,
                Err(e) => return Reconcile::Uncertain(e),
            };
            match self.delegate.reconcile(run, key, &inner).await {
                Reconcile::Done(result) => {
                    Reconcile::Done(self.checked_output(entry.as_ref(), result))
                }
                other => other,
            }
        })
    }
    fn execution_state<'a>(
        &'a self,
        run: &'a str,
        state: &'a str,
        reason: Option<&'a str>,
    ) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async move {
            self.delegate.execution_state(run, state, reason).await?;
            if matches!(
                state,
                "completed" | "completed_with_pending" | "failed" | "cancelled"
            ) {
                self.native_pins
                    .lock()
                    .map_err(|_| "native catalog pin lock poisoned")?
                    .remove(run);
            }
            Ok(())
        })
    }
    fn begin_step<'a>(&'a self, run: &'a str, step: u64) -> BoxFuture<'a, Result<(), String>> {
        self.delegate.begin_step(run, step)
    }
    fn end_step<'a>(&'a self, run: &'a str, step: u64) -> BoxFuture<'a, Result<(), String>> {
        self.delegate.end_step(run, step)
    }
    fn interrupt_execution<'a>(
        &'a self,
        run: &'a str,
        cancelled: bool,
        tool: Option<&'a PreparedTool>,
    ) -> BoxFuture<'a, Result<bool, String>> {
        Box::pin(async move {
            let inner = tool.and_then(|t| self.unwrap(run, t).ok().map(|v| v.0));
            self.delegate
                .interrupt_execution(run, cancelled, inner.as_ref())
                .await
        })
    }
    fn configure_execution_limits<'a>(
        &'a self,
        run: &'a str,
        limits: ExecutionLimits,
    ) -> BoxFuture<'a, Result<(), String>> {
        self.delegate.configure_execution_limits(run, limits)
    }
    fn execution_resources<'a>(
        &'a self,
        run: &'a str,
    ) -> BoxFuture<'a, Result<Option<ExecutionResources>, String>> {
        self.delegate.execution_resources(run)
    }
    fn sanitize_durable(&self, value: &Value) -> Result<Value, String> {
        self.delegate.sanitize_durable(value)
    }
    fn pending_approval<'a>(&'a self, run: &'a str) -> BoxFuture<'a, bool> {
        self.delegate.pending_approval(run)
    }
    fn approval_blocked<'a>(&'a self, run: &'a str) -> BoxFuture<'a, Option<String>> {
        self.delegate.approval_blocked(run)
    }
    fn observe_untrusted<'a>(
        &'a self,
        run: &'a str,
        source: &'a str,
    ) -> BoxFuture<'a, Result<(), String>> {
        self.delegate.observe_untrusted(run, source)
    }
    fn configure_process_boundary<'a>(
        &'a self,
        run: &'a str,
        boundary: ProcessBoundary,
    ) -> BoxFuture<'a, Result<(), String>> {
        self.delegate.configure_process_boundary(run, boundary)
    }
    fn execution_tool_budget<'a>(
        &'a self,
        run: &'a str,
    ) -> BoxFuture<'a, Result<Option<u64>, String>> {
        self.delegate.execution_tool_budget(run)
    }
    fn result_requires_approval(&self, result: &ToolResult) -> bool {
        self.delegate.result_requires_approval(result)
    }
    fn durable_blob_inspector(&self) -> Option<Box<dyn DurableByteInspector>> {
        self.delegate.durable_blob_inspector()
    }
    fn audit_builtin<'a>(
        &'a self,
        run: &'a str,
        call: &'a ToolCall,
        result: Option<&'a ToolResult>,
    ) -> BoxFuture<'a, Result<(), String>> {
        self.delegate.audit_builtin(run, call, result)
    }
    fn configure_network_authorizer<'a>(
        &'a self,
        run: &'a str,
        authorizer: Arc<dyn NetworkAuthorizer>,
    ) -> BoxFuture<'a, Result<(), String>> {
        self.delegate.configure_network_authorizer(run, authorizer)
    }
    fn finalization_guard<'a>(
        &'a self,
        run: &'a str,
    ) -> BoxFuture<'a, Result<Box<dyn FinalizationGuard>, String>> {
        self.delegate.finalization_guard(run)
    }
}
