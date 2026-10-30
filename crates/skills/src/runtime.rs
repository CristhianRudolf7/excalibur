//! Durable run sessions composed outside the host's existing authorization stack.
use crate::{groups::CatalogScope, index::SkillIndex, session::SkillSession};
use excalibur_context::TokenCounter;
use excalibur_store::{Database, Priority, ReadLane, rusqlite::OptionalExtension};
use excalibur_types::runtime::*;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};

pub fn definitions() -> Vec<ToolDefinition> {
    vec![
        ToolDefinition{name:"search_skills".into(),description:"Search the frozen skill catalog by query and optional group.".into(),input_schema:json!({"type":"object","properties":{"query":{"type":"string","minLength":1,"maxLength":8192},"group":{"type":"string"}},"required":["query"],"additionalProperties":false})},
        ToolDefinition{name:"load_skill".into(),description:"Load a validated skill body and its associated tool schemas; active allowed-tools further restrict authority.".into(),input_schema:json!({"type":"object","properties":{"name":{"type":"string","minLength":1,"maxLength":64}},"required":["name"],"additionalProperties":false})},
    ]
}
fn own(name: &str) -> bool {
    matches!(name, "search_skills" | "load_skill")
}
fn digest(body: &str) -> String {
    format!("{:x}", Sha256::digest(body.as_bytes()))
}
fn storage(e: impl std::fmt::Display) -> String {
    e.to_string()
}

