//! Host-bound per-run workspaces. Agent hints select only durable worktrees
//! already created for their own parent task under a host-managed base.
use excalibur_orchestrator::{Agent, Project};
use excalibur_security::{CapabilityToken, Root};
use excalibur_store::{
    Database, Priority, ReadLane,
    rusqlite::{self, OptionalExtension},
};
use excalibur_tools::{
    LocalTools, ToolPolicy,
    project::{ChildAuthority, ParentAuthority},
};
use excalibur_types::runtime::*;
use excalibur_workspace::{GitIdentity, IntegrationChecks};
use serde_json::Value;
use std::{
    collections::BTreeMap,
    path::PathBuf,
    sync::{Arc, Mutex},
};

#[derive(Clone)]
pub struct GitOptions {
    pub identity: GitIdentity,
    pub checks: Arc<dyn IntegrationChecks>,
}
#[derive(Clone, Debug)]
pub struct WorkerBinding {
    pub workspace: PathBuf,
    pub token: CapabilityToken,
}
#[derive(Clone)]
struct Route {
    local: Arc<LocalTools>,
    source: Arc<LocalTools>,
    allowed: Vec<PathBuf>,
    denied: Vec<PathBuf>,
    owner: Option<(String, String)>,
}
pub struct WorkerTools {
    db: Database,
    default: Arc<LocalTools>,
    policy: ToolPolicy,
    git: Option<GitOptions>,
    sources: BTreeMap<String, Arc<LocalTools>>,
    bases: BTreeMap<String, PathBuf>,
    general_denied: Vec<PathBuf>,
    project_roots: BTreeMap<String, Vec<PathBuf>>,
    privacy_roots: BTreeMap<String, Vec<PathBuf>>,
    routes: Mutex<BTreeMap<String, Route>>,
    bind_gate: tokio::sync::Mutex<()>,
    cgroup_root: Option<PathBuf>,
}
impl WorkerTools {
    pub async fn new(
        db: Database,
        default: Arc<LocalTools>,
        policy: ToolPolicy,
        projects: Vec<Project>,
        git: Option<GitOptions>,
    ) -> Result<Self, String> {
        let project_roots = projects
            .iter()
            .map(|project| {
                Ok((
                    format!("project:{}", project.id),
                    project
                        .roots
                        .iter()
                        .map(|p| p.canonicalize().map_err(|e| e.to_string()))
                        .collect::<Result<Vec<_>, String>>()?,
                ))
            })
            .collect::<Result<BTreeMap<_, _>, String>>()?;
        // Persistent registration is also a privacy boundary. Omitted startup
        // projects do not get a runtime or a grant, but their data stays private.
        let mut privacy_roots = db
            .read(ReadLane::Interactive, |c| {
                let mut statement = c.prepare("SELECT id,COALESCE(roots,'[]') FROM projects")?;
                let rows = statement
                    .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
                    .collect::<Result<Vec<_>, _>>()?;
                rows.into_iter()
                    .map(|(id, json)| {
                        let roots: Vec<PathBuf> = serde_json::from_str(&json).map_err(|_| {
                            excalibur_store::Error::Invalid("registered project roots invalid")
                        })?;
                        if roots.iter().any(|path| !path.is_absolute()) {
                            return Err(excalibur_store::Error::Invalid(
                                "registered project roots must be absolute",
                            ));
                        }
                        Ok((format!("project:{id}"), roots))
                    })
                    .collect::<excalibur_store::Result<BTreeMap<_, _>>>()
            })
            .await
            .map_err(|e| e.to_string())?;
        privacy_roots.extend(project_roots.clone());
        let mut general_denied = privacy_roots
            .values()
            .flatten()
            .cloned()
            .collect::<Vec<_>>();
        general_denied.sort();
        general_denied.dedup();
        let general = if general_denied.is_empty() {
            default.clone()
        } else {
            let mut general_policy = policy.clone();
            general_policy
                .protected_paths
                .extend(general_denied.clone());
            Arc::new(
                LocalTools::new(
                    db.clone(),
                    default.workspace().root().to_path_buf(),
                    general_policy,
                )
                .await?,
            )
        };
        let mut sources = BTreeMap::from([("general".into(), general)]);
        for project in projects {
            let root = project
                .roots
                .first()
                .ok_or("project root required")?
                .canonicalize()
                .map_err(|e| e.to_string())?;
            if !root.starts_with(default.workspace().root()) {
                return Err("project root exceeds host runtime root".into());
            }
            excalibur_tools::terminal::prepare_project_boundary(&root)?;
            let source = if root == default.workspace().root() {
                default.clone()
            } else {
                let mut policy = policy.clone();
                if let Some(sandbox) = &mut policy.sandbox {
                    sandbox.workspace = root.clone();
                    sandbox.read_roots = vec![root.clone()];
                    if !sandbox.write_roots.is_empty() {
                        sandbox.write_roots = vec![root.clone()];
                    }
                }
                Arc::new(LocalTools::new(db.clone(), root, policy).await?)
            };
            sources.insert(format!("project:{}", project.id), source);
        }
        let mut bases = BTreeMap::new();
        if git.is_some() {
            for (scope, source) in &sources {
                if scope == "general" && !general_denied.is_empty() {
                    continue;
                }
                let base = source.workspace().managed_worktree_root();
                std::fs::create_dir_all(&base).map_err(|e| e.to_string())?;
                bases.insert(
                    scope.clone(),
                    base.canonicalize().map_err(|e| e.to_string())?,
                );
            }
        }
        db.write(Priority::Journal,|c|{c.execute_batch("CREATE TABLE IF NOT EXISTS daemon_worker_bindings(run_id TEXT PRIMARY KEY REFERENCES runs(id),agent_id TEXT UNIQUE NOT NULL,source_root TEXT NOT NULL,workspace TEXT NOT NULL,parent_task TEXT NOT NULL,owner TEXT,scope TEXT NOT NULL);CREATE UNIQUE INDEX IF NOT EXISTS daemon_worktree_owner ON daemon_worker_bindings(source_root,parent_task,owner) WHERE owner IS NOT NULL;")?;Ok(())}).await.map_err(|e|e.to_string())?;
        Ok(Self {
            db,
            default,
            policy,
            git,
            sources,
            bases,
            general_denied,
            project_roots,
            privacy_roots,
            routes: Mutex::new(BTreeMap::new()),
            bind_gate: tokio::sync::Mutex::new(()),
            cgroup_root: None,
        })
    }
    /// Service-manager delegated cgroup root; no controller is enabled implicitly.
    pub fn with_cgroup_root(mut self, root: Option<PathBuf>) -> Result<Self, String> {
        if let Some(root) = root {
            #[cfg(not(target_os = "linux"))]
            {
                let _ = root;
                return Err("delegated cgroups require Linux".into());
            }
            #[cfg(target_os = "linux")]
            {
                use std::os::unix::ffi::OsStrExt;
                let root = root.canonicalize().map_err(|e| e.to_string())?;
                let path = std::ffi::CString::new(root.as_os_str().as_bytes())
                    .map_err(|_| "invalid cgroup root")?;
                let mut info: libc::statfs = unsafe { std::mem::zeroed() };
                if unsafe { libc::statfs(path.as_ptr(), &mut info) } < 0
                    || info.f_type != 0x63677270
                {
                    return Err("delegated root must be a real cgroup v2 filesystem".into());
                }
                let controllers = std::fs::read_to_string(root.join("cgroup.subtree_control"))
                    .map_err(|e| e.to_string())?;
                if !controllers
                    .split_whitespace()
                    .any(|controller| controller == "cpu")
                {
                    return Err("CPU controller is not delegated to child cgroups".into());
                }
                self.cgroup_root = Some(root);
            }
        }
        Ok(self)
    }
    fn bind_cgroup(&self, run: &str, local: &LocalTools) -> Result<(), String> {
        if let Some(root) = &self.cgroup_root {
            if run.len() > 128 {
                return Err("invalid execution id for cgroup".into());
            }
            let name = run
                .as_bytes()
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>();
            let directory = root.join(format!("excalibur-{name}"));
            match std::fs::create_dir(&directory) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(format!("execution cgroup cannot be created: {error}")),
            }
            if directory.canonicalize().map_err(|e| e.to_string())? != directory {
                return Err("execution cgroup must not be a symlink".into());
            }
            local.terminals().bind_cgroup(run, directory)?;
        }
        Ok(())
    }
    pub fn workspace_for(&self, run: &str) -> Result<PathBuf, String> {
        Ok(self.route(run)?.local.workspace().root().to_path_buf())
    }
    pub async fn verifier_tools(&self, run: &str) -> Result<Arc<LocalTools>, String> {
        let route = self.route(run)?;
        let root = route.local.workspace().root().to_path_buf();
        let mut policy = self.policy.clone();
        policy.protected_paths.extend(route.denied.clone());
        policy.write = excalibur_tools::Access::Deny;
        policy.exec = excalibur_tools::Access::Allow;
        let mut sandbox = excalibur_tools::terminal::SandboxConfig::read_only(root.clone());
        sandbox.read_roots = route.allowed.clone();
        policy.sandbox = Some(sandbox);
        let tools = if let Some((task, owner)) = &route.owner {
            LocalTools::for_registered_worktree(
                self.db.clone(),
                route.source.workspace(),
                task,
                owner,
                policy,
            )
            .await?
        } else {
            LocalTools::new(self.db.clone(), root, policy).await?
        };
        self.bind_cgroup(run, &tools)?;
        Ok(Arc::new(tools.restrict_scope_roots(route.allowed.clone())?))
    }
    pub fn managed_worktree_roots(&self) -> BTreeMap<String, PathBuf> {
        self.bases.clone()
    }
    pub async fn bind_agent(
        &self,
        agent: &Agent,
        parent: Option<&Agent>,
    ) -> Result<WorkerBinding, String> {
        let _guard = self.bind_gate.lock().await;
        let run = agent.run_id.as_ref().ok_or("worker run required")?;
        let mut source = self
            .sources
            .get(&agent.scope)
            .cloned()
            .ok_or("no runtime registered for agent scope")?;
        if parent.is_some_and(|p| agent.parent_id.as_deref() != Some(&p.id)) {
            return Err("worker parent binding mismatch".into());
        }
        let granted = self
            .project_roots
            .iter()
            .filter(|(scope, _)| agent.token.data_scopes.contains(*scope))
            .map(|(scope, roots)| (scope.clone(), roots.clone()))
            .collect::<BTreeMap<_, _>>();
        if agent.scope.starts_with("project:") && !granted.contains_key(&agent.scope) {
            return Err("agent project scope is absent from its capability token".into());
        }
        let explicit = granted.len() > 1 || (agent.scope == "general" && !granted.is_empty());
        let granted_roots = granted.values().flatten().cloned().collect::<Vec<_>>();
        let mut denied = if agent.scope == "general" && !explicit {
            self.general_denied.clone()
        } else {
            self.privacy_roots
                .iter()
                .filter(|(scope, _)| !granted.contains_key(*scope))
                .flat_map(|(_, roots)| roots.iter())
                .filter(|root| {
                    !granted_roots.iter().any(|allowed| allowed == *root)
                        && (!granted_roots
                            .iter()
                            .any(|allowed| allowed.starts_with(root))
                            || granted_roots
                                .iter()
                                .any(|allowed| root.starts_with(allowed)))
                })
                .cloned()
                .collect::<Vec<_>>()
        };
        for root in &granted_roots {
            if denied
                .iter()
                .any(|path| path != root && path.starts_with(root))
            {
                let metadata = root.join(".git");
                if metadata.exists() {
                    denied.push(metadata);
                }
            }
        }
        // Source Git authority is narrowed to the current source repository; the
        // common runtime below is used only for explicitly granted shared work.
        let source_allowed = narrow_roots(&agent.token.roots, source.workspace().root())
            .into_iter()
            .map(|r| r.path)
            .collect::<Vec<_>>();
        let source_whole = source_allowed
            .iter()
            .any(|root| root == source.workspace().root());
        if !source_allowed.is_empty() {
            source = Arc::new(
                (*source)
                    .clone()
                    .restrict_scope_paths(source_allowed, denied.clone())?,
            );
        }
        let owner = agent.delegation.as_ref().and_then(|d| {
            d.inputs
                .iter()
                .find_map(|s| s.strip_prefix("worktree-owner:").map(str::to_owned))
        });
        let mut token = agent.token.clone();
        let mut workspace = source.workspace().root().to_path_buf();
        let mut parent_task = agent.task.clone();
        let allowed: Vec<PathBuf>;
        let mut child_owner = None;

        let local = if let Some(owner) = &owner {
            if agent.depth != 2 {
                return Err("only a worker may select its parent's worktree".into());
            }
            let parent = parent.ok_or("parent agent required for worktree")?;
            if parent.depth != 1 || parent.scope != agent.scope {
                return Err("worktree scope or parent role mismatch".into());
            }
            let git = self
                .git
                .as_ref()
                .ok_or("host Git identity and integration checks are not configured")?;
            parent_task = parent.task.clone();
            let tree = source
                .workspace()
                .git()
                .worktree(&parent.task, owner)
                .await
                .map_err(|e| e.to_string())?;
            let base = self
                .bases
                .get(&agent.scope)
                .ok_or("worktree base is not authorized")?;
            let canonical = tree.path.canonicalize().map_err(|e| e.to_string())?;
            if canonical != tree.path || canonical == *base || !canonical.starts_with(base) {
                return Err("registered worktree escaped its host base".into());
            }
            let roots = narrow_roots(&token.roots, &canonical);
            if roots.is_empty() {
                return Err("requested token does not authorize the registered worktree".into());
            }
            token.roots = roots;
            workspace = canonical;
            allowed = token.roots.iter().map(|r| r.path.clone()).collect();
            denied = denied
                .into_iter()
                .filter_map(|p| {
                    p.strip_prefix(source.workspace().root())
                        .ok()
                        .map(|relative| workspace.join(relative))
                })
                .collect();
            child_owner = Some((parent.task.clone(), owner.clone()));
            token.validate()?;
            self.claim_binding(
                agent,
                &source,
                &workspace,
                &parent_task,
                Some(owner.as_str()),
            )
            .await?;
            excalibur_tools::terminal::prepare_project_boundary(&workspace)?;
            source.authorize_child(
                run,
                ChildAuthority {
                    task: parent.task.clone(),
                    owner: owner.clone(),
                    identity: git.identity.clone(),
                },
            )?;
            if let Ok(existing) = self.route(run) {
                if existing.local.workspace().root() != workspace {
                    return Err("worker runtime cannot change workspace".into());
                }
                existing.local
            } else {
                let mut child_policy = self.policy.clone();
                child_policy.protected_paths.extend(denied.clone());
                Arc::new(
                    LocalTools::for_registered_worktree(
                        self.db.clone(),
                        source.workspace(),
                        &parent.task,
                        owner,
                        child_policy,
                    )
                    .await?
                    .restrict_scope_roots(allowed.clone())?,
                )
            }
        } else {
            let roots = if granted_roots.is_empty() {
                narrow_roots(&token.roots, source.workspace().root())
            } else {
                granted_roots
                    .iter()
                    .flat_map(|root| narrow_roots(&token.roots, root))
                    .collect::<Vec<_>>()
            };
            allowed = roots.iter().map(|r| r.path.clone()).collect();
            if allowed.is_empty() {
                return Err("worker has no authorized registered project paths".into());
            }
            if agent.depth == 2 {
                token.roots = roots;
            } else {
                let mut effective = roots;
                if let Some(base) = self.bases.get(&agent.scope) {
                    effective.extend(narrow_roots(&token.roots, base));
                }
                token.roots = effective;
            }
            if agent.depth == 1
                && source_whole
                && self.bases.contains_key(&agent.scope)
                && let Some(git) = &self.git
            {
                source.authorize_parent(
                    run,
                    ParentAuthority {
                        task: agent.task.clone(),
                        identity: git.identity.clone(),
                        checks: git.checks.clone(),
                    },
                )?;
            }
            if explicit
                || allowed
                    .iter()
                    .any(|root| !root.starts_with(source.workspace().root()))
            {
                workspace = self.default.workspace().root().to_path_buf();
                Arc::new(
                    (*self.default)
                        .clone()
                        .restrict_scope_paths(allowed.clone(), denied.clone())?,
                )
            } else {
                Arc::new((*source).clone().restrict_scope_roots(allowed.clone())?)
            }
        };
        token.validate()?;
        self.bind_cgroup(run, &local)?;
        self.claim_binding(agent, &source, &workspace, &parent_task, owner.as_deref())
            .await?;
        let mut routes = self
            .routes
            .lock()
            .map_err(|_| "worker route lock poisoned")?;
        if let Some(old) = routes.get(run) {
            if old.local.workspace().root() != local.workspace().root() {
                return Err("worker runtime cannot be rebound".into());
            }
        } else {
            routes.insert(
                run.clone(),
                Route {
                    local,
                    source,
                    allowed,
                    denied,
                    owner: child_owner,
                },
            );
        }
        Ok(WorkerBinding { workspace, token })
    }
    async fn claim_binding(
        &self,
        agent: &Agent,
        source: &LocalTools,
        workspace: &std::path::Path,
        parent_task: &str,
        owner: Option<&str>,
    ) -> Result<(), String> {
        let (run_copy, agent_id, source_root, path, task, owner_copy, scope) = (
            agent.run_id.clone().ok_or("worker run required")?,
            agent.id.clone(),
            source.workspace().root().to_string_lossy().to_string(),
            workspace.to_string_lossy().to_string(),
            parent_task.to_owned(),
            owner.map(str::to_owned),
            agent.scope.clone(),
        );
        self.db.write(Priority::Journal,move|c|{
            let old:Option<(String,String,String,String,Option<String>,String)>=c.query_row("SELECT agent_id,source_root,workspace,parent_task,owner,scope FROM daemon_worker_bindings WHERE run_id=?1",[&run_copy],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?))).optional()?;
            if old.as_ref().is_some_and(|old|old!=&(agent_id.clone(),source_root.clone(),path.clone(),task.clone(),owner_copy.clone(),scope.clone())){return Err(excalibur_store::Error::Invalid("worker runtime is already bound to another workspace"));}
            c.execute("INSERT OR IGNORE INTO daemon_worker_bindings VALUES(?1,?2,?3,?4,?5,?6,?7)",rusqlite::params![run_copy,agent_id,source_root,path,task,owner_copy,scope])?;
            let bound:Option<String>=c.query_row("SELECT agent_id FROM daemon_worker_bindings WHERE run_id=?1",[&run_copy],|r|r.get(0)).optional()?;if bound.as_deref()!=Some(&agent_id){return Err(excalibur_store::Error::Invalid("worktree owner is already assigned to another agent"));}Ok(())
        }).await.map_err(|e|e.to_string())?;
        Ok(())
    }
    fn route(&self, run: &str) -> Result<Route, String> {
        self.routes
            .lock()
            .map_err(|_| "worker route lock poisoned")?
            .get(run)
            .cloned()
            .ok_or_else(|| "worker runtime has not been bound by the host".into())
    }
    fn target(&self, run: &str, name: &str) -> Result<Arc<LocalTools>, String> {
        let route = self.route(run)?;
        Ok(if matches!(name, "worktree" | "integration_queue") {
            route.source
        } else {
            route.local
        })
    }
    pub fn live_status(&self, run: &str) -> Vec<Value> {
        self.route(run)
            .map(|r| r.local.live_status(run))
            .unwrap_or_default()
    }
    pub async fn interrupt_tool(&self, run: &str) -> Result<bool, String> {
        self.route(run)?.local.terminals().cancel_run(run).await;
        Ok(true)
    }
    pub async fn kill_largest_tool(&self, run: &str) -> Result<bool, String> {
        Ok(self
            .route(run)?
            .local
            .terminals()
            .terminate_largest(run)
            .await?
            .is_some())
    }
    pub fn with_project_services(
        mut self,
        services: BTreeMap<String, Arc<excalibur_tools::intelligence::ProjectIntelligence>>,
    ) -> Result<Self, String> {
        for (scope, services) in services {
            if !scope.starts_with("project:") {
                return Err("language services require a provisioned project scope".into());
            }
            let local = self
                .sources
                .get(&scope)
                .ok_or("language service scope is not provisioned")?;
            self.sources.insert(
                scope,
                Arc::new(local.as_ref().clone().with_intelligence(services)?),
            );
        }
        Ok(self)
    }
    pub fn active_processes(&self) -> usize {
        let runs = self
            .routes
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .keys()
            .cloned()
            .collect::<Vec<_>>();
        runs.iter()
            .map(|run| {
                self.live_status(run)
                    .iter()
                    .filter(|status| status["exited"] == false)
                    .count()
            })
            .sum()
    }
    pub async fn unload_idle(&self) -> Result<usize, String> {
        let mut locals = self.sources.values().cloned().collect::<Vec<_>>();
        locals.extend(
            self.routes
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .values()
                .map(|r| r.local.clone()),
        );
        let mut seen = std::collections::BTreeSet::new();
        let mut count = 0;
        for local in locals {
            if let Some(services) = local.project_intelligence()
                && let Some(lsp) = &services.lsp
                && seen.insert(Arc::as_ptr(lsp) as usize)
            {
                count += lsp.unload_idle().await;
            }
        }
        Ok(count)
    }
    pub async fn shutdown(&self) {
        let _ = self.unload_idle().await;
        let mut all = self.sources.values().cloned().collect::<Vec<_>>();
        if let Ok(routes) = self.routes.lock() {
            all.extend(routes.values().map(|r| r.local.clone()));
        }
        let mut seen = std::collections::BTreeSet::new();
        for runtime in all {
            if seen.insert(runtime.workspace().root().to_path_buf()) {
                runtime.terminals().shutdown().await;
            }
        }
    }
}
fn narrow_roots(roots: &[Root], workspace: &std::path::Path) -> Vec<Root> {
    let mut result = roots
        .iter()
        .filter_map(|root| {
            if workspace.starts_with(&root.path) {
                Some(Root {
                    path: workspace.to_path_buf(),
                    write: root.write,
                })
            } else if root.path.starts_with(workspace) {
                Some(root.clone())
            } else {
                None
            }
        })
        .collect::<Vec<_>>();
    result.sort_by(|a, b| a.path.cmp(&b.path).then(a.write.cmp(&b.write)));
    result.dedup_by(|a, b| a == b);
    result
}
impl ToolRuntime for WorkerTools {
    fn filesystem_confinement(&self, run: &str) -> Option<FilesystemConfinement> {
        self.route(run).ok()?.local.filesystem_confinement(run)
    }

