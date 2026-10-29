//! Scoped planning tools backed by the shared task store and writer.
use excalibur_store::{Database, ReadLane, rusqlite::OptionalExtension};
use excalibur_tasks::{TaskPolicy, TaskStore};
use excalibur_types::runtime::*;
use serde_json::{Value, json};
use std::sync::Arc;

const LOCAL: &[&str] = &[
    "apply_patch",
    "edit_file",
    "exec",
    "exec_io",
    "glob",
    "grep",
    "read_file",
    "write_file",
    "file_history",
    "revert_edit",
    "workspace_snapshot",
    "worktree",
    "integration_queue",
    "code_graph",
    "lsp",
];
#[derive(Clone)]
pub struct EpisodeTools {
    delegate: Arc<dyn ToolRuntime>,
    db: Database,
    tasks: TaskStore,
    scope: String,
    catalog: Vec<ToolDefinition>,
    workspace: Option<std::path::PathBuf>,
}
impl EpisodeTools {
    pub async fn new(
        delegate: Arc<dyn ToolRuntime>,
        db: Database,
        scope: impl Into<String>,
    ) -> Result<Self, String> {
        let scope = scope.into();
        if scope.is_empty() || scope.len() > 128 || scope.chars().any(char::is_control) {
            return Err("episode scope must contain 1..128 printable bytes".into());
        }
        let tasks = TaskStore::open(db.clone())
            .await
            .map_err(|e| e.to_string())?
            .with_policy(TaskPolicy::episode(&scope));
        let mut catalog = delegate
            .definitions()
            .into_iter()
            .filter(|definition| LOCAL.contains(&definition.name.as_str()))
            .collect::<Vec<_>>();
        catalog.sort_by(|a, b| a.name.cmp(&b.name));
        catalog.dedup_by(|a, b| a.name == b.name);
        Ok(Self {
            delegate,
            db,
            tasks,
            scope,
            catalog,
            workspace: None,
        })
    }
    pub fn task_store(&self) -> TaskStore {
        self.tasks.clone()
    }
    pub fn with_redactor(mut self, redactor: excalibur_vault::Redactor) -> Self {
        self.tasks = self.tasks.with_redactor(redactor);
        self
    }
    /// Enable executable checks in the host-authorized workspace.
    pub fn with_workspace(mut self, workspace: std::path::PathBuf) -> Self {
        self.workspace = Some(workspace);
        self
    }

