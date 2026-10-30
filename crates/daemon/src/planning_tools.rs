//! Per-run planning metadata and declared checks, reusing the shared task adapter.
use excalibur_orchestrator::Agent;
use excalibur_store::Database;
use excalibur_tools::evaluation::EpisodeTools;
use excalibur_types::runtime::*;
use excalibur_vault::Redactor;
use serde_json::Value;
use std::{
    collections::BTreeMap,
    path::PathBuf,
    sync::{Arc, Mutex},
};
#[derive(Clone)]
struct Route {
    episode: Arc<EpisodeTools>,
    agent: String,
    task: String,
    scope: String,
    profile: String,
    workspace: PathBuf,
    readonly: bool,
    checks: bool,
}
pub struct ScopedPlanning {
    delegate: Arc<dyn ToolRuntime>,
    db: Database,
    redactor: Redactor,
    prototype: EpisodeTools,
    routes: Mutex<BTreeMap<String, Route>>,
    binding: tokio::sync::Mutex<()>,
}
impl ScopedPlanning {
    pub async fn new(
        delegate: Arc<dyn ToolRuntime>,
        db: Database,
        redactor: Redactor,
    ) -> Result<Self, String> {
        let prototype = EpisodeTools::new(delegate.clone(), db.clone(), "general")
            .await?
            .with_redactor(redactor.clone())
            .with_planning_autonomy(2)?;
        Ok(Self {
            delegate,
            db,
            redactor,
            prototype,
            routes: Default::default(),
            binding: Default::default(),
        })
    }
    /// Called only after the trusted host has fixed the actual workspace and
    /// installed the run's security token. Client/model paths cannot bind routes.
    pub async fn bind_agent(&self, agent: &Agent, workspace: PathBuf) -> Result<(), String> {
        let run = agent
            .run_id
            .as_ref()
            .ok_or("planning route requires an execution")?;
        if agent.depth == 0 {
            return Err("the conversational master does not use worker planning tools".into());
        }
        let workspace = workspace
            .canonicalize()
            .map_err(|_| "planning workspace unavailable")?;
        if !workspace.is_dir() {
            return Err("planning workspace must be a directory".into());
        }
        let _binding = self.binding.lock().await;
        if let Some(old) = self
            .routes
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .get(run)
        {
            if old.agent != agent.id
                || old.task != agent.task
                || old.scope != agent.scope
                || old.profile != agent.profile
                || old.workspace != workspace
            {
                return Err("planning execution is already bound to another contract".into());
            }
            return Ok(());
        }
        let episode =
            EpisodeTools::new(self.delegate.clone(), self.db.clone(), agent.scope.clone())
                .await?
                .with_workspace(workspace.clone())
                .with_redactor(self.redactor.clone())
                .with_planning_autonomy(2)?;
        let readonly = matches!(
            agent.profile.as_str(),
            "explorer" | "researcher" | "reviewer" | "security-auditor" | "verifier"
        );
        let checks = agent.token.processes
            && !matches!(
                agent.profile.as_str(),
                "explorer" | "researcher" | "reviewer" | "security-auditor" | "planner"
            );
        let route = Route {
            episode: Arc::new(episode),
            agent: agent.id.clone(),
            task: agent.task.clone(),
            scope: agent.scope.clone(),
            profile: agent.profile.clone(),
            workspace,
            readonly,
            checks,
        };
        self.routes
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .insert(run.clone(), route);
        Ok(())
    }
    fn route(&self, run: &str) -> Result<Route, String> {
        self.routes
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .get(run)
            .cloned()
            .ok_or_else(|| "execution has no host-bound planning route".into())
    }
    fn guard(route: &Route, call: &ToolCall) -> Result<(), String> {
        if call.name == "status" {
            return Err("status belongs to the orchestrator".into());
        }
        if call.name != "task_db" {
            return Ok(());
        }
        let args: Value =
            serde_json::from_str(&call.arguments).map_err(|_| "task arguments must be JSON")?;
        let operations = args["ops"]
            .as_array()
            .filter(|ops| !ops.is_empty())
            .ok_or("task operations required")?;
        for operation in operations {
            let name = operation["op"]
                .as_str()
                .ok_or("task operation name required")?;
            if name == "run_check" && !route.checks {
                return Err("this role has no declared-command capability".into());
            }
            if route.readonly && name != "query" && !(name == "run_check" && route.checks) {
                return Err("this role cannot mutate planning metadata".into());
            }
        }
        Ok(())
    }
    fn catalog(route: Option<&Route>, episode: &EpisodeTools) -> Vec<ToolDefinition> {
        episode.definitions().into_iter().filter(|definition|definition.name!="status").map(|mut definition|{
            if definition.name=="task_db" && route.is_some_and(|route|route.readonly){
                let checks=route.is_some_and(|route|route.checks);
                definition.input_schema["properties"]["ops"]["items"]["properties"]["op"]["enum"]=if checks{serde_json::json!(["query","run_check"])}else{serde_json::json!(["query"])};
                definition.description="Read scoped planning records; this role cannot mutate metadata. Declared checks additionally require command capability.".into();
            }definition
        }).collect()
    }
}
impl ToolRuntime for ScopedPlanning {
    fn definitions(&self) -> Vec<ToolDefinition> {
        Self::catalog(None, &self.prototype)
    }
    fn run_definitions<'a>(
        &'a self,
        run: &'a str,
    ) -> BoxFuture<'a, Result<Option<Vec<ToolDefinition>>, String>> {
        Box::pin(async move {
            let route = self.route(run)?;
            Ok(Some(Self::catalog(Some(&route), &route.episode)))
        })
    }
    fn prepare<'a>(
        &'a self,
        run: &'a str,
        call: ToolCall,
    ) -> BoxFuture<'a, Result<PreparedTool, String>> {
        Box::pin(async move {
            let route = self.route(run)?;
            Self::guard(&route, &call)?;
            route.episode.prepare(run, call).await
        })
    }
    fn execute<'a>(
        &'a self,
        run: &'a str,
        key: &'a str,
        tool: &'a PreparedTool,
    ) -> BoxFuture<'a, ToolResult> {
        Box::pin(async move {
            let route = match self.route(run) {
                Ok(route) => route,
                Err(error) => return ToolResult::error(error),
            };
            if let Err(error) = Self::guard(&route, &tool.call) {
                return ToolResult::error(error);
            }
            route.episode.execute(run, key, tool).await
        })
    }
    fn reconcile<'a>(
        &'a self,
        run: &'a str,
        key: &'a str,
        tool: &'a PreparedTool,
    ) -> BoxFuture<'a, Reconcile> {
        Box::pin(async move {
            let route = match self.route(run) {
                Ok(route) => route,
                Err(error) => return Reconcile::Uncertain(error),
            };
            if let Err(error) = Self::guard(&route, &tool.call) {
                return Reconcile::Uncertain(error);
            }
            route.episode.reconcile(run, key, tool).await
        })
    }
    fn scheduling<'a>(
        &'a self,
        run: &'a str,
        call: &'a ToolCall,
    ) -> BoxFuture<'a, Option<ToolScheduling>> {
        Box::pin(async move {
            let route = self.route(run).ok()?;
            Self::guard(&route, call).ok()?;
            route.episode.scheduling(run, call).await
        })
    }
    fn interrupt_execution<'a>(
        &'a self,
        run: &'a str,
        cancelled: bool,
        tool: Option<&'a PreparedTool>,
    ) -> BoxFuture<'a, Result<bool, String>> {
        Box::pin(async move {
            match self.route(run) {
                Ok(route) => {
                    route
                        .episode
                        .interrupt_execution(run, cancelled, tool)
                        .await
                }
                Err(_) => {
                    self.delegate
                        .interrupt_execution(run, cancelled, tool)
                        .await
                }
            }
        })
    }
    fn execution_state<'a>(
        &'a self,
        run: &'a str,
        state: &'a str,
        reason: Option<&'a str>,
    ) -> BoxFuture<'a, Result<(), String>> {
        self.delegate.execution_state(run, state, reason)
    }
    fn begin_step<'a>(&'a self, run: &'a str, step: u64) -> BoxFuture<'a, Result<(), String>> {
        self.delegate.begin_step(run, step)
    }
    fn end_step<'a>(&'a self, run: &'a str, step: u64) -> BoxFuture<'a, Result<(), String>> {
        self.delegate.end_step(run, step)
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
    fn deferred_definitions<'a>(
        &'a self,
        run: &'a str,
    ) -> BoxFuture<'a, Result<Vec<ToolDefinition>, String>> {
        self.delegate.deferred_definitions(run)
    }
    fn live_status(&self, run: &str) -> Vec<Value> {
        self.delegate.live_status(run)
    }
}