pub struct SkillRuntime {
    db: Database,
    delegate: Arc<dyn ToolRuntime>,
    indexes: BTreeMap<String, Arc<SkillIndex>>,
    window: usize,
    counter: TokenCounter,
    core_tools: Vec<ToolDefinition>,
    gates: std::sync::Mutex<BTreeMap<String, std::sync::Weak<tokio::sync::RwLock<()>>>>,
}
impl SkillRuntime {
    pub async fn new(
        db: Database,
        delegate: Arc<dyn ToolRuntime>,
        indexes: BTreeMap<String, Arc<SkillIndex>>,
        window: usize,
        counter: TokenCounter,
    ) -> Result<Self, String> {
        if window == 0 {
            return Err("skill context window must be positive".into());
        }
        db.write(Priority::Journal,|c|{c.execute_batch("CREATE TABLE IF NOT EXISTS skill_run_sessions(run TEXT PRIMARY KEY,scope TEXT NOT NULL,body TEXT NOT NULL,hash TEXT NOT NULL,suggestions TEXT NOT NULL); CREATE TABLE IF NOT EXISTS skill_tool_receipts(run TEXT NOT NULL,operation TEXT NOT NULL,call TEXT NOT NULL,result TEXT NOT NULL,PRIMARY KEY(run,operation));")?;Ok(())}).await.map_err(storage)?;
        Ok(Self {
            db,
            delegate,
            indexes,
            window,
            counter,
            core_tools: vec![],
            gates: Default::default(),
        })
    }
    /// Engine-owned utility schemas supplied by the trusted host. This resolves
    /// definitions only; it never bypasses the engine's builtin admission.
    pub fn with_core_tools(mut self, tools: Vec<ToolDefinition>) -> Self {
        self.core_tools = tools;
        self
    }
    fn gate(&self, run: &str) -> Arc<tokio::sync::RwLock<()>> {
        let mut gates = self.gates.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(gate) = gates.get(run).and_then(std::sync::Weak::upgrade) {
            return gate;
        }
        gates.retain(|_, gate| gate.strong_count() > 0);
        let gate = Arc::new(tokio::sync::RwLock::new(()));
        gates.insert(run.into(), Arc::downgrade(&gate));
        gate
    }
    /// Authority is selected by the host, never by a tool's project argument.
    /// Existing run bindings remain immutable across restart and catalog changes.
    pub async fn bind(
        &self,
        run: &str,
        scope: &str,
        profile: CatalogScope,
        query: &str,
    ) -> Result<(), String> {
        let index = self
            .indexes
            .get(scope)
            .ok_or("no skill index for authorized scope")?
            .clone();
        let existing = self.read(run).await?;
        if let Some((old, _, _)) = existing {
            return if old == scope {
                Ok(())
            } else {
                Err("skill run scope is immutable".into())
            };
        }
        let window = self.window;
        let counter = self.counter.clone();
        let query = query.to_owned();
        let (body, suggestions) = tokio::task::spawn_blocking(move || {
            let session =
                SkillSession::new(&index, &profile, window, |text| counter.count_text(text));
            let suggestions = session
                .suggestions(&index, &query, &counter.model)
                .iter()
                .map(|hit| format!("{}: matched {}", hit.name, hit.matched_terms.join(", ")))
                .collect::<Vec<_>>();
            Ok::<_, String>((
                session.save()?,
                serde_json::to_string(&suggestions).map_err(storage)?,
            ))
        })
        .await
        .map_err(storage)??;
        let value: Value = serde_json::from_str(&body).map_err(storage)?;
        if self.delegate.sanitize_durable(&value)? != value {
            return Err("unsafe literal in frozen skill snapshot".into());
        }
        let (run, scope) = (run.to_owned(), scope.to_owned());
        let hash = digest(&body);
        self.db
            .write(Priority::Journal, move |c| {
                c.execute(
                    "INSERT OR IGNORE INTO skill_run_sessions VALUES(?1,?2,?3,?4,?5)",
                    (&run, &scope, &body, &hash, &suggestions),
                )?;
                let old: String = c.query_row(
                    "SELECT scope FROM skill_run_sessions WHERE run=?1",
                    [&run],
                    |r| r.get(0),
                )?;
                if old != scope {
                    return Err(excalibur_store::Error::Invalid("skill run scope changed"));
                }
                Ok(())
            })
            .await
            .map_err(storage)
    }
    async fn read(&self, run: &str) -> Result<Option<(String, SkillSession, Vec<String>)>, String> {
        let run = run.to_owned();
        let row: Option<(String, String, String, String)> = self
            .db
            .read(ReadLane::Interactive, move |c| {
                Ok(c.query_row(
                    "SELECT scope,body,hash,suggestions FROM skill_run_sessions WHERE run=?1",
                    [run],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
                )
                .optional()?)
            })
            .await
            .map_err(storage)?;
        tokio::task::spawn_blocking(move || {
            row.map(|(scope, body, hash, suggestions)| {
                if digest(&body) != hash {
                    return Err("skill session digest mismatch".into());
                }
                Ok((
                    scope,
                    SkillSession::restore(&body)?,
                    serde_json::from_str(&suggestions).map_err(storage)?,
                ))
            })
            .transpose()
        })
        .await
        .map_err(storage)?
    }
    async fn check(&self, run: &str, call: &ToolCall) -> Result<(), String> {
        let (_, session, _) = self
            .read(run)
            .await?
            .ok_or("skill session is not host bound")?;
        let mut names = BTreeSet::from([call.name.clone()]);
        if call.name == "call_tool" {
            let args: Value = serde_json::from_str(&call.arguments).map_err(storage)?;
            names.clear();
            names.insert(
                args["name"]
                    .as_str()
                    .ok_or("deferred tool name missing")?
                    .into(),
            );
        }
        if session.allowed_tools(&names) != names {
            return Err("tool denied by active skill allowed-tools".into());
        }
        Ok(())
    }
    async fn schemas(&self, run: &str) -> Result<BTreeMap<String, ToolDefinition>, String> {
        let mut all = self
            .delegate
            .run_definitions(run)
            .await?
            .unwrap_or_else(|| self.delegate.definitions());
        all.extend(self.delegate.deferred_definitions(run).await?);
        all.extend(definitions());
        for builtin in &self.core_tools {
            if all.iter().any(|tool| tool.name == builtin.name)
                && self.delegate.overrides_builtin(run, &builtin.name).await
            {
                continue;
            }
            all.retain(|tool| tool.name != builtin.name);
            all.push(builtin.clone());
        }
        Ok(all.into_iter().map(|d| (d.name.clone(), d)).collect())
    }
    async fn receipt(
        &self,
        run: &str,
        key: &str,
        call: &ToolCall,
    ) -> Result<Option<ToolResult>, String> {
        let (run, key, call) = (
            run.to_owned(),
            key.to_owned(),
            serde_json::to_string(call).map_err(storage)?,
        );
        self.db
            .read(ReadLane::Interactive, move |c| {
                let row: Option<(String, String)> = c
                    .query_row(
                        "SELECT call,result FROM skill_tool_receipts WHERE run=?1 AND operation=?2",
                        (&run, &key),
                        |r| Ok((r.get(0)?, r.get(1)?)),
                    )
                    .optional()?;
                row.map(|(old, result)| {
                    if old != call {
                        return Err(excalibur_store::Error::Invalid(
                            "skill operation key reused",
                        ));
                    }
                    serde_json::from_str(&result)
                        .map_err(|_| excalibur_store::Error::Invalid("invalid skill result"))
                })
                .transpose()
            })
            .await
            .map_err(storage)
    }
    async fn perform(
        &self,
        run: &str,
        key: &str,
        tool: &PreparedTool,
    ) -> Result<ToolResult, String> {
        if let Some(result) = self.receipt(run, key, &tool.call).await? {
            return Ok(result);
        }
        self.check(run, &tool.call).await?;
        let args: Value = serde_json::from_str(&tool.call.arguments).map_err(storage)?;
        let schemas = self.schemas(run).await?;
        let (scope, _, _) = self.read(run).await?.ok_or("unbound skill run")?;
        let index = self
            .indexes
            .get(&scope)
            .ok_or("skill scope unavailable")?
            .clone();
        let (run, key, call, name) = (
            run.to_owned(),
            key.to_owned(),
            serde_json::to_string(&tool.call).map_err(storage)?,
            tool.call.name.clone(),
        );
        self.db.write(Priority::Journal,move|c|{
            let previous:Option<(String,String)>=c.query_row("SELECT call,result FROM skill_tool_receipts WHERE run=?1 AND operation=?2",(&run,&key),|r|Ok((r.get(0)?,r.get(1)?))).optional()?;
            if let Some((old,result))=previous {if old!=call{return Err(excalibur_store::Error::Invalid("skill operation key reused"));}return serde_json::from_str(&result).map_err(|_|excalibur_store::Error::Invalid("invalid skill receipt"));}
            let (body,hash):(String,String)=c.query_row("SELECT body,hash FROM skill_run_sessions WHERE run=?1",[&run],|r|Ok((r.get(0)?,r.get(1)?)))?;
            if digest(&body)!=hash {return Err(excalibur_store::Error::Invalid("skill session digest mismatch"));}
            let mut session=SkillSession::restore(&body).map_err(|_|excalibur_store::Error::Invalid("invalid skill session"))?;
            let operation=(||->Result<Value,String>{
                let requested=BTreeSet::from([name.clone()]);
                if session.allowed_tools(&requested)!=requested {return Err("tool denied by active skill allowed-tools".into());}
                if name=="search_skills" {return Ok(json!({"matches":session.search(args["query"].as_str().ok_or("query required")?,args["group"].as_str())?}));}
                let loaded=session.load(&index,args["name"].as_str().ok_or("name required")?,|names|{
                    if names.iter().any(|name|!schemas.contains_key(name)) {return Err("associated tool schema unavailable to this run".into());}Ok(())
                })?;
                let names=loaded.tools.iter().cloned().collect::<BTreeSet<_>>();
                if session.allowed_tools(&names)!=names {return Err("associated tool excluded by active skills".into());}
                let resolved=loaded.tools.iter().filter_map(|name|schemas.get(name)).collect::<Vec<_>>();
                Ok(json!({"skill":loaded,"tools":resolved}))
            })();
            let result=match operation {Ok(value)=>ToolResult::ok(value),Err(error)=>ToolResult::error(error)};
            // Activation and its receipt commit together. Failed loads never alter policy.
            if result.success {
                let body=session.save().map_err(|_|excalibur_store::Error::Invalid("skill serialization failed"))?;
                c.execute("UPDATE skill_run_sessions SET body=?2,hash=?3 WHERE run=?1",(&run,&body,digest(&body)))?;
            }
            c.execute("INSERT INTO skill_tool_receipts VALUES(?1,?2,?3,?4)",(&run,&key,&call,serde_json::to_string(&result).unwrap()))?;
            Ok(result)
        }).await.map_err(storage)
    }
}

