//! Engine tool runtime for the memory tools. The trusted host binds each run to a grant
//! (principal, explicit scopes, conversation, task and trust ceiling); arguments can only
//! narrow it. Writes are keyed: a durable receipt per run and tool key makes a replay
//! after an interruption return the first result instead of writing twice.
use crate::{
    Error, MemoryGrant, MemoryStore, Result, RunTree, Trust, call_memory_tool, digest, invalid,
    memory_tool_definitions,
};
use excalibur_store::{
    Priority, ReadLane,
    rusqlite::{self, OptionalExtension},
};
use excalibur_types::runtime::*;
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, RwLock},
};

/// Tools that change memory, documents or the library.
pub const MEMORY_WRITE_TOOLS: &[&str] = &[
    "memory_write",
    "memory_propose",
    "doc_edit",
    "library_ingest",
];
/// Tools the context layer exposes as core memory tools; `library_ingest` is a deferred tool.
pub const MEMORY_CORE_TOOLS: &[&str] = &[
    "doc_edit",
    "doc_read",
    "history_search",
    "library_search",
    "memory_propose",
    "memory_search",
    "memory_write",
];

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BindingRole {
    Master,
    Worker,
}

/// Host-issued memory authority for one run.
#[derive(Clone, Debug)]
pub struct MemoryBinding {
    pub run: String,
    pub principal: String,
    /// Granted scopes; the first write default is `primary_scope`.
    pub scopes: BTreeSet<String>,
    pub primary_scope: String,
    pub conversation: Option<String>,
    pub task: Option<String>,
    pub parent_run: Option<String>,
    pub role: BindingRole,
    pub timezone: chrono_tz::Tz,
}
impl MemoryBinding {
    pub fn grant(&self) -> MemoryGrant {
        let mut grant = MemoryGrant::new(self.principal.clone(), self.scopes.iter().cloned())
            .with_run(self.run.clone())
            .with_timezone(self.timezone)
            .with_max_trust(match self.role {
                BindingRole::Master => Trust::User,
                BindingRole::Worker => Trust::Agent,
            });
        grant.conversation_id = self.conversation.clone();
        grant.task_id = self.task.clone();
        grant
    }
}

/// Receives the second consecutive `doc_edit` mismatch so the run escalates to its parent
/// (or the master) instead of insisting.
pub trait MemoryEscalation: Send + Sync {
    fn escalate<'a>(
        &'a self,
        run: &'a str,
        detail: Value,
    ) -> BoxFuture<'a, std::result::Result<(), String>>;
}

type StoredBinding = (
    String,
    String,
    String,
    Option<String>,
    Option<String>,
    Option<String>,
    String,
    String,
);

