//! In-memory skill index with root precedence, content hashes and hybrid search.
use crate::{
    builtin::BUILTIN_SKILLS,
    format::{Requirements, Risk, SkillState, parse_skill},
    groups::GroupRegistry,
};
use excalibur_types::runtime::BoxFuture;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    io::Read,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, RwLock},
    time::Duration,
};

pub const MAX_SKILLS_PER_ROOT: usize = 4096;
const MAX_FILES_PER_SKILL: usize = 512;
const MAX_BYTES_PER_SKILL: u64 = 16 * 1024 * 1024;

/// Skill roots, from highest to lowest precedence.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[serde(rename_all = "snake_case")]
pub enum RootKind {
    Project,
    User,
    Installed,
    Builtin,
}
impl RootKind {
    /// Installed packages stay in quarantine until audited and approved.
    pub fn default_state(self) -> SkillState {
        match self {
            Self::Installed => SkillState::Quarantine,
            _ => SkillState::Active,
        }
    }
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct SkillRoot {
    pub kind: RootKind,
    /// `None` only for the embedded integrated skills.
    pub path: Option<PathBuf>,
}
impl SkillRoot {
    /// Project skills live in `.agents/skills` under the project root.
    pub fn project(project_root: impl AsRef<Path>) -> Self {
        Self {
            kind: RootKind::Project,
            path: Some(project_root.as_ref().join(".agents").join("skills")),
        }
    }
    pub fn user(path: impl Into<PathBuf>) -> Self {
        Self {
            kind: RootKind::User,
            path: Some(path.into()),
        }
    }
    pub fn installed(path: impl Into<PathBuf>) -> Self {
        Self {
            kind: RootKind::Installed,
            path: Some(path.into()),
        }
    }
    pub fn builtin() -> Self {
        Self {
            kind: RootKind::Builtin,
            path: None,
        }
    }
}

/// Host embedding adapter for the vector half of skill search.
pub trait SkillEmbedder: Send + Sync {
    fn model(&self) -> &str;
    fn embed<'a>(&'a self, text: &'a str) -> BoxFuture<'a, Result<Vec<f32>, String>>;
}
/// Checks `excalibur.requires`; the default inspects PATH and the environment.
pub type RequirementCheck = Arc<dyn Fn(&Requirements) -> Result<(), String> + Send + Sync>;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct SkillEntry {
    pub name: String,
    pub description: String,
    pub group: String,
    pub root: RootKind,
    /// Filesystem path of SKILL.md, or `builtin:<name>`.
    pub path: String,
    /// Skill folder for references and scripts (absent for embedded skills).
    pub directory: Option<PathBuf>,
    /// SHA-256 over every file of the skill folder.
    pub hash: String,
    pub version: Option<String>,
    pub state: SkillState,
    pub platforms: Vec<String>,
    pub risk: Option<Risk>,
    pub tools: Vec<String>,
    pub allowed_tools: Vec<String>,
    pub requires: Requirements,
    pub display: BTreeMap<String, crate::format::DisplayText>,
    pub compatibility: Option<String>,
    pub headings: Vec<String>,
    /// Relative paths of `scripts/`, `references/`, `assets/` and other files.
    pub resources: Vec<String>,
    pub available: bool,
    pub unavailable_reason: Option<String>,
    #[serde(skip)]
    pub body: String,
}
impl SkillEntry {
    /// Listed, suggested and loadable.
    pub fn usable(&self) -> bool {
        self.state == SkillState::Active && self.available
    }
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Diagnostic {
    pub root: RootKind,
    pub path: String,
    pub name: Option<String>,
    pub errors: Vec<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Conflict {
    pub name: String,
    pub winner: RootKind,
    pub winner_path: String,
    pub shadowed: RootKind,
    pub shadowed_path: String,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct RefreshReport {
    pub revision: u64,
    pub added: Vec<String>,
    pub changed: Vec<String>,
    pub removed: Vec<String>,
}
impl RefreshReport {
    pub fn is_empty(&self) -> bool {
        self.added.is_empty() && self.changed.is_empty() && self.removed.is_empty()
    }
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct SearchHit {
    pub name: String,
    pub description: String,
    pub group: String,
    pub path: String,
    pub score: f64,
    pub bm25: f64,
    pub cosine: Option<f64>,
    pub matched_terms: Vec<String>,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct SkillStats {
    pub loads: u64,
    pub suggestions: u64,
}
/// Task outcomes per model and group, with and without a skill of the group loaded.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct GroupEvidence {
    pub with_total: u64,
    pub with_success: u64,
    pub without_total: u64,
    pub without_success: u64,
}
impl GroupEvidence {
    /// Negative when loading the group's skills measurably lowers the success rate.
    pub fn negative(&self, min_samples: u64, margin: f64) -> bool {
        if self.with_total < min_samples || self.without_total < min_samples {
            return false;
        }
        let with = self.with_success as f64 / self.with_total as f64;
        let without = self.without_success as f64 / self.without_total as f64;
        with + margin < without
    }
}

#[derive(Default)]
pub struct IndexSnapshot {
    pub revision: u64,
    pub hash: String,
    pub entries: BTreeMap<String, SkillEntry>,
    pub diagnostics: Vec<Diagnostic>,
    pub conflicts: Vec<Conflict>,
    pub vector_model: Option<String>,
    terms: BTreeMap<String, BTreeMap<String, usize>>,
    df: BTreeMap<String, usize>,
    average_length: f64,
    vectors: BTreeMap<String, Vec<f32>>,
}
impl IndexSnapshot {
    pub fn usable(&self) -> impl Iterator<Item = &SkillEntry> {
        self.entries.values().filter(|e| e.usable())
    }
    pub fn has_vector(&self, name: &str) -> bool {
        self.vectors.contains_key(name)
    }
}

pub struct SkillIndex {
    roots: Vec<SkillRoot>,
    groups: RwLock<GroupRegistry>,
    platform: String,
    requirements: RequirementCheck,
    embedder: Option<Arc<dyn SkillEmbedder>>,
    embedding_timeout: Duration,
    snapshot: RwLock<Arc<IndexSnapshot>>,
    refresh_lock: tokio::sync::Mutex<()>,
    stats: Mutex<BTreeMap<String, SkillStats>>,
    evidence: Mutex<BTreeMap<(String, String), GroupEvidence>>,
}
fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
pub fn current_platform() -> &'static str {
    match std::env::consts::OS {
        "macos" => "macos",
        "windows" => "windows",
        "android" => "android",
        "ios" => "ios",
        _ => "linux",
    }
}
fn default_requirements(requirements: &Requirements) -> Result<(), String> {
    for name in &requirements.env {
        if std::env::var_os(name).is_none() {
            return Err(format!("environment variable {name} is not set"));
        }
    }
    let paths = std::env::var_os("PATH").unwrap_or_default();
    for binary in &requirements.binaries {
        let found = std::env::split_paths(&paths).any(|dir| {
            dir.join(binary).is_file()
                || (cfg!(windows) && dir.join(format!("{binary}.exe")).is_file())
        });
        if !found {
            return Err(format!("binary {binary} is not on PATH"));
        }
    }
    Ok(())
}
pub(crate) fn tokenize(text: &str) -> Vec<String> {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_lowercase())
        .collect()
}
fn cosine(a: &[f32], b: &[f32]) -> f64 {
    let dot = a
        .iter()
        .zip(b)
        .map(|(a, b)| f64::from(*a) * f64::from(*b))
        .sum::<f64>();
    let norm = |v: &[f32]| v.iter().map(|v| f64::from(*v).powi(2)).sum::<f64>().sqrt();
    let n = norm(a) * norm(b);
    if n == 0.0 { 0.0 } else { dot / n }
}
fn valid_vector(v: &[f32]) -> bool {
    !v.is_empty()
        && v.len() <= 4096
        && v.iter().all(|x| x.is_finite())
        && v.iter().any(|x| *x != 0.0)
}

struct Candidate {
    root: RootKind,
    entry: Option<SkillEntry>,
    diagnostic: Option<Diagnostic>,
}

impl SkillIndex {
    pub fn new(mut roots: Vec<SkillRoot>) -> Self {
        roots.sort_by_key(|r| r.kind);
        Self {
            roots,
            groups: RwLock::new(GroupRegistry::default()),
            platform: current_platform().into(),
            requirements: Arc::new(default_requirements),
            embedder: None,
            embedding_timeout: Duration::from_secs(2),
            snapshot: RwLock::new(Arc::new(IndexSnapshot::default())),
            refresh_lock: tokio::sync::Mutex::new(()),
            stats: Mutex::new(BTreeMap::new()),
            evidence: Mutex::new(BTreeMap::new()),
        }
    }
    pub fn with_embedder(mut self, embedder: Arc<dyn SkillEmbedder>) -> Self {
        self.embedder = Some(embedder);
        self
    }
    pub fn with_platform(mut self, platform: &str) -> Self {
        self.platform = platform.into();
        self
    }
    pub fn with_requirement_check(mut self, check: RequirementCheck) -> Self {
        self.requirements = check;
        self
    }
    pub fn with_groups(self, groups: GroupRegistry) -> Self {
        *self.groups.write().unwrap_or_else(|e| e.into_inner()) = groups;
        self
    }
    pub fn groups(&self) -> GroupRegistry {
        self.groups
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
    pub fn roots(&self) -> &[SkillRoot] {
        &self.roots
    }
    pub fn snapshot(&self) -> Arc<IndexSnapshot> {
        self.snapshot
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
    pub fn get(&self, name: &str) -> Option<SkillEntry> {
        self.snapshot().entries.get(name).cloned()
    }
    pub fn has_embedder(&self) -> bool {
        self.embedder.is_some()
    }

    fn build_entry(
        &self,
        root: RootKind,
        folder: &str,
        path: String,
        directory: Option<PathBuf>,
        content: &str,
        fingerprint: (String, Vec<String>),
    ) -> Candidate {
        let (hash, resources) = fingerprint;
        let parsed = match parse_skill(folder, content) {
            Ok(parsed) => parsed,
            Err(errors) => {
                return Candidate {
                    root,
                    entry: None,
                    diagnostic: Some(Diagnostic {
                        root,
                        path,
                        name: Some(folder.into()),
                        errors,
                    }),
                };
            }
        };
        let manifest = parsed.manifest;
        let meta = manifest.excalibur;
        if self
            .groups
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(&meta.group)
            .is_none()
        {
            return Candidate {
                root,
                entry: None,
                diagnostic: Some(Diagnostic {
                    root,
                    path,
                    name: Some(manifest.name),
                    errors: vec![format!(
                        "group {} is not registered; register it with a description first",
                        meta.group
                    )],
                }),
            };
        }
        let state = meta
            .state
            .map(|s| root.default_state().restrict(s))
            .unwrap_or(root.default_state());
        let unavailable_reason =
            if !meta.platforms.is_empty() && !meta.platforms.iter().any(|p| p == &self.platform) {
                Some(format!("not supported on {}", self.platform))
            } else {
                (self.requirements)(&meta.requires).err()
            };
        Candidate {
            root,
            diagnostic: None,
            entry: Some(SkillEntry {
                name: manifest.name,
                description: manifest.description,
                group: meta.group,
                root,
                path,
                directory,
                hash,
                version: meta.version,
                state,
                platforms: meta.platforms,
                risk: meta.risk,
                tools: meta.tools,
                allowed_tools: manifest.allowed_tools,
                requires: meta.requires,
                display: meta.display,
                compatibility: manifest.compatibility,
                headings: parsed.headings,
                resources,
                available: unavailable_reason.is_none(),
                unavailable_reason,
                body: parsed.body,
            }),
        }
    }

    fn scan_root(&self, root: &SkillRoot) -> Vec<Candidate> {
        let kind = root.kind;
        let Some(path) = &root.path else {
            return BUILTIN_SKILLS
                .iter()
                .map(|(name, content)| {
                    self.build_entry(
                        kind,
                        name,
                        format!("builtin:{name}"),
                        None,
                        content,
                        (digest(content.as_bytes()), Vec::new()),
                    )
                })
                .collect();
        };
        let fail = |message: String| {
            vec![Candidate {
                root: kind,
                entry: None,
                diagnostic: Some(Diagnostic {
                    root: kind,
                    path: path.display().to_string(),
                    name: None,
                    errors: vec![message],
                }),
            }]
        };
        if !path.exists() {
            return Vec::new();
        }
        let Ok(canonical_root) = path.canonicalize() else {
            return fail("skill root cannot be resolved".into());
        };
        let Ok(read) = std::fs::read_dir(&canonical_root) else {
            return fail("skill root is unreadable".into());
        };
        let mut dirs = read
            .filter_map(|e| e.ok())
            .filter(|e| !e.file_name().to_string_lossy().starts_with('.'))
            .take(MAX_SKILLS_PER_ROOT + 1)
            .collect::<Vec<_>>();
        dirs.sort_by_key(|e| e.file_name());
        if dirs.len() > MAX_SKILLS_PER_ROOT {
            return fail(format!("skill root exceeds {MAX_SKILLS_PER_ROOT} entries"));
        }
        let mut candidates = Vec::new();
        for dir in dirs {
            let folder = dir.file_name().to_string_lossy().to_string();
            let display = dir.path().join("SKILL.md").display().to_string();
            let diagnostic = |errors: Vec<String>| Candidate {
                root: kind,
                entry: None,
                diagnostic: Some(Diagnostic {
                    root: kind,
                    path: display.clone(),
                    name: Some(folder.clone()),
                    errors,
                }),
            };
            let Ok(canonical) = dir.path().canonicalize() else {
                candidates.push(diagnostic(vec!["skill folder cannot be resolved".into()]));
                continue;
            };
            if !canonical.is_dir() {
                continue;
            }
            if !canonical.starts_with(&canonical_root) {
                candidates.push(diagnostic(vec![
                    "skill folder escapes its root through a link".into(),
                ]));
                continue;
            }
            let skill_file = canonical.join("SKILL.md");
            if !skill_file.is_file() {
                candidates.push(diagnostic(vec!["missing SKILL.md".into()]));
                continue;
            }
            let (hash, resources, content) = match hash_folder(&canonical) {
                Ok(v) => v,
                Err(e) => {
                    candidates.push(diagnostic(vec![e]));
                    continue;
                }
            };
            candidates.push(self.build_entry(
                kind,
                &folder,
                skill_file.display().to_string(),
                Some(canonical),
                &content,
                (hash, resources),
            ));
        }
        candidates
    }

    /// Rescan every root. Entries whose content hash changed are revalidated
    /// and reindexed (lexical and vector); unchanged vectors are reused.
    pub async fn refresh(&self) -> RefreshReport {
        let _serial = self.refresh_lock.lock().await;
        let previous = self.snapshot();
        let mut entries: BTreeMap<String, SkillEntry> = BTreeMap::new();
        let mut diagnostics = Vec::new();
        let mut conflicts = Vec::new();
        for root in &self.roots {
            for candidate in self.scan_root(root) {
                if let Some(d) = candidate.diagnostic {
                    diagnostics.push(d);
                }
                let Some(entry) = candidate.entry else {
                    continue;
                };
                if let Some(winner) = entries.get(&entry.name) {
                    conflicts.push(Conflict {
                        name: entry.name.clone(),
                        winner: winner.root,
                        winner_path: winner.path.clone(),
                        shadowed: candidate.root,
                        shadowed_path: entry.path.clone(),
                    });
                    continue;
                }
                entries.insert(entry.name.clone(), entry);
            }
        }
        let mut report = RefreshReport::default();
        for (name, entry) in &entries {
            match previous.entries.get(name) {
                None => report.added.push(name.clone()),
                Some(old)
                    if old.hash != entry.hash
                        || old.state != entry.state
                        || old.available != entry.available
                        || old.root != entry.root =>
                {
                    report.changed.push(name.clone())
                }
                _ => {}
            }
        }
        report.removed = previous
            .entries
            .keys()
            .filter(|k| !entries.contains_key(*k))
            .cloned()
            .collect();
        // Vectors: reuse unchanged, embed new or changed.
        let mut vectors = BTreeMap::new();
        let vector_model = self.embedder.as_ref().map(|e| e.model().to_string());
        if let Some(embedder) = &self.embedder {
            for (name, entry) in &entries {
                let unchanged = previous
                    .entries
                    .get(name)
                    .is_some_and(|old| old.hash == entry.hash)
                    && previous.vector_model == vector_model;
                if unchanged && let Some(v) = previous.vectors.get(name) {
                    vectors.insert(name.clone(), v.clone());
                    continue;
                }
                let text = format!(
                    "{}\n{}\n{}",
                    entry.name,
                    entry.description,
                    entry.headings.join("\n")
                );
                if let Ok(Ok(v)) =
                    tokio::time::timeout(self.embedding_timeout, embedder.embed(&text)).await
                    && valid_vector(&v)
                {
                    vectors.insert(name.clone(), v);
                }
            }
        }
        let fingerprint = entries
            .values()
            .map(|e| {
                (
                    e.name.as_str(),
                    e.hash.as_str(),
                    e.state,
                    e.available,
                    e.root,
                )
            })
            .collect::<Vec<_>>();
        let hash = digest(&serde_json::to_vec(&fingerprint).unwrap_or_default());
        let revision = if hash == previous.hash {
            previous.revision
        } else {
            previous.revision + 1
        };
        report.revision = revision;
        let (terms, df, average_length) = lexical(&entries);
        let next = IndexSnapshot {
            revision,
            hash,
            entries,
            diagnostics,
            conflicts,
            vector_model,
            terms,
            df,
            average_length,
            vectors,
        };
        *self.snapshot.write().unwrap_or_else(|e| e.into_inner()) = Arc::new(next);
        report
    }

    /// Hybrid search (BM25 over name, description and body headings, fused with
    /// host vectors when configured) over usable skills.
    pub async fn search(
        &self,
        query: &str,
        group: Option<&str>,
        limit: usize,
    ) -> Result<Vec<SearchHit>, String> {
        if query.trim().is_empty() || query.len() > 8192 {
            return Err("search query requires 1..8192 bytes".into());
        }
        let snapshot = self.snapshot();
        let query_vector = match &self.embedder {
            Some(embedder) if !snapshot.vectors.is_empty() => {
                match tokio::time::timeout(self.embedding_timeout, embedder.embed(query)).await {
                    Ok(Ok(v)) if valid_vector(&v) => Some(v),
                    _ => None,
                }
            }
            _ => None,
        };
        Ok(search_snapshot(
            &snapshot,
            query,
            group,
            limit,
            query_vector.as_deref(),
        ))
    }

    pub fn record_load(&self, name: &str) {
        self.stats
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry(name.into())
            .or_default()
            .loads += 1;
    }
    pub fn record_suggestion(&self, name: &str) {
        self.stats
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry(name.into())
            .or_default()
            .suggestions += 1;
    }
    pub fn stats(&self, name: &str) -> SkillStats {
        self.stats
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(name)
            .cloned()
            .unwrap_or_default()
    }
    /// Record a finished task: for each relevant group, whether one of its
    /// skills was loaded and whether the task succeeded, for that model.
    pub fn record_task_outcome(
        &self,
        model: &str,
        relevant_groups: &BTreeSet<String>,
        loaded_skills: &[String],
        success: bool,
    ) {
        let snapshot = self.snapshot();
        let used = loaded_skills
            .iter()
            .filter_map(|n| snapshot.entries.get(n).map(|e| e.group.clone()))
            .collect::<BTreeSet<_>>();
        let mut evidence = self.evidence.lock().unwrap_or_else(|e| e.into_inner());
        for group in relevant_groups.union(&used) {
            let e = evidence
                .entry((model.to_string(), group.clone()))
                .or_default();
            if used.contains(group) {
                e.with_total += 1;
                e.with_success += u64::from(success);
            } else {
                e.without_total += 1;
                e.without_success += u64::from(success);
            }
        }
    }
    pub fn group_evidence(&self, model: &str, group: &str) -> GroupEvidence {
        self.evidence
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&(model.to_string(), group.to_string()))
            .cloned()
            .unwrap_or_default()
    }
    /// Groups whose suggestions are disabled for `model` by negative evidence.
    pub fn disabled_groups(&self, model: &str, min_samples: u64, margin: f64) -> BTreeSet<String> {
        self.evidence
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .filter(|((m, _), e)| m == model && e.negative(min_samples, margin))
            .map(|((_, g), _)| g.clone())
            .collect()
    }
}
type Lexical = (
    BTreeMap<String, BTreeMap<String, usize>>,
    BTreeMap<String, usize>,
    f64,
);
fn lexical(entries: &BTreeMap<String, SkillEntry>) -> Lexical {
    let mut terms = BTreeMap::new();
    let mut df: BTreeMap<String, usize> = BTreeMap::new();
    let mut total = 0usize;
    for (name, entry) in entries {
        let mut words = Vec::new();
        for _ in 0..3 {
            words.extend(tokenize(&entry.name));
        }
        for _ in 0..2 {
            words.extend(tokenize(&entry.description));
        }
        for heading in &entry.headings {
            words.extend(tokenize(heading));
        }
        let mut counts: BTreeMap<String, usize> = BTreeMap::new();
        for word in words {
            *counts.entry(word).or_default() += 1;
        }
        total += counts.values().sum::<usize>();
        for word in counts.keys() {
            *df.entry(word.clone()).or_default() += 1;
        }
        terms.insert(name.clone(), counts);
    }
    let average = total as f64 / entries.len().max(1) as f64;
    (terms, df, average)
}
pub(crate) fn search_snapshot(
    snapshot: &IndexSnapshot,
    query: &str,
    group: Option<&str>,
    limit: usize,
    query_vector: Option<&[f32]>,
) -> Vec<SearchHit> {
    let query_terms = tokenize(query).into_iter().collect::<BTreeSet<_>>();
    let eligible = snapshot
        .usable()
        .filter(|e| group.is_none_or(|g| e.group == g))
        .collect::<Vec<_>>();
    let n = snapshot.entries.len() as f64;
    let mut lexical = Vec::new();
    let mut semantic = Vec::new();
    for entry in &eligible {
        let terms = &snapshot.terms[&entry.name];
        let length = terms.values().sum::<usize>() as f64;
        let mut score = 0.0;
        let mut matched = Vec::new();
        for term in &query_terms {
            let tf = *terms.get(term).unwrap_or(&0) as f64;
            if tf == 0.0 {
                continue;
            }
            matched.push(term.clone());
            let df = *snapshot.df.get(term).unwrap_or(&0) as f64;
            let idf = (1.0 + (n - df + 0.5) / (df + 0.5)).ln();
            score += idf * tf * 2.2
                / (tf + 1.2 * (0.25 + 0.75 * length / snapshot.average_length.max(1.0)));
        }
        if entry.name == query.trim() {
            score += 10.0;
        }
        if score > 0.0 {
            lexical.push((entry.name.clone(), score, matched));
        }
        if let (Some(q), Some(v)) = (query_vector, snapshot.vectors.get(&entry.name))
            && q.len() == v.len()
        {
            let similarity = cosine(q, v);
            if similarity > 0.0 {
                semantic.push((entry.name.clone(), similarity));
            }
        }
    }
    lexical.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    semantic.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    let mut fused: BTreeMap<String, SearchHit> = BTreeMap::new();
    let hit = |name: &str| {
        let e = &snapshot.entries[name];
        SearchHit {
            name: e.name.clone(),
            description: e.description.clone(),
            group: e.group.clone(),
            path: e.path.clone(),
            score: 0.0,
            bm25: 0.0,
            cosine: None,
            matched_terms: Vec::new(),
        }
    };
    for (rank, (name, score, matched)) in lexical.into_iter().enumerate() {
        let h = fused.entry(name.clone()).or_insert_with(|| hit(&name));
        h.bm25 = score;
        h.matched_terms = matched;
        h.score += 1.0 / (61.0 + rank as f64);
    }
    for (rank, (name, similarity)) in semantic.into_iter().enumerate() {
        let h = fused.entry(name.clone()).or_insert_with(|| hit(&name));
        h.cosine = Some(similarity);
        h.score += 1.0 / (61.0 + rank as f64);
    }
    let mut hits = fused.into_values().collect::<Vec<_>>();
    hits.sort_by(|a, b| {
        b.score
            .total_cmp(&a.score)
            .then_with(|| b.bm25.total_cmp(&a.bm25))
            .then_with(|| a.name.cmp(&b.name))
    });
    hits.truncate(limit.clamp(1, 20));
    hits
}
/// Hash every file of a skill folder (bounded) and list its resources.
fn hash_folder(root: &Path) -> Result<(String, Vec<String>, String), String> {
    let capability = cap_std::fs::Dir::open_ambient_dir(root, cap_std::ambient_authority())
        .map_err(|_| "skill folder is unreadable".to_string())?;
    let mut files = Vec::new();
    let mut manifest = None;
    let mut stack = vec![root.to_path_buf()];
    let mut bytes = 0u64;
    let mut nodes = 0usize;
    while let Some(dir) = stack.pop() {
        let read = std::fs::read_dir(&dir).map_err(|_| "skill folder is unreadable".to_string())?;
        for item in read {
            nodes += 1;
            if nodes > MAX_FILES_PER_SKILL {
                return Err("skill folder exceeds 512 filesystem entries".into());
            }
            let item = item.map_err(|_| "skill folder is unreadable".to_string())?;
            let kind = item
                .file_type()
                .map_err(|_| "skill file metadata unavailable".to_string())?;
            let path = item.path();
            if kind.is_symlink() {
                let target = path
                    .canonicalize()
                    .map_err(|_| "dangling link in skill folder".to_string())?;
                if !target.starts_with(root) {
                    return Err("skill folder contains a link outside the skill".into());
                }
                if target.is_dir() {
                    continue;
                }
                if !target.is_file() {
                    return Err("skill resources must be regular files".into());
                }
            } else if kind.is_dir() {
                stack.push(path);
                continue;
            } else if !kind.is_file() {
                return Err("skill resources must be regular files".into());
            }
            let relative = path
                .strip_prefix(root)
                .map_err(|_| "skill file outside folder".to_string())?
                .to_string_lossy()
                .replace('\\', "/");
            if files.len() >= MAX_FILES_PER_SKILL {
                return Err("skill folder exceeds 512 files or 16 MiB".into());
            }
            let file = capability
                .open(&relative)
                .map_err(|_| format!("cannot read confined resource {relative}"))?;
            if !file
                .metadata()
                .map_err(|_| format!("cannot inspect {relative}"))?
                .is_file()
            {
                return Err("skill resources must be regular files".into());
            }
            let mut content = Vec::new();
            file.take(MAX_BYTES_PER_SKILL - bytes + 1)
                .read_to_end(&mut content)
                .map_err(|_| format!("cannot read {relative}"))?;
            bytes += content.len() as u64;
            if bytes > MAX_BYTES_PER_SKILL {
                return Err("skill folder exceeds 512 files or 16 MiB".into());
            }
            if relative == "SKILL.md" {
                if content.len() > crate::format::MAX_SKILL_FILE_BYTES {
                    return Err("SKILL.md exceeds the size limit".into());
                }
                manifest = Some(
                    String::from_utf8(content.clone())
                        .map_err(|_| "SKILL.md must be UTF-8 text".to_string())?,
                );
            }
            files.push((relative, digest(&content)));
        }
    }
    files.sort();
    let hash = digest(&serde_json::to_vec(&files).unwrap_or_default());
    let resources = files
        .into_iter()
        .map(|(p, _)| p)
        .filter(|p| p != "SKILL.md")
        .collect();
    Ok((hash, resources, manifest.ok_or("missing SKILL.md")?))
}