    pub fn with_planning_autonomy(mut self, autonomy: u8) -> Result<Self, String> {
        if autonomy > 3 {
            return Err("planning autonomy must be 0..3".into());
        }
        let mut policy = TaskPolicy::episode(self.scope.clone());
        policy.autonomy = autonomy;
        self.tasks = self.tasks.with_policy(policy);
        Ok(self)
    }
    pub fn scope(&self) -> &str {
        &self.scope
    }
    async fn planning_admission(&self, run: &str, call: &ToolCall) -> Result<(), String> {
        let mutates = matches!(
            call.name.as_str(),
            "write_file"
                | "edit_file"
                | "apply_patch"
                | "exec"
                | "revert_edit"
                | "worktree"
                | "integration_queue"
        ) || call.name == "workspace_snapshot"
            && self.args(call)?["action"] == "restore"
            || call.name == "lsp" && self.args(call)?["action"] == "rename"
            || call.name == "exec_io"
                && self.args(call)?["action"]
                    .as_str()
                    .is_some_and(|a| matches!(a, "write" | "keys"));
        if mutates {
            self.tasks
                .effect_admission(run)
                .await
                .map_err(|e| e.to_string())?;
        }
        Ok(())
    }
    fn args(&self, call: &ToolCall) -> Result<Value, String> {
        let args: Value = serde_json::from_str(&call.arguments)
            .map_err(|_| "episode tools require a JSON object")?;
        if !args.is_object() {
            return Err("episode tools require a JSON object".into());
        }
        if self.delegate.sanitize_durable(&args)? != args {
            return Err("sensitive content cannot enter episode tool arguments".into());
        }
        Ok(args)
    }
    fn dynamic(&self, call: &ToolCall) -> Result<ToolCall, String> {
        let args = self.args(call)?;
        keys(&args, &["name", "arguments"])?;
        let name = text(&args, "name")?;
        if !self
            .catalog
            .iter()
            .any(|definition| definition.name == name)
        {
            return Err("requested tool is unavailable in this frozen episode catalog".into());
        }
        let arguments = args
            .get("arguments")
            .filter(|v| v.is_object())
            .ok_or("call_tool.arguments must be an object")?;
        Ok(ToolCall {
            id: call.id.clone(),
            name: name.into(),
            arguments: arguments.to_string(),
        })
    }
    fn wrap(&self, call: ToolCall, inner: PreparedTool) -> PreparedTool {
        PreparedTool {
            call,
            effect: inner.effect,
            resources: inner.resources.clone(),
            exclusive: inner.exclusive,
            data: json!({"scope":self.scope,"inner":inner}),
        }
    }
    fn inner(&self, tool: &PreparedTool) -> Result<PreparedTool, String> {
        if tool.data["scope"] != self.scope {
            return Err("prepared tool belongs to another episode scope".into());
        }
        let inner: PreparedTool = serde_json::from_value(tool.data["inner"].clone())
            .map_err(|_| "invalid delegated episode preparation")?;
        let expected = if tool.call.name == "call_tool" {
            self.dynamic(&tool.call)?.name
        } else {
            tool.call.name.clone()
        };
        if !self.catalog.iter().any(|d| d.name == expected)
            || inner.call.name != expected
            || inner.call.id != tool.call.id
        {
            return Err("delegated tool differs from the frozen episode catalog".into());
        }
        Ok(inner)
    }
    fn own_args(&self, tool: &PreparedTool) -> Result<Value, String> {
        let args = self.args(&tool.call)?;
        if tool.data["scope"] != self.scope || tool.data["args"] != args {
            return Err("episode preparation changed after admission".into());
        }
        Ok(args)
    }
    async fn receipt(&self, key: &str, args: &Value) -> Result<Option<Value>, String> {
        self.tasks
            .receipt(&self.scope, key, args)
            .await
            .map(|value| value.map(episode_view))
            .map_err(|e| e.to_string())
    }
    async fn task_db(&self, key: &str, args: Value) -> Result<Value, String> {
        self.tasks
            .batch(&self.scope, key, args)
            .await
            .map(episode_view)
            .map_err(|e| e.to_string())
    }
    async fn own_execute(
        &self,
        run: &str,
        key: &str,
        tool: &PreparedTool,
    ) -> Result<Value, String> {
        let args = self.own_args(tool)?;
        match tool.call.name.as_str() {
            "task_db" => {
                if let Some(check) = check_request(&args)? {
                    self.tasks
                        .effect_admission(run)
                        .await
                        .map_err(|e| e.to_string())?;
                    let task = self
                        .tasks
                        .task_for_run(run)
                        .await
                        .map_err(|e| e.to_string())?
                        .ok_or("check execution requires a bound task")?;
                    let root = self
                        .workspace
                        .clone()
                        .ok_or("check execution requires a host workspace")?;
                    let evidence = excalibur_tasks::verification::CheckRunner::new(
                        self.delegate.clone(),
                        root,
                    )
                    .run_keyed(&self.tasks, &task.id, check, run, key)
                    .await
                    .map_err(|e| e.to_string())?;
                    return Ok(json!({"evidence":evidence}));
                }
                self.task_db(key, args).await
            }
            "clock" => Ok(json!({"unix_seconds":excalibur_store::unix_seconds(),"timezone":"UTC"})),
            "status" => {
                if args["action"] == "block" {
                    return Ok(
                        json!({"task_state":"blocked","reason":args["reason"],"scope":self.scope}),
                    );
                }
                let live = self.delegate.live_status(run);
                let (scope, run) = (self.scope.clone(), run.to_owned());
                self.db.read(ReadLane::Interactive,move|c| {
                    let state=c.query_row("SELECT state FROM runs WHERE id=?1",[&run],|row|row.get::<_,String>(0)).optional()?;
                    Ok(json!({"scope":scope,"run_id":run,"run_state":state,"planning_available":true,"live_tools":live}))
                }).await.map_err(|e|e.to_string())
            }
            "search_tools" => {
                let query = args
                    .get("query")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_lowercase();
                let results = self
                    .catalog
                    .iter()
                    .filter(|d| {
                        format!("{} {}", d.name, d.description)
                            .to_lowercase()
                            .contains(&query)
                    })
                    .take(20)
                    .collect::<Vec<_>>();
                Ok(json!({"tools":results,"frozen":true,"scope":self.scope}))
            }
            _ => Err("tool is unavailable in the evaluation episode".into()),
        }
    }
}
impl ToolRuntime for EpisodeTools {
    fn deferred_definitions<'a>(
        &'a self,
        run: &'a str,
    ) -> excalibur_types::runtime::BoxFuture<
        'a,
        std::result::Result<Vec<excalibur_types::runtime::ToolDefinition>, String>,
    > {
        self.delegate.deferred_definitions(run)
    }
    fn live_status(&self, run: &str) -> Vec<serde_json::Value> {
        self.delegate.live_status(run)
    }
    fn execution_state<'a>(
        &'a self,
        run: &'a str,
        state: &'a str,
        reason: Option<&'a str>,
    ) -> excalibur_types::runtime::BoxFuture<'a, Result<(), String>> {
        self.delegate.execution_state(run, state, reason)
    }
    fn begin_step<'a>(
        &'a self,
        run: &'a str,
        step: u64,
    ) -> excalibur_types::runtime::BoxFuture<'a, Result<(), String>> {
        self.delegate.begin_step(run, step)
    }
    fn end_step<'a>(
        &'a self,
        run: &'a str,
        step: u64,
    ) -> excalibur_types::runtime::BoxFuture<'a, Result<(), String>> {
        self.delegate.end_step(run, step)
    }
    fn interrupt_execution<'a>(
        &'a self,
        run: &'a str,
        cancelled: bool,
        tool: Option<&'a PreparedTool>,
    ) -> BoxFuture<'a, Result<bool, String>> {
        Box::pin(async move {
            let inner = tool
                .filter(|t| LOCAL.contains(&t.call.name.as_str()) || t.call.name == "call_tool")
                .map(|tool| self.inner(tool))
                .transpose()?;
            if let Some(inner) = &inner {
                self.delegate
                    .interrupt_execution(run, cancelled, Some(inner))
                    .await
            } else if tool.is_none()
                || tool.is_some_and(|t| {
                    t.call.name == "task_db"
                        && check_request(&self.args(&t.call).unwrap_or(Value::Null))
                            .ok()
                            .flatten()
                            .is_some()
                })
            {
                self.delegate
                    .interrupt_execution(run, cancelled, None)
                    .await
            } else {
                Ok(false)
            }
        })
    }
    fn configure_execution_limits<'a>(
        &'a self,
        run: &'a str,
        limits: excalibur_types::runtime::ExecutionLimits,
    ) -> excalibur_types::runtime::BoxFuture<'a, std::result::Result<(), String>> {
        self.delegate.configure_execution_limits(run, limits)
    }
    fn execution_resources<'a>(
        &'a self,
        run: &'a str,
    ) -> excalibur_types::runtime::BoxFuture<
        'a,
        std::result::Result<Option<excalibur_types::runtime::ExecutionResources>, String>,
    > {
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
    fn execution_tool_budget<'a>(
        &'a self,
        run: &'a str,
    ) -> BoxFuture<'a, Result<Option<u64>, String>> {
        self.delegate.execution_tool_budget(run)
    }
    fn finalization_guard<'a>(
        &'a self,
        run: &'a str,
    ) -> BoxFuture<'a, Result<Box<dyn FinalizationGuard>, String>> {
        self.delegate.finalization_guard(run)
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
    fn configure_process_boundary<'a>(
        &'a self,
        run: &'a str,
        boundary: ProcessBoundary,
    ) -> BoxFuture<'a, Result<(), String>> {
        self.delegate.configure_process_boundary(run, boundary)
    }
    fn configure_network_authorizer<'a>(
        &'a self,
        run: &'a str,
        guard: Arc<dyn NetworkAuthorizer>,
    ) -> BoxFuture<'a, Result<(), String>> {
        self.delegate.configure_network_authorizer(run, guard)
    }
    fn definitions(&self) -> Vec<ToolDefinition> {
        let mut catalog = self.catalog.clone();
        for (name, description, schema) in [
            (
                "task_db",
                "Atomic scoped tasks, plans, requirements, criteria, checks and dependencies. Mutable records require expected_version; queries are bounded. Evidence and completion are host-derived, never writable success claims. Submit {ops:[{op:run_check,check:ID}]} alone to execute a declared check through the sandbox.",
                task_schema(),
            ),
            (
                "clock",
                "Read the current Unix time in UTC.",
                json!({"type":"object","additionalProperties":false}),
            ),
            (
                "status",
                "Read this episode's state, or report missing mandatory input with action=block and a reason; blocking never asks the user.",
                json!({"type":"object","properties":{"action":{"enum":["get","block"]},"reason":{"type":"string","minLength":1,"maxLength":2048}},"additionalProperties":false}),
            ),
            (
                "search_tools",
                "Search only the actual frozen local-tool catalog; no installation or discovery outside the episode.",
                json!({"type":"object","properties":{"query":{"type":"string","maxLength":256}},"additionalProperties":false}),
            ),
            (
                "call_tool",
                "Execute a tool from the frozen local catalog through its existing security adapter.",
                json!({"type":"object","required":["name","arguments"],"properties":{"name":{"type":"string"},"arguments":{"type":"object"}},"additionalProperties":false}),
            ),
        ] {
            catalog.push(ToolDefinition {
                name: name.into(),
                description: description.into(),
                input_schema: schema,
            });
        }
        catalog.sort_by(|a, b| a.name.cmp(&b.name));
        catalog
    }
    fn scheduling<'a>(
        &'a self,
        run: &'a str,
        call: &'a ToolCall,
    ) -> BoxFuture<'a, Option<ToolScheduling>> {
        Box::pin(async move {
            if LOCAL.contains(&call.name.as_str()) {
                self.delegate.scheduling(run, call).await
            } else if call.name == "call_tool" {
                let inner = self.dynamic(call).ok()?;
                self.delegate.scheduling(run, &inner).await
            } else {
                Some(ToolScheduling {
                    resources: vec![Resource {
                        key: format!("episode:{}", self.scope),
                        write: call.name == "task_db",
                    }],
                    exclusive: false,
                })
            }
        })
    }
    fn prepare<'a>(
        &'a self,
        run: &'a str,
        call: ToolCall,
    ) -> BoxFuture<'a, Result<PreparedTool, String>> {
        Box::pin(async move {
            if LOCAL.contains(&call.name.as_str()) {
                if !self
                    .catalog
                    .iter()
                    .any(|definition| definition.name == call.name)
                {
                    return Err("tool is absent from the frozen episode catalog".into());
                }
                self.planning_admission(run, &call).await?;
                let inner = self.delegate.prepare(run, call.clone()).await?;
                return Ok(self.wrap(call, inner));
            }
            if call.name == "call_tool" {
                let dynamic = self.dynamic(&call)?;
                self.planning_admission(run, &dynamic).await?;
                let inner = self.delegate.prepare(run, dynamic).await?;
                return Ok(self.wrap(call, inner));
            }
            let args = self.args(&call)?;
            match call.name.as_str() {
                "task_db" => {
                    if check_request(&args)?.is_some() {
                        if self.workspace.is_none()
                            || self
                                .tasks
                                .task_for_run(run)
                                .await
                                .map_err(|e| e.to_string())?
                                .is_none()
                        {
                            return Err(
                                "run_check requires a bound task and a host-authorized workspace"
                                    .into(),
                            );
                        }
                    } else {
                        self.tasks
                            .validate_batch(&self.scope, &args)
                            .map_err(|e| e.to_string())?;
                    }
                }
                "clock" => keys(&args, &[])?,
                "status" => {
                    keys(&args, &["action", "reason"])?;
                    match args.get("action").and_then(Value::as_str) {
                        None if args.get("action").is_none() => {}
                        Some("get") => {}
                        Some("block") => {
                            if args["reason"]
                                .as_str()
                                .is_none_or(|s| s.trim().is_empty() || s.len() > 2048)
                            {
                                return Err(
                                    "blocking requires a reason of at most 2048 bytes".into()
                                );
                            }
                        }
                        _ => return Err("unknown episode status action".into()),
                    }
                    if args["action"] != "block" && args.get("reason").is_some() {
                        return Err("a reason is valid only for a blocking status".into());
                    }
                }
                "search_tools" => {
                    keys(&args, &["query"])?;
                    if args
                        .get("query")
                        .is_some_and(|q| !q.is_string() || q.as_str().unwrap().len() > 256)
                    {
                        return Err("search query must be a string up to 256 bytes".into());
                    }
                }
                _ => return Err("tool is disabled or unavailable in the evaluation episode".into()),
            }
            let keyed = call.name == "task_db";
            let non_idempotent_check = keyed && check_request(&args)?.is_some();
            Ok(PreparedTool {
                call,
                effect: if non_idempotent_check {
                    Effect::NonIdempotent
                } else if keyed {
                    Effect::Keyed
                } else {
                    Effect::Pure
                },
                resources: vec![Resource {
                    key: format!("episode:{}", self.scope),
                    write: keyed,
                }],
                exclusive: false,
                data: json!({"scope":self.scope,"args":args}),
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
            if LOCAL.contains(&tool.call.name.as_str()) || tool.call.name == "call_tool" {
                return match self.inner(tool) {
                    Ok(inner) => {
                        if let Err(error) = self.planning_admission(run, &inner.call).await {
                            return ToolResult::error(error);
                        }
                        let _guard = if matches!(
                            inner.call.name.as_str(),
                            "write_file" | "edit_file" | "apply_patch"
                        ) {
                            Some(self.tasks.begin_mutation().await)
                        } else {
                            None
                        };
                        let result = self.delegate.execute(run, key, &inner).await;
                        let paths = result.output["files"]
                            .as_array()
                            .into_iter()
                            .flatten()
                            .filter_map(|file| file["path"].as_str().map(str::to_owned))
                            .collect::<Vec<_>>();
                        if !paths.is_empty()
                            && let Err(error) = self
                                .tasks
                                .invalidate_paths_locked(&self.scope, &paths)
                                .await
                        {
                            return ToolResult::error(format!(
                                "file effect confirmed; evidence invalidation requires recovery: {error}"
                            ));
                        }
                        result
                    }
                    Err(error) => ToolResult::error(error),
                };
            }
            if self
                .delegate
                .audit_builtin(run, &tool.call, None)
                .await
                .is_err()
            {
                return ToolResult::error("episode tool audit admission failed");
            }
            let result = match self.own_execute(run, key, tool).await {
                Ok(value) => match self.delegate.sanitize_durable(&value) {
                    Ok(value) => ToolResult::ok(value),
                    Err(error) => ToolResult::error(error),
                },
                Err(error) => ToolResult::error(error),
            };
            if self
                .delegate
                .audit_builtin(run, &tool.call, Some(&result))
                .await
                .is_err()
            {
                return ToolResult::error(
                    "episode tool result audit failed; inspect the durable operation receipt",
                );
            }
            result
        })
    }
    fn reconcile<'a>(
        &'a self,
        run: &'a str,
        key: &'a str,
        tool: &'a PreparedTool,
    ) -> BoxFuture<'a, Reconcile> {
        Box::pin(async move {
            if LOCAL.contains(&tool.call.name.as_str()) || tool.call.name == "call_tool" {
                return match self.inner(tool) {
                    Ok(inner) => self.delegate.reconcile(run, key, &inner).await,
                    Err(error) => Reconcile::Uncertain(error),
                };
            }
            let args = match self.own_args(tool) {
                Ok(args) => args,
                Err(error) => return Reconcile::Uncertain(error),
            };
            if tool.call.name != "task_db" {
                return Reconcile::Retry;
            }
            if check_request(&args).ok().flatten().is_some() {
                let operation = format!("{run}:{key}");
                let id = self
                    .db
                    .read(ReadLane::Interactive, move |c| {
                        Ok(c.query_row(
                            "SELECT evidence_id FROM task_check_attempts WHERE operation_key=?1",
                            [operation],
                            |r| r.get::<_, Option<String>>(0),
                        )
                        .optional()?
                        .flatten())
                    })
                    .await;
                return match id {
                    Ok(Some(id)) => match self.tasks.evidence(&id).await {
                        Ok(Some(evidence)) => Reconcile::Done(ToolResult::ok(json!({"evidence":evidence}))),
                        _ => Reconcile::Uncertain("check receipt references unavailable evidence".into()),
                    },
                    _ => Reconcile::Uncertain("check interrupted before its evidence receipt; effects will not be repeated automatically".into()),
                };
            }
            match self.receipt(key, &args).await {
                Ok(Some(result)) => match self.delegate.sanitize_durable(&result) {
                    Ok(result) => Reconcile::Done(ToolResult::ok(result)),
                    Err(error) => Reconcile::Uncertain(error),
                },
                Ok(None) => Reconcile::Retry,
                Err(error) => Reconcile::Uncertain(error),
            }
        })
    }
}
fn task_schema() -> Value {
    excalibur_tasks::task_schema()
}
fn text<'a>(value: &'a Value, key: &str) -> Result<&'a str, String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty() && s.len() <= 128 && !s.chars().any(char::is_control))
        .ok_or_else(|| format!("{key} must contain 1..128 printable bytes"))
}
fn keys(value: &Value, allowed: &[&str]) -> Result<(), String> {
    if value
        .as_object()
        .ok_or("expected object")?
        .keys()
        .any(|key| !allowed.contains(&key.as_str()))
    {
        Err("unsupported field or namespace in episode operation".into())
    } else {
        Ok(())
    }
}

fn check_request(args: &Value) -> Result<Option<&str>, String> {
    let Some(ops) = args.get("ops").and_then(Value::as_array) else {
        return Ok(None);
    };
    if !ops.iter().any(|op| op["op"] == "run_check") {
        return Ok(None);
    }
    keys(args, &["ops"])?;
    if ops.len() != 1 {
        return Err("run_check is an executable operation and must be submitted alone".into());
    }
    keys(&ops[0], &["op", "check"])?;
    Ok(Some(text(&ops[0], "check")?))
}

// Preserve the episode DTO for opaque task fields; canonical fields remain
// available at the row's top level. The same view is used for durable receipts.
fn episode_view(mut value: Value) -> Value {
    for result in value["results"].as_array_mut().into_iter().flatten() {
        for row in result["rows"].as_array_mut().into_iter().flatten() {
            if row["entity"] == "task" && row["data"].is_object() && row["metadata"].is_object() {
                let mut data = row["metadata"].as_object().cloned().unwrap_or_default();
                for field in ["title", "detail"] {
                    if let Some(value) = row.get(field) {
                        data.insert(field.into(), value.clone());
                    }
                }
                row["data"] = Value::Object(data);
            }
        }
    }
    value
}
