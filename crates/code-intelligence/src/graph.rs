//! Transactional, per-project graph over real Tree-sitter syntax trees.
use crate::*;
use excalibur_store::{
    Database, Priority, ReadLane,
    rusqlite::{Connection, OptionalExtension, params},
};
use excalibur_vault::Redactor;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Component, Path},
    sync::{Arc, Mutex, OnceLock, Weak},
};
mod parser;
use parser::{CachedTree, ParsedFile};
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ParseMode {
    Unchanged,
    Cold,
    Incremental,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct IndexReport {
    pub file_id: String,
    pub path: String,
    pub hash: String,
    pub mode: ParseMode,
    pub nodes: usize,
    pub edges: usize,
    pub syntax_errors: Vec<SyntaxIssue>,
    pub changed_ranges: Vec<Range>,
    pub redacted_nodes: usize,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct GraphNode {
    pub id: String,
    pub path: Option<String>,
    pub kind: String,
    pub name: String,
    pub qualified_name: String,
    pub start_byte: usize,
    pub end_byte: usize,
    pub range: Range,
    pub attributes: Value,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct GraphEdge {
    pub id: String,
    pub source_id: String,
    pub destination_id: String,
    pub kind: String,
    pub resolution: String,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct GraphPage {
    pub nodes: Vec<GraphNode>,
    pub edges: Vec<GraphEdge>,
    pub has_more: bool,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "operation", rename_all = "snake_case")]
pub enum GraphQuery {
    Symbols {
        query: String,
        limit: usize,
    },
    Callers {
        symbol: String,
        limit: usize,
    },
    Dependencies {
        path: String,
        limit: usize,
    },
    Implementations {
        requirement_id: String,
        limit: usize,
    },
    File {
        path: String,
        limit: usize,
    },
}
struct Shared {
    serial: tokio::sync::Mutex<()>,
    trees: Mutex<BTreeMap<String, CachedTree>>,
}
type SharedRegistry = Mutex<BTreeMap<String, Weak<Shared>>>;
static SHARED: OnceLock<SharedRegistry> = OnceLock::new();
#[derive(Clone)]
pub struct CodeGraph {
    pub(crate) db: Database,
    project: String,
    shared: Arc<Shared>,
    pub(crate) redactor: Redactor,
    max_file_bytes: usize,
}
impl std::fmt::Debug for CodeGraph {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CodeGraph")
            .field("project", &self.project)
            .finish_non_exhaustive()
    }
}
impl CodeGraph {
    pub async fn open(db: Database, project_id: &str) -> Result<Self> {
        if project_id.is_empty() || project_id.len() > 256 {
            return Err(invalid("project identity required"));
        }
        let project = project_id.to_owned();
        let sql = include_str!("graph.sql");
        let checksum = hash(sql.as_bytes());
        db.write(Priority::Normal,move|c|{
            let codegraph:bool=c.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type='table' AND name='files') AND EXISTS(SELECT 1 FROM sqlite_schema WHERE type='table' AND name='nodes')",[],|r|r.get(0))?;
            if !codegraph{return Err(excalibur_store::Error::Invalid("code intelligence requires the project codegraph database"));}
            c.execute_batch("CREATE TABLE IF NOT EXISTS ci_schema(version INTEGER PRIMARY KEY,checksum TEXT NOT NULL)")?;
            let old:Option<String>=c.query_row("SELECT checksum FROM ci_schema WHERE version=1",[],|r|r.get(0)).optional()?;
            match old{Some(old)if old!=checksum=>return Err(excalibur_store::Error::Invalid("code graph schema checksum mismatch")),Some(_)=>{},None=>{c.execute_batch(sql)?;c.execute("INSERT INTO ci_schema VALUES(1,?1)",[checksum])?;}}
            let owner:Option<String>=c.query_row("SELECT project_id FROM ci_project LIMIT 1",[],|r|r.get(0)).optional()?;
            if owner.as_ref().is_some_and(|owner|owner!=&project){return Err(excalibur_store::Error::Invalid("code graph belongs to another project"));}
            c.execute("INSERT OR IGNORE INTO ci_project VALUES(?1)",[project])?;Ok(())
        }).await?;
        let key = db.path().to_string_lossy().into_owned();
        let shared = {
            let mut locks = SHARED.get_or_init(Default::default).lock().unwrap();
            locks.retain(|_, v| v.strong_count() > 0);
            if let Some(shared) = locks.get(&key).and_then(Weak::upgrade) {
                shared
            } else {
                let shared = Arc::new(Shared {
                    serial: tokio::sync::Mutex::new(()),
                    trees: Mutex::new(BTreeMap::new()),
                });
                locks.insert(key, Arc::downgrade(&shared));
                shared
            }
        };
        Ok(Self {
            db,
            project: project_id.into(),
            shared,
            redactor: Redactor::default(),
            max_file_bytes: 8 * 1024 * 1024,
        })
    }
    pub fn with_redactor(mut self, redactor: Redactor) -> Self {
        self.redactor = redactor;
        self
    }
    pub fn database(&self) -> &Database {
        &self.db
    }
    pub async fn indexed_paths(&self) -> Result<Vec<String>> {
        self.db
            .read(ReadLane::Background, |c| {
                let mut query = c.prepare("SELECT path FROM files ORDER BY path")?;
                Ok(query
                    .query_map([], |r| r.get::<_, String>(0))?
                    .collect::<std::result::Result<Vec<_>, _>>()?)
            })
            .await
            .map_err(Error::from)
    }
    pub async fn index_path_content(&self, path: &Path, content: &str) -> Result<IndexReport> {
        self.index_file(
            path,
            Language::from_path(path).ok_or(Error::UnsupportedLanguage)?,
            content,
        )
        .await
    }
    pub async fn index_file(
        &self,
        path: &Path,
        language: Language,
        content: &str,
    ) -> Result<IndexReport> {
        if content.len() > self.max_file_bytes {
            return Err(invalid(
                "source exceeds the configured eight MiB parser limit",
            ));
        }
        let path = relative(path)?;
        let digest = hash(content.as_bytes());
        let grammar = format!("ci-v1:{}", language.id());
        let _serial = self.shared.serial.lock().await;
        let (lookup, lang, wanted) = (path.clone(), language.id().to_owned(), grammar.clone());
        let known=self.db.read(ReadLane::Background,move|c|Ok(c.query_row("SELECT f.id,f.hash,f.language,m.grammar,m.syntax_errors FROM files f LEFT JOIN ci_file_meta m ON m.file_id=f.id WHERE f.path=?1",[lookup],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,Option<String>>(2)?,r.get::<_,Option<String>>(3)?,r.get::<_,Option<String>>(4)?))).optional()?)).await?;
        let file_id = known.as_ref().map(|row| row.0.clone()).unwrap_or_else(|| {
            format!(
                "file_{}",
                hash(format!("{}\0{path}", self.project).as_bytes())
            )
        });
        if let Some((id, old, old_language, old_grammar, issues)) = known
            && old == digest
            && old_language.as_deref() == Some(&lang)
            && old_grammar.as_deref() == Some(&wanted)
        {
            let lookup = id.clone();
            let(nodes,edges)=self.db.read(ReadLane::Background,move|c|Ok((c.query_row("SELECT count(*) FROM nodes WHERE file_id=?1",[&lookup],|r|r.get::<_,usize>(0))?,c.query_row("SELECT count(*) FROM edges WHERE source_id IN(SELECT id FROM nodes WHERE file_id=?1)",[lookup],|r|r.get::<_,usize>(0))?))).await?;
            return Ok(IndexReport {
                file_id: id,
                path,
                hash: digest,
                mode: ParseMode::Unchanged,
                nodes,
                edges,
                syntax_errors: serde_json::from_str(&issues.unwrap_or_else(|| "[]".into()))?,
                changed_ranges: vec![],
                redacted_nodes: 0,
            });
        }
        let parse_file_id = file_id.clone();
        let (shared, parse_path, source, redactor) = (
            self.shared.clone(),
            path.clone(),
            content.to_owned(),
            self.redactor.clone(),
        );
        let parsed = tokio::task::spawn_blocking(move || {
            let old = shared.trees.lock().unwrap().remove(&parse_path);
            let (parsed, cache) =
                parser::parse(&parse_path, language, source, old, &redactor, parse_file_id)?;
            shared.trees.lock().unwrap().insert(parse_path, cache);
            Ok::<_, Error>(parsed)
        })
        .await
        .map_err(|_| invalid("source parser worker failed"))??;
        let (mode, syntax_errors, changed_ranges, redacted_nodes) = (
            parsed.mode,
            parsed.issues.clone(),
            parsed.changed_ranges.clone(),
            parsed.redacted,
        );
        let (stored_path, stored_hash, stored_id) = (path.clone(), digest.clone(), file_id.clone());
        let (nodes, edges) = self
            .db
            .write(Priority::Background, move |c| {
                store_parsed(
                    c,
                    &stored_path,
                    &stored_hash,
                    &stored_id,
                    language,
                    &grammar,
                    parsed,
                )
                .map_err(|e| excalibur_store::Error::Configuration(e.to_string()))
            })
            .await?;
        Ok(IndexReport {
            file_id,
            path,
            hash: digest,
            mode,
            nodes,
            edges,
            syntax_errors,
            changed_ranges,
            redacted_nodes,
        })
    }
    pub async fn remove_file(&self, path: &Path) -> Result<bool> {
        let path = relative(path)?;
        let _serial = self.shared.serial.lock().await;
        self.shared.trees.lock().unwrap().remove(&path);
        Ok(self
            .db
            .write(Priority::Background, move |c| {
                let mut q = c.prepare(
                    "SELECT name FROM nodes WHERE file_id=(SELECT id FROM files WHERE path=?1)",
                )?;
                let names = q
                    .query_map([&path], |r| r.get::<_, Option<String>>(0))?
                    .filter_map(|r| r.transpose())
                    .collect::<std::result::Result<Vec<_>, _>>()?;
                let count = c.execute("DELETE FROM files WHERE path=?1", [&path])?;
                resolve_refs(c, &BTreeSet::from_iter(names), Some(&path))
                    .map_err(|e| excalibur_store::Error::Configuration(e.to_string()))?;
                Ok(count != 0)
            })
            .await?)
    }
    pub async fn query(&self, query: GraphQuery) -> Result<GraphPage> {
        let redactor = self.redactor.clone();
        self.db
            .read(ReadLane::Interactive, move |c| {
                query_graph(c, query)
                    .map_err(|e| excalibur_store::Error::Configuration(e.to_string()))
            })
            .await
            .map(|mut page| {
                for node in &mut page.nodes {
                    node.name = redactor.redact_text(&node.name);
                    node.qualified_name = redactor.redact_text(&node.qualified_name);
                    node.attributes = redactor.redact_json(&node.attributes);
                }
                for edge in &mut page.edges {
                    edge.resolution = redactor.redact_text(&edge.resolution);
                }
                page
            })
            .map_err(Error::from)
    }
    pub async fn impacted_tests(&self, paths: &[String]) -> Result<Vec<GraphNode>> {
        let paths = paths
            .iter()
            .map(|p| relative(Path::new(p)))
            .collect::<Result<Vec<_>>>()?;
        self.db.read(ReadLane::Interactive,move|c|{
            let mut ids=BTreeSet::new();let mut pending=Vec::new();for path in paths{let mut q=c.prepare("SELECT id FROM nodes WHERE file_id=(SELECT id FROM files WHERE path=?1)")?;pending.extend(q.query_map([path],|r|r.get::<_,String>(0))?.collect::<std::result::Result<Vec<_>,_>>()?);}
            while let Some(id)=pending.pop(){if !ids.insert(id.clone()){continue;}let mut q=c.prepare("SELECT source_id FROM edges WHERE destination_id=?1 AND kind IN('call','reference','import','tests','defines')")?;pending.extend(q.query_map([id],|r|r.get::<_,String>(0))?.collect::<std::result::Result<Vec<_>,_>>()?);if ids.len()>100_000{return Err(excalibur_store::Error::Invalid("impact traversal exceeds graph budget"));}}
            let mut tests=BTreeSet::new();for id in ids{let file:Option<String>=c.query_row("SELECT file_id FROM nodes WHERE id=?1",[&id],|r|r.get(0))?;if let Some(file)=file{let mut q=c.prepare("SELECT id FROM nodes WHERE file_id=?1 AND kind='test'")?;tests.extend(q.query_map([file],|r|r.get::<_,String>(0))?.collect::<std::result::Result<Vec<_>,_>>()?);}}
            tests.into_iter().map(|id|load_node(c,&id).map_err(|e|excalibur_store::Error::Configuration(e.to_string()))).collect()
        }).await.map_err(Error::from)
    }
    /// Host-authorized requirement mappings are explicit, never guessed from natural language.
    pub async fn link_requirement(
        &self,
        requirement: &str,
        targets: &[String],
        evidence_reference: &str,
    ) -> Result<()> {
        if requirement.is_empty() || evidence_reference.is_empty() {
            return Err(invalid("requirement and provenance are required"));
        }
        let project = self.project.clone();
        let (requirement, targets, evidence) = (
            requirement.to_owned(),
            targets.to_vec(),
            self.redactor.redact_text(evidence_reference),
        );
        let _serial = self.shared.serial.lock().await;
        self.db.write(Priority::Normal,move|c|{let id=format!("requirement_{}",hash(format!("{project}\0{requirement}").as_bytes()));c.execute("INSERT INTO nodes(id,kind,name) VALUES(?1,'requirement',?2) ON CONFLICT(id) DO NOTHING",params![id,requirement])?;for target in targets{let exists:bool=c.query_row("SELECT EXISTS(SELECT 1 FROM nodes WHERE id=?1 AND file_id IS NOT NULL)",[&target],|r|r.get(0))?;if !exists{return Err(excalibur_store::Error::Invalid("requirement target does not exist"));}let edge=format!("requirement_{}",hash(format!("{id}\0{target}").as_bytes()));c.execute("INSERT INTO edges(id,source_id,destination_id,kind) VALUES(?1,?2,?3,'implements') ON CONFLICT(id) DO NOTHING",params![edge,id,target])?;c.execute("INSERT INTO ci_edge_meta VALUES(?1,?2) ON CONFLICT(edge_id) DO UPDATE SET resolution=excluded.resolution",params![edge,format!("host:{evidence}")])?;}Ok(())}).await?;
        Ok(())
    }
    /// A semantic location may enrich a current graph only under matching file hashes.
    pub async fn enrich_edge(
        &self,
        source: &str,
        target: &str,
        kind: &str,
        source_hash: &str,
        target_hash: &str,
    ) -> Result<()> {
        if !matches!(kind, "call" | "reference" | "import" | "tests") {
            return Err(invalid("unsupported semantic edge kind"));
        }
        let (source, target, kind, sh, th) = (
            source.to_owned(),
            target.to_owned(),
            kind.to_owned(),
            source_hash.to_owned(),
            target_hash.to_owned(),
        );
        let _serial = self.shared.serial.lock().await;
        self.db.write(Priority::Background,move|c|{for(id,wanted)in [(&source,&sh),(&target,&th)]{let actual:String=c.query_row("SELECT f.hash FROM files f JOIN nodes n ON n.file_id=f.id WHERE n.id=?1",[id],|r|r.get(0))?;if &actual!=wanted{return Err(excalibur_store::Error::Invalid("semantic graph enrichment is stale"));}}let id=format!("lsp_{}",hash(format!("{source}\0{target}\0{kind}").as_bytes()));c.execute("INSERT INTO edges(id,source_id,destination_id,kind) VALUES(?1,?2,?3,?4) ON CONFLICT(id) DO NOTHING",params![id,source,target,kind])?;c.execute("INSERT OR REPLACE INTO ci_edge_meta VALUES(?1,'lsp')",[id])?;Ok(())}).await?;
        Ok(())
    }
}
pub(crate) fn relative(path: &Path) -> Result<String> {
    let mut parts = Vec::new();
    for part in path.components() {
        match part {
            Component::Normal(v) => parts.push(
                v.to_str()
                    .ok_or_else(|| invalid("source path must be UTF-8"))?,
            ),
            Component::CurDir => {}
            _ => return Err(invalid("source path must stay relative to the project")),
        }
    }
    if parts.is_empty() {
        return Err(invalid("source path required"));
    }
    Ok(parts.join("/"))
}
fn store_parsed(
    c: &Connection,
    path: &str,
    digest: &str,
    file: &str,
    language: Language,
    grammar: &str,
    parsed: ParsedFile,
) -> Result<(usize, usize)> {
    let mut old = c.prepare("SELECT name FROM nodes WHERE file_id=?1")?;
    let mut names = old
        .query_map([file], |r| r.get::<_, Option<String>>(0))?
        .filter_map(|r| r.transpose())
        .collect::<std::result::Result<BTreeSet<_>, _>>()
        .map_err(excalibur_store::Error::from)?;
    c.execute("INSERT INTO files(id,path,hash,language,content) VALUES(?1,?2,?3,?4,'') ON CONFLICT(path) DO UPDATE SET hash=excluded.hash,language=excluded.language,content='',updated_at=unixepoch()",params![file,path,digest,language.id()]).map_err(excalibur_store::Error::from)?;
    c.execute("DELETE FROM nodes WHERE file_id=?1", [file])
        .map_err(excalibur_store::Error::from)?;
    for node in &parsed.nodes {
        names.insert(node.name.clone());
        c.execute("INSERT INTO nodes(id,file_id,kind,name,start_offset,end_offset) VALUES(?1,?2,?3,?4,?5,?6)",params![node.id,file,node.kind,node.name,node.start_byte,node.end_byte]).map_err(excalibur_store::Error::from)?;
        c.execute(
            "INSERT INTO ci_node_meta VALUES(?1,?2,?3,?4,?5,?6,?7)",
            params![
                node.id,
                node.qualified_name,
                node.range.start.line,
                node.range.start.character,
                node.range.end.line,
                node.range.end.character,
                node.attributes.to_string()
            ],
        )
        .map_err(excalibur_store::Error::from)?;
    }
    for edge in &parsed.edges {
        insert_edge(
            c,
            &edge.id,
            &edge.source_id,
            &edge.destination_id,
            &edge.kind,
            &edge.resolution,
        )?;
    }
    for reference in parsed.references {
        c.execute(
            "INSERT INTO ci_references VALUES(?1,?2,?3,?4,?5,?6)",
            params![
                reference.id,
                reference.source,
                reference.kind,
                reference.target,
                reference.path,
                reference.offset
            ],
        )
        .map_err(excalibur_store::Error::from)?;
    }
    c.execute("INSERT INTO ci_file_meta VALUES(?1,?2,?3) ON CONFLICT(file_id) DO UPDATE SET grammar=excluded.grammar,syntax_errors=excluded.syntax_errors",params![file,grammar,serde_json::to_string(&parsed.issues)?]).map_err(excalibur_store::Error::from)?;
    resolve_refs(c, &names, Some(path))?;
    let edges = c
        .query_row(
            "SELECT count(*) FROM edges WHERE source_id IN(SELECT id FROM nodes WHERE file_id=?1)",
            [file],
            |r| r.get::<_, usize>(0),
        )
        .map_err(excalibur_store::Error::from)?;
    Ok((parsed.nodes.len(), edges))
}
fn insert_edge(
    c: &Connection,
    id: &str,
    source: &str,
    target: &str,
    kind: &str,
    resolution: &str,
) -> Result<()> {
    c.execute("INSERT INTO edges(id,source_id,destination_id,kind) VALUES(?1,?2,?3,?4) ON CONFLICT(id) DO UPDATE SET source_id=excluded.source_id,destination_id=excluded.destination_id,kind=excluded.kind",params![id,source,target,kind]).map_err(excalibur_store::Error::from)?;
    c.execute("INSERT INTO ci_edge_meta VALUES(?1,?2) ON CONFLICT(edge_id) DO UPDATE SET resolution=excluded.resolution",params![id,resolution]).map_err(excalibur_store::Error::from)?;
    Ok(())
}
fn resolve_refs(
    c: &Connection,
    names: &BTreeSet<String>,
    changed_path: Option<&str>,
) -> Result<()> {
    let names = serde_json::to_string(names)?;
    let mut q=c.prepare("SELECT r.id,r.source_id,r.kind,r.target_name,r.target_path,s.file_id FROM ci_references r JOIN nodes s ON s.id=r.source_id LEFT JOIN files f ON f.id=s.file_id WHERE f.path=?1 OR r.target_path=?1 OR r.target_name IN(SELECT value FROM json_each(?2)) OR NOT EXISTS(SELECT 1 FROM edges e WHERE e.id=r.id)").map_err(excalibur_store::Error::from)?;
    let refs = q
        .query_map(params![changed_path, names], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, Option<String>>(4)?,
                r.get::<_, Option<String>>(5)?,
            ))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(excalibur_store::Error::from)?;
    for (id, source, kind, target, path, file) in refs {
        let mut candidates = if let Some(path) = path {
            let mut q=c.prepare("SELECT n.id FROM nodes n JOIN files f ON f.id=n.file_id WHERE (f.path=?1 OR f.path=?1||'.rs' OR f.path=?1||'/mod.rs' OR f.path=?1||'.py' OR f.path=?1||'/__init__.py' OR f.path=?1||'.ts' OR f.path=?1||'.tsx' OR f.path=?1||'.js' OR f.path=?1||'/index.ts' OR f.path=?1||'/index.js') AND ((n.kind='file' AND ?2='') OR n.name=?2 OR json_extract((SELECT attributes FROM ci_node_meta WHERE node_id=n.id),'$.slug')=?2) ORDER BY n.id").map_err(excalibur_store::Error::from)?;
            q.query_map(params![path, target], |r| r.get::<_, String>(0))?
                .collect::<std::result::Result<Vec<_>, _>>()
                .map_err(excalibur_store::Error::from)?
        } else {
            let mut q=c.prepare("SELECT id FROM nodes WHERE name=?1 AND kind IN('function','method','type','module','test') ORDER BY file_id IS ?2 DESC,id").map_err(excalibur_store::Error::from)?;
            q.query_map(params![target, file], |r| r.get::<_, String>(0))?
                .collect::<std::result::Result<Vec<_>, _>>()
                .map_err(excalibur_store::Error::from)?
        };
        // Prefer a unique same-file binding, otherwise retain ambiguity explicitly.
        if candidates.len() > 1 {
            let local = candidates
                .iter()
                .filter(|id| {
                    c.query_row(
                        "SELECT file_id IS ?2 FROM nodes WHERE id=?1",
                        params![id, file],
                        |r| r.get::<_, bool>(0),
                    )
                    .unwrap_or(false)
                })
                .cloned()
                .collect::<Vec<_>>();
            if local.len() == 1 {
                candidates = local;
            }
        }
        let (target_id, resolution) = if candidates.len() == 1 {
            (candidates.remove(0), "syntactic_name")
        } else {
            let unresolved = format!(
                "unresolved_{}",
                hash(format!("{kind}\0{target}").as_bytes())
            );
            c.execute("INSERT INTO nodes(id,kind,name) VALUES(?1,'unresolved',?2) ON CONFLICT(id) DO NOTHING",params![unresolved,target]).map_err(excalibur_store::Error::from)?;
            (unresolved, "unresolved")
        };
        insert_edge(c, &id, &source, &target_id, &kind, resolution)?;
    }
    c.execute("DELETE FROM nodes WHERE kind='unresolved' AND NOT EXISTS(SELECT 1 FROM edges WHERE destination_id=nodes.id)",[]).map_err(excalibur_store::Error::from)?;
    Ok(())
}
fn load_node(c: &Connection, id: &str) -> Result<GraphNode> {
    c.query_row("SELECT n.id,f.path,n.kind,COALESCE(n.name,''),COALESCE(m.qualified_name,n.name,''),COALESCE(n.start_offset,0),COALESCE(n.end_offset,0),COALESCE(m.start_line,0),COALESCE(m.start_character,0),COALESCE(m.end_line,0),COALESCE(m.end_character,0),COALESCE(m.attributes,'{}') FROM nodes n LEFT JOIN files f ON f.id=n.file_id LEFT JOIN ci_node_meta m ON m.node_id=n.id WHERE n.id=?1",[id],|r|Ok(GraphNode{id:r.get(0)?,path:r.get(1)?,kind:r.get(2)?,name:r.get(3)?,qualified_name:r.get(4)?,start_byte:r.get(5)?,end_byte:r.get(6)?,range:Range{start:Position{line:r.get(7)?,character:r.get(8)?},end:Position{line:r.get(9)?,character:r.get(10)?}},attributes:serde_json::from_str(&r.get::<_,String>(11)?).unwrap_or(Value::Null)})).map_err(|e|Error::Store(e.into()))
}
fn query_graph(c: &Connection, query: GraphQuery) -> Result<GraphPage> {
    let (limit, ids, edges) = match query {
        GraphQuery::Symbols { query, limit } => {
            let mut q=c.prepare("SELECT id FROM nodes WHERE kind<>'unresolved' AND name LIKE ?1 ESCAPE '\\' ORDER BY name,id LIMIT ?2").map_err(excalibur_store::Error::from)?;
            let ids = q
                .query_map(
                    params![
                        format!(
                            "%{}%",
                            query
                                .replace('\\', "\\\\")
                                .replace('%', "\\%")
                                .replace('_', "\\_")
                        ),
                        limit.min(100) + 1
                    ],
                    |r| r.get::<_, String>(0),
                )?
                .collect::<std::result::Result<Vec<_>, _>>()
                .map_err(excalibur_store::Error::from)?;
            (limit, ids, vec![])
        }
        GraphQuery::File { path, limit } => {
            let path = relative(Path::new(&path))?;
            let mut q=c.prepare("SELECT id FROM nodes WHERE file_id=(SELECT id FROM files WHERE path=?1) ORDER BY start_offset,id LIMIT ?2").map_err(excalibur_store::Error::from)?;
            let ids = q
                .query_map(params![path, limit.min(100) + 1], |r| r.get::<_, String>(0))?
                .collect::<std::result::Result<Vec<_>, _>>()
                .map_err(excalibur_store::Error::from)?;
            (limit, ids, vec![])
        }
        GraphQuery::Callers { symbol, limit } => edges_query(
            c,
            "SELECT e.id,e.source_id,e.destination_id,e.kind,COALESCE(m.resolution,'syntactic') FROM edges e JOIN nodes d ON d.id=e.destination_id LEFT JOIN ci_edge_meta m ON m.edge_id=e.id WHERE (d.id=?1 OR d.name=?1) AND e.kind IN('call','tests','reference') ORDER BY e.id LIMIT ?2",
            &symbol,
            limit,
        )?,
        GraphQuery::Dependencies { path, limit } => {
            let path = relative(Path::new(&path))?;
            edges_query(
                c,
                "SELECT e.id,e.source_id,e.destination_id,e.kind,COALESCE(m.resolution,'syntactic') FROM edges e JOIN nodes s ON s.id=e.source_id JOIN files f ON f.id=s.file_id LEFT JOIN ci_edge_meta m ON m.edge_id=e.id WHERE f.path=?1 AND e.kind IN('import','call','reference') ORDER BY e.id LIMIT ?2",
                &path,
                limit,
            )?
        }
        GraphQuery::Implementations {
            requirement_id,
            limit,
        } => edges_query(
            c,
            "SELECT e.id,e.source_id,e.destination_id,e.kind,COALESCE(m.resolution,'host') FROM edges e JOIN nodes s ON s.id=e.source_id LEFT JOIN ci_edge_meta m ON m.edge_id=e.id WHERE (s.id=?1 OR s.name=?1) AND e.kind='implements' ORDER BY e.id LIMIT ?2",
            &requirement_id,
            limit,
        )?,
    };
    if limit == 0 {
        return Err(invalid("query limit must be positive"));
    }
    let limit = limit.min(100);
    let has_more = ids.len() > limit || edges.len() > limit;
    let nodes = ids
        .into_iter()
        .take(limit)
        .map(|id| load_node(c, &id))
        .collect::<Result<Vec<_>>>()?;
    Ok(GraphPage {
        nodes,
        edges: edges.into_iter().take(limit).collect(),
        has_more,
    })
}
fn edges_query(
    c: &Connection,
    sql: &str,
    value: &str,
    limit: usize,
) -> Result<(usize, Vec<String>, Vec<GraphEdge>)> {
    let mut q = c.prepare(sql).map_err(excalibur_store::Error::from)?;
    let edges = q
        .query_map(params![value, limit.min(100) + 1], |r| {
            Ok(GraphEdge {
                id: r.get(0)?,
                source_id: r.get(1)?,
                destination_id: r.get(2)?,
                kind: r.get(3)?,
                resolution: r.get(4)?,
            })
        })?
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(excalibur_store::Error::from)?;
    let ids = edges
        .iter()
        .flat_map(|e| [e.source_id.clone(), e.destination_id.clone()])
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    Ok((limit, ids, edges))
}