    fn definitions(&self) -> Vec<ToolDefinition> {
        self.default.definitions()
    }
    fn run_definitions<'a>(
        &'a self,
        run: &'a str,
    ) -> BoxFuture<'a, Result<Option<Vec<ToolDefinition>>, String>> {
        Box::pin(async move { Ok(Some(self.route(run)?.local.definitions())) })
    }
    fn prepare<'a>(
        &'a self,
        run: &'a str,
        call: ToolCall,
    ) -> BoxFuture<'a, Result<PreparedTool, String>> {
        Box::pin(async move { self.target(run, &call.name)?.prepare(run, call).await })
    }
    fn execute<'a>(
        &'a self,
        run: &'a str,
        key: &'a str,
        tool: &'a PreparedTool,
    ) -> BoxFuture<'a, ToolResult> {
        Box::pin(async move {
            match self.target(run, &tool.call.name) {
                Ok(runtime) => runtime.execute(run, key, tool).await,
                Err(error) => ToolResult::error(error),
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
            match self.target(run, &tool.call.name) {
                Ok(runtime) => runtime.reconcile(run, key, tool).await,
                Err(error) => Reconcile::Uncertain(error),
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
            if let Ok(route) = self.route(run) {
                route.local.execution_state(run, state, reason).await?;
            }
            Ok(())
        })
    }
    fn begin_step<'a>(&'a self, run: &'a str, step: u64) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async move { self.route(run)?.local.begin_step(run, step).await })
    }
    fn end_step<'a>(&'a self, run: &'a str, step: u64) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async move { self.route(run)?.local.end_step(run, step).await })
    }
    fn interrupt_execution<'a>(
        &'a self,
        run: &'a str,
        cancelled: bool,
        tool: Option<&'a PreparedTool>,
    ) -> BoxFuture<'a, Result<bool, String>> {
        Box::pin(async move {
            if let Some(tool) = tool {
                self.target(run, &tool.call.name)?
                    .interrupt_execution(run, cancelled, Some(tool))
                    .await
            } else {
                self.route(run)?
                    .local
                    .interrupt_execution(run, cancelled, None)
                    .await
            }
        })
    }
    fn configure_execution_limits<'a>(
        &'a self,
        run: &'a str,
        limits: ExecutionLimits,
    ) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async move {
            self.route(run)?
                .local
                .configure_execution_limits(run, limits)
                .await
        })
    }
    fn execution_resources<'a>(
        &'a self,
        run: &'a str,
    ) -> BoxFuture<'a, Result<Option<ExecutionResources>, String>> {
        Box::pin(async move { self.route(run)?.local.execution_resources(run).await })
    }
    fn sanitize_durable(&self, value: &Value) -> Result<Value, String> {
        self.default.sanitize_durable(value)
    }
    fn pending_approval<'a>(&'a self, run: &'a str) -> BoxFuture<'a, bool> {
        Box::pin(async move {
            if let Ok(route) = self.route(run) {
                route.local.pending_approval(run).await
            } else {
                false
            }
        })
    }
    fn approval_blocked<'a>(&'a self, run: &'a str) -> BoxFuture<'a, Option<String>> {
        Box::pin(async move {
            if let Ok(route) = self.route(run) {
                route.local.approval_blocked(run).await
            } else {
                None
            }
        })
    }
    fn observe_untrusted<'a>(
        &'a self,
        run: &'a str,
        source: &'a str,
    ) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async move { self.route(run)?.local.observe_untrusted(run, source).await })
    }
    fn configure_process_boundary<'a>(
        &'a self,
        run: &'a str,
        mut boundary: ProcessBoundary,
    ) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async move {
            let route = self.route(run)?;
            let root = route.local.workspace().root().to_path_buf();
            if boundary
                .workspace
                .canonicalize()
                .map_err(|e| e.to_string())?
                != root
            {
                return Err("process workspace differs from its bound runtime".into());
            }
            boundary.denied_paths.extend(route.denied.clone());
            boundary.read_roots = narrow_paths(&boundary.read_roots, &root);
            boundary.write_roots = narrow_paths(&boundary.write_roots, &root);
            route.local.configure_process_boundary(run, boundary).await
        })
    }
    fn execution_tool_budget<'a>(
        &'a self,
        run: &'a str,
    ) -> BoxFuture<'a, Result<Option<u64>, String>> {
        Box::pin(async move { self.route(run)?.local.execution_tool_budget(run).await })
    }
    fn result_requires_approval(&self, result: &ToolResult) -> bool {
        self.default.result_requires_approval(result)
    }
    fn durable_blob_inspector(&self) -> Option<Box<dyn DurableByteInspector>> {
        self.default.durable_blob_inspector()
    }
    fn audit_builtin<'a>(
        &'a self,
        run: &'a str,
        call: &'a ToolCall,
        result: Option<&'a ToolResult>,
    ) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async move {
            self.route(run)?
                .local
                .audit_builtin(run, call, result)
                .await
        })
    }
    fn configure_network_authorizer<'a>(
        &'a self,
        run: &'a str,
        authorizer: Arc<dyn NetworkAuthorizer>,
    ) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async move {
            self.route(run)?
                .local
                .configure_network_authorizer(run, authorizer)
                .await
        })
    }
    fn finalization_guard<'a>(
        &'a self,
        run: &'a str,
    ) -> BoxFuture<'a, Result<Box<dyn FinalizationGuard>, String>> {
        Box::pin(async move { self.route(run)?.local.finalization_guard(run).await })
    }
    fn deferred_definitions<'a>(
        &'a self,
        run: &'a str,
    ) -> BoxFuture<'a, Result<Vec<ToolDefinition>, String>> {
        Box::pin(async move { self.route(run)?.local.deferred_definitions(run).await })
    }
    fn live_status(&self, run: &str) -> Vec<Value> {
        WorkerTools::live_status(self, run)
    }
    fn scheduling<'a>(
        &'a self,
        run: &'a str,
        call: &'a ToolCall,
    ) -> BoxFuture<'a, Option<ToolScheduling>> {
        Box::pin(async move {
            if let Ok(runtime) = self.target(run, &call.name) {
                runtime.scheduling(run, call).await
            } else {
                None
            }
        })
    }
}
fn narrow_paths(paths: &[PathBuf], root: &std::path::Path) -> Vec<PathBuf> {
    paths
        .iter()
        .filter_map(|path| {
            if root.starts_with(path) {
                Some(root.to_path_buf())
            } else if path.starts_with(root) {
                Some(path.clone())
            } else {
                None
            }
        })
        .collect()
}