impl MemoryStore {
    /// Persist a run binding. A run keeps the contract it was first bound to.
    pub async fn bind_run(&self, binding: &MemoryBinding, now: i64) -> Result<()> {
        binding.grant().validate()?;
        if binding.run.trim().is_empty() || binding.run.len() > 512 {
            return Err(invalid("run id must contain 1 to 512 bytes"));
        }
        if !binding.scopes.contains(&binding.primary_scope) {
            return Err(Error::Scope("primary scope must be granted".into()));
        }
        self.ensure_principal(&binding.principal).await?;
        let b = binding.clone();
        let scopes = serde_json::to_string(&b.scopes)?;
        let role = match b.role {
            BindingRole::Master => "master",
            BindingRole::Worker => "worker",
        };
        self.db
            .write(Priority::Journal, move |c| {
                let existing: Option<StoredBinding> = c
                    .query_row(
                        "SELECT principal_id,scopes,primary_scope,conversation_id,task_id,parent_run_id,role,timezone FROM memory_run_bindings WHERE run_id=?1",
                        [&b.run],
                        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?, r.get(7)?)),
                    )
                    .optional()?;
                if let Some((principal, old_scopes, primary, conversation, task, parent, old_role, timezone)) = existing {
                    if principal != b.principal || old_scopes != scopes || primary != b.primary_scope
                        || conversation != b.conversation || task != b.task || parent != b.parent_run
                        || old_role != role || timezone != b.timezone.name() {
                        return Err(excalibur_store::Error::Invalid(
                            "run is already bound to another memory contract",
                        ));
                    }
                    return Ok(());
                }
                c.execute(
                    "INSERT INTO memory_run_bindings(run_id,principal_id,scopes,primary_scope,conversation_id,task_id,parent_run_id,role,bound_at,timezone) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",
                    rusqlite::params![b.run, b.principal, scopes, b.primary_scope, b.conversation, b.task, b.parent_run, role, now, b.timezone.name()],
                )?;
                Ok(())
            })
            .await?;
        Ok(())
    }
    /// Durable binding lookup (survives restarts).
    pub async fn run_binding(&self, run: &str) -> Result<Option<MemoryBinding>> {
        let run = run.to_string();
        let row = self
            .db
            .read(ReadLane::Interactive, move |c| {
                Ok(c.query_row(
                    "SELECT principal_id,scopes,primary_scope,conversation_id,task_id,parent_run_id,role,timezone FROM memory_run_bindings WHERE run_id=?1",
                    [&run],
                    |r| {
                        Ok((
                            run.clone(),
                            r.get::<_, String>(0)?,
                            r.get::<_, String>(1)?,
                            r.get::<_, String>(2)?,
                            r.get::<_, Option<String>>(3)?,
                            r.get::<_, Option<String>>(4)?,
                            r.get::<_, Option<String>>(5)?,
                            r.get::<_, String>(6)?,
                            r.get::<_, String>(7)?,
                        ))
                    },
                )
                .optional()?)
            })
            .await?;
        Ok(row.map(
            |(
                run,
                principal,
                scopes,
                primary_scope,
                conversation,
                task,
                parent_run,
                role,
                timezone,
            )| {
                MemoryBinding {
                    run,
                    principal,
                    scopes: serde_json::from_str(&scopes).unwrap_or_default(),
                    primary_scope,
                    conversation,
                    task,
                    parent_run,
                    role: if role == "master" {
                        BindingRole::Master
                    } else {
                        BindingRole::Worker
                    },
                    timezone: timezone.parse().unwrap_or(chrono_tz::UTC),
                }
            },
        ))
    }
    /// Execute one memory tool call for a bound run with a keyed receipt for writes.
    pub async fn call_bound(
        &self,
        binding: &MemoryBinding,
        name: &str,
        args: Value,
        key: &str,
        explicit_user_request: bool,
        now: i64,
    ) -> Result<Value> {
        if key.trim().is_empty() || key.len() > 512 {
            return Err(invalid("tool key must contain 1 to 512 bytes"));
        }
        self.bind_run(binding, now).await?;
        let write = MEMORY_WRITE_TOOLS.contains(&name);
        let request_hash = digest(&serde_json::to_vec(&json!([name, &args]))?);
        if write && let Some(receipt) = self.receipt(&binding.run, key, &request_hash).await? {
            return Ok(receipt);
        }
        if write {
            let (run, receipt_key, tool, hash) = (
                binding.run.clone(),
                key.to_string(),
                name.to_string(),
                request_hash.clone(),
            );
            let claimed = self.db.write(Priority::Journal, move |c| {
                Ok(c.execute("INSERT OR IGNORE INTO memory_tool_receipts(run_id,key,tool,request_hash,result,at,state) VALUES(?1,?2,?3,?4,'null',?5,'pending')",
                    rusqlite::params![run, receipt_key, tool, hash, now])? == 1)
            }).await?;
            if !claimed {
                return self
                    .receipt(&binding.run, key, &request_hash)
                    .await?
                    .ok_or_else(|| invalid("memory receipt disappeared"));
            }
        }
        let mut grant = binding.grant();
        if binding.role == BindingRole::Master && !explicit_user_request {
            grant.max_trust = Trust::Agent;
        }
        let mut args = args;
        if matches!(name, "memory_write" | "memory_propose")
            && args.get("scope").is_none()
            && let Some(object) = args.as_object_mut()
        {
            object.insert("scope".into(), json!(binding.primary_scope));
        }
        let result = call_memory_tool(self, &grant, name, args, explicit_user_request, now).await?;
        if write {
            let (run, key, tool, encoded) = (
                binding.run.clone(),
                key.to_string(),
                name.to_string(),
                result.to_string(),
            );
            self.db
                .write(Priority::Journal, move |c| {
                    c.execute(
                        "UPDATE memory_tool_receipts SET result=?5,at=?6,state='completed' WHERE run_id=?1 AND key=?2 AND tool=?3 AND request_hash=?4 AND state='pending'",
                        rusqlite::params![run, key, tool, request_hash, encoded, now],
                    )?;
                    Ok(())
                })
                .await?;
        }
        Ok(result)
    }
    async fn receipt(&self, run: &str, key: &str, request_hash: &str) -> Result<Option<Value>> {
        let (run, key) = (run.to_string(), key.to_string());
        let row: Option<(String, String, String)> = self
            .db
            .read(ReadLane::Interactive, move |c| {
                Ok(c.query_row(
                    "SELECT request_hash,result,state FROM memory_tool_receipts WHERE run_id=?1 AND key=?2",
                    rusqlite::params![run, key],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                )
                .optional()?)
            })
            .await?;
        match row {
            Some((hash, _, _)) if hash != request_hash => {
                Err(invalid("tool key was reused for a different request"))
            }
            Some((_, _, state)) if state != "completed" => Err(Error::Unavailable(
                "memory effect was interrupted or is still running; reconcile before retrying"
                    .into(),
            )),
            Some((_, result, _)) => Ok(Some(serde_json::from_str(&result)?)),
            None => Ok(None),
        }
    }
}

