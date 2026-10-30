//! Reusable local tools. Hosts supply policy and optional formatting/diagnostic integrations.
pub mod deferred;
pub mod entries;
pub mod evaluation;
mod file_authority;
mod files;
pub mod intelligence;
pub mod monitor;
mod patch;
mod path_scope;
pub mod project;
mod redaction;
pub mod terminal;
use excalibur_store::{Database, Priority};
use excalibur_types::runtime::{
    BoxFuture, Effect, FilesystemConfinement, PreparedTool, Reconcile, Resource, ToolCall,
    ToolDefinition, ToolResult, ToolRuntime, ToolScheduling,
};
use files::{FileEdit, hash, required_str};
use serde_json::{Value, json};
use std::{
    collections::{BTreeSet, HashMap, HashSet},
    path::{Path, PathBuf},
    sync::{Arc, Mutex, OnceLock, Weak},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Access {
    Allow,
    Ask,
    Deny,
}
#[derive(Clone, Copy)]
pub(crate) enum Permission {
    Read,
    Write,
    Exec,
}
/// Formatter must return transformed bytes without touching the requested file.
/// after_edit performs configured diagnostics and evidence invalidation, returning its real report.
pub trait PostEditHooks: Send + Sync {
    fn format<'a>(
        &'a self,
        path: &'a Path,
        content: Vec<u8>,
    ) -> BoxFuture<'a, Result<Vec<u8>, String>>;
    fn after_edit<'a>(&'a self, path: &'a Path) -> BoxFuture<'a, Result<Value, String>>;
}
#[derive(Clone)]
pub struct ToolPolicy {
    pub redactor: excalibur_vault::Redactor,
    /// Trusted host process boundary override. Data/vault exclusions are always added.
    pub sandbox: Option<terminal::SandboxConfig>,
    /// Canonical host-owned vault/evaluation paths excluded from all file tools.
    pub protected_paths: Vec<PathBuf>,
    pub read: Access,
    pub write: Access,
    pub exec: Access,
    pub output_budget_bytes: usize,
    /// Exact command strings declared safely repeatable by the host/project.
    pub repeatable_commands: BTreeSet<String>,
    pub hooks: Option<Arc<dyn PostEditHooks>>,
}
impl Default for ToolPolicy {
    fn default() -> Self {
        Self {
            redactor: excalibur_vault::Redactor::default(),
            sandbox: None,
            protected_paths: Vec::new(),
            read: Access::Allow,
            write: Access::Ask,
            exec: Access::Deny,
            output_budget_bytes: 50 * 1024,
            repeatable_commands: BTreeSet::new(),
            hooks: None,
        }
    }
}
#[derive(Clone)]
pub struct LocalTools {
    db: Database,
    root: PathBuf,
    directory: Arc<cap_std::fs::Dir>,
    path_scope: Option<Arc<path_scope::PathScope>>,
    policy: ToolPolicy,
    terminal: Arc<terminal::TerminalManager>,
    workspace: excalibur_workspace::Workspace,
    active_runs: Arc<Mutex<HashSet<String>>>,
    steps: Arc<Mutex<HashMap<String, u64>>>,
    exec_runs: Arc<Mutex<HashSet<String>>>,
    exec_open_steps: Arc<Mutex<HashSet<String>>>,
    shadow_warnings: Arc<Mutex<HashMap<String, Vec<String>>>>,
    parents: Arc<Mutex<HashMap<String, project::ParentAuthority>>>,
    child_authorities: Arc<Mutex<HashMap<String, project::ChildAuthority>>>,
    intelligence: Option<Arc<intelligence::ProjectIntelligence>>,
}
const SCHEMA: &str = "CREATE TABLE IF NOT EXISTS tool_readset(run_id TEXT NOT NULL,path TEXT NOT NULL,fp TEXT NOT NULL,blob TEXT NOT NULL REFERENCES blobs(hash),mismatches INTEGER NOT NULL DEFAULT 0,PRIMARY KEY(run_id,path)); CREATE TABLE IF NOT EXISTS tool_changed_files(path TEXT NOT NULL,run_id TEXT NOT NULL,call_id TEXT NOT NULL,created_at INTEGER NOT NULL DEFAULT(unixepoch()),PRIMARY KEY(path,run_id,call_id));";
// Additive upgrade: absence conflicts need counters even without a prior readset row.
const SCHEMA_2: &str = "CREATE TABLE IF NOT EXISTS tool_conflicts(run_id TEXT NOT NULL,path TEXT NOT NULL,mismatches INTEGER NOT NULL DEFAULT 0 CHECK(mismatches>=0),PRIMARY KEY(run_id,path)); INSERT OR IGNORE INTO tool_conflicts(run_id,path,mismatches) SELECT run_id,path,mismatches FROM tool_readset WHERE mismatches>0;";
impl LocalTools {
    pub async fn new(db: Database, root: PathBuf, policy: ToolPolicy) -> Result<Self, String> {
        Self::new_inner(db, root, policy, false).await
    }
    /// Trusted host adaptation for one durable, registered child worktree.
    /// Only automatic enclosing data-directory hides are narrowed; explicit
    /// host denials and all database/blob paths remain protected.
    pub async fn for_registered_worktree(
        db: Database,
        source: excalibur_workspace::Workspace,
        task: &str,
        owner: &str,
        mut policy: ToolPolicy,
    ) -> Result<Self, String> {
        if db.path() != source.database_path() {
            return Err("worktree and tools must share principal storage".into());
        }
        let tree = source
            .git()
            .worktree(task, owner)
            .await
            .map_err(|e| e.to_string())?;
        let base = source
            .managed_worktree_root()
            .canonicalize()
            .map_err(|e| e.to_string())?;
        let root = tree.path.canonicalize().map_err(|e| e.to_string())?;
        if root != tree.path || root == base || !root.starts_with(&base) || root == source.root() {
            return Err("registered worktree escaped its canonical host base".into());
        }
        if let Some(sandbox) = &mut policy.sandbox {
            sandbox.workspace = root.clone();
            sandbox.read_roots = vec![root.clone()];
            if !sandbox.write_roots.is_empty() {
                sandbox.write_roots = vec![root.clone()];
            }
        }
        Self::new_inner(db, root, policy, true).await
    }
    async fn new_inner(
        db: Database,
        root: PathBuf,
        mut policy: ToolPolicy,
        registered_worktree: bool,
    ) -> Result<Self, String> {
        if policy.output_budget_bytes < 256 || policy.output_budget_bytes > 1024 * 1024 {
            return Err("output budget must be 256..=1048576 bytes".into());
        }
        if policy
            .protected_paths
            .iter()
            .any(|p| !p.is_absolute() || p.exists() && p.canonicalize().ok().as_ref() != Some(p))
        {
            return Err("protected paths must be canonical absolute paths".into());
        }
        let explicit_protected = policy.protected_paths.clone();
        let (root, directory) = files::open_root(root)?;
        let data_parent = db.path().parent().ok_or("database parent missing")?;
        if data_parent.starts_with(&root) && data_parent != root {
            policy.protected_paths.push(data_parent.to_path_buf());
        } else {
            policy.protected_paths.extend([
                db.path().to_path_buf(),
                db.path().with_file_name(format!(
                    "{}-wal",
                    db.path().file_name().unwrap().to_string_lossy()
                )),
                db.path().with_file_name(format!(
                    "{}-shm",
                    db.path().file_name().unwrap().to_string_lossy()
                )),
                data_parent.join("blobs"),
                data_parent.join("backups"),
                data_parent.join("tool-spool"),
                data_parent.join("workspaces"),
            ]);
        }
        if registered_worktree {
            let automatic_workspaces = data_parent.join("workspaces");
            // Removing an automatic ancestor permits only the selected cap-dir
            // root, not any sibling worktree or database directory.
            if root.starts_with(&automatic_workspaces) {
                policy
                    .protected_paths
                    .retain(|p| p != &automatic_workspaces || explicit_protected.contains(p));
            }
        }
        policy.protected_paths.push(db.blobs().root().to_path_buf());
        policy.protected_paths.sort();
        policy.protected_paths.dedup();
        let checksum = hash(SCHEMA.as_bytes());
        db.write(Priority::Normal,move|conn|{
            use excalibur_store::rusqlite::OptionalExtension;
            conn.execute_batch("CREATE TABLE IF NOT EXISTS tool_schema(version INTEGER PRIMARY KEY,checksum TEXT NOT NULL)")?;
            let old=conn.query_row("SELECT checksum FROM tool_schema WHERE version=1",[],|r|r.get::<_,String>(0)).optional()?;
            if let Some(old)=old {if old!=checksum{return Err(excalibur_store::Error::Invalid("tool schema checksum mismatch"));}}
            else{conn.execute_batch(SCHEMA)?;conn.execute("INSERT INTO tool_schema VALUES(1,?1)",[checksum])?;}
            let checksum_2=hash(SCHEMA_2.as_bytes());
            let old_2=conn.query_row("SELECT checksum FROM tool_schema WHERE version=2",[],|r|r.get::<_,String>(0)).optional()?;
            if let Some(old)=old_2 {if old!=checksum_2{return Err(excalibur_store::Error::Invalid("tool schema checksum mismatch"));}}
            else {conn.execute_batch(SCHEMA_2)?;conn.execute("INSERT INTO tool_schema VALUES(2,?1)",[checksum_2])?;}
            Ok(())
        }).await.map_err(|e|e.to_string())?;
        let mut sandbox = policy
            .sandbox
            .clone()
            .unwrap_or_else(|| terminal::SandboxConfig::project(root.clone()));
        if sandbox
            .workspace
            .canonicalize()
            .map_err(|e| e.to_string())?
            != root
        {
            return Err(
                "host sandbox working directory must equal the configured project root".into(),
            );
        }
        sandbox.hidden_paths.extend(policy.protected_paths.clone());
        if !registered_worktree {
            sandbox
                .hidden_paths
                .push(db.path().parent().unwrap().to_path_buf());
        }
        sandbox.hidden_paths.extend([
            db.path().to_path_buf(),
            db.path().with_file_name(format!(
                "{}-wal",
                db.path().file_name().unwrap().to_string_lossy()
            )),
            db.path().with_file_name(format!(
                "{}-shm",
                db.path().file_name().unwrap().to_string_lossy()
            )),
            db.path().parent().unwrap().join("blobs"),
            db.path().parent().unwrap().join("backups"),
        ]);
        let terminal = terminal::TerminalManager::new(terminal::TerminalConfig {
            spool_dir: db
                .path()
                .parent()
                .ok_or("database parent missing")?
                .join("tool-spool"),
            shell: None,
            redacted_secrets: Vec::new(),
            max_per_run: 16,
            max_global: 64,
            protected_recent: 8,
            sandbox: Some(sandbox),
            redactor: policy.redactor.clone(),
        })?;
        Self::project_receipt_schema(&db).await?;
        let workspace = excalibur_workspace::Workspace::open(
            db.clone(),
            root.clone(),
            db.path()
                .parent()
                .ok_or("database parent missing")?
                .join("workspaces")
                .join(hash(root.to_string_lossy().as_bytes())),
            policy.redactor.clone(),
        )
        .await
        .map_err(|e| e.to_string())?;
        Ok(Self {
            workspace,
            parents: Arc::new(Mutex::new(HashMap::new())),
            child_authorities: Arc::new(Mutex::new(HashMap::new())),
            intelligence: None,
            active_runs: Arc::new(Mutex::new(HashSet::new())),
            steps: Arc::new(Mutex::new(HashMap::new())),
            exec_runs: Arc::new(Mutex::new(HashSet::new())),
            exec_open_steps: Arc::new(Mutex::new(HashSet::new())),
            shadow_warnings: Arc::new(Mutex::new(HashMap::new())),
            db,
            root,
            directory: Arc::new(directory),
            path_scope: None,
            policy,
            terminal: Arc::new(terminal),
        })
    }
    fn push_shadow_warnings(&self, run: &str, warnings: Vec<String>) {
        if warnings.is_empty() {
            return;
        }
        if let Ok(mut buffered) = self.shadow_warnings.lock() {
            buffered
                .entry(run.into())
                .or_default()
                .extend(warnings.into_iter().take(8));
        }
    }
    fn take_shadow_warnings(&self, run: &str) -> Vec<String> {
        self.shadow_warnings
            .lock()
            .ok()
            .and_then(|mut buffered| buffered.remove(run))
            .unwrap_or_default()
    }
    /// True while any run has an open shadow-tracked exec step. The project
    /// monitor treats changes observed in that window as agent-made instead
    /// of reporting its own `exec` output as an outside change.
    pub(crate) fn has_open_exec_step(&self) -> bool {
        self.exec_open_steps
            .lock()
            .map(|open| !open.is_empty())
            .unwrap_or(false)
    }
    fn mark_exec_step_open(&self, run: &str) {
        if let Ok(mut open) = self.exec_open_steps.lock() {
            open.insert(run.into());
        }
    }
    fn mark_exec_step_closed(&self, run: &str) {
        if let Ok(mut open) = self.exec_open_steps.lock() {
            open.remove(run);
        }
    }
    /// Complete in-memory metadata; resources themselves are resolved by prepare.
    pub fn catalog(&self) -> Vec<Value> {
        let mut catalog: Vec<Value> = self.definitions().into_iter().map(|definition| {
            let write=matches!(definition.name.as_str(),"write_file"|"edit_file"|"apply_patch");
            let exec=matches!(definition.name.as_str(),"exec"|"exec_io");
            json!({"name":definition.name,"description":definition.description,"input_schema":definition.input_schema,
                "output_schema":{"$schema":"https://json-schema.org/draft/2020-12/schema","type":"object"},
                "effect":if exec{"exec"}else if write{"write_local"}else{"read_only"},
                "idempotency":if exec{Effect::NonIdempotent}else if write{Effect::Idempotent}else{Effect::Pure},
                "resources":"resolved canonical paths or owned terminal session in prepare",
                "risk":if exec{"high"}else if write{"medium"}else{"low"},
                "timeout_ms":if exec{300000}else if write{120000}else{30000},"output_budget_bytes":self.policy.output_budget_bytes,
                "platforms":"available when the embedding host supplies the corresponding filesystem/process capability",
                "inverse":null,"probe":if write{Some("fingerprint_pre_post")}else{None},
                "dry_run":"prepare performs validation and durable preimages without requested external effects",
                "fast":false,"cancel":if exec{"graceful"}else if write{"after_completion"}else{"immediate"}})
        }).collect();
        catalog.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
        catalog
    }
    /// Inject an explicitly configured terminal adapter, including redaction and shell policy.
    pub fn with_terminal(mut self, manager: Arc<terminal::TerminalManager>) -> Self {
        self.terminal = manager;
        self
    }
    pub fn terminals(&self) -> Arc<terminal::TerminalManager> {
        self.terminal.clone()
    }
    pub(crate) fn check_permission(&self, permission: Permission) -> Result<(), String> {
        match match permission {
            Permission::Read => self.policy.read,
            Permission::Write => self.policy.write,
            Permission::Exec => self.policy.exec,
        } {
            Access::Allow => Ok(()),
            Access::Ask => Err(
                "permission_required: host approval is required for this exact operation".into(),
            ),
            Access::Deny => Err("permission_denied: operation is disabled by host policy".into()),
        }
    }
    fn schema(name: &str) -> Option<Value> {
        if let Some(definition) = project::definitions().into_iter().find(|d| d.name == name) {
            return Some(definition.input_schema);
        }
        if matches!(name, "exec" | "exec_io") {
            let mut schema = terminal::TerminalManager::definitions()
                .into_iter()
                .find(|d| d.name == name)?
                .input_schema;
            schema["$schema"] = json!("https://json-schema.org/draft/2020-12/schema");
            return Some(schema);
        }
        let string = json!({"type":"string"});
        let path = json!({"type":"string","minLength":1});
        let (properties, required) = match name {
            "read_file" => (
                json!({"path":path,"start_line":{"type":"integer","minimum":1},"line_count":{"type":"integer","minimum":1,"maximum":2000}}),
                vec!["path"],
            ),
            "write_file" => (
                json!({"path":path,"content":string,"fp":string}),
                vec!["path", "content"],
            ),
            "edit_file" => (
                json!({"path":path,"old_text":{"type":"string","minLength":1},"new_text":string,"fp":string}),
                vec!["path", "old_text", "new_text"],
            ),
            "apply_patch" => (
                json!({"patch":{"type":"string","minLength":1}}),
                vec!["patch"],
            ),
            "grep" => (
                json!({"pattern":{"type":"string","minLength":1},"glob":string,"limit":{"type":"integer","minimum":1,"maximum":100}}),
                vec!["pattern"],
            ),
            "glob" => (
                json!({"pattern":{"type":"string","minLength":1},"limit":{"type":"integer","minimum":1,"maximum":1000}}),
                vec!["pattern"],
            ),
            _ => return None,
        };
        Some(
            json!({"$schema":"https://json-schema.org/draft/2020-12/schema","type":"object","properties":properties,"required":required,"additionalProperties":false}),
        )
    }
    fn arguments(call: &ToolCall) -> Result<Value, String> {
        if call.arguments.len() > 4 * 1024 * 1024 {
            return Err("arguments exceed 4 MiB limit".into());
        }
        let args: Value = serde_json::from_str(&call.arguments)
            .or_else(|_| json5::from_str(&call.arguments))
            .map_err(|e| format!("arguments: invalid JSON: {e}"))?;
        let schema =
            Self::schema(&call.name).ok_or_else(|| format!("unknown tool: {}", call.name))?;
        let validator = jsonschema::validator_for(&schema).map_err(|e| e.to_string())?;
        if let Err(error) = validator.validate(&args) {
            return Err(format!("{}: {}", error.instance_path(), error));
        }
        Ok(args)
    }
    /// Discover only scheduling resources. No file bytes, fingerprints or durable
    /// preparation data are read or written here.
    fn scheduling_resources(
        &self,
        run: &str,
        call: &ToolCall,
    ) -> Result<(Vec<Resource>, bool), String> {
        let args = Self::arguments(call)?;
        let mut resources = Vec::new();
        let mut exclusive = false;
        let file_resource = |path: &str, write: bool| -> Result<Resource, String> {
            let path = self.resolve(path)?;
            Ok(Resource {
                key: self.root.join(path).to_string_lossy().into_owned(),
                write,
            })
        };
        match call.name.as_str() {
            "read_file" => resources.push(file_resource(required_str(&args, "path")?, false)?),
            "write_file" | "edit_file" => {
                resources.push(file_resource(required_str(&args, "path")?, true)?)
            }
            "apply_patch" => {
                for operation in patch::parse(required_str(&args, "patch")?)? {
                    match operation {
                        patch::Operation::Add { path, .. } | patch::Operation::Delete { path } => {
                            resources.push(file_resource(&path, true)?)
                        }
                        patch::Operation::Update { path, moved, .. } => {
                            resources.push(file_resource(&path, true)?);
                            if let Some(target) = moved {
                                resources.push(file_resource(&target, true)?);
                            }
                        }
                    }
                }
            }
            "grep" | "glob" => {}
            "exec" => {
                let raw = args.get("cwd").and_then(Value::as_str).unwrap_or(".");
                let cwd = if raw == "." || Path::new(raw) == self.root {
                    self.root.clone()
                } else {
                    self.root.join(self.resolve(raw)?)
                };
                if !cwd.is_dir() {
                    return Err("cwd must be a directory".into());
                }
                resources.push(Resource {
                    key: self.root.to_string_lossy().into_owned(),
                    write: true,
                });
                exclusive = true;
            }
            "exec_io" => {
                let read_only =
                    matches!(args["action"].as_str(), Some("list" | "read" | "wait_for"));
                let session = args
                    .get("session_id")
                    .or_else(|| args.get("session"))
                    .and_then(Value::as_str)
                    .unwrap_or(run);
                resources.push(Resource {
                    key: format!("terminal:{session}"),
                    write: !read_only,
                });
                exclusive = !read_only;
            }
            _ => return Err("unknown tool".into()),
        }
        resources
            .sort_by(|left, right| left.key.cmp(&right.key).then(left.write.cmp(&right.write)));
        resources.dedup_by(|left, right| left.key == right.key && left.write == right.write);
        Ok((resources, exclusive))
    }
    async fn prepare_call(&self, run: &str, mut call: ToolCall) -> Result<PreparedTool, String> {
        let mut args = Self::arguments(&call)?;
        if project::DOMAIN_TOOLS.contains(&call.name.as_str()) {
            return self.prepare_domain(run, call, args).await;
        }
        let mut edits = Vec::new();
        let mut resources = Vec::new();
        let effect = match call.name.as_str() {
            "read_file" => {
                self.check_permission(Permission::Read)?;
                let path = self.resolve(required_str(&args, "path")?)?;
                args["path"] = json!(path);
                resources.push(Resource {
                    key: self.root.join(&path).to_string_lossy().into(),
                    write: false,
                });
                Effect::Pure
            }
            "glob" | "grep" => {
                self.check_permission(Permission::Read)?;
                Effect::Pure
            }
            "write_file" | "edit_file" => {
                self.check_permission(Permission::Write)?;
                let path = self.resolve(required_str(&args, "path")?)?;
                let mut bytes = if call.name == "edit_file" {
                    let old = self.content(&path)?.ok_or("file does not exist")?;
                    self.ensure_read(run, &path, &old, args.get("fp").and_then(Value::as_str))
                        .await?;
                    patch::replace(
                        std::str::from_utf8(&old).map_err(|_| "file is not UTF-8")?,
                        required_str(&args, "old_text")?,
                        required_str(&args, "new_text")?,
                    )?
                    .into_bytes()
                } else {
                    required_str(&args, "content")?.as_bytes().to_vec()
                };
                if let Some(hooks) = &self.policy.hooks {
                    bytes = hooks.format(&self.root.join(&path), bytes).await?;
                }
                edits.push(
                    self.prepare_edit(
                        run,
                        &call.id,
                        &path,
                        Some(bytes),
                        args.get("fp").and_then(Value::as_str),
                    )
                    .await?,
                );
                Effect::Idempotent
            }
            "apply_patch" => {
                self.check_permission(Permission::Write)?;
                for operation in patch::parse(required_str(&args, "patch")?)? {
                    match operation {
                        patch::Operation::Add { path, content } => {
                            let normalized = self.resolve(&path)?;
                            if self.content(&normalized)?.is_some() {
                                return Err("Add File cannot overwrite existing file".into());
                            }
                            let mut bytes = content.into_bytes();
                            if let Some(hooks) = &self.policy.hooks {
                                bytes = hooks.format(&self.root.join(&normalized), bytes).await?;
                            }
                            edits.push(
                                self.prepare_edit(run, &call.id, &path, Some(bytes), None)
                                    .await?,
                            );
                        }
                        patch::Operation::Delete { path } => {
                            let path = self.resolve(&path)?;
                            if self.content(&path)?.is_none() {
                                return Err("Delete File target missing".into());
                            }
                            edits.push(self.prepare_edit(run, &call.id, &path, None, None).await?);
                        }
                        patch::Operation::Update { path, moved, hunks } => {
                            let path = self.resolve(&path)?;
                            let bytes = self.content(&path)?.ok_or("Update File target missing")?;
                            self.ensure_read(run, &path, &bytes, None).await?;
                            let mut new = patch::apply_hunks(
                                std::str::from_utf8(&bytes).map_err(|_| "file is not UTF-8")?,
                                &hunks,
                            )?
                            .into_bytes();
                            let target = moved.as_deref().unwrap_or(&path);
                            let target = self.resolve(target)?;
                            if let Some(hooks) = &self.policy.hooks {
                                new = hooks.format(&self.root.join(&target), new).await?;
                            }
                            if moved.is_some() && target != path {
                                if self.content(&target)?.is_some() {
                                    return Err("Move target already exists".into());
                                }
                                edits.push(
                                    self.prepare_edit(run, &call.id, &path, None, None).await?,
                                );
                            }
                            edits.push(
                                self.prepare_edit(run, &call.id, &target, Some(new), None)
                                    .await?,
                            );
                        }
                    }
                }
                Effect::Idempotent
            }
            "exec" => {
                self.check_permission(Permission::Exec)?;
                let raw = args.get("cwd").and_then(Value::as_str).unwrap_or(".");
                let cwd = if raw == "." || Path::new(raw) == self.root {
                    self.root.clone()
                } else {
                    self.root.join(self.resolve(raw)?)
                };
                if !cwd.is_dir() {
                    return Err("cwd must be a directory".into());
                }
                args["cwd"] = json!(cwd);
                resources.push(Resource {
                    key: self.root.to_string_lossy().into(),
                    write: true,
                });
                if self
                    .policy
                    .repeatable_commands
                    .contains(required_str(&args, "cmd")?)
                {
                    Effect::Idempotent
                } else {
                    Effect::NonIdempotent
                }
            }
            "exec_io" => {
                self.check_permission(Permission::Exec)?;
                let read_only =
                    matches!(args["action"].as_str(), Some("list" | "read" | "wait_for"));
                resources.push(Resource {
                    key: format!(
                        "terminal:{}",
                        args.get("session_id")
                            .or_else(|| args.get("session"))
                            .and_then(Value::as_str)
                            .unwrap_or(run)
                    ),
                    write: !read_only,
                });
                if read_only {
                    Effect::Pure
                } else {
                    Effect::NonIdempotent
                }
            }

            _ => return Err("unknown tool".into()),
        };
        let mut paths = HashSet::new();
        for edit in &edits {
            if !paths.insert(edit.path.clone()) {
                return Err("patch touches a path more than once".into());
            }
            resources.push(Resource {
                key: self.root.join(&edit.path).to_string_lossy().into(),
                write: true,
            });
        }
        resources
            .sort_by(|left, right| left.key.cmp(&right.key).then(left.write.cmp(&right.write)));
        resources.dedup_by(|left, right| left.key == right.key && left.write == right.write);
        call.arguments = args.to_string();
        Ok(PreparedTool {
            exclusive: call.name == "exec" || (call.name == "exec_io" && effect != Effect::Pure),
            call,
            effect,
            resources,
            data: json!({"args":args,"diagnostics_before":self.diagnostics_before(&edits).await?,"edits":edits,"root":self.root,"version":1}),
        })
    }
    async fn execute_call(
        &self,
        run: &str,
        key: &str,
        tool: &PreparedTool,
    ) -> Result<Value, String> {
        let args = &tool.data["args"];
        if project::DOMAIN_TOOLS.contains(&tool.call.name.as_str()) {
            return if matches!(tool.call.name.as_str(), "worktree" | "integration_queue")
                && tool.effect != Effect::Pure
            {
                self.execute_project_operation(run, key, tool).await
            } else {
                self.execute_domain(run, key, tool).await
            };
        }
        match tool.call.name.as_str() {
            "read_file" => {
                self.check_permission(Permission::Read)?;
                let path = self.resolve(required_str(args, "path")?)?;
                let bytes = self.content(&path)?.ok_or("file does not exist")?;
                let text =
                    std::str::from_utf8(&bytes).map_err(|_| "file is binary or not UTF-8")?;
                let start = args.get("start_line").and_then(Value::as_u64).unwrap_or(1) as usize;
                let count = args
                    .get("line_count")
                    .and_then(Value::as_u64)
                    .unwrap_or(200) as usize;
                let lines = text
                    .lines()
                    .enumerate()
                    .skip(start - 1)
                    .take(count)
                    .map(|(i, line)| json!({"line":i+1,"text":line}))
                    .collect::<Vec<_>>();
                let total = text.lines().count();
                let fp = self.remember(run, &tool.call.id, &path, bytes).await?;
                Ok(
                    json!({"path":path,"fp":fp,"lines":lines,"total_lines":total,"more":total>=start+count}),
                )
            }
            "glob" | "grep" => {
                self.check_permission(Permission::Read)?;
                self.search(&tool.call.name, args)
            }
            "write_file" | "edit_file" | "apply_patch" => self.perform_edits(run, key, tool).await,
            _ => Err("not a file tool".into()),
        }
    }
    fn search(&self, name: &str, args: &Value) -> Result<Value, String> {
        let limit = args.get("limit").and_then(Value::as_u64).unwrap_or(100) as usize;
        let pattern = required_str(args, "pattern")?;
        let glob = globset::Glob::new(if name == "glob" {
            pattern
        } else {
            args.get("glob").and_then(Value::as_str).unwrap_or("**/*")
        })
        .map_err(|e| e.to_string())?
        .compile_matcher();
        let regex = if name == "grep" {
            Some(
                regex::RegexBuilder::new(pattern)
                    .size_limit(1024 * 1024)
                    .build()
                    .map_err(|e| e.to_string())?,
            )
        } else {
            None
        };
        let protected = self.policy.protected_paths.clone();
        let scope = self.path_scope.clone();
        let files = ignore::WalkBuilder::new(&self.root)
            .filter_entry(move |entry| {
                !protected.iter().any(|p| entry.path().starts_with(p))
                    && scope.as_ref().is_none_or(|s| {
                        s.roots.iter().any(|root| {
                            entry.path().starts_with(root) || root.starts_with(entry.path())
                        })
                    })
            })
            .hidden(false)
            .git_ignore(true)
            .git_exclude(true)
            .require_git(false)
            .follow_links(false)
            .sort_by_file_name(|a, b| a.cmp(b))
            .build()
            .filter_map(Result::ok)
            .filter(|e| e.file_type().is_some_and(|t| t.is_file()))
            .map(|e| e.path().to_owned());
        let mut output = Vec::new();
        let mut authority = file_authority::ReadAuthority::default();
        let mut truncated = false;
        for file in files {
            let relative = file.strip_prefix(&self.root).map_err(|e| e.to_string())?;
            if relative.components().any(|p| p.as_os_str() == ".git") || !glob.is_match(relative) {
                continue;
            }
            let path = self.resolve(relative.to_str().ok_or("non UTF-8 path")?)?;
            if let Some(regex) = &regex {
                let bytes = match self.content_guarded(&path, &mut authority) {
                    Ok(Some(v)) => v,
                    _ => continue,
                };
                let Ok(text) = std::str::from_utf8(&bytes) else {
                    continue;
                };
                let mut match_count = 0u64;
                let mut first_lines = Vec::new();
                for (index, line) in text.lines().enumerate() {
                    if regex.is_match(line) {
                        match_count += 1;
                        if first_lines.len() < 3 {
                            first_lines.push(json!({"line":index+1,"text":head_tail(line,512)}));
                        }
                    }
                }
                if match_count == 0 {
                    continue;
                }
                output
                    .push(json!({"path":path,"match_count":match_count,"first_lines":first_lines}));
            } else {
                if self.check_content_authority(&path, &mut authority).is_err() {
                    continue;
                }
                output.push(json!(path));
            }
            if output.len() >= limit {
                truncated = true;
                break;
            }
        }
        Ok(json!({"results":output,"truncated":truncated,"limit":limit}))
    }
    async fn perform_edits(
        &self,
        run: &str,
        key: &str,
        tool: &PreparedTool,
    ) -> Result<Value, String> {
        let edits: Vec<FileEdit> =
            serde_json::from_value(tool.data["edits"].clone()).map_err(|e| e.to_string())?;
        let paths = edits
            .iter()
            .map(|e| self.root.join(&e.path).to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        let paths = paths.into_iter().map(PathBuf::from).collect::<Vec<_>>();
        let wait = tool.data["_resource_wait_remaining_ms"]
            .as_u64()
            .map(std::time::Duration::from_millis)
            .unwrap_or(std::time::Duration::from_secs(30));
        let _guard = self
            .workspace
            .leases()
            .acquire_many_with_wait(run, &paths, &tool.call.name, wait)
            .await
            .map_err(|e| e.to_string())?;
        let result = self.perform_edits_inner(run, key, tool).await;
        // Standalone hosts without lifecycle notifications own leases for this call only.
        let active = self
            .active_runs
            .lock()
            .map_err(|_| "run state lock poisoned")?
            .contains(run);
        if !active {
            self.workspace
                .leases()
                .release_run(run)
                .await
                .map_err(|e| e.to_string())?;
        }
        result
    }
    async fn perform_edits_inner(
        &self,
        run: &str,
        key: &str,
        tool: &PreparedTool,
    ) -> Result<Value, String> {
        self.check_permission(Permission::Write)?;
        let edits: Vec<FileEdit> =
            serde_json::from_value(tool.data["edits"].clone()).map_err(|e| e.to_string())?;
        // Validate all entries before the first effect, preserving recorded metadata.
        for edit in &edits {
            let path = if edit.metadata_guard {
                self.resolve_entry(&edit.path)?
            } else {
                self.resolve(&edit.path)?
            };
            self.check_write_path(&path)?;
            if path != edit.path {
                return Err("entry parent changed after preparation".into());
            }
            let current = self.current_entry(&path)?;
            if !entries::entry_matches(edit, &current, false)
                && !entries::entry_matches(edit, &current, true)
            {
                return Err(self.entry_conflict(run, edit, &current).await?);
            }
        }
        let mut reports = Vec::new();
        for edit in &edits {
            let current = self.current_entry(&edit.path)?;
            if !entries::entry_matches(edit, &current, true) {
                let bytes = match &edit.post {
                    Some(hash) => Some(
                        self.db
                            .read_blob(
                                hash.parse()
                                    .map_err(|e: excalibur_store::Error| e.to_string())?,
                                files::MAX_FILE_BYTES,
                            )
                            .await
                            .map_err(|e| e.to_string())?,
                    ),
                    None => None,
                };
                // Blob loading may yield: repeat both content and metadata checks.
                let observed = self.current_entry(&edit.path)?;
                if !entries::entry_matches(edit, &observed, false)
                    && !entries::entry_matches(edit, &observed, true)
                {
                    return Err(self.entry_conflict(run, edit, &observed).await?);
                }
                if !entries::entry_matches(edit, &observed, true) {
                    self.atomic_entry(edit, bytes.as_deref())?;
                }
            }
            self.record_edit(run, key, &tool.call.id, edit).await?;
            let mut report = if let Some(hash) = &edit.post {
                let bytes = self
                    .db
                    .read_blob(
                        hash.parse()
                            .map_err(|e: excalibur_store::Error| e.to_string())?,
                        files::MAX_FILE_BYTES,
                    )
                    .await
                    .map_err(|e| e.to_string())?;
                let report = self.edit_diff(run, &tool.call.id, edit, &bytes).await?;
                self.remember(run, &tool.call.id, &edit.path, bytes).await?;
                report
            } else {
                self.forget_snapshot(run, &edit.path).await?;
                self.edit_diff(run, &tool.call.id, edit, &[]).await?
            };
            let (path, run, call) = (
                self.root.join(&edit.path).to_string_lossy().into_owned(),
                run.to_owned(),
                tool.call.id.clone(),
            );
            self.db.write(Priority::Normal,move|conn|{conn.execute("INSERT OR IGNORE INTO tool_changed_files(path,run_id,call_id) VALUES(?1,?2,?3)",[path,run,call])?;Ok(())}).await.map_err(|e|e.to_string())?;
            let postprocessing = match &self.policy.hooks {
                _ if edit.metadata_guard
                    && edit.kind == excalibur_workspace::EntryKind::Symlink =>
                {
                    json!({"formatter":"not_applicable_symbolic_link","diagnostics":"not_applicable_symbolic_link","change_recorded":true})
                }
                Some(hooks) => match hooks.after_edit(&self.root.join(&edit.path)).await {
                    Ok(report) => report,
                    Err(error) => json!({"hook_error":error}),
                },
                None => {
                    json!({"formatter":"not_configured","diagnostics":"not_configured","evidence_invalidation":"pending_host","change_recorded":true})
                }
            };
            report["path"] = json!(edit.path);
            report["fp"] = json!(edit.post);
            report["kind"] = json!(edit.kind);
            report["executable"] = json!(edit.executable);
            report["postprocessing"] = postprocessing;
            report["intelligence"] = if edit.metadata_guard
                && edit.kind == excalibur_workspace::EntryKind::Symlink
            {
                json!({"status":"not_applicable_symbolic_link"})
            } else {
                match self
                    .intelligence_after(&edit.path, &tool.data["diagnostics_before"][&edit.path])
                    .await
                {
                    Ok(value) => value,
                    Err(error) => json!({"hook_error":error}),
                }
            };
            reports.push(report);
        }
        Ok(json!({"files":reports}))
    }
    async fn output(&self, run: &str, call: &str, mut result: ToolResult) -> ToolResult {
        result.output = self.policy.redactor.redact_json(&result.output);
        if result.full_output_blob.is_none()
            && let Some(path) = result.output.get("spool_path").and_then(Value::as_str)
        {
            let path = PathBuf::from(path);
            let expected = self.terminal.spool_dir();
            if path.starts_with(expected) {
                let blobs = self.db.blobs();
                let cursor = result
                    .output
                    .get("cursor")
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                let redactor = self.policy.redactor.clone();
                let prepared = tokio::task::spawn_blocking(move || {
                    use std::io::Read;
                    blobs.prepare_reader(redaction::RedactedReader::new(
                        std::fs::File::open(path)?.take(cursor),
                        &redactor,
                    ))
                })
                .await;
                match prepared {
                    Ok(Ok(prepared)) => match self
                        .db
                        .attach_blob(
                            prepared,
                            "tool_output".into(),
                            format!("{run}:{call}"),
                            "terminal".into(),
                        )
                        .await
                    {
                        Ok(hash) => {
                            result.full_output_blob = Some(hash.as_str().to_owned());
                            result.output["archived_through_cursor"] = json!(cursor);
                            if result.output.get("exited").and_then(Value::as_bool) == Some(true)
                                && let Some(id) =
                                    result.output.get("session_id").and_then(Value::as_str)
                            {
                                match self.terminal.ack_output(run, id, hash.as_str()) {
                                    Ok(()) => result.output["spool_path"] = Value::Null,
                                    Err(error) => {
                                        result.output["spool_cleanup_warning"] = json!(error)
                                    }
                                }
                            }
                        }
                        Err(error) => {
                            return ToolResult::error(format!(
                                "terminal output persistence failed: {error}"
                            ));
                        }
                    },
                    Ok(Err(error)) => {
                        return ToolResult::error(format!(
                            "terminal output persistence failed: {error}"
                        ));
                    }
                    Err(error) => {
                        return ToolResult::error(format!("terminal output task failed: {error}"));
                    }
                }
            }
        }
        if result.full_output_blob.is_some()
            && let Some(object) = result.output.as_object_mut()
        {
            object.remove("spool_path");
        }
        let text = serde_json::to_string(&result.output).unwrap_or_default();
        if result.full_output_blob.is_none() {
            match self
                .save_blob(run, call, "full-output", text.clone().into_bytes())
                .await
            {
                Ok(hash) => result.full_output_blob = Some(hash),
                Err(error) => {
                    result.output = json!({"error":error,"original_output":result.output});
                    result.success = false;
                    return result;
                }
            }
        }
        if text.len() > self.policy.output_budget_bytes && result.success {
            let original = result.output;
            let mut errors = Vec::new();
            collect_error_text(&original, &mut errors);
            result.output = json!({"truncated":true,"head_tail":head_tail(&text,self.policy.output_budget_bytes),"bytes":text.len()});
            if !errors.is_empty() {
                result.output["errors"] = json!(errors);
            }
            for key in [
                "session_id",
                "pid",
                "cursor",
                "archived_through_cursor",
                "exited",
                "exit_code",
                "timed_out",
                "error",
            ] {
                if let Some(value) = original.get(key) {
                    result.output[key] = value.clone();
                }
            }
        }

        result
    }
    async fn execute_locked(&self, run: &str, key: &str, tool: &PreparedTool) -> ToolResult {
        let mut keys = tool
            .resources
            .iter()
            .map(|r| r.key.clone())
            .collect::<Vec<_>>();
        keys.sort();
        keys.dedup();
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(30);
        let locks = keys.iter().map(|k| resource_lock(k)).collect::<Vec<_>>();
        let mut guards = Vec::new();
        let mut activity = entries::ResourceActivity::new();
        for (resource, lock) in keys.iter().zip(&locks) {
            match tokio::time::timeout_at(deadline, lock.lock()).await {
                Ok(guard) => {
                    guards.push(guard);
                    activity.acquired(resource, run, &tool.call.name);
                }
                Err(_) => {
                    let holder = entries::ResourceActivity::owner(resource);
                    let leases = self
                        .workspace
                        .leases()
                        .owners()
                        .await
                        .unwrap_or_default()
                        .into_iter()
                        .filter(|owner| resource.contains(owner.path.to_string_lossy().as_ref()))
                        .collect::<Vec<_>>();
                    return ToolResult::error(format!(
                        "resource busy after 30-second admission deadline: {}",
                        json!({"resource":resource,"owner":holder,"leases":leases})
                    ));
                }
            }
        }
        let mut bounded = tool.clone();
        bounded.data["_resource_wait_remaining_ms"] = json!(
            deadline
                .saturating_duration_since(tokio::time::Instant::now())
                .as_millis()
                .min(30_000) as u64
        );
        let tool = &bounded;
        let result = match tool.call.name.as_str() {
            "exec" | "exec_io" => {
                if let Err(e) = self.check_permission(Permission::Exec) {
                    ToolResult::error(e)
                } else if tool.call.name == "exec" {
                    // A degraded step snapshot warns instead of failing the
                    // command: the execution proceeds and the warning travels
                    // with its result.
                    let mut warnings = match self.start_shadow_command(run).await {
                        Ok(warnings) => warnings,
                        Err(error) => vec![error],
                    };
                    let mut result = self.terminal.exec(run, tool.data["args"].clone()).await;
                    if !warnings.is_empty() {
                        warnings.append(&mut self.take_shadow_warnings(run));
                        result.output["snapshot_warnings"] = json!(warnings);
                    }
                    result
                } else {
                    self.terminal.io(run, tool.data["args"].clone()).await
                }
            }
            _ => match self.execute_call(run, key, tool).await {
                Ok(value) => ToolResult::ok(value),
                Err(error) => ToolResult::error(error),
            },
        };
        self.output(run, &tool.call.id, result).await
    }
}
fn collect_error_text(value: &Value, errors: &mut Vec<Value>) {
    match value {
        Value::Object(map) => {
            for (key, value) in map {
                if matches!(key.as_str(), "error" | "hook_error" | "diagnostics")
                    && !value.is_null()
                {
                    errors.push(value.clone());
                } else {
                    collect_error_text(value, errors);
                }
            }
        }
        Value::Array(values) => {
            for value in values {
                collect_error_text(value, errors);
            }
        }
        _ => {}
    }
}
fn resource_lock(key: &str) -> Arc<tokio::sync::Mutex<()>> {
    static LOCKS: OnceLock<Mutex<HashMap<String, Weak<tokio::sync::Mutex<()>>>>> = OnceLock::new();
    let mut map = LOCKS
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    if let Some(lock) = map.get(key).and_then(Weak::upgrade) {
        return lock;
    }
    map.retain(|_, v| v.strong_count() > 0);
    let lock = Arc::new(tokio::sync::Mutex::new(()));
    map.insert(key.into(), Arc::downgrade(&lock));
    lock
}
pub fn head_tail(text: &str, budget: usize) -> String {
    if text.len() <= budget {
        return text.into();
    }
    let marker = "\n… truncated …\n";
    let half = budget.saturating_sub(marker.len()) / 2;
    let mut end = half.min(text.len());
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    let mut start = text.len().saturating_sub(half);
    while !text.is_char_boundary(start) {
        start += 1;
    }
    format!("{}{}{}", &text[..end], marker, &text[start..])
}
impl ToolRuntime for LocalTools {
    fn filesystem_confinement(&self, run: &str) -> Option<FilesystemConfinement> {
        self.scope_roots().map(|roots| FilesystemConfinement {
            workspace: self.root.clone(),
            read_roots: roots.to_vec(),
            snapshot_owner: run.into(),
        })
    }