impl ToolRuntime for SkillRuntime {
    fn overrides_builtin<'a>(&'a self, run: &'a str, name: &'a str) -> BoxFuture<'a, bool> {
        self.delegate.overrides_builtin(run, name)
    }

    fn filesystem_confinement(&self, _run: &str) -> Option<FilesystemConfinement> {
        self.delegate.filesystem_confinement(_run)
    }
    fn execution_state<'a>(
        &'a self,
        _run: &'a str,
        _state: &'a str,
        _reason: Option<&'a str>,
    ) -> BoxFuture<'a, Result<(), String>> {
        self.delegate.execution_state(_run, _state, _reason)
    }
    fn begin_step<'a>(&'a self, _run: &'a str, _step: u64) -> BoxFuture<'a, Result<(), String>> {
        self.delegate.begin_step(_run, _step)
    }
    fn end_step<'a>(&'a self, _run: &'a str, _step: u64) -> BoxFuture<'a, Result<(), String>> {
        self.delegate.end_step(_run, _step)
    }
    fn interrupt_execution<'a>(
        &'a self,
        _run: &'a str,
        _cancelled: bool,
        _tool: Option<&'a PreparedTool>,
    ) -> BoxFuture<'a, Result<bool, String>> {
        self.delegate.interrupt_execution(_run, _cancelled, _tool)
    }
    fn configure_execution_limits<'a>(
        &'a self,
        _run: &'a str,
        limits: ExecutionLimits,
    ) -> BoxFuture<'a, Result<(), String>> {
        self.delegate.configure_execution_limits(_run, limits)
    }
    fn execution_resources<'a>(
        &'a self,
        _run: &'a str,
    ) -> BoxFuture<'a, Result<Option<ExecutionResources>, String>> {
        self.delegate.execution_resources(_run)
    }
    fn sanitize_durable(&self, value: &Value) -> Result<Value, String> {
        self.delegate.sanitize_durable(value)
    }
    fn pending_approval<'a>(&'a self, _run: &'a str) -> BoxFuture<'a, bool> {
        self.delegate.pending_approval(_run)
    }
    fn approval_blocked<'a>(&'a self, _run: &'a str) -> BoxFuture<'a, Option<String>> {
        self.delegate.approval_blocked(_run)
    }
    fn observe_untrusted<'a>(
        &'a self,
        _run: &'a str,
        _source: &'a str,
    ) -> BoxFuture<'a, Result<(), String>> {
        self.delegate.observe_untrusted(_run, _source)
    }
    fn configure_process_boundary<'a>(
        &'a self,
        _run: &'a str,
        _boundary: ProcessBoundary,
    ) -> BoxFuture<'a, Result<(), String>> {
        self.delegate.configure_process_boundary(_run, _boundary)
    }
    fn execution_tool_budget<'a>(
        &'a self,
        _run: &'a str,
    ) -> BoxFuture<'a, Result<Option<u64>, String>> {
        self.delegate.execution_tool_budget(_run)
    }
    fn result_requires_approval(&self, _result: &ToolResult) -> bool {
        self.delegate.result_requires_approval(_result)
    }
    fn durable_blob_inspector(&self) -> Option<Box<dyn DurableByteInspector>> {
        self.delegate.durable_blob_inspector()
    }
    fn configure_network_authorizer<'a>(
        &'a self,
        _run: &'a str,
        _authorizer: std::sync::Arc<dyn NetworkAuthorizer>,
    ) -> BoxFuture<'a, Result<(), String>> {
        self.delegate
            .configure_network_authorizer(_run, _authorizer)
    }
    fn finalization_guard<'a>(
        &'a self,
        _run: &'a str,
    ) -> BoxFuture<'a, Result<Box<dyn FinalizationGuard>, String>> {
        self.delegate.finalization_guard(_run)
    }
    fn deferred_definitions<'a>(
        &'a self,
        _run: &'a str,
    ) -> BoxFuture<'a, Result<Vec<ToolDefinition>, String>> {
        self.delegate.deferred_definitions(_run)
    }
    fn live_status(&self, _run: &str) -> Vec<serde_json::Value> {
        self.delegate.live_status(_run)
    }

    fn definitions(&self) -> Vec<ToolDefinition> {
        let mut d = self.delegate.definitions();
        d.extend(definitions());
        d.sort_by(|a, b| a.name.cmp(&b.name));
        d
    }
    fn run_definitions<'a>(
        &'a self,
        run: &'a str,
    ) -> BoxFuture<'a, Result<Option<Vec<ToolDefinition>>, String>> {
        Box::pin(async move {
            self.read(run).await?.ok_or("unbound skill session")?;
            let mut d = self
                .delegate
                .run_definitions(run)
                .await?
                .unwrap_or_else(|| self.delegate.definitions());
            d.extend(definitions());
            d.sort_by(|a, b| a.name.cmp(&b.name));
            Ok(Some(d))
        })
    }
    fn session_context<'a>(&'a self, run: &'a str) -> BoxFuture<'a, Result<RunContext, String>> {
        Box::pin(async move {
            let mut context = self.delegate.session_context(run).await?;
            let (scope, session, suggestions) =
                self.read(run).await?.ok_or("unbound skill session")?;
            context
                .identity
                .push_str(&format!("\nFrozen skill catalog:\n{}", session.catalog));
            context.suggested_skills.extend(suggestions);
            context.suggested_skills.truncate(3);
            let index = self.indexes.get(&scope).ok_or("skill scope unavailable")?;
            for delta in session.deltas(index) {
                context.updates.push((
                    format!(
                        "skill-delta:{}:{}:{}",
                        delta.name,
                        delta.previous_hash,
                        delta.current_hash.as_deref().unwrap_or("removed")
                    ),
                    format!(
                        "Skill context update: {}",
                        serde_json::to_string(&delta).map_err(storage)?
                    ),
                ));
            }
            Ok(context)
        })
    }
    fn scheduling<'a>(
        &'a self,
        run: &'a str,
        call: &'a ToolCall,
    ) -> BoxFuture<'a, Option<ToolScheduling>> {
        Box::pin(async move {
            if own(&call.name) {
                Some(ToolScheduling::isolated())
            } else {
                self.delegate.scheduling(run, call).await
            }
        })
    }
    fn audit_builtin<'a>(
        &'a self,
        run: &'a str,
        call: &'a ToolCall,
        result: Option<&'a ToolResult>,
    ) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async move {
            self.check(run, call).await?;
            self.delegate.audit_builtin(run, call, result).await
        })
    }
    fn prepare<'a>(
        &'a self,
        run: &'a str,
        call: ToolCall,
    ) -> BoxFuture<'a, Result<PreparedTool, String>> {
        Box::pin(async move {
            self.check(run, &call).await?;
            if !own(&call.name) {
                return self.delegate.prepare(run, call).await;
            }
            let args: Value = serde_json::from_str(&call.arguments).map_err(storage)?;
            let definition = definitions()
                .into_iter()
                .find(|d| d.name == call.name)
                .unwrap();
            if !jsonschema::validator_for(&definition.input_schema)
                .map_err(storage)?
                .is_valid(&args)
            {
                return Err("invalid skill tool arguments".into());
            }
            self.delegate.audit_builtin(run, &call, None).await?;
            Ok(PreparedTool {
                call,
                effect: Effect::Keyed,
                resources: vec![],
                exclusive: true,
                data: json!({"skill_run":run}),
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
            let gate = self.gate(run);
            let (_shared, _exclusive) = if tool.call.name == "load_skill" {
                (None, Some(gate.write_owned().await))
            } else {
                (Some(gate.read_owned().await), None)
            };
            if own(&tool.call.name) {
                if tool.data["skill_run"] != run {
                    return ToolResult::error("skill prepared run mismatch");
                }
                let result = match self.perform(run, key, tool).await {
                    Ok(r) => r,
                    Err(e) => ToolResult::error(e),
                };
                if let Err(e) = self
                    .delegate
                    .audit_builtin(run, &tool.call, Some(&result))
                    .await
                {
                    return ToolResult::error(e);
                }
                result
            } else {
                if let Err(e) = self.check(run, &tool.call).await {
                    return ToolResult::error(e);
                }
                self.delegate.execute(run, key, tool).await
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
            let _guard = self.gate(run).read_owned().await;
            if own(&tool.call.name) {
                if tool.data["skill_run"] != run {
                    return Reconcile::Uncertain("skill prepared run mismatch".into());
                }
                return match self.receipt(run, key, &tool.call).await {
                    Ok(Some(r)) => Reconcile::Done(r),
                    Ok(None) => Reconcile::Retry,
                    Err(e) => Reconcile::Uncertain(e),
                };
            }
            if let Err(e) = self.check(run, &tool.call).await {
                return Reconcile::Uncertain(e);
            }
            self.delegate.reconcile(run, key, tool).await
        })
    }
}
