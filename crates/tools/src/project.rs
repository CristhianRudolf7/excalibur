//! Project-domain tools. All restorations re-enter the ordinary durable edit pipeline.
use crate::{
    LocalTools, Permission,
    files::{FileEdit, hash, required_str},
};
use excalibur_types::runtime::*;
use excalibur_workspace::{GitIdentity, IntegrationChecks};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    sync::Arc,
};
#[derive(Clone)]
pub struct ParentAuthority {
    pub task: String,
    pub identity: GitIdentity,
    pub checks: Arc<dyn IntegrationChecks>,
}
/// Trusted host binding for a child's own registered task branch only.
#[derive(Clone)]
pub struct ChildAuthority {
    pub task: String,
    pub owner: String,
    pub identity: GitIdentity,
}
enum GitAuthority {
    Parent(ParentAuthority),
    Child(ChildAuthority),
}
pub const DOMAIN_TOOLS: &[&str] = &[
    "file_history",
    "revert_edit",
    "workspace_snapshot",
    "worktree",
    "integration_queue",
    "code_graph",
    "lsp",
];
pub fn definitions() -> Vec<ToolDefinition> {
    let string = json!({"type":"string","minLength":1});
    let mut result = vec![];
    for (name, description, properties, required) in [
        (
            "file_history",
            "List durable pre/post versions of an authorized project file.",
            json!({"path":string,"limit":{"type":"integer","minimum":1,"maximum":1000}}),
            vec!["path"],
        ),
        (
            "revert_edit",
            "Restore a single version through fingerprint-checked three-way merge; preserve later edits.",
            json!({"edit_id":string,"path":string}),
            vec!["edit_id", "path"],
        ),
        (
            "workspace_snapshot",
            "List command-step shadow snapshots, show their diff or restore explicitly authorized changed paths.",
            json!({"action":{"enum":["list","diff","restore"]},"before":string,"after":string,"paths":{"type":"array","items":string,"minItems":1}}),
            vec!["action"],
        ),
        (
            "worktree",
            "Parent-only isolated task branches/worktrees; never merge or push the user branch.",
            json!({"action":{"enum":["mode","create","list","remove","checkpoint","assign"]},"owner":string,"message":string,"paths":{"type":"array","items":string,"minItems":1,"maxItems":512}}),
            vec!["action"],
        ),
        (
            "integration_queue",
            "Checkpoint or submit the child's own finished branch and inspect its queue status; only the parent can process or retry integration.",
            json!({"action":{"enum":["checkpoint","enqueue","list","process","retry"]},"owner":string,"message":string,"intent":string,"id":string}),
            vec!["action"],
        ),
        (
            "code_graph",
            "Query indexed symbols, callers, dependencies, requirement links and impacted tests.",
            json!({"operation":{"enum":["symbols","callers","dependencies","implementations","file","impacted_tests"]},"query":{"type":"string"},"symbol":string,"path":string,"requirement_id":string,"paths":{"type":"array","items":string},"limit":{"type":"integer","minimum":1,"maximum":1000}}),
            vec!["operation"],
        ),
        (
            "lsp",
            "Query a configured shared language server or apply its rename through checked file edits.",
            json!({"action":{"enum":["definition","references","document_symbols","workspace_symbols","diagnostics","rename"]},"path":string,"line":{"type":"integer","minimum":0},"character":{"type":"integer","minimum":0},"query":{"type":"string"},"language":string,"new_name":string,"paths":{"type":"array","items":string}}),
            vec!["action"],
        ),
    ] {
        result.push(ToolDefinition{name:name.into(),description:description.into(),input_schema:json!({"$schema":"https://json-schema.org/draft/2020-12/schema","type":"object","properties":properties,"required":required,"additionalProperties":false})});
    }
    result
}
impl LocalTools {
    pub fn workspace(&self) -> excalibur_workspace::Workspace {
        self.workspace.clone()
    }
    /// Trusted host authority. A model cannot select a task, Git identity or test executor.
    pub fn authorize_parent(&self, run: &str, authority: ParentAuthority) -> Result<(), String> {
        if run.is_empty() || authority.task.is_empty() {
            return Err("parent run and task required".into());
        }
        let mut parents = self
            .parents
            .lock()
            .map_err(|_| "parent authority lock poisoned")?;
        if self
            .child_authorities
            .lock()
            .map_err(|_| "child authority lock poisoned")?
            .contains_key(run)
        {
            return Err("child execution cannot become a parent workspace authority".into());
        }
        parents.insert(run.into(), authority);
        Ok(())
    }
    /// Host-only, immutable binding. Task, branch owner and Git identity are never
    /// accepted from model arguments; the host must rebind after runtime restart.
    pub fn authorize_child(&self, run: &str, authority: ChildAuthority) -> Result<(), String> {
        if [
            run,
            &authority.task,
            &authority.owner,
            &authority.identity.name,
            &authority.identity.email,
        ]
        .iter()
        .any(|value| value.trim().is_empty() || value.chars().any(char::is_control))
        {
            return Err("safe child run, task, owner and Git identity required".into());
        }
        let parents = self
            .parents
            .lock()
            .map_err(|_| "parent authority lock poisoned")?;
        if parents.contains_key(run) {
            return Err("parent execution cannot become a child workspace authority".into());
        }
        let mut children = self
            .child_authorities
            .lock()
            .map_err(|_| "child authority lock poisoned")?;
        if let Some(old) = children.get(run) {
            return if old.task == authority.task
                && old.owner == authority.owner
                && old.identity.name == authority.identity.name
                && old.identity.email == authority.identity.email
            {
                Ok(())
            } else {
                Err("child workspace authority cannot be rebound".into())
            };
        }
        children.insert(run.into(), authority);
        Ok(())
    }
    fn git_authority(&self, run: &str, name: &str, args: &Value) -> Result<GitAuthority, String> {
        if let Some(parent) = self
            .parents
            .lock()
            .map_err(|_| "parent authority lock poisoned")?
            .get(run)
            .cloned()
        {
            return Ok(GitAuthority::Parent(parent));
        }
        if name == "integration_queue"
            && matches!(
                args["action"].as_str(),
                Some("checkpoint" | "enqueue" | "list")
            )
        {
            let child = self
                .child_authorities
                .lock()
                .map_err(|_| "child authority lock poisoned")?
                .get(run)
                .cloned()
                .ok_or("child workspace authority required")?;
            if args.get("task").is_some()
                || args.get("identity").is_some()
                || args
                    .get("owner")
                    .is_some_and(|owner| owner.as_str() != Some(child.owner.as_str()))
            {
                return Err("child workspace operation exceeds its bound owner".into());
            }
            return Ok(GitAuthority::Child(child));
        }
        Err("parent workspace authority required".into())
    }
    pub(crate) fn authorized_project_files(&self) -> Result<BTreeMap<PathBuf, Vec<u8>>, String> {
        self.authorized_project_files_selected(None)
    }
    pub(crate) fn authorized_project_files_selected(
        &self,
        selected: Option<&BTreeSet<PathBuf>>,
    ) -> Result<BTreeMap<PathBuf, Vec<u8>>, String> {
        let protected = self.policy.protected_paths.clone();
        let scope = self.path_scope.clone();
        let mut files = BTreeMap::new();
        let mut authority = crate::file_authority::ReadAuthority::excluding_credential_names();
        for entry in ignore::WalkBuilder::new(&self.root)
            .hidden(false)
            .git_ignore(true)
            .git_exclude(true)
            .require_git(false)
            .follow_links(false)
            .filter_entry(move |entry| {
                !entry.path().components().any(|p| {
                    matches!(
                        p.as_os_str().to_str(),
                        Some(".git" | "target" | "node_modules")
                    )
                }) && !protected.iter().any(|p| entry.path().starts_with(p))
                    && scope.as_ref().is_none_or(|s| {
                        s.roots.iter().any(|root| {
                            entry.path().starts_with(root) || root.starts_with(entry.path())
                        })
                    })
            })
            .build()
        {
            let entry = entry.map_err(|e| e.to_string())?;
            if !entry.file_type().is_some_and(|t| t.is_file()) {
                continue;
            }
            if selected.is_some_and(|paths| !paths.contains(entry.path())) {
                continue;
            }
            let relative = entry
                .path()
                .strip_prefix(&self.root)
                .map_err(|e| e.to_string())?;
            let path = self.resolve(relative.to_str().ok_or("project path must be UTF-8")?)?;
            // Metadata and credential paths require their own explicit host workflows.
            if self.check_write_path(&path).is_err()
                || excalibur_sandbox::file_authority::credential_path(Path::new(&path))
            {
                continue;
            }
            match self.content_guarded(&path, &mut authority) {
                Ok(Some(bytes)) => {
                    if self.policy.redactor.contains_secret(&bytes) {
                        continue;
                    }
                    files.insert(PathBuf::from(path), bytes);
                }
                Ok(None) => {}
                Err(_) => continue,
            }
        }
        Ok(files)
    }
    pub(crate) fn mark_exec_run(&self, run: &str) {
        if let Ok(mut runs) = self.exec_runs.lock() {
            runs.insert(run.into());
        }
    }
    /// A run "uses exec" once it attempted an exec call or still owns terminal
    /// sessions. Steps of such runs record filesystem state at their
    /// boundaries even when the step itself only drives background sessions
    /// through exec_io.
    pub(crate) fn uses_exec(&self, run: &str) -> bool {
        if self
            .exec_runs
            .lock()
            .map(|runs| runs.contains(run))
            .unwrap_or(false)
        {
            return true;
        }
        !self.terminal.live_status(run).is_empty()
    }
    pub(crate) async fn has_shadow_snapshot(
        &self,
        run: &str,
        step: u64,
        phase: &str,
    ) -> Result<bool, String> {
        Ok(self
            .workspace
            .shadow()
            .list_snapshots(run)
            .await
            .map_err(|e| e.to_string())?
            .iter()
            .any(|s| s.step == step && s.phase == phase))
    }
    pub(crate) async fn start_shadow_command(&self, run: &str) -> Result<Vec<String>, String> {
        self.mark_exec_run(run);
        let step = self
            .steps
            .lock()
            .map_err(|_| "step lock poisoned")?
            .get(run)
            .copied()
            .unwrap_or(0);
        // The per-step entry is idempotent; check before walking the tree so a
        // repeated exec in the same step does not re-read every file.
        if self.has_shadow_snapshot(run, step, "before").await? {
            self.mark_exec_step_open(run);
            return Ok(Vec::new());
        }
        let (_, warnings) = self
            .workspace
            .shadow()
            .snapshot_entries(run, step, "before", self.authorized_snapshot_entries()?)
            .await
            .map_err(|e| e.to_string())?;
        self.mark_exec_step_open(run);
        Ok(warnings)
    }
    pub(crate) async fn begin_shadow_step(
        &self,
        run: &str,
        step: u64,
    ) -> Result<Vec<String>, String> {
        if !self.uses_exec(run) {
            return Ok(Vec::new());
        }
        if self.has_shadow_snapshot(run, step, "before").await? {
            self.mark_exec_step_open(run);
            return Ok(Vec::new());
        }
        let (_, warnings) = self
            .workspace
            .shadow()
            .snapshot_entries(run, step, "before", self.authorized_snapshot_entries()?)
            .await
            .map_err(|e| e.to_string())?;
        self.mark_exec_step_open(run);
        Ok(warnings)
    }
    pub(crate) async fn finish_shadow_step(
        &self,
        run: &str,
        step: u64,
    ) -> Result<Vec<String>, String> {
        if self.has_shadow_snapshot(run, step, "after").await? {
            self.mark_exec_step_closed(run);
            return Ok(Vec::new());
        }
        if !self.uses_exec(run) && !self.has_shadow_snapshot(run, step, "before").await? {
            self.mark_exec_step_closed(run);
            return Ok(Vec::new());
        }
        let entries = self.authorized_snapshot_entries()?;
        let attribution = entries
            .iter()
            .map(|(path, entry)| (path.to_string_lossy().into_owned(), hash(&entry.content)))
            .collect::<Vec<_>>();
        let (_, warnings) = self
            .workspace
            .shadow()
            .snapshot_entries(run, step, "after", entries)
            .await
            .map_err(|e| e.to_string())?;
        // Attribute the closing content to the agent's own `exec` work so
        // the project monitor does not report it as an outside change.
        // Pending rows queued by a scan that raced the step close are
        // corrected as well; only already-processed rows keep their verdict.
        let project = self.root.to_string_lossy().into_owned();
        self.db
            .write(excalibur_store::Priority::Normal, move |c| {
                c.execute_batch("CREATE TABLE IF NOT EXISTS project_exec_attribution(project TEXT NOT NULL,path TEXT NOT NULL,hash TEXT NOT NULL,PRIMARY KEY(project,path))")?;
                for (path, file_hash) in &attribution {
                    c.execute(
                        "INSERT INTO project_exec_attribution(project,path,hash) VALUES(?1,?2,?3) ON CONFLICT(project,path) DO UPDATE SET hash=excluded.hash",
                        excalibur_store::rusqlite::params![project, path, file_hash],
                    )?;
                }
                // `project_changes` only exists where a monitor ran; without
                // it there is nothing to correct.
                let has_changes: i64 = c.query_row(
                    "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='project_changes'",
                    [],
                    |r| r.get(0),
                )?;
                if has_changes > 0 {
                    c.execute(
                        "UPDATE project_changes SET external=0 WHERE state='pending' AND EXISTS(SELECT 1 FROM project_exec_attribution a WHERE a.project=project_changes.project AND a.path=project_changes.path AND a.hash=project_changes.current_hash)",
                        [],
                    )?;
                }
                Ok(())
            })
            .await
            .map_err(|e| e.to_string())?;
        self.mark_exec_step_closed(run);
        Ok(warnings)
    }
    pub(crate) async fn prepare_domain(
        &self,
        run: &str,
        call: ToolCall,
        args: Value,
    ) -> Result<PreparedTool, String> {
        let mut edits = Vec::new();
        let mut effect = Effect::Pure;
        if call.name == "workspace_snapshot"
            && matches!(args["action"].as_str(), Some("diff" | "restore"))
        {
            let snapshots = self.scoped_snapshots(run).await?;
            for field in if args["action"] == "diff" {
                vec!["before", "after"]
            } else {
                vec!["before"]
            } {
                let id = required_str(&args, field)?;
                if !snapshots.iter().any(|snapshot| snapshot.id == id) {
                    return Err("snapshot is not owned by this execution".into());
                }
            }
        }
        match call.name.as_str() {
            "file_history" => {
                self.check_permission(Permission::Read)?;
                self.resolve_entry(required_str(&args, "path")?)?;
            }
            "revert_edit" => {
                self.check_permission(Permission::Write)?;
                let path = self.resolve_entry(required_str(&args, "path")?)?;
                self.check_write_path(&path)?;
                let id = required_str(&args, "edit_id")?;
                let version = self
                    .workspace
                    .history()
                    .version(id)
                    .await
                    .map_err(|e| e.to_string())?;
                if self.resolve_entry(version.path.to_str().ok_or("invalid version path")?)? != path
                {
                    return Err("edit does not belong to the explicitly authorized file".into());
                }
                let plan = self
                    .workspace
                    .history()
                    .plan_revert_entry(id, self.current_entry(&path)?)
                    .await
                    .map_err(|e| e.to_string())?;
                edits = self
                    .prepare_restore(run, &call, plan, &BTreeSet::from([path]))
                    .await?;
                effect = Effect::Idempotent;
            }
            "workspace_snapshot" if args["action"] == "restore" => {
                self.check_permission(Permission::Write)?;
                let paths = args["paths"]
                    .as_array()
                    .ok_or("restore requires explicit paths")?
                    .iter()
                    .map(|p| self.resolve_entry(p.as_str().ok_or("invalid path")?))
                    .collect::<Result<BTreeSet<_>, String>>()?;
                if paths.is_empty() {
                    return Err("restore paths required".into());
                }
                let current = paths
                    .iter()
                    .map(|p| Ok((PathBuf::from(p), self.current_entry(p)?)))
                    .collect::<Result<BTreeMap<_, _>, String>>()?;
                let plan = self
                    .workspace
                    .shadow()
                    .plan_restore_entries(required_str(&args, "before")?, current, true)
                    .await
                    .map_err(|e| e.to_string())?;
                edits = self.prepare_restore(run, &call, plan, &paths).await?;
                effect = Effect::Idempotent;
            }
            "workspace_snapshot" => {
                self.check_permission(Permission::Read)?;
                if args["action"] == "diff" {
                    required_str(&args, "before")?;
                    required_str(&args, "after")?;
                }
            }
            "worktree" | "integration_queue" => {
                if args["action"] != "mode" {
                    self.git_authority(run, &call.name, &args)?;
                }
                if matches!(args["action"].as_str(), Some("list" | "mode")) {
                    self.check_permission(Permission::Read)?;
                } else {
                    self.check_permission(Permission::Write)?;
                    effect = Effect::NonIdempotent;
                }
            }
            "code_graph" => {
                self.check_permission(Permission::Read)?;
                if self.intelligence.is_none() {
                    return Err("project code intelligence is not active".into());
                }
            }
            "lsp" => {
                self.check_permission(Permission::Read)?;
                if args["action"] == "rename" {
                    self.check_permission(Permission::Write)?;
                    edits = self.prepare_lsp_rename(run, &call, &args).await?;
                    effect = Effect::Idempotent;
                } else if self.intelligence.is_none() {
                    return Err("project code intelligence is not active".into());
                }
            }
            _ => return Err("unknown project domain tool".into()),
        }
        let mut resources = edits
            .iter()
            .map(|edit| Resource {
                key: self.root.join(&edit.path).to_string_lossy().into_owned(),
                write: true,
            })
            .collect::<Vec<_>>();
        if resources.is_empty() {
            resources.push(Resource {
                key: format!("project:{}", self.root.display()),
                write: effect != Effect::Pure,
            });
        }
        Ok(PreparedTool {
            call,
            effect,
            resources,
            exclusive: effect != Effect::Pure,
            data: json!({"args":args,"diagnostics_before":self.diagnostics_before(&edits).await?,"edits":edits,"root":self.root,"version":1}),
        })
    }
    pub(crate) async fn prepare_restore(
        &self,
        run: &str,
        call: &ToolCall,
        plan: excalibur_workspace::RevertPlan,
        allowed: &BTreeSet<String>,
    ) -> Result<Vec<FileEdit>, String> {
        if !plan.conflicts.is_empty() {
            return Err(format!(
                "merge_conflict: {}",
                serde_json::to_string(&plan.conflicts).map_err(|e| e.to_string())?
            ));
        }
        let mut edits = vec![];
        for proposed in plan.files {
            let path =
                self.resolve_entry(proposed.path.to_str().ok_or("invalid restoration path")?)?;
            if !allowed.contains(&path) {
                return Err("restoration exceeds explicitly authorized paths".into());
            }
            edits.push(self.prepare_entry_edit(run, &call.id, &proposed).await?);
        }
        Ok(edits)
    }
    pub(crate) async fn execute_domain(
        &self,
        run: &str,
        key: &str,
        tool: &PreparedTool,
    ) -> Result<Value, String> {
        let args = &tool.data["args"];
        match tool.call.name.as_str() {
            "revert_edit" => self.perform_edits(run, key, tool).await,
            "workspace_snapshot" => match args["action"].as_str() {
                Some("restore") => self.perform_edits(run, key, tool).await,
                Some("list") => Ok(json!({"snapshots":self.scoped_snapshots(run).await?})),
                Some("diff") => Ok(
                    json!({"diff":self.workspace.shadow().diff(required_str(args,"before")?,required_str(args,"after")?).await.map_err(|e|e.to_string())?}),
                ),
                _ => Err("unsupported snapshot action".into()),
            },
            "file_history" => {
                self.check_permission(Permission::Read)?;
                let path = self.resolve_entry(required_str(args, "path")?)?;
                Ok(
                    json!({"versions":self.workspace.history().file_history(Path::new(&path),args["limit"].as_u64().unwrap_or(100) as u32).await.map_err(|e|e.to_string())?}),
                )
            }
            "worktree" | "integration_queue" => {
                let git = self.workspace.git();
                if args["action"] == "mode" {
                    return Ok(json!({"mode":git.mode().await.map_err(|e|e.to_string())?}));
                }
                let auth = match self.git_authority(run, &tool.call.name, args)? {
                    GitAuthority::Parent(parent) => parent,
                    GitAuthority::Child(child) => {
                        return match args["action"].as_str() {
                            Some("checkpoint") => Ok(
                                json!({"commit":self.checkpoint_child(&child.task,&child.identity,&child.owner,required_str(args,"message")?).await?}),
                            ),
                            Some("enqueue") => Ok(
                                json!({"id":git.enqueue(&child.task,&child.owner,required_str(args,"intent")?).await.map_err(|e|e.to_string())?}),
                            ),
                            Some("list") => {
                                let queue = git
                                    .list(&child.task)
                                    .await
                                    .map_err(|e| e.to_string())?
                                    .into_iter()
                                    .filter(|item| item.child == child.owner)
                                    .collect::<Vec<_>>();
                                Ok(json!({"queue":queue}))
                            }
                            _ => Err("parent workspace authority required".into()),
                        };
                    }
                };
                match (tool.call.name.as_str(), args["action"].as_str()) {
                    ("worktree", Some("assign")) => {
                        let mut selected = BTreeSet::new();
                        for target in args["paths"]
                            .as_array()
                            .ok_or("assignment paths required")?
                        {
                            let path =
                                self.resolve(target.as_str().ok_or("invalid assignment path")?)?;
                            self.content(&path)?;
                            selected.insert(PathBuf::from(&path));
                            if let Some(services) = &self.intelligence {
                                self.refresh_graph().await?;
                                let page = services
                                    .graph
                                    .query(excalibur_code_intelligence::GraphQuery::Dependencies {
                                        path,
                                        limit: 100,
                                    })
                                    .await
                                    .map_err(|e| e.to_string())?;
                                if page.has_more {
                                    return Err("dependency partition exceeds bounded query; narrow the assignment".into());
                                }
                                for node in page.nodes {
                                    if let Some(path) = node.path {
                                        let path = self.resolve(&path)?;
                                        self.content(&path)?;
                                        selected.insert(PathBuf::from(path));
                                    }
                                }
                            }
                        }
                        let selected = selected.into_iter().collect::<Vec<_>>();
                        let overlaps = self
                            .workspace
                            .assign_files(&auth.task, run, required_str(args, "owner")?, &selected)
                            .await
                            .map_err(|e| e.to_string())?;
                        if !overlaps.is_empty() {
                            let payload = json!({"id":format!("overlap:{}",hash(serde_json::to_string(&overlaps).map_err(|e|e.to_string())?.as_bytes())),"text":format!("File assignments overlap: {}. Serialize the work, give the shared file to one child, or use worktrees.",serde_json::to_string(&overlaps).map_err(|e|e.to_string())?)});
                            let journal = excalibur_journal::Journal::new(self.db.clone())
                                .await
                                .map_err(|e| e.to_string())?;
                            journal
                                .append(
                                    run,
                                    "directive_received",
                                    &format!(
                                        "directive:{}:received",
                                        payload["id"].as_str().unwrap()
                                    ),
                                    payload,
                                )
                                .await
                                .map_err(|e| e.to_string())?;
                        }
                        Ok(
                            json!({"files":selected,"overlaps":overlaps,"dependency_source":if self.intelligence.is_some(){"code_graph"}else{"explicit_host_files"}}),
                        )
                    }
                    ("worktree", Some("create")) => Ok(
                        json!({"worktree":git.create(&auth.task,required_str(args,"owner")?).await.map_err(|e|e.to_string())?}),
                    ),
                    ("worktree", Some("list")) => Ok(
                        json!({"worktrees":git.list_worktrees(&auth.task).await.map_err(|e|e.to_string())?}),
                    ),
                    ("worktree", Some("remove")) => {
                        git.remove(&auth.task, required_str(args, "owner")?)
                            .await
                            .map_err(|e| e.to_string())?;
                        Ok(json!({"removed":true}))
                    }
                    ("worktree", Some("checkpoint")) => Ok(
                        json!({"commit":self.checkpoint_child(&auth.task,&auth.identity,required_str(args,"owner")?,required_str(args,"message")?).await?}),
                    ),
                    ("integration_queue", Some("checkpoint")) => Ok(
                        json!({"commit":self.checkpoint_child(&auth.task,&auth.identity,required_str(args,"owner")?,required_str(args,"message")?).await?}),
                    ),
                    ("integration_queue", Some("enqueue")) => Ok(
                        json!({"id":git.enqueue(&auth.task,required_str(args,"owner")?,required_str(args,"intent")?).await.map_err(|e|e.to_string())?}),
                    ),
                    ("integration_queue", Some("list")) => {
                        Ok(json!({"queue":git.list(&auth.task).await.map_err(|e|e.to_string())?}))
                    }
                    ("integration_queue", Some("process")) => Ok(
                        json!({"report":git.process_next(&auth.task,&auth.identity,auth.checks.as_ref()).await.map_err(|e|e.to_string())?}),
                    ),
                    ("integration_queue", Some("retry")) => {
                        let id = required_str(args, "id")?;
                        if !git
                            .list(&auth.task)
                            .await
                            .map_err(|e| e.to_string())?
                            .iter()
                            .any(|item| item.id == id)
                        {
                            return Err(
                                "queue entry does not belong to the authorized parent task".into(),
                            );
                        }
                        git.retry(id, &auth.identity)
                            .await
                            .map_err(|e| e.to_string())?;
                        Ok(json!({"queued":true}))
                    }
                    _ => Err("unsupported workspace operation".into()),
                }
            }
            "code_graph" => self.query_graph(args).await,
            "lsp" if args["action"] == "rename" => self.perform_edits(run, key, tool).await,
            "lsp" => self.query_lsp(args).await,
            _ => Err("unknown project domain tool".into()),
        }
    }
}
impl LocalTools {
    pub(crate) async fn project_receipt_schema(
        db: &excalibur_store::Database,
    ) -> Result<(), String> {
        const SQL: &str = "CREATE TABLE IF NOT EXISTS project_tool_receipts(run_id TEXT NOT NULL,operation_key TEXT NOT NULL,call_hash TEXT NOT NULL,result TEXT NOT NULL,PRIMARY KEY(run_id,operation_key));";
        let checksum = hash(SQL.as_bytes());
        db.write(excalibur_store::Priority::Journal, move |c| {
            use excalibur_store::rusqlite::OptionalExtension;
            let old: Option<String> = c
                .query_row(
                    "SELECT checksum FROM tool_schema WHERE version=3",
                    [],
                    |r| r.get(0),
                )
                .optional()?;
            if old.as_ref().is_some_and(|v| v != &checksum) {
                return Err(excalibur_store::Error::Invalid(
                    "project tool schema checksum mismatch",
                ));
            }
            c.execute_batch(SQL)?;
            c.execute("INSERT OR IGNORE INTO tool_schema VALUES(3,?1)", [checksum])?;
            Ok(())
        })
        .await
        .map_err(|e| e.to_string())
    }
    async fn project_receipt(
        &self,
        run: &str,
        key: &str,
        tool: &PreparedTool,
    ) -> Result<Option<Value>, String> {
        let (run, key, digest) = (
            run.to_owned(),
            key.to_owned(),
            hash(
                serde_json::to_string(&tool.call)
                    .map_err(|e| e.to_string())?
                    .as_bytes(),
            ),
        );
        self.db.read(excalibur_store::ReadLane::Interactive,move|c|{use excalibur_store::rusqlite::OptionalExtension;let old:Option<(String,String)>=c.query_row("SELECT call_hash,result FROM project_tool_receipts WHERE run_id=?1 AND operation_key=?2",[run,key],|r|Ok((r.get(0)?,r.get(1)?))).optional()?;match old{Some((expected,result)) if expected==digest=>Ok(Some(serde_json::from_str::<Value>(&result).map_err(|_|excalibur_store::Error::Invalid("project receipt invalid"))?)),Some(_)=>Err(excalibur_store::Error::Invalid("project operation key reused")),None=>Ok(None)}}).await.map_err(|e|e.to_string())
    }
    async fn scoped_snapshots(
        &self,
        run: &str,
    ) -> Result<Vec<excalibur_workspace::Snapshot>, String> {
        let snapshots = self
            .workspace
            .shadow()
            .list_snapshots(run)
            .await
            .map_err(|e| e.to_string())?;
        if self.path_scope.is_none() {
            return Ok(snapshots);
        }
        let mut allowed = Vec::new();
        for snapshot in snapshots {
            let paths = self
                .workspace
                .shadow()
                .snapshot_paths(&snapshot.id)
                .await
                .map_err(|e| e.to_string())?;
            if paths.iter().all(|path| {
                self.scope_allows(&self.root.join(path))
                    && self.check_write_path(&path.to_string_lossy()).is_ok()
            }) {
                allowed.push(snapshot);
            }
        }
        Ok(allowed)
    }
    pub(crate) async fn execute_project_operation(
        &self,
        run: &str,
        key: &str,
        tool: &PreparedTool,
    ) -> Result<Value, String> {
        if let Some(result) = self.project_receipt(run, key, tool).await? {
            return Ok(result);
        }
        let result = self.execute_domain(run, key, tool).await?;
        let result = self.policy.redactor.redact_json(&result);
        let (run, key, digest, result_json) = (
            run.to_owned(),
            key.to_owned(),
            hash(
                serde_json::to_string(&tool.call)
                    .map_err(|e| e.to_string())?
                    .as_bytes(),
            ),
            result.to_string(),
        );
        self.db
            .write(excalibur_store::Priority::Journal, move |c| {
                c.execute(
                    "INSERT INTO project_tool_receipts VALUES(?1,?2,?3,?4)",
                    excalibur_store::rusqlite::params![run, key, digest, result_json],
                )?;
                Ok(())
            })
            .await
            .map_err(|e| e.to_string())?;
        Ok(result)
    }
    pub(crate) async fn reconcile_project_operation(
        &self,
        run: &str,
        key: &str,
        tool: &PreparedTool,
    ) -> Reconcile {
        match self.project_receipt(run,key,tool).await {Ok(Some(value))=>Reconcile::Done(ToolResult::ok(value)),Ok(None)=>Reconcile::Uncertain("project operation has no confirmed receipt; inspect the worktree or integration queue before resuming".into()),Err(error)=>Reconcile::Uncertain(error)}
    }
}
/// Metadata for the implemented project-domain runtime; no placeholder external connectors.
pub fn deferred_catalog() -> Vec<crate::deferred::CatalogTool> {
    use crate::deferred::{CancellationClass, CatalogTool, DryRunBehavior, ToolEffect, ToolRisk};
    definitions()
        .into_iter()
        .map(|definition| {
            let mut tool = CatalogTool::from_definition(definition);
            tool.risk = ToolRisk::Low;
            tool.platforms = vec!["host_filesystem".into()];
            tool.tags = match tool.name.as_str() {
                "file_history" => vec!["file", "version", "history"],
                "revert_edit" => vec!["undo", "restore", "merge", "edit"],
                "workspace_snapshot" => vec!["command", "shadow", "snapshot", "diff", "restore"],
                "worktree" => vec!["git", "branch", "parallel", "isolation"],
                "integration_queue" => vec!["git", "rebase", "integration", "test"],
                "code_graph" => vec!["code", "callers", "dependencies", "impact", "tests"],
                _ => vec![
                    "language",
                    "definition",
                    "references",
                    "symbols",
                    "diagnostics",
                    "rename",
                ],
            }
            .into_iter()
            .map(str::to_owned)
            .collect();
            if !matches!(tool.name.as_str(), "file_history" | "code_graph") {
                tool.effect = ToolEffect::WriteLocal;
                tool.risk = ToolRisk::Medium;
                tool.idempotency = if matches!(tool.name.as_str(), "worktree" | "integration_queue")
                {
                    Effect::NonIdempotent
                } else {
                    Effect::Idempotent
                };
                tool.timeout_ms = 120_000;
                tool.cancel = CancellationClass::AfterCompletion;
                tool.dry_run = DryRunBehavior::Adapter;
                if matches!(
                    tool.name.as_str(),
                    "revert_edit" | "workspace_snapshot" | "lsp"
                ) {
                    tool.probe = Some("fingerprint_pre_post".into());
                    tool.inverse = Some("file_history/revert_edit".into());
                }
            }
            tool
        })
        .collect()
}
/// Reader rooted in a registered child worktree. It grants no process or edit authority.
struct WorktreeCheckpointReader(LocalTools);
impl excalibur_workspace::CheckpointEntriesGuard for WorktreeCheckpointReader {
    fn verify<'a>(
        &'a self,
        tree: &'a excalibur_workspace::Worktree,
        expected: &'a BTreeMap<PathBuf, Option<excalibur_workspace::SnapshotEntry>>,
    ) -> BoxFuture<'a, excalibur_workspace::Result<()>> {
        Box::pin(async move {
            if tree.path != self.0.root {
                return Err(excalibur_workspace::Error::Invalid(
                    "checkpoint root changed",
                ));
            }
            let captured = self.0.authorized_snapshot_entries().map_err(|_| {
                excalibur_workspace::Error::Invalid("checkpoint file authority changed")
            })?;
            for (path, wanted) in expected {
                let path = self
                    .0
                    .resolve_entry(path.to_str().ok_or(excalibur_workspace::Error::Invalid(
                        "invalid checkpoint path",
                    ))?)
                    .map_err(|_| {
                        excalibur_workspace::Error::Invalid("checkpoint path authority changed")
                    })?;
                if let Some(wanted) = wanted {
                    if captured.get(Path::new(&path)) != Some(wanted) {
                        return Err(excalibur_workspace::Error::Invalid(
                            "checkpoint content changed after authorized read",
                        ));
                    }
                } else {
                    match self.0.directory.symlink_metadata(&path) {
                        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                        _ => {
                            return Err(excalibur_workspace::Error::Invalid(
                                "checkpoint deletion changed after authorized read",
                            ));
                        }
                    }
                }
            }
            Ok(())
        })
    }
}
impl LocalTools {
    fn checkpoint_reader(
        &self,
        tree: &excalibur_workspace::Worktree,
    ) -> Result<WorktreeCheckpointReader, String> {
        let (root, directory) = crate::files::open_root(tree.path.clone())?;
        if root != tree.path {
            return Err("registered worktree moved".into());
        }
        let mapped_scope = self.scope_roots().map(|paths| {
            paths
                .iter()
                .filter_map(|p| {
                    p.strip_prefix(&self.root)
                        .ok()
                        .map(|relative| root.join(relative))
                })
                .collect::<Vec<_>>()
        });
        let mapped_protected = self
            .policy
            .protected_paths
            .iter()
            .filter_map(|p| {
                p.strip_prefix(&self.root)
                    .ok()
                    .map(|relative| root.join(relative))
            })
            .collect::<Vec<_>>();
        let mut reader = self.clone();
        reader.path_scope = None;
        reader.root = root.clone();
        reader.directory = Arc::new(directory);
        reader.intelligence = None;
        // The private data directory is an ancestor of task worktrees. Its other files
        // remain outside this descriptor root; private hardlinks fail the inventory.
        reader
            .policy
            .protected_paths
            .retain(|p| !root.starts_with(p));
        reader.policy.protected_paths.extend(mapped_protected);
        if let Some(roots) = mapped_scope {
            reader = reader.restrict_scope_roots(roots)?;
        }
        Ok(WorktreeCheckpointReader(reader))
    }
    async fn checkpoint_child(
        &self,
        task: &str,
        identity: &GitIdentity,
        owner: &str,
        message: &str,
    ) -> Result<String, String> {
        let git = self.workspace.git();
        let tree = git.worktree(task, owner).await.map_err(|e| e.to_string())?;
        let reader = self.checkpoint_reader(&tree)?;
        let mut files = reader
            .0
            .authorized_snapshot_entries()?
            .into_iter()
            .map(|(p, entry)| (p, Some(entry)))
            .collect::<BTreeMap<_, _>>();
        for path in git
            .list_tracked_paths(task, owner)
            .await
            .map_err(|e| e.to_string())?
        {
            if files.contains_key(&path)
                || excalibur_sandbox::file_authority::credential_path(&path)
            {
                continue;
            }
            let Ok(path) = reader
                .0
                .resolve_entry(path.to_str().ok_or("invalid tracked path")?)
            else {
                continue;
            };
            if reader.0.check_write_path(&path).is_err() {
                continue;
            }
            if matches!(reader.0.directory.symlink_metadata(&path),Err(error) if error.kind()==std::io::ErrorKind::NotFound)
            {
                files.insert(PathBuf::from(path), None);
            }
        }
        git.checkpoint_entries(task, owner, message, identity, files, &reader)
            .await
            .map_err(|e| e.to_string())
    }
}