/// Run tree from host bindings: a run and every run bound with it as an ancestor. A master
/// binding also owns the runs bound to its conversation.
pub struct BindingRunTree {
    pub store: MemoryStore,
}
impl RunTree for BindingRunTree {
    fn subtree<'a>(
        &'a self,
        root: &'a str,
    ) -> BoxFuture<'a, std::result::Result<BTreeSet<String>, String>> {
        let root = root.to_string();
        Box::pin(async move {
            self.store
                .db
                .read(ReadLane::Interactive, move |c| {
                    let mut out = BTreeSet::from([root.clone()]);
                    let master: Option<(String, Option<String>, String)> = c
                        .query_row(
                            "SELECT role,conversation_id,principal_id FROM memory_run_bindings WHERE run_id=?1",
                            [&root],
                            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                        )
                        .optional()?;
                    let Some((role, conversation, principal)) = master else {
                        return Ok(out);
                    };
                    if let Some(conversation) = conversation
                        && role == "master"
                    {
                        let mut s = c.prepare("SELECT run_id FROM memory_run_bindings WHERE conversation_id=?1 AND principal_id=?2 AND role='worker'")?;
                        for run in s.query_map(rusqlite::params![conversation, principal], |r| r.get::<_, String>(0))? {
                            out.insert(run?);
                        }
                    }
                    let mut s = c.prepare("SELECT run_id FROM memory_run_bindings WHERE parent_run_id=?1 AND principal_id=?2")?;
                    let mut stack: Vec<String> = out.iter().cloned().collect();
                    while let Some(node) = stack.pop() {
                        for child in s.query_map(rusqlite::params![node, principal], |r| r.get::<_, String>(0))? {
                            let child = child?;
                            if out.insert(child.clone()) {
                                stack.push(child);
                            }
                        }
                    }
                    Ok(out)
                })
                .await
                .map_err(|e| e.to_string())
        })
    }
}