    fn live_status(&self, run: &str) -> Vec<Value> {
        self.terminal.live_status(run)
    }
    fn execution_state<'a>(
        &'a self,
        run: &'a str,
        state: &'a str,
        reason: Option<&'a str>,
    ) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async move {
            let active = matches!(state, "running" | "verifying")
                || state == "waiting" && matches!(reason, Some("tool" | "subagents"));
            let was_active = {
                let mut runs = self
                    .active_runs
                    .lock()
                    .map_err(|_| "run state lock poisoned")?;
                let was_active = runs.contains(run);
                if active {
                    runs.insert(run.into());
                } else {
                    runs.remove(run);
                }
                was_active && !active
            };
            // A run leaving active execution (pause, cancel, approval wait,
            // block) ends its current step outside the normal step boundary.
            // Record the closing filesystem state so the step stays restorable.
            // A degraded snapshot buffers a warning for the next exec result
            // instead of failing the transition.
            if was_active {
                let step = self
                    .steps
                    .lock()
                    .map_err(|_| "step state lock poisoned")?
                    .get(run)
                    .copied();
                if let Some(step) = step {
                    self.push_shadow_warnings(
                        run,
                        self.finish_shadow_step(run, step)
                            .await
                            .unwrap_or_else(|error| vec![error]),
                    );
                }
            }
            self.workspace
                .leases()
                .heartbeat(run, state, reason)
                .await
                .map_err(|e| e.to_string())
        })
    }
    fn begin_step<'a>(&'a self, run: &'a str, step: u64) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async move {
            self.steps
                .lock()
                .map_err(|_| "step state lock poisoned")?
                .insert(run.into(), step);
            // A degraded snapshot buffers a warning for the next exec result
            // instead of failing the step boundary.
            self.push_shadow_warnings(
                run,
                self.begin_shadow_step(run, step)
                    .await
                    .unwrap_or_else(|error| vec![error]),
            );
            Ok(())
        })
    }
    fn end_step<'a>(&'a self, run: &'a str, step: u64) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async move {
            self.push_shadow_warnings(
                run,
                self.finish_shadow_step(run, step)
                    .await
                    .unwrap_or_else(|error| vec![error]),
            );
            Ok(())
        })
    }
    fn interrupt_execution<'a>(
        &'a self,
        run: &'a str,
        cancelled: bool,
        tool: Option<&'a PreparedTool>,
    ) -> BoxFuture<'a, Result<bool, String>> {
        Box::pin(async move {
            // A declared background session outlives a pause between effects.
            // Cancellation still tears down every managed process of the run.
            if !cancelled && tool.is_none() {
                return Ok(false);
            }
            if tool.is_some_and(|t| !matches!(t.call.name.as_str(), "exec" | "exec_io")) {
                return Ok(false);
            }
            let active = self.terminal.execution_resources(run).processes > 0;
            self.terminal.cancel_run(run).await;
            Ok(active)
        })
    }
    fn configure_execution_limits<'a>(
        &'a self,
        run: &'a str,
        limits: excalibur_types::runtime::ExecutionLimits,
    ) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async move { self.terminal.set_execution_limits(run, limits) })
    }
    fn execution_resources<'a>(
        &'a self,
        run: &'a str,
    ) -> BoxFuture<'a, Result<Option<excalibur_types::runtime::ExecutionResources>, String>> {
        Box::pin(async move { Ok(Some(self.terminal.execution_resources(run))) })
    }

    fn configure_network_authorizer<'a>(
        &'a self,
        run: &'a str,
        authorizer: Arc<dyn excalibur_types::runtime::NetworkAuthorizer>,
    ) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async move { self.terminal.set_network_authorizer(run, authorizer) })
    }
    fn configure_process_boundary<'a>(
        &'a self,
        run: &'a str,
        boundary: excalibur_types::runtime::ProcessBoundary,
    ) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async move {
            use excalibur_types::runtime::ProcessNetwork;
            let mut sandbox = self
                .terminal
                .sandbox_config()
                .ok_or("no host sandbox is configured")?;
            sandbox.workspace = boundary.workspace;
            sandbox.profile = match boundary.profile.as_str() {
                "project" => terminal::SandboxProfile::Project,
                "project-restricted" => terminal::SandboxProfile::ProjectRestricted,
                "read-only" => terminal::SandboxProfile::ReadOnly,
                "file-analysis" => terminal::SandboxProfile::FileAnalysis,
                "tool-package" => terminal::SandboxProfile::ToolPackage,
                "project-net" => terminal::SandboxProfile::ProjectNet,
                "eval" => terminal::SandboxProfile::Eval,
                _ => return Err("unknown mandatory process sandbox profile".into()),
            };
            #[cfg(unix)]
            {
                use excalibur_sandbox::network::NetworkPolicy;
                if sandbox.profile == terminal::SandboxProfile::Eval {
                    sandbox.eval_network=match boundary.network {
                        ProcessNetwork::None=>false,
                        ProcessNetwork::Full if sandbox.eval_network=>true,
                        ProcessNetwork::Full=>return Err("evaluation networking exceeds the host container declaration".into()),
                        ProcessNetwork::Domains(_)=>return Err("evaluation domain restrictions require an enforcing container network adapter; unavailable".into()),
                    };
                    sandbox.network = None;
                } else {
                    sandbox.network = match boundary.network {
                        ProcessNetwork::None => None,
                        network => {
                            if sandbox.profile != terminal::SandboxProfile::ProjectNet {
                                return Err(
                                    "network authority requires the project-net process profile"
                                        .into(),
                                );
                            }
                            let mut proxy = sandbox
                                .network
                                .ok_or("no host proxy ceiling is configured")?;
                            proxy.policy = match network {
                                ProcessNetwork::Domains(domains) => {
                                    NetworkPolicy::Domains(domains.into_iter().collect())
                                }
                                ProcessNetwork::Full => NetworkPolicy::Full,
                                ProcessNetwork::None => unreachable!(),
                            };
                            Some(proxy)
                        }
                    };
                }
                if sandbox.profile == terminal::SandboxProfile::ProjectNet
                    && sandbox.network.is_none()
                {
                    return Err("project-net requires effective network authority".into());
                }
            }
            #[cfg(not(unix))]
            if boundary.network != ProcessNetwork::None {
                return Err(
                    "native network process confinement is unavailable on this target".into(),
                );
            }
            sandbox.read_roots = boundary.read_roots;
            sandbox.write_roots = boundary.write_roots;
            sandbox.hidden_paths = boundary.denied_paths;
            self.terminal.set_run_sandbox(run, sandbox)
        })
    }
    fn definitions(&self) -> Vec<ToolDefinition> {
        ["apply_patch","edit_file","exec","exec_io","glob","grep","read_file","write_file"].into_iter().map(|name|ToolDefinition{name:name.into(),description:match name{"read_file"=>"Read a file with numbered lines and a fingerprint, default 200 lines.","write_file"=>"Create or rewrite a file after checking the run's read fingerprint.","edit_file"=>"Replace unique text using exact, typography-normalized or trailing-space matching.","apply_patch"=>"Apply Codex block patches adding, deleting or updating multiple files.","grep"=>"Search regex in files respecting gitignore; return bounded per-file summaries.","glob"=>"Find paths matching a glob while respecting gitignore.","exec"=>"Run a command in a persistent pipe or PTY session under host execution policy.",_=>"Read, write, wait for, signal or close owned terminal sessions."}.into(),input_schema:Self::schema(name).unwrap()}).chain(project::definitions()).collect()
    }
    fn scheduling<'a>(
        &'a self,
        run: &'a str,
        call: &'a ToolCall,
    ) -> BoxFuture<'a, Option<ToolScheduling>> {
        Box::pin(async move {
            self.scheduling_resources(run, call)
                .ok()
                .map(|(resources, exclusive)| ToolScheduling {
                    resources,
                    exclusive,
                })
        })
    }
    fn prepare<'a>(
        &'a self,
        run: &'a str,
        call: ToolCall,
    ) -> BoxFuture<'a, Result<PreparedTool, String>> {
        Box::pin(self.prepare_call(run, call))
    }
    fn execute<'a>(
        &'a self,
        run: &'a str,
        key: &'a str,
        tool: &'a PreparedTool,
    ) -> BoxFuture<'a, ToolResult> {
        Box::pin(async move {
            if tool.data["root"].as_str() != self.root.to_str() || tool.data["version"] != 1 {
                return ToolResult::error(
                    "prepared tool belongs to another workspace or unsupported preparation version",
                );
            }
            if matches!(
                tool.call.name.as_str(),
                "write_file"
                    | "edit_file"
                    | "apply_patch"
                    | "revert_edit"
                    | "workspace_snapshot"
                    | "lsp"
                    | "worktree"
                    | "integration_queue"
            ) && tool.effect != Effect::Pure
            {
                let (tools, run, key, tool) =
                    (self.clone(), run.to_owned(), key.to_owned(), tool.clone());
                // Once admitted, atomic filesystem writes and their durable history complete
                // even if the caller cancels its wait. Recovery owns any interruption on timeout.
                match tokio::spawn(async move {tokio::time::timeout(std::time::Duration::from_secs(120),tools.execute_locked(&run,&key,&tool)).await.unwrap_or_else(|_|ToolResult::error("file operation timed out; reconcile its durable preconditions before retrying"))}).await {
                    Ok(result)=>result,Err(error)=>ToolResult::error(format!("file operation task failed: {error}")),
                }
            } else if tool.effect == Effect::Pure {
                let timeout = if tool.call.name == "exec_io" {
                    std::time::Duration::from_millis(
                        tool.data["args"]["timeout_ms"]
                            .as_u64()
                            .unwrap_or(30_000)
                            .min(300_000),
                    )
                    .saturating_add(std::time::Duration::from_secs(1))
                } else {
                    std::time::Duration::from_secs(30)
                };
                tokio::time::timeout(timeout, self.execute_locked(run, key, tool))
                    .await
                    .unwrap_or_else(|_| ToolResult::error("read operation timed out"))
            } else {
                self.execute_locked(run, key, tool).await
            }
        })
    }

    fn reconcile<'a>(
        &'a self,
        run: &'a str,
        key: &'a str,
        tool: &'a PreparedTool,
    ) -> BoxFuture<'a, Reconcile> {
        Box::pin(async move {
            if tool.data["root"].as_str() != self.root.to_str() || tool.data["version"] != 1 {
                return Reconcile::Uncertain(
                    "prepared tool workspace or version differs from this adapter".into(),
                );
            }
            if tool.effect == Effect::Pure {
                return Reconcile::Retry;
            }
            if tool.call.name == "exec" && tool.effect == Effect::Idempotent {
                if tool.data["args"]["cmd"]
                    .as_str()
                    .is_some_and(|cmd| self.policy.repeatable_commands.contains(cmd))
                {
                    return Reconcile::Retry;
                }
                return Reconcile::Uncertain(
                    "repeatable command approval is no longer present".into(),
                );
            }
            if matches!(tool.call.name.as_str(), "worktree" | "integration_queue") {
                return self.reconcile_project_operation(run, key, tool).await;
            }
            if matches!(tool.call.name.as_str(), "exec" | "exec_io") {
                return Reconcile::Uncertain(
                    "terminal effect may have run; inspect the retained session before retrying"
                        .into(),
                );
            }
            let edits: Vec<FileEdit> = match serde_json::from_value(tool.data["edits"].clone()) {
                Ok(v) => v,
                Err(e) => return Reconcile::Uncertain(e.to_string()),
            };
            let mut keys = tool
                .resources
                .iter()
                .map(|r| r.key.clone())
                .collect::<Vec<_>>();
            keys.sort();
            keys.dedup();
            let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(30);
            let locks = keys.iter().map(|k| resource_lock(k)).collect::<Vec<_>>();
            let mut guards = Vec::new();
            let mut activity = entries::ResourceActivity::new();
            for (resource, lock) in keys.iter().zip(&locks) {
                match tokio::time::timeout_at(deadline, lock.lock()).await {
                    Ok(guard) => {
                        guards.push(guard);
                        activity.acquired(resource, run, &tool.call.name);
                    }
                    Err(_) => {
                        return Reconcile::Uncertain(format!(
                            "resource busy after 30-second recovery admission deadline: {}",
                            json!({"resource":resource,"owner":entries::ResourceActivity::owner(resource)})
                        ));
                    }
                }
            }
            let mut bounded = tool.clone();
            bounded.data["_resource_wait_remaining_ms"] = json!(
                deadline
                    .saturating_duration_since(tokio::time::Instant::now())
                    .as_millis()
                    .min(30_000) as u64
            );
            let tool = &bounded;
            for edit in &edits {
                let state = match (if edit.metadata_guard {
                    self.resolve_entry(&edit.path)
                } else {
                    self.resolve(&edit.path)
                })
                .and_then(|path| self.current_entry(&path))
                {
                    Ok(v) => v,
                    Err(e) => return Reconcile::Uncertain(e),
                };
                if !entries::entry_matches(edit, &state, false)
                    && !entries::entry_matches(edit, &state, true)
                {
                    return Reconcile::Uncertain(format!(
                        "external content, kind or mode change prevents recovery of {}",
                        edit.path
                    ));
                }
            }
            match self.perform_edits(run, key, tool).await {
                Ok(value) => Reconcile::Done(ToolResult::ok(value)),
                Err(e) => Reconcile::Uncertain(e),
            }
        })
    }
}
