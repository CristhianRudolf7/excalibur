//! Version-bound evidence, targeted invalidation and workspace fingerprints.
pub use crate::verification::CheckRunner;
use crate::{Result, TaskStore, fail, new_id};
use excalibur_store::{
    Priority, ReadLane,
    rusqlite::{self, OptionalExtension},
};
use globset::{Glob, GlobSet, GlobSetBuilder};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CheckMethod {
    Command,
    Test,
    Inspection,
    Metric,
    UserConfirmation,
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceResult {
    Pass,
    Fail,
    Error,
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceKind {
    Execution,
    Judgment,
    IndependentVerification,
    UserConfirmation,
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CriterionState {
    Unverified,
    Failed,
    Satisfied,
    NotApplicable,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct WorkspaceSnapshot {
    pub instruction_revision: u64,
    pub commit: Option<String>,
    pub tree_hash: String,
    pub fingerprints: BTreeMap<String, String>,
    pub impact_patterns: Vec<String>,
    #[serde(default)]
    pub workspace: PathBuf,
    #[serde(default)]
    pub projection_paths: Vec<PathBuf>,
    #[serde(default)]
    pub excluded_paths: Vec<PathBuf>,
}
impl WorkspaceSnapshot {
    pub async fn capture(
        workspace: PathBuf,
        patterns: Vec<String>,
        revision: u64,
        projections: Vec<PathBuf>,
        exclusions: Vec<PathBuf>,
    ) -> Result<Self> {
        tokio::task::spawn_blocking(move || {
            capture(workspace, patterns, revision, projections, exclusions)
        })
        .await
        .map_err(|_| fail("fingerprint worker interrupted"))?
    }
    pub async fn fresh(&self, revision: u64) -> Result<bool> {
        if revision != self.instruction_revision {
            return Ok(false);
        }
        let now = Self::capture(
            self.workspace.clone(),
            self.impact_patterns.clone(),
            revision,
            self.projection_paths.clone(),
            self.excluded_paths.clone(),
        )
        .await?;
        Ok(now.fingerprints == self.fingerprints)
    }
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct EvidenceRecord {
    pub id: String,
    pub task_id: String,
    pub criterion_id: String,
    pub check_id: String,
    pub run_id: String,
    pub result: EvidenceResult,
    pub kind: EvidenceKind,
    pub output_blob: Option<String>,
    pub recorded_at: u64,
    pub snapshot: WorkspaceSnapshot,
    pub tool_call_id: Option<String>,
    pub trusted: bool,
    pub stale: bool,
    pub explanation: String,
    #[serde(default)]
    pub producer_snapshot: Value,
    #[serde(default)]
    pub tool_calls: u64,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct CriterionAssessment {
    pub criterion_id: String,
    pub state: CriterionState,
    pub reason: String,
    pub evidence: Option<EvidenceRecord>,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct RequirementBrief {
    pub id: String,
    pub text: String,
    pub version: u64,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct CriterionBrief {
    pub id: String,
    pub requirement_id: String,
    pub statement: String,
    pub method: CheckMethod,
    pub expected: Value,
    pub impact_area: Vec<String>,
    pub checks: Vec<crate::verification::CheckBrief>,
}
fn globset(patterns: &[String]) -> Result<GlobSet> {
    let mut b = GlobSetBuilder::new();
    for p in patterns {
        if p.starts_with('/') || p.split('/').any(|x| x == "..") {
            return Err(fail("impact patterns must be workspace relative"));
        }
        b.add(Glob::new(p).map_err(|_| fail("invalid impact glob"))?);
    }
    b.build().map_err(|_| fail("invalid impact glob set"))
}
pub(crate) fn matches(patterns: &[String], path: &str) -> bool {
    globset(patterns).is_ok_and(|g| g.is_match(path))
}
fn capture(
    workspace: PathBuf,
    patterns: Vec<String>,
    revision: u64,
    projections: Vec<PathBuf>,
    mut exclusions: Vec<PathBuf>,
) -> Result<WorkspaceSnapshot> {
    use cap_std::{ambient_authority, fs::Dir};
    use excalibur_sandbox::file_authority::{ReadAuthority, credential_path};
    use sha2::{Digest, Sha256};
    use std::io::Read;
    let root = workspace
        .canonicalize()
        .map_err(|e| fail(format!("workspace unavailable: {e}")))?;
    globset(&patterns)?;
    exclusions.push(root.join(".git"));
    exclusions.sort();
    exclusions.dedup();
    let patterns = patterns
        .into_iter()
        .map(|p| {
            if !p.contains(['*', '?', '[', '{']) && root.join(&p).is_dir() {
                format!("{p}/**")
            } else {
                p
            }
        })
        .collect::<Vec<_>>();
    let set = globset(&patterns)?;
    let mut fingerprints = BTreeMap::new();
    // A narrow impact never traverses target/, dependencies or unrelated trees.
    let prefixes = patterns
        .iter()
        .map(|p| {
            let literal = p.split(['*', '?', '[', '{']).next().unwrap_or("");
            (
                literal
                    .rsplit_once('/')
                    .map_or("", |(prefix, _)| prefix)
                    .to_owned(),
                p.contains(['*', '?', '[', '{']),
            )
        })
        .collect::<Vec<_>>();
    let descend = |relative: &str| {
        prefixes.iter().any(|(prefix, wild)| {
            (*wild && prefix.is_empty())
                || relative == prefix
                || relative.starts_with(&format!("{prefix}/"))
                || prefix.starts_with(&format!("{relative}/"))
        })
    };
    let capability =
        Dir::open_ambient_dir(&root, ambient_authority()).map_err(|e| fail(e.to_string()))?;
    let mut authority = ReadAuthority::excluding_credential_names();
    let mut queue = if patterns.is_empty() {
        vec![]
    } else {
        vec![PathBuf::new()]
    };
    let mut entries = 0usize;
    let mut hashed_bytes = 0u64;
    let mut directories = 0usize;
    while let Some(relative_dir) = queue.pop() {
        directories += 1;
        if directories > 4096 {
            return Err(fail(
                "workspace fingerprint exceeds 4096 directories; narrow impact patterns",
            ));
        }
        let dir = if relative_dir.as_os_str().is_empty() {
            capability.try_clone()
        } else {
            capability.open_dir(&relative_dir)
        }
        .map_err(|e| fail(format!("fingerprint directory: {e}")))?;
        let mut children = dir
            .entries()
            .map_err(|e| fail(e.to_string()))?
            .take(100001usize.saturating_sub(entries))
            .collect::<std::io::Result<Vec<_>>>()
            .map_err(|e| fail(e.to_string()))?;
        children.sort_by_key(|e| e.file_name());
        for child in children {
            entries += 1;
            if entries > 100_000 {
                return Err(fail(
                    "workspace fingerprint exceeds 100000 entries; narrow impact patterns",
                ));
            }
            let relative = relative_dir.join(child.file_name());
            let path = root.join(&relative);
            if exclusions.iter().any(|e| path.starts_with(e)) || credential_path(&relative) {
                continue;
            }
            let key = relative.to_string_lossy().replace('\\', "/");
            let metadata = dir
                .symlink_metadata(child.file_name())
                .map_err(|e| fail(e.to_string()))?;
            if metadata.is_dir() {
                if descend(&key) {
                    queue.push(relative)
                }
                continue;
            }
            if !set.is_match(&key) {
                continue;
            }
            let canonical = path.canonicalize().map_err(|e| fail(e.to_string()))?;
            if !canonical.starts_with(&root)
                || exclusions.iter().any(|e| canonical.starts_with(e))
                || credential_path(canonical.strip_prefix(&root).unwrap_or(&canonical))
            {
                return Err(fail("impact path crosses protected read authority"));
            }
            let mut file = capability
                .open(&relative)
                .map_err(|e| fail(format!("fingerprint file authority: {e}")))?;
            authority
                .check(&capability, &root, &exclusions, &file)
                .map_err(fail)?;
            // Recheck the pathname after opening; descriptor identity and kernel
            // capability resolution prevent symlink races from escaping the root.
            let after = path.canonicalize().map_err(|e| fail(e.to_string()))?;
            if after != canonical {
                return Err(fail("impact path changed while opening"));
            }
            let current = capability
                .open(&relative)
                .map_err(|e| fail(e.to_string()))?;
            if !excalibur_sandbox::file_authority::same_file(&file, &current).map_err(fail)? {
                return Err(fail("impact file identity changed while opening"));
            }
            let opened = file.metadata().map_err(|e| fail(e.to_string()))?;
            if !opened.is_file() {
                return Err(fail("impact includes unsupported special file"));
            }
            let mut hasher = Sha256::new();
            if metadata.file_type().is_symlink() {
                hasher.update(
                    capability
                        .read_link(&relative)
                        .map_err(|e| fail(e.to_string()))?
                        .to_string_lossy()
                        .as_bytes(),
                );
            }
            if projections.iter().any(|p| p == &path) {
                if opened.len() > 64 * 1024 * 1024 {
                    return Err(fail("managed checklist exceeds 64 MiB"));
                }
                let mut bytes = Vec::new();
                std::io::Read::by_ref(&mut file)
                    .take(64 * 1024 * 1024 + 1)
                    .read_to_end(&mut bytes)
                    .map_err(|e| fail(e.to_string()))?;
                if bytes.len() > 64 * 1024 * 1024 {
                    return Err(fail("managed checklist grew beyond 64 MiB"));
                }
                hashed_bytes = hashed_bytes.saturating_add(bytes.len() as u64);
                if hashed_bytes > 512 * 1024 * 1024 {
                    return Err(fail(
                        "impact fingerprint exceeds 512 MiB; narrow patterns or host exclusions",
                    ));
                }
                hasher.update(crate::checklist::canonical_content(&bytes)?);
            } else {
                let mut buffer = [0u8; 64 * 1024];
                loop {
                    let n = file.read(&mut buffer).map_err(|e| fail(e.to_string()))?;
                    if n == 0 {
                        break;
                    }
                    hashed_bytes = hashed_bytes.saturating_add(n as u64);
                    if hashed_bytes > 512 * 1024 * 1024 {
                        return Err(fail(
                            "impact fingerprint exceeds 512 MiB; narrow patterns or host exclusions",
                        ));
                    }
                    hasher.update(&buffer[..n]);
                }
            }
            let final_metadata = file.metadata().map_err(|e| fail(e.to_string()))?;
            if final_metadata.len() != opened.len()
                || final_metadata.modified().ok() != opened.modified().ok()
            {
                return Err(fail("impact file changed while hashing"));
            }
            authority
                .check(&capability, &root, &exclusions, &file)
                .map_err(fail)?;
            fingerprints.insert(key, format!("{:x}", hasher.finalize()));
        }
    }
    for p in &patterns {
        if !p.contains(['*', '?', '[', '{']) && !fingerprints.contains_key(p) {
            fingerprints.insert(p.clone(), "absent".into());
        }
    }
    for projection in &projections {
        if let Ok(relative) = projection.strip_prefix(&root) {
            let relative = relative.to_string_lossy().replace('\\', "/");
            if set.is_match(&relative) && !projection.exists() {
                fingerprints.insert(relative, crate::digest(b""));
            }
        }
    }
    let tree_hash = crate::digest(&serde_json::to_vec(&fingerprints)?);
    let commit = git_commit(&root);
    Ok(WorkspaceSnapshot {
        instruction_revision: revision,
        commit,
        tree_hash,
        fingerprints,
        impact_patterns: patterns,
        workspace: root,
        projection_paths: projections,
        excluded_paths: exclusions,
    })
}
fn git_commit(root: &Path) -> Option<String> {
    let git = root.join(".git");
    let git = if git.is_file() {
        let text = std::fs::read_to_string(&git).ok()?;
        let path = text.trim().strip_prefix("gitdir: ")?;
        root.join(path)
    } else {
        git
    };
    let head = std::fs::read_to_string(git.join("HEAD")).ok()?;
    let value = if let Some(reference) = head.trim().strip_prefix("ref: ") {
        std::fs::read_to_string(git.join(reference))
            .ok()
            .or_else(|| {
                std::fs::read_to_string(git.join("packed-refs"))
                    .ok()?
                    .lines()
                    .find_map(|l| {
                        l.split_once(' ')
                            .filter(|(_, r)| *r == reference)
                            .map(|(h, _)| h.to_owned())
                    })
            })?
    } else {
        head
    };
    let value = value.trim();
    (value.len() == 40 || value.len() == 64)
        .then(|| value.to_owned())
        .filter(|s| s.bytes().all(|b| b.is_ascii_hexdigit()))
}
impl TaskStore {
    pub async fn initialize_evidence(&self) -> Result<()> {
        let sql = include_str!("evidence.sql");
        let hash = crate::digest(sql.as_bytes());
        self.db
            .write(Priority::Journal, move |c| {
                let old: Option<String> = c
                    .query_row(
                        "SELECT checksum FROM tasks_schema WHERE version=2",
                        [],
                        |r| r.get(0),
                    )
                    .optional()?;
                match old {
                    Some(old) if old != hash => {
                        return Err(excalibur_store::Error::Invalid(
                            "evidence schema checksum mismatch",
                        ));
                    }
                    Some(_) => {}
                    None => {
                        c.execute_batch(sql)?;
                        c.execute(
                            "INSERT INTO tasks_schema(version,checksum) VALUES(2,?1)",
                            [hash],
                        )?;
                    }
                }
                Ok(())
            })
            .await?;
        Ok(())
    }
    /// Serializes host file effects and task mutations against a completion guard.
    pub async fn begin_mutation(&self) -> tokio::sync::OwnedRwLockWriteGuard<()> {
        self.mutation_lock.clone().write_owned().await
    }
    pub async fn register_verifier_run(
        &self,
        task_id: &str,
        run: &str,
        parent: &str,
    ) -> Result<()> {
        if run == parent {
            return Err(fail("verifier execution must differ from its parent"));
        }
        let _lease = self.mutation_lock.write().await;
        let task = self
            .task_for_run(parent)
            .await?
            .ok_or_else(|| fail("verification parent is not task bound"))?;
        if task.id != task_id {
            return Err(fail("verification task differs from parent"));
        }
        let scope = task.scope_id;
        let principal = self.policy.principal_id.clone();
        let (task, run, parent) = (task_id.to_owned(), run.to_owned(), parent.to_owned());
        self.db.write(Priority::Journal,move|c|{
            let(owner,namespace,existing):(Option<String>,Option<String>,Option<String>)=c.query_row("SELECT principal_id,scope_id,task_id FROM runs WHERE id=?1",[&run],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?)))?;
            if owner.is_some()&&owner!=principal||namespace.as_ref().is_some_and(|s|s!=&scope)||existing.as_ref().is_some_and(|id|id!=&task){return Err(excalibur_store::Error::Invalid("verifier execution has a different principal, scope or task"))}
            c.execute("INSERT INTO task_run_links(run_id,task_id,parent_run_id) VALUES(?1,?2,?3) ON CONFLICT(run_id) DO NOTHING",rusqlite::params![run,task,parent])?;
            let(known,known_parent):(String,Option<String>)=c.query_row("SELECT task_id,parent_run_id FROM task_run_links WHERE run_id=?1",[&run],|r|Ok((r.get(0)?,r.get(1)?)))?;
            if known!=task||known_parent.as_deref()!=Some(&parent){return Err(excalibur_store::Error::Invalid("verifier run already has a different task or parent"))}
            c.execute("UPDATE runs SET task_id=?1,scope_id=?2,principal_id=?3 WHERE id=?4",rusqlite::params![task,scope,principal,run])?;Ok(())}).await?;
        Ok(())
    }
    pub(crate) async fn authorize_task_id(&self, id: &str) -> Result<String> {
        let (id, principal) = (id.to_owned(), self.policy.principal_id.clone());
        let scope = self
            .db
            .read(ReadLane::Interactive, move |c| {
                Ok(c.query_row(
                    "SELECT scope_id FROM tasks WHERE id=?1 AND principal_id IS ?2",
                    rusqlite::params![id, principal],
                    |r| r.get::<_, String>(0),
                )
                .optional()?)
            })
            .await?
            .ok_or_else(|| fail("task is not in the authorized principal"))?;
        self.check_scope(&scope)?;
        Ok(scope)
    }
    async fn authorize_criterion_id(&self, id: &str) -> Result<String> {
        let (id, principal) = (id.to_owned(), self.policy.principal_id.clone());
        let scope = self
            .db
            .read(ReadLane::Interactive, move |c| {
                Ok(c.query_row(
                    "SELECT scope_id FROM criteria WHERE id=?1 AND principal_id IS ?2",
                    rusqlite::params![id, principal],
                    |r| r.get::<_, String>(0),
                )
                .optional()?)
            })
            .await?
            .ok_or_else(|| fail("criterion is not in the authorized principal"))?;
        self.check_scope(&scope)?;
        Ok(scope)
    }
    pub async fn requirements_and_criteria(
        &self,
        task_id: &str,
    ) -> Result<(Vec<RequirementBrief>, Vec<CriterionBrief>)> {
        self.authorize_task_id(task_id).await?;

        let task_id = task_id.to_owned();
        let principal = self.policy.principal_id.clone();
        let (requirements,criteria)=self.db.read(ReadLane::Interactive,move|c|{
            let mut s=c.prepare("SELECT r.id,r.text,r.version FROM requirements r JOIN task_requirements tr ON tr.requirement_id=r.id WHERE tr.task_id=?1 AND r.principal_id IS ?2 ORDER BY r.id")?;
            let req=s.query_map(rusqlite::params![task_id,principal],|r|Ok(RequirementBrief{id:r.get(0)?,text:r.get(1)?,version:r.get(2)?}))?.collect::<std::result::Result<Vec<_>,_>>()?;
            let mut s=c.prepare("SELECT cr.id,cr.requirement_id,cr.statement,cr.method,cr.expected,cr.impact_area FROM criteria cr JOIN task_requirements tr ON tr.requirement_id=cr.requirement_id WHERE tr.task_id=?1 AND cr.principal_id IS ?2 ORDER BY cr.id")?;
            let rows=s.query_map(rusqlite::params![task_id,principal],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,String>(2)?,r.get::<_,String>(3)?,r.get::<_,Option<String>>(4)?,r.get::<_,Option<String>>(5)?)))?.collect::<std::result::Result<Vec<_>,_>>()?;
            let mut criteria=Vec::new();for(id,requirement_id,statement,method,expected,impact)in rows{
                let method=serde_json::from_value(json!(method)).map_err(|_|excalibur_store::Error::Invalid("invalid criterion method"))?;
                let mut s=c.prepare("SELECT id,definition,environment FROM checks WHERE criterion_id=?1 ORDER BY id")?;
                let checks=s.query_map([&id],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,Option<String>>(2)?)))?.collect::<std::result::Result<Vec<_>,_>>()?.into_iter().map(|(id,definition,environment)|Ok(crate::verification::CheckBrief{id,definition:serde_json::from_str(&definition).map_err(|_|excalibur_store::Error::Invalid("unsupported declared check definition"))?,environment})).collect::<excalibur_store::Result<Vec<_>>>()?;
                criteria.push(CriterionBrief{id,requirement_id,statement,method,expected:expected.and_then(|v|serde_json::from_str(&v).ok()).unwrap_or(Value::Null),impact_area:impact.and_then(|v|serde_json::from_str(&v).ok()).unwrap_or_default(),checks});}
            Ok((req,criteria))}).await?;
        Ok((requirements, criteria))
    }
    pub(crate) async fn criterion_revision(&self, id: &str) -> Result<u64> {
        self.authorize_criterion_id(id).await?;

        let id = id.to_owned();
        Ok(self
            .db
            .read(ReadLane::Interactive, move |c| {
                Ok(c.query_row(
                    "SELECT instruction_revision FROM criteria WHERE id=?1",
                    [id],
                    |r| r.get(0),
                )?)
            })
            .await?)
    }
    pub async fn evidence(&self, id: &str) -> Result<Option<EvidenceRecord>> {
        let (id, principal) = (id.to_owned(), self.policy.principal_id.clone());
        let row=self.db.read(ReadLane::Interactive,move|c|Ok(c.query_row("SELECT scope_id,metadata,stale FROM evidence WHERE id=?1 AND principal_id IS ?2",rusqlite::params![id,principal],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,bool>(2)?))).optional()?)).await?;
        if let Some((scope, text, stale)) = row {
            self.check_scope(&scope)?;
            let mut e: EvidenceRecord = serde_json::from_str(&text)?;
            e.stale = stale;
            Ok(Some(e))
        } else {
            Ok(None)
        }
    }
    pub async fn task_evidence(&self, task_id: &str) -> Result<Vec<EvidenceRecord>> {
        self.authorize_task_id(task_id).await?;

        let task_id = task_id.to_owned();
        let principal = self.policy.principal_id.clone();
        let rows=self.db.read(ReadLane::Interactive,move|c|{let mut s=c.prepare("SELECT e.metadata,e.stale FROM evidence e WHERE json_extract(e.metadata,'$.task_id')=?1 AND e.principal_id IS ?2 ORDER BY e.created_at,e.rowid")?;Ok(s.query_map(rusqlite::params![task_id,principal],|r|Ok((r.get::<_,String>(0)?,r.get::<_,bool>(1)?)))?.collect::<std::result::Result<Vec<_>,_>>()?)}).await?;
        rows.into_iter()
            .map(|(s, stale)| {
                let mut e: EvidenceRecord = serde_json::from_str(&s)?;
                e.stale = stale;
                Ok(e)
            })
            .collect()
    }
    pub(crate) async fn store_evidence(
        &self,
        mut record: EvidenceRecord,
        output: Value,
    ) -> Result<EvidenceRecord> {
        let output = self.redactor.redact_json(&output);
        let bytes = serde_json::to_vec(&output)?;
        let prepared = self.db.blobs().prepare(&bytes)?;
        record.output_blob = Some(prepared.hash.as_str().to_owned());
        record.explanation = self.redactor.redact_text(&record.explanation);
        let metadata = serde_json::to_value(&record)?;
        if self.redactor.redact_json(&metadata) != metadata {
            return Err(fail("evidence metadata contains protected material"));
        }
        let saved = record.clone();
        let principal = self.policy.principal_id.clone();
        let row = record.clone();
        self.db.write_with_blob(prepared,Priority::Journal,move|c,hash|{
            let scope:String=c.query_row("SELECT scope_id FROM tasks WHERE id=?1",[&row.task_id],|r|r.get(0))?;
            let result=serde_json::to_value(row.result).unwrap().as_str().unwrap().to_owned();let kind=serde_json::to_value(row.kind).unwrap().as_str().unwrap().to_owned();
            c.execute("INSERT INTO evidence(id,check_id,run_id,result,output_blob,code_version,fingerprints,tool_call_id,principal_id,scope_id,instruction_revision,evidence_kind,stale,trusted,metadata) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,1,?14)",rusqlite::params![row.id,row.check_id,row.run_id,result,hash.as_str(),row.snapshot.commit,serde_json::to_string(&row.snapshot.fingerprints).unwrap(),row.tool_call_id,principal,scope,row.snapshot.instruction_revision,kind,row.stale,serde_json::to_string(&row).unwrap()])?;
            if let Some(call)=&row.tool_call_id{c.execute("UPDATE task_check_attempts SET evidence_id=?1,state='completed' WHERE run_id=?2 AND json_extract(prepared,'$.call.id')=?3 AND state='intent'",rusqlite::params![row.id,row.run_id,call])?;}
            crate::ops::edge(c,&scope,principal.as_deref(),"check",&row.check_id,"proves","evidence",&row.id).map_err(|e|excalibur_store::Error::Configuration(e.to_string()))?;
            crate::ops::edge(c,&scope,principal.as_deref(),"evidence",&row.id,"produced_by","run",&row.run_id).map_err(|e|excalibur_store::Error::Configuration(e.to_string()))?;
            for p in &row.snapshot.impact_patterns{c.execute("INSERT OR IGNORE INTO task_impact_index(criterion_id,pattern) VALUES(?1,?2)",rusqlite::params![row.criterion_id,p])?;}Ok(())}).await?;
        Ok(saved)
    }
    pub async fn invalidate_paths(&self, scope: &str, paths: &[String]) -> Result<Vec<String>> {
        self.check_scope(scope)?;
        let _guard = self.mutation_lock.write().await;
        self.invalidate_paths_locked(scope, paths).await
    }
    pub async fn invalidate_paths_locked(
        &self,
        scope: &str,
        paths: &[String],
    ) -> Result<Vec<String>> {
        self.check_scope(scope)?;
        let scope = scope.to_owned();
        let mut paths = paths.to_vec();
        let (query_scope, principal) = (scope.clone(), self.policy.principal_id.clone());
        let roots=self.db.read(ReadLane::Interactive,move|c|{let mut s=c.prepare("SELECT DISTINCT json_extract(metadata,'$.workspace') FROM tasks WHERE scope_id=?1 AND principal_id IS ?2 AND json_type(metadata,'$.workspace')='text'")?;Ok(s.query_map(rusqlite::params![query_scope,principal],|r|r.get::<_,String>(0))?.collect::<std::result::Result<Vec<_>,_>>()?)}).await?;
        for path in paths.clone() {
            let p = Path::new(&path);
            if p.is_absolute() {
                for root in &roots {
                    if let Ok(relative) = p.strip_prefix(root) {
                        paths.push(relative.to_string_lossy().replace('\\', "/"));
                    }
                }
            }
        }
        let mut prefixes = vec![String::new()];
        for path in &paths {
            let mut at = String::new();
            for part in path.split('/') {
                if !at.is_empty() {
                    at.push('/')
                }
                at.push_str(part);
                prefixes.push(at.clone());
            }
        }
        prefixes.sort();
        prefixes.dedup();
        let prefixes = serde_json::to_string(&prefixes)?;
        let principal = self.policy.principal_id.clone();
        Ok(self.db.write(Priority::Journal,move|c|{
            let mut s=c.prepare("SELECT DISTINCT cr.id,cr.impact_area FROM task_impact_roots ir JOIN criteria cr ON cr.id=ir.criterion_id WHERE ir.scope_id=?1 AND cr.principal_id IS ?2 AND ir.prefix IN (SELECT value FROM json_each(?3))")?;
            let rows=s.query_map(rusqlite::params![scope,principal,prefixes],|r|Ok((r.get::<_,String>(0)?,r.get::<_,Option<String>>(1)?)))?.collect::<std::result::Result<Vec<_>,_>>()?;let mut changed=Vec::new();
            for(id,p)in rows{let p:Vec<String>=p.and_then(|s|serde_json::from_str(&s).ok()).unwrap_or_default();if paths.iter().any(|path|matches(&p,path)){c.execute("UPDATE evidence SET stale=1 WHERE check_id IN(SELECT id FROM checks WHERE criterion_id=?1)",[&id])?;c.execute("INSERT INTO task_criterion_state(criterion_id,state,reason) VALUES(?1,'unverified','affected file changed') ON CONFLICT(criterion_id) DO UPDATE SET state='unverified',reason='affected file changed'",[&id])?;changed.push(id);}}Ok(changed)}).await?)
    }
    pub async fn invalidate_instruction_revision(
        &self,
        scope: &str,
        task_id: &str,
        revision: u64,
    ) -> Result<()> {
        self.task(scope, task_id).await?;
        let _guard = self.mutation_lock.write().await;
        let task = task_id.to_owned();
        self.db.write(Priority::Journal,move|c|{
            c.execute("UPDATE criteria SET instruction_revision=?1 WHERE requirement_id IN(SELECT requirement_id FROM task_requirements WHERE task_id=?2)",rusqlite::params![revision,task])?;
            c.execute("UPDATE evidence SET stale=1 WHERE instruction_revision<>?1 AND check_id IN(SELECT ck.id FROM checks ck JOIN criteria cr ON cr.id=ck.criterion_id JOIN task_requirements tr ON tr.requirement_id=cr.requirement_id WHERE tr.task_id=?2)",rusqlite::params![revision,task])?;Ok(())}).await?;
        Ok(())
    }
    pub async fn assess_criterion(
        &self,
        criterion: &CriterionBrief,
        revision: u64,
    ) -> Result<CriterionAssessment> {
        self.authorize_criterion_id(&criterion.id).await?;

        let exempt_id = criterion.id.clone();
        let exemption=self.db.read(ReadLane::Interactive,move|c|Ok(c.query_row("SELECT e.justification FROM task_criterion_exemptions e JOIN criteria cr ON cr.id=e.criterion_id WHERE e.criterion_id=?1 AND e.criterion_version=cr.version AND e.instruction_revision=cr.instruction_revision AND e.instruction_revision=?2",rusqlite::params![exempt_id,revision],|r|r.get::<_,String>(0)).optional()?)).await?;
        if let Some(reason) = exemption.filter(|r| !r.trim().is_empty()) {
            return Ok(CriterionAssessment {
                criterion_id: criterion.id.clone(),
                state: CriterionState::NotApplicable,
                reason,
                evidence: None,
            });
        }
        let id = criterion.id.clone();
        let rows=self.db.read(ReadLane::Interactive,move|c|{let mut s=c.prepare("SELECT e.metadata,e.stale FROM evidence e JOIN checks ck ON ck.id=e.check_id WHERE ck.criterion_id=?1 ORDER BY e.created_at DESC,e.rowid DESC")?;Ok(s.query_map([id],|r|Ok((r.get::<_,String>(0)?,r.get::<_,bool>(1)?)))?.collect::<std::result::Result<Vec<_>,_>>()?)}).await?;
        let mut latest = BTreeMap::new();
        for (text, stale) in rows {
            let Ok(mut e) = serde_json::from_str::<EvidenceRecord>(&text) else {
                continue;
            };
            e.stale = stale;
            latest.entry(e.check_id.clone()).or_insert(e);
        }
        let required = if criterion.checks.is_empty() {
            vec![format!("inspection:{}", criterion.id)]
        } else {
            criterion.checks.iter().map(|c| c.id.clone()).collect()
        };
        let mut last = None;
        for check in required {
            let Some(e) = latest.get(&check) else {
                return Ok(CriterionAssessment {
                    criterion_id: criterion.id.clone(),
                    state: CriterionState::Unverified,
                    reason: format!("no evidence for {check}"),
                    evidence: last,
                });
            };
            if !e.trusted || e.stale || !e.snapshot.fresh(revision).await? {
                return Ok(CriterionAssessment {
                    criterion_id: criterion.id.clone(),
                    state: CriterionState::Unverified,
                    reason: "evidence is stale or untrusted".into(),
                    evidence: Some(e.clone()),
                });
            }
            if matches!(
                criterion.method,
                CheckMethod::Command | CheckMethod::Test | CheckMethod::Metric
            ) && e.kind != EvidenceKind::Execution
            {
                return Ok(CriterionAssessment {
                    criterion_id: criterion.id.clone(),
                    state: CriterionState::Unverified,
                    reason: "executable criterion requires execution evidence".into(),
                    evidence: Some(e.clone()),
                });
            }
            if e.result != EvidenceResult::Pass {
                return Ok(CriterionAssessment {
                    criterion_id: criterion.id.clone(),
                    state: CriterionState::Failed,
                    reason: e.explanation.clone(),
                    evidence: Some(e.clone()),
                });
            }
            last = Some(e.clone());
        }
        Ok(CriterionAssessment {
            criterion_id: criterion.id.clone(),
            state: CriterionState::Satisfied,
            reason: if criterion.method == CheckMethod::Inspection {
                "verified by judgment with fresh inspection evidence"
            } else {
                "all declared checks have fresh passing evidence"
            }
            .into(),
            evidence: last,
        })
    }
    /// Host-only inspection/confirmation producer. Code checks cannot use judgments.
    #[allow(clippy::too_many_arguments)]
    pub async fn record_judgment(
        &self,
        task: &str,
        criterion: &str,
        run: &str,
        snapshot: WorkspaceSnapshot,
        passed: bool,
        explanation: &str,
        confirmation: bool,
    ) -> Result<EvidenceRecord> {
        let _lease = self.mutation_lock.write().await;
        let task_record = self
            .task_for_run(run)
            .await?
            .ok_or_else(|| fail("judgment run is not task bound"))?;
        if task_record.id != task {
            return Err(fail("judgment task mismatch"));
        }
        let (_, criteria) = self.requirements_and_criteria(task).await?;
        let cr = criteria
            .iter()
            .find(|c| c.id == criterion)
            .ok_or_else(|| fail("criterion is not in task"))?;
        if cr.method
            != if confirmation {
                CheckMethod::UserConfirmation
            } else {
                CheckMethod::Inspection
            }
        {
            return Err(fail("judgment cannot fulfill executable checks"));
        }
        let check = format!("inspection:{criterion}");
        let cid = criterion.to_owned();
        let ck = check.clone();
        self.db.write(Priority::Journal,move|c|{c.execute("INSERT OR IGNORE INTO checks(id,criterion_id,definition,principal_id,scope_id) SELECT ?1,id,'{\"kind\":\"inspection\"}',principal_id,scope_id FROM criteria WHERE id=?2",rusqlite::params![ck,cid])?;Ok(())}).await?;
        self.store_evidence(EvidenceRecord{id:new_id("evidence"),task_id:task.into(),criterion_id:criterion.into(),check_id:check,run_id:run.into(),result:if passed{EvidenceResult::Pass}else{EvidenceResult::Fail},kind:if confirmation{EvidenceKind::UserConfirmation}else{EvidenceKind::Judgment},output_blob:None,recorded_at:excalibur_store::unix_seconds().max(0)as u64,snapshot,tool_call_id:None,trusted:true,stale:false,explanation:explanation.into(),producer_snapshot:json!({"kind":if confirmation{"user_confirmation"}else{"judgment"}}),tool_calls:0},json!({"passed":passed,"explanation":explanation,"kind":if confirmation{"user_confirmation"}else{"inspection"}})).await
    }
}
impl TaskStore {
    /// A host decision, versioned against the accepted criterion. Model CRUD cannot
    /// create exemptions; subsequent requirement/revision changes revoke it.
    pub async fn mark_not_applicable(
        &self,
        scope: &str,
        criterion: &str,
        version: u64,
        justification: &str,
        actor: &str,
    ) -> Result<()> {
        self.check_scope(scope)?;
        if justification.trim().is_empty() || actor.trim().is_empty() {
            return Err(fail(
                "not-applicable requires an actor and durable justification",
            ));
        }
        let _lease = self.mutation_lock.write().await;
        let (scope, id, reason, actor, principal) = (
            scope.to_owned(),
            criterion.to_owned(),
            self.redactor.redact_text(justification),
            self.redactor.redact_text(actor),
            self.policy.principal_id.clone(),
        );
        self.db.write(Priority::Journal,move|c|{let current:Option<(u64,u64)>=c.query_row("SELECT version,instruction_revision FROM criteria WHERE id=?1 AND scope_id=?2 AND principal_id IS ?3",rusqlite::params![id,scope,principal],|r|Ok((r.get(0)?,r.get(1)?))).optional()?;let Some((current,revision))=current else{return Err(excalibur_store::Error::Invalid("criterion not in authorized scope"))};if current!=version{return Err(excalibur_store::Error::Invalid("criterion version changed"))}c.execute("INSERT INTO task_criterion_exemptions VALUES(?1,?2,?3,?4,?5,unixepoch()) ON CONFLICT(criterion_id) DO UPDATE SET criterion_version=excluded.criterion_version,instruction_revision=excluded.instruction_revision,justification=excluded.justification,actor=excluded.actor,recorded_at=excluded.recorded_at",rusqlite::params![id,version,revision,reason,actor])?;Ok(())}).await?;
        Ok(())
    }
    pub async fn refresh_criteria(&self, scope: &str) -> Result<()> {
        let _lease = self.mutation_lock.write().await;
        self.refresh_criteria_locked(scope).await
    }
    pub async fn refresh_criteria_locked(&self, scope: &str) -> Result<()> {
        for (a, revision) in self.compute_criteria_scope_locked(scope).await? {
            self.persist_criterion_assessment(a, revision).await?;
        }
        Ok(())
    }
    pub async fn compute_criteria_scope_locked(
        &self,
        scope: &str,
    ) -> Result<Vec<(CriterionAssessment, u64)>> {
        self.check_scope(scope)?;
        let scope = scope.to_owned();
        let principal = self.policy.principal_id.clone();
        let ids=self.db.read(ReadLane::Interactive,move|c|{let mut s=c.prepare("SELECT DISTINCT t.id FROM tasks t JOIN task_requirements tr ON tr.task_id=t.id WHERE t.scope_id=?1 AND t.principal_id IS ?2")?;Ok(s.query_map(rusqlite::params![scope,principal],|r|r.get::<_,String>(0))?.collect::<std::result::Result<Vec<_>,_>>()?)}).await?;
        let mut updates = BTreeMap::new();
        for id in ids {
            let (_, criteria) = self.requirements_and_criteria(&id).await?;
            for criterion in criteria {
                let revision = self.criterion_revision(&criterion.id).await?;
                let a = self.assess_criterion(&criterion, revision).await?;
                updates.insert(criterion.id, (a, revision));
            }
        }
        Ok(updates.into_values().collect())
    }
    pub(crate) async fn persist_criterion_assessment(
        &self,
        a: CriterionAssessment,
        revision: u64,
    ) -> Result<()> {
        self.db.write(Priority::Normal,move|c|{let state=serde_json::to_value(a.state).unwrap().as_str().unwrap().to_owned();c.execute("INSERT INTO task_criterion_state(criterion_id,state,reason,latest_evidence_id,instruction_revision) VALUES(?1,?2,?3,?4,?5) ON CONFLICT(criterion_id) DO UPDATE SET state=excluded.state,reason=excluded.reason,latest_evidence_id=excluded.latest_evidence_id,instruction_revision=excluded.instruction_revision",rusqlite::params![a.criterion_id,state,a.reason,a.evidence.map(|e|e.id),revision])?;Ok(())}).await?;
        Ok(())
    }
}
impl TaskStore {
    /// Registered project checks are independent of the current task's acceptance
    /// requirements. They are read and executed under the current host workspace.
    pub async fn project_check_catalog(&self, task_id: &str) -> Result<Vec<CriterionBrief>> {
        let scope = self.authorize_task_id(task_id).await?;
        let task = self.task(&scope, task_id).await?;
        let Some(project) = task.project_id else {
            return Ok(vec![]);
        };
        let principal = self.policy.principal_id.clone();
        Ok(self.db.read(ReadLane::Interactive,move|c|{
            let mut s=c.prepare("SELECT DISTINCT cr.id,cr.requirement_id,cr.statement,cr.method,cr.expected,cr.impact_area FROM criteria cr JOIN requirements r ON r.id=cr.requirement_id WHERE cr.scope_id=?1 AND cr.principal_id IS ?2 AND(r.project_id=?3 OR EXISTS(SELECT 1 FROM task_requirements tr JOIN tasks t ON t.id=tr.task_id WHERE tr.requirement_id=r.id AND t.project_id=?3 AND t.scope_id=?1 AND t.principal_id IS ?2)) AND EXISTS(SELECT 1 FROM checks ck WHERE ck.criterion_id=cr.id AND json_extract(ck.definition,'$.kind')<>'inspection') ORDER BY cr.id")?;
            let rows=s.query_map(rusqlite::params![scope,principal,project],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,String>(2)?,r.get::<_,String>(3)?,r.get::<_,Option<String>>(4)?,r.get::<_,Option<String>>(5)?)))?.collect::<std::result::Result<Vec<_>,_>>()?;
            let mut result=Vec::new();for(id,requirement_id,statement,method,expected,impact)in rows{let mut s=c.prepare("SELECT id,definition,environment FROM checks WHERE criterion_id=?1 AND json_extract(definition,'$.kind')<>'inspection' ORDER BY id")?;let checks=s.query_map([&id],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,Option<String>>(2)?)))?.collect::<std::result::Result<Vec<_>,_>>()?.into_iter().map(|(id,definition,environment)|Ok(crate::verification::CheckBrief{id,definition:serde_json::from_str(&definition).map_err(|_|excalibur_store::Error::Invalid("unsupported project check definition"))?,environment})).collect::<excalibur_store::Result<Vec<_>>>()?;
                result.push(CriterionBrief{id,requirement_id,statement,method:serde_json::from_value(json!(method)).map_err(|_|excalibur_store::Error::Invalid("invalid project criterion method"))?,expected:expected.and_then(|s|serde_json::from_str(&s).ok()).unwrap_or(Value::Null),impact_area:impact.and_then(|s|serde_json::from_str(&s).ok()).unwrap_or_else(||vec!["**".into()]),checks});}Ok(result)
        }).await?)
    }
}