/// Wraps the host runtime and adds the memory tools for bound runs. Unbound runs (for
/// example the evaluation profile) never see them.
pub struct MemoryTools {
    delegate: Arc<dyn ToolRuntime>,
    store: MemoryStore,
    bindings: RwLock<BTreeMap<String, MemoryBinding>>,
    escalation: Option<Arc<dyn MemoryEscalation>>,
}
impl MemoryTools {
    pub fn new(delegate: Arc<dyn ToolRuntime>, store: MemoryStore) -> Self {
        Self {
            delegate,
            store,
            bindings: RwLock::new(BTreeMap::new()),
            escalation: None,
        }
    }
    pub fn with_escalation(mut self, escalation: Arc<dyn MemoryEscalation>) -> Self {
        self.escalation = Some(escalation);
        self
    }
    pub fn store(&self) -> &MemoryStore {
        &self.store
    }
    /// Called only by the trusted host after it fixed the run's capability token.
    pub async fn bind(&self, binding: MemoryBinding) -> Result<()> {
        if binding.role != BindingRole::Worker {
            return Err(invalid("engine runs bind as workers"));
        }
        self.store
            .bind_run(&binding, excalibur_store::unix_seconds())
            .await?;
        self.bindings
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .insert(binding.run.clone(), binding);
        Ok(())
    }
    async fn binding(&self, run: &str) -> Option<MemoryBinding> {
        if let Some(b) = self
            .bindings
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(run)
            .cloned()
        {
            return Some(b);
        }
        let found = self.store.run_binding(run).await.ok().flatten()?;
        if found.role != BindingRole::Worker {
            return None;
        }
        self.bindings
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .insert(run.to_string(), found.clone());
        Some(found)
    }
    fn own(name: &str) -> bool {
        crate::MEMORY_TOOLS.contains(&name)
    }
    fn memory_definitions() -> Vec<ToolDefinition> {
        memory_tool_definitions()
    }
    fn parse(call: &ToolCall) -> std::result::Result<Value, String> {
        let args: Value = serde_json::from_str(&call.arguments)
            .map_err(|_| "memory tool arguments must be a JSON object")?;
        if !args.is_object() {
            return Err("memory tool arguments must be a JSON object".into());
        }
        Ok(args)
    }
}
impl ToolRuntime for MemoryTools {
    fn filesystem_confinement(&self, run: &str) -> Option<FilesystemConfinement> {
        self.delegate.filesystem_confinement(run)
    }
    fn definitions(&self) -> Vec<ToolDefinition> {
        let mut definitions = self.delegate.definitions();
        definitions.retain(|d| !Self::own(&d.name));
        definitions.extend(Self::memory_definitions());
        definitions.sort_by(|a, b| a.name.cmp(&b.name));
        definitions
    }
    fn run_definitions<'a>(
        &'a self,
        run: &'a str,
    ) -> BoxFuture<'a, std::result::Result<Option<Vec<ToolDefinition>>, String>> {
        Box::pin(async move {
            let mut definitions = match self.delegate.run_definitions(run).await? {
                Some(definitions) => definitions,
                None => self.delegate.definitions(),
            };
            definitions.retain(|d| !Self::own(&d.name));
            if self.binding(run).await.is_some() {
                definitions.extend(
                    Self::memory_definitions()
                        .into_iter()
                        .filter(|d| MEMORY_CORE_TOOLS.contains(&d.name.as_str())),
                );
            }
            definitions.sort_by(|a, b| a.name.cmp(&b.name));
            Ok(Some(definitions))
        })
    }
    fn scheduling<'a>(
        &'a self,
        run: &'a str,
        call: &'a ToolCall,
    ) -> BoxFuture<'a, Option<ToolScheduling>> {
        Box::pin(async move {
            if Self::own(&call.name) {
                self.binding(run).await?;
                return Some(ToolScheduling {
                    resources: vec![],
                    exclusive: false,
                });
            }
            self.delegate.scheduling(run, call).await
        })
    }
    fn prepare<'a>(
        &'a self,
        run: &'a str,
        call: ToolCall,
    ) -> BoxFuture<'a, std::result::Result<PreparedTool, String>> {
        Box::pin(async move {
            if !Self::own(&call.name) {
                return self.delegate.prepare(run, call).await;
            }
            self.binding(run)
                .await
                .ok_or("this execution has no host-bound memory grant")?;
            let args = Self::parse(&call)?;
            self.delegate.audit_builtin(run, &call, None).await?;
            let write = MEMORY_WRITE_TOOLS.contains(&call.name.as_str());
            Ok(PreparedTool {
                effect: if write {
                    Effect::Keyed
                } else {
                    Effect::Idempotent
                },
                resources: vec![],
                exclusive: false,
                data: json!({"memory_version":1,"run":run,"args_hash":digest(&serde_json::to_vec(&json!([call.name, args])).unwrap_or_default())}),
                call,
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
            if !Self::own(&tool.call.name) {
                return self.delegate.execute(run, key, tool).await;
            }
            if tool.data["memory_version"] != 1 || tool.data["run"] != run {
                return ToolResult::error("invalid memory tool preparation owner");
            }
            let Some(binding) = self.binding(run).await else {
                return ToolResult::error("this execution has no host-bound memory grant");
            };
            let args = match Self::parse(&tool.call) {
                Ok(args) => args,
                Err(e) => return ToolResult::error(e),
            };
            if tool.data["args_hash"]
                != digest(&serde_json::to_vec(&json!([tool.call.name, args])).unwrap_or_default())
            {
                return ToolResult::error("memory tool preparation changed");
            }
            let result = self
                .store
                .call_bound(
                    &binding,
                    &tool.call.name,
                    args,
                    key,
                    false,
                    excalibur_store::unix_seconds(),
                )
                .await;
            match result {
                Ok(output) => {
                    if output["outcome"] == "mismatch"
                        && output["escalate"] == true
                        && let Some(escalation) = &self.escalation
                    {
                        let detail = json!({"reason":"doc_edit_conflict","tool_key":key,"current":output["current"]});
                        if let Err(error) = escalation.escalate(run, detail).await {
                            let mut result = ToolResult::ok(output);
                            result.output["escalation_error"] = json!(error);
                            return result;
                        }
                    }
                    if (tool.call.name == "library_search"
                        || output["actions"].as_array().is_some_and(|a| !a.is_empty())
                        || contains_untrusted(&output))
                        && let Err(error) = self
                            .delegate
                            .observe_untrusted(run, "memory:untrusted_external")
                            .await
                    {
                        return ToolResult::error(error);
                    }
                    ToolResult::ok(output)
                }
                Err(Error::Scope(reason)) => {
                    let mut result = ToolResult::error(format!("scope denied: {reason}"));
                    result.output["code"] = json!("memory_scope_denied");
                    result
                }
                Err(error) => ToolResult::error(error.to_string()),
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
            if !Self::own(&tool.call.name) {
                return self.delegate.reconcile(run, key, tool).await;
            }
            if tool.data["memory_version"] != 1
                || tool.data["run"] != run
                || self.binding(run).await.is_none()
            {
                return Reconcile::Uncertain("invalid memory tool preparation owner".into());
            }
            let Ok(args) = Self::parse(&tool.call) else {
                return Reconcile::Uncertain("memory tool arguments changed".into());
            };
            if tool.data["args_hash"]
                != digest(&serde_json::to_vec(&json!([tool.call.name, args])).unwrap_or_default())
            {
                return Reconcile::Uncertain("memory tool preparation changed".into());
            }
            let hash =
                digest(&serde_json::to_vec(&json!([tool.call.name, &args])).unwrap_or_default());
            match self.store.receipt(run, key, &hash).await {
                Ok(Some(result)) => Reconcile::Done(ToolResult::ok(result)),
                Ok(None) => Reconcile::Retry,
                Err(error) => Reconcile::Uncertain(error.to_string()),
            }
        })
    }
    fn execution_state<'a>(
        &'a self,
        run: &'a str,
        state: &'a str,
        reason: Option<&'a str>,
    ) -> BoxFuture<'a, std::result::Result<(), String>> {
        self.delegate.execution_state(run, state, reason)
    }
    fn begin_step<'a>(
        &'a self,
        run: &'a str,
        step: u64,
    ) -> BoxFuture<'a, std::result::Result<(), String>> {
        self.delegate.begin_step(run, step)
    }
    fn end_step<'a>(
        &'a self,
        run: &'a str,
        step: u64,
    ) -> BoxFuture<'a, std::result::Result<(), String>> {
        self.delegate.end_step(run, step)
    }
    fn interrupt_execution<'a>(
        &'a self,
        run: &'a str,
        cancelled: bool,
        tool: Option<&'a PreparedTool>,
    ) -> BoxFuture<'a, std::result::Result<bool, String>> {
        if tool.is_some_and(|t| Self::own(&t.call.name)) {
            return Box::pin(async { Ok(false) });
        }
        self.delegate.interrupt_execution(run, cancelled, tool)
    }
    fn configure_execution_limits<'a>(
        &'a self,
        run: &'a str,
        limits: ExecutionLimits,
    ) -> BoxFuture<'a, std::result::Result<(), String>> {
        self.delegate.configure_execution_limits(run, limits)
    }
    fn execution_resources<'a>(
        &'a self,
        run: &'a str,
    ) -> BoxFuture<'a, std::result::Result<Option<ExecutionResources>, String>> {
        self.delegate.execution_resources(run)
    }
    fn sanitize_durable(&self, value: &Value) -> std::result::Result<Value, String> {
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
    ) -> BoxFuture<'a, std::result::Result<(), String>> {
        self.delegate.observe_untrusted(run, source)
    }
    fn configure_process_boundary<'a>(
        &'a self,
        run: &'a str,
        boundary: ProcessBoundary,
    ) -> BoxFuture<'a, std::result::Result<(), String>> {
        self.delegate.configure_process_boundary(run, boundary)
    }
    fn execution_tool_budget<'a>(
        &'a self,
        run: &'a str,
    ) -> BoxFuture<'a, std::result::Result<Option<u64>, String>> {
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
    ) -> BoxFuture<'a, std::result::Result<(), String>> {
        self.delegate.audit_builtin(run, call, result)
    }
    fn configure_network_authorizer<'a>(
        &'a self,
        run: &'a str,
        authorizer: Arc<dyn NetworkAuthorizer>,
    ) -> BoxFuture<'a, std::result::Result<(), String>> {
        self.delegate.configure_network_authorizer(run, authorizer)
    }
    fn finalization_guard<'a>(
        &'a self,
        run: &'a str,
    ) -> BoxFuture<'a, std::result::Result<Box<dyn FinalizationGuard>, String>> {
        self.delegate.finalization_guard(run)
    }
    fn deferred_definitions<'a>(
        &'a self,
        run: &'a str,
    ) -> BoxFuture<'a, std::result::Result<Vec<ToolDefinition>, String>> {
        self.delegate.deferred_definitions(run)
    }
    fn live_status(&self, run: &str) -> Vec<Value> {
        self.delegate.live_status(run)
    }
}

fn contains_untrusted(value: &Value) -> bool {
    match value {
        Value::String(s) => s == "untrusted_external",
        Value::Array(items) => items.iter().any(contains_untrusted),
        Value::Object(map) => {
            map.get("kind").and_then(Value::as_str) == Some("page")
                || map.values().any(contains_untrusted)
        }
        _ => false,
    }
}
