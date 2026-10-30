//! Daemon wiring for long-term memory: run bindings from capability tokens, memory blocks
//! for prepared delegations, master memory tools and citation checks, escalation of
//! repeated `doc_edit` conflicts and the background maintenance loop.
use excalibur_master::FastActions;
use excalibur_memory::{
    BindingRole, BindingRunTree, ConsolidationRoles, ContextBudget, EmbeddingProvider,
    IDENTITY_MARKER, MEMORY_CORE_TOOLS, MaintenanceConfig, MemoryBinding, MemoryEscalation,
    MemoryGrant, MemoryServices, MemoryStore, MemoryTools, ModelRole, PREFERENCES_MARKER,
    ReviewBudget, TASK_MEMORY_MARKER, UserActivity, memory_tool_definitions, replace_block,
};
use excalibur_models::ModelGateway;
use excalibur_orchestrator::{Agent, Delegation, Orchestrator};
use excalibur_store::{ReadLane, rusqlite::OptionalExtension};
use excalibur_types::runtime::{BoxFuture, ToolDefinition, ToolRuntime};
use serde_json::{Value, json};
use std::{
    collections::BTreeSet,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

/// `library_ingest` is a deferred domain tool: a keyed local write, not a fast master tool.
pub fn library_ingest_entry() -> excalibur_tools::deferred::CatalogTool {
    use excalibur_tools::deferred::{CatalogTool, ToolEffect, ToolRisk};
    let definition = memory_tool_definitions()
        .into_iter()
        .find(|d| d.name == "library_ingest")
        .expect("library_ingest definition");
    let mut entry = CatalogTool::from_definition(definition);
    entry.tags = vec!["documents".into(), "library".into(), "memory".into()];
    entry.effect = ToolEffect::WriteLocal;
    entry.idempotency = excalibur_types::runtime::Effect::Keyed;
    entry.risk = ToolRisk::Low;
    entry.timeout_ms = 60_000;
    entry.resources_description =
        "Stores a versioned original and its chunks in the principal database.".into();
    entry
}

/// Trusted host memory settings. Clients cannot change them.
#[derive(Clone)]
pub struct MemoryOptions {
    pub principal: String,
    pub timezone: excalibur_memory::chrono_tz::Tz,
    /// Embedding provider for the active space; without one, retrieval is lexical,
    /// temporal and relational and says so.
    pub embedder: Option<Arc<dyn EmbeddingProvider>>,
    /// Optional calibrated roles; the daemon supplies its user-lane cheap fallback.
    pub services: MemoryServices,
    /// Background maintenance period; `None` leaves maintenance to explicit calls.
    pub maintenance_interval: Option<Duration>,
    pub review_budget: ReviewBudget,
    pub context_budget: ContextBudget,
}
impl Default for MemoryOptions {
    fn default() -> Self {
        Self {
            principal: "local".into(),
            timezone: excalibur_memory::chrono_tz::UTC,
            embedder: None,
            services: MemoryServices::default(),
            maintenance_interval: Some(Duration::from_secs(300)),
            review_budget: ReviewBudget::default(),
            context_budget: ContextBudget::default(),
        }
    }
}

pub struct MemoryHost {
    pub store: MemoryStore,
    pub tools: Arc<MemoryTools>,
    pub options: MemoryOptions,
    orchestrator: Arc<Mutex<Option<Orchestrator>>>,
    stop: Arc<AtomicBool>,
    maintenance: Mutex<Option<tokio::task::AbortHandle>>,
}

fn scopes_for(token_scopes: &BTreeSet<String>, scope: &str) -> BTreeSet<String> {
    token_scopes
        .iter()
        .filter(|s| s.as_str() == "general" || s.as_str() == scope)
        .cloned()
        .collect()
}
fn master_run(conversation: &str, scope: &str) -> String {
    format!("master:{conversation}:{scope}")
}

impl MemoryHost {
    pub async fn open(
        store: MemoryStore,
        delegate: Arc<dyn ToolRuntime>,
        options: MemoryOptions,
    ) -> Result<Arc<Self>, String> {
        if options
            .maintenance_interval
            .is_some_and(|period| period.is_zero())
        {
            return Err("memory maintenance interval must be positive".into());
        }
        if let Some(embedder) = &options.embedder {
            store
                .embeddings()
                .attach(embedder.clone(), 512, excalibur_store::unix_seconds())
                .await
                .map_err(|e| e.to_string())?;
        }
        let store = store.with_services(options.services.clone());
        let orchestrator = Arc::new(Mutex::new(None));
        let tools =
            MemoryTools::new(delegate, store.clone()).with_escalation(Arc::new(Escalation {
                orchestrator: orchestrator.clone(),
            }));
        Ok(Arc::new(Self {
            tools: Arc::new(tools),
            store,
            options,
            orchestrator,
            stop: Arc::new(AtomicBool::new(false)),
            maintenance: Mutex::new(None),
        }))
    }
    /// Escalation needs the orchestrator, which is created after the tool runtime.
    pub fn attach(&self, orchestrator: &Orchestrator) {
        *self.orchestrator.lock().unwrap_or_else(|e| e.into_inner()) = Some(orchestrator.clone());
    }
    /// Bind a worker run to the data scopes of its capability token.
    pub async fn bind_agent(
        &self,
        agent: &Agent,
        parent_run: Option<String>,
    ) -> Result<(), String> {
        let Some(run) = &agent.run_id else {
            return Ok(());
        };
        let scopes = scopes_for(&agent.token.data_scopes, &agent.scope);
        if !scopes.contains(&agent.scope) {
            return Err("agent scope is not in its capability token".into());
        }
        self.tools
            .bind(MemoryBinding {
                run: run.clone(),
                principal: self.options.principal.clone(),
                scopes,
                primary_scope: agent.scope.clone(),
                conversation: Some(agent.origin_conversation.clone()),
                task: Some(agent.task.clone()),
                parent_run,
                role: BindingRole::Worker,
                timezone: self.options.timezone,
            })
            .await
            .map_err(|e| e.to_string())
    }
    /// Memory blocks for a delegation, rendered for the child's own grant.
    pub async fn prepare_context(&self, delegation: &mut Delegation) -> Result<(), String> {
        let scopes = scopes_for(&delegation.token.data_scopes, &delegation.scope);
        let context = &mut delegation.spec.context;
        if scopes.is_empty() {
            replace_block(&mut context.layers[1], IDENTITY_MARKER, "");
            replace_block(&mut context.layers[2], PREFERENCES_MARKER, "");
            replace_block(&mut context.layers[3], TASK_MEMORY_MARKER, "");
            return Ok(());
        }
        let grant = MemoryGrant::new(self.options.principal.clone(), scopes)
            .with_timezone(self.options.timezone)
            .with_conversation(delegation.origin_conversation.clone())
            .with_task(delegation.task.clone());
        let memory = self
            .store
            .task_context(
                &grant,
                &format!("{}\n{}", delegation.goal, delegation.deliverable),
                None,
                &self.options.context_budget,
                excalibur_store::unix_seconds(),
            )
            .await
            .map_err(|e| e.to_string())?;
        replace_block(&mut context.layers[1], IDENTITY_MARKER, &memory.identity);
        replace_block(
            &mut context.layers[2],
            PREFERENCES_MARKER,
            &memory.preferences,
        );
        replace_block(&mut context.layers[3], TASK_MEMORY_MARKER, &memory.recall);
        Ok(())
    }
    async fn conversation_scope(&self, conversation: &str) -> Result<String, String> {
        let conversation = conversation.to_string();
        self.store
            .database()
            .read(ReadLane::Interactive, move |c| {
                Ok(c.query_row(
                    "SELECT COALESCE(active_scope,'general') FROM conversations WHERE id=?1",
                    [&conversation],
                    |r| r.get::<_, String>(0),
                )
                .optional()?
                .unwrap_or_else(|| "general".into()))
            })
            .await
            .map_err(|e| e.to_string())
    }
    async fn master_binding(
        &self,
        conversation: &str,
        scope: &str,
    ) -> Result<MemoryBinding, String> {
        let mut scopes = BTreeSet::from(["general".to_string()]);
        scopes.insert(scope.to_string());
        let binding = MemoryBinding {
            run: master_run(conversation, scope),
            principal: self.options.principal.clone(),
            scopes,
            primary_scope: scope.to_string(),
            conversation: Some(conversation.to_string()),
            task: None,
            parent_run: None,
            role: BindingRole::Master,
            timezone: self.options.timezone,
        };
        self.store
            .bind_run(&binding, excalibur_store::unix_seconds())
            .await
            .map_err(|e| e.to_string())?;
        Ok(binding)
    }
    /// A profile edit is an explicit user request only when its provenance is a user message
    /// of this conversation.
    async fn user_message_in(&self, conversation: &str, message: Option<&str>) -> bool {
        let Some(message) = message else {
            return false;
        };
        let (conversation, message, principal) = (
            conversation.to_string(),
            message.to_string(),
            self.options.principal.clone(),
        );
        self.store
            .database()
            .read(ReadLane::Interactive, move |c| {
                Ok(c.query_row(
                    "SELECT EXISTS(SELECT 1 FROM messages m JOIN master_user_sources s ON s.id=m.id WHERE m.id=?1 AND m.conversation_id=?2 AND m.principal_id=?3 AND m.role='user')",
                    [&message, &conversation, &principal],
                    |r| r.get::<_, bool>(0),
                )?)
            })
            .await
            .unwrap_or(false)
    }
    pub async fn master_tool(
        &self,
        conversation: &str,
        scope: &str,
        name: &str,
        args: Value,
        key: &str,
    ) -> Result<Value, String> {
        self.master_tool_from_source(conversation, scope, None, name, args, key)
            .await
    }
    #[allow(clippy::too_many_arguments)]
    async fn master_tool_from_source(
        &self,
        conversation: &str,
        scope: &str,
        source: Option<&str>,
        name: &str,
        args: Value,
        key: &str,
    ) -> Result<Value, String> {
        let binding = self.master_binding(conversation, scope).await?;
        let human = self.user_message_in(conversation, source).await;
        let explicit = human
            && (name != "doc_edit" || args.get("provenance").and_then(Value::as_str) == source);
        self.store
            .call_bound(
                &binding,
                name,
                args,
                key,
                explicit,
                excalibur_store::unix_seconds(),
            )
            .await
            .map_err(|e| e.to_string())
    }
    async fn finalize(&self, conversation: &str, scope: &str, text: &str) -> String {
        if !text.contains("[[") {
            return text.to_owned();
        }
        let binding = match self.master_binding(conversation, scope).await {
            Ok(binding) => binding,
            Err(_) => return strip_citations(text),
        };
        let tree = BindingRunTree {
            store: self.store.clone(),
        };
        match self
            .store
            .validate_citations(&binding.grant(), &binding.run, &tree, text, "en")
            .await
        {
            Ok(cited) => cited.text,
            Err(_) => {
                // Never publish unchecked citation tokens when storage/tree lookup fails.
                strip_citations(text)
            }
        }
    }
    async fn stable(&self, conversation: &str, scope: &str) -> (String, String) {
        let Ok(binding) = self.master_binding(conversation, scope).await else {
            return (String::new(), String::new());
        };
        match self
            .store
            .task_context(
                &binding.grant(),
                "",
                None,
                &self.options.context_budget,
                excalibur_store::unix_seconds(),
            )
            .await
        {
            Ok(memory) => (memory.identity, memory.preferences),
            Err(_) => (String::new(), String::new()),
        }
    }
    /// Background maintenance on the work runtime with the maintenance model lane.
    pub fn spawn_maintenance(
        self: &Arc<Self>,
        handle: &tokio::runtime::Handle,
        model: Option<Arc<dyn ModelGateway>>,
        scopes: Vec<String>,
    ) {
        let Some(interval) = self.options.maintenance_interval else {
            return;
        };
        let mut maintenance = self.maintenance.lock().unwrap_or_else(|e| e.into_inner());
        if maintenance.is_some() {
            return;
        }
        let host = self.clone();
        let task = handle.spawn(async move {
            let roles = model.map(|model| {
                let role = Arc::new(ModelRole::new("memory-maintenance", model));
                ConsolidationRoles {
                    extractor: role.clone(),
                    compactor: Some(role),
                }
            });
            let config = MaintenanceConfig {
                principal: host.options.principal.clone(),
                timezone: host.options.timezone,
                scopes,
                budget: host.options.review_budget.clone(),
                queue_batch: 64,
            };
            let activity = RecentUserActivity {
                host: host.clone(),
                active: AtomicBool::new(false),
            };
            loop {
                tokio::time::sleep(interval).await;
                if host.stop.load(Ordering::SeqCst) {
                    return;
                }
                activity.refresh().await;
                let tick = host.store.maintenance_tick(
                    &config,
                    roles.as_ref(),
                    &activity,
                    excalibur_store::unix_seconds(),
                );
                tokio::pin!(tick);
                loop {
                    tokio::select! {
                        _ = &mut tick => break,
                        _ = tokio::time::sleep(Duration::from_millis(250)) => {
                            if host.stop.load(Ordering::SeqCst) { return; }
                            activity.refresh().await;
                        }
                    }
                }
            }
        });
        *maintenance = Some(task.abort_handle());
    }
    /// Release the host attachment after worker/master shutdown. The runtime's
    /// citation tree and authority otherwise keep each other's database alive.
    pub fn detach(&self) {
        self.orchestrator
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
    }
    pub fn stop(&self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(task) = self
            .maintenance
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
        {
            task.abort();
        }
    }
}

/// The user counts as active when a user message arrived in the last five minutes.
struct RecentUserActivity {
    host: Arc<MemoryHost>,
    active: AtomicBool,
}
impl RecentUserActivity {
    async fn refresh(&self) {
        let principal = self.host.options.principal.clone();
        let since = excalibur_store::unix_seconds() - 300;
        let active = self
            .host
            .store
            .database()
            .read(ReadLane::Background, move |c| {
                Ok(c.query_row(
                    "SELECT EXISTS(SELECT 1 FROM messages WHERE principal_id=?1 AND role='user' AND sent_at>=?2)",
                    excalibur_store::rusqlite::params![principal, since],
                    |r| r.get::<_, bool>(0),
                )?)
            })
            .await
            .unwrap_or(true);
        self.active.store(active, Ordering::SeqCst);
    }
}
impl UserActivity for RecentUserActivity {
    fn active(&self) -> bool {
        self.active.load(Ordering::SeqCst)
    }
}

struct Escalation {
    orchestrator: Arc<Mutex<Option<Orchestrator>>>,
}
impl MemoryEscalation for Escalation {
    fn escalate<'a>(&'a self, run: &'a str, detail: Value) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async move {
            let orchestrator = self
                .orchestrator
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone()
                .ok_or("memory escalation is not attached")?;
            let agent = orchestrator
                .agent_for_run(run)
                .await
                .map_err(|e| e.to_string())?;
            let parent = agent
                .parent_id
                .clone()
                .ok_or("the run has no parent to escalate to")?;
            let key = detail["tool_key"].as_str().unwrap_or("doc_edit");
            orchestrator
                .send_message(
                    &format!("memory-escalation:{run}:{key}"),
                    &agent.id,
                    &parent,
                    "blocked",
                    json!({"reason": "Two consecutive doc_edit hash mismatches on the same fragment; resolve the edit before retrying.", "detail": detail}),
                    "normal",
                )
                .await
                .map_err(|e| e.to_string())?;
            Ok(())
        })
    }
}

/// Master fast actions plus the memory tools of the master role.
pub struct MemoryFastActions {
    inner: Arc<dyn FastActions>,
    host: Arc<MemoryHost>,
}
impl MemoryFastActions {
    pub fn new(inner: Arc<dyn FastActions>, host: Arc<MemoryHost>) -> Self {
        Self { inner, host }
    }
    fn own(name: &str) -> bool {
        MEMORY_CORE_TOOLS.contains(&name)
    }
}
impl FastActions for MemoryFastActions {
    fn definitions(&self) -> Vec<ToolDefinition> {
        let mut definitions = self.inner.definitions();
        definitions.retain(|d| !Self::own(&d.name));
        definitions.extend(
            memory_tool_definitions()
                .into_iter()
                .filter(|d| Self::own(&d.name)),
        );
        definitions
    }
    fn execute<'a>(
        &'a self,
        conversation: &'a str,
        command: &'a str,
        target: Option<&'a str>,
        key: &'a str,
    ) -> BoxFuture<'a, Result<Value, String>> {
        self.inner.execute(conversation, command, target, key)
    }
    fn confirm_cancel<'a>(
        &'a self,
        conversation: &'a str,
        scope: &'a str,
        target: &'a str,
        key: &'a str,
    ) -> BoxFuture<'a, Result<Value, String>> {
        // This host-only route retains the inner durable human-decision guard;
        // it is deliberately absent from model-facing tool definitions.
        self.inner.confirm_cancel(conversation, scope, target, key)
    }
    fn invoke<'a>(
        &'a self,
        conversation: &'a str,
        name: &'a str,
        arguments: Value,
        key: &'a str,
    ) -> BoxFuture<'a, Result<Value, String>> {
        if !Self::own(name) {
            return self.inner.invoke(conversation, name, arguments, key);
        }
        Box::pin(async move {
            let scope = self.host.conversation_scope(conversation).await?;
            self.host
                .master_tool(conversation, &scope, name, arguments, key)
                .await
        })
    }
    fn invoke_source_scoped<'a>(
        &'a self,
        conversation: &'a str,
        scope: &'a str,
        source_message_id: &'a str,
        name: &'a str,
        arguments: Value,
        key: &'a str,
    ) -> BoxFuture<'a, Result<Value, String>> {
        if !Self::own(name) {
            return self.inner.invoke_source_scoped(
                conversation,
                scope,
                source_message_id,
                name,
                arguments,
                key,
            );
        }
        Box::pin(async move {
            self.host
                .master_tool_from_source(
                    conversation,
                    scope,
                    Some(source_message_id),
                    name,
                    arguments,
                    key,
                )
                .await
        })
    }
    fn invoke_scoped<'a>(
        &'a self,
        conversation: &'a str,
        scope: &'a str,
        name: &'a str,
        arguments: Value,
        key: &'a str,
    ) -> BoxFuture<'a, Result<Value, String>> {
        if !Self::own(name) {
            return self
                .inner
                .invoke_scoped(conversation, scope, name, arguments, key);
        }
        Box::pin(async move {
            self.host
                .master_tool(conversation, scope, name, arguments, key)
                .await
        })
    }
    fn context<'a>(&'a self, conversation: &'a str) -> BoxFuture<'a, Result<Value, String>> {
        self.inner.context(conversation)
    }
    fn stable_context<'a>(
        &'a self,
        conversation: &'a str,
        scope: &'a str,
    ) -> BoxFuture<'a, (String, String)> {
        Box::pin(async move {
            let (inner_identity, inner_scope) =
                self.inner.stable_context(conversation, scope).await;
            let (identity, preferences) = self.host.stable(conversation, scope).await;
            let join = |a: String, b: String| match (a.is_empty(), b.is_empty()) {
                (true, _) => b,
                (_, true) => a,
                _ => format!("{a}\n{b}"),
            };
            (
                join(inner_identity, identity),
                join(inner_scope, preferences),
            )
        })
    }
    fn finalize_response_scoped<'a>(
        &'a self,
        conversation: &'a str,
        scope: &'a str,
        text: &'a str,
    ) -> BoxFuture<'a, String> {
        Box::pin(async move {
            let text = self
                .inner
                .finalize_response_scoped(conversation, scope, text)
                .await;
            self.host.finalize(conversation, scope, &text).await
        })
    }
    fn finalize_response<'a>(
        &'a self,
        conversation: &'a str,
        text: &'a str,
    ) -> BoxFuture<'a, String> {
        Box::pin(async move {
            match self.host.conversation_scope(conversation).await {
                Ok(scope) => {
                    self.finalize_response_scoped(conversation, &scope, text)
                        .await
                }
                Err(_) => strip_citations(text),
            }
        })
    }
}

/// Conservative fallback when the citation database is unavailable.
fn strip_citations(text: &str) -> String {
    let mut out = String::new();
    let mut rest = text;
    while let Some(start) = rest.find("[[") {
        out.push_str(&rest[..start]);
        let tail = &rest[start..];
        if let Some(end) = tail.find("]]") {
            let token = &tail[..end + 2];
            if token.starts_with("[[doc:") || token.starts_with("[[web:") {
                out.push_str("[unsourced]");
            } else {
                out.push_str(token);
            }
            rest = &tail[end + 2..];
        } else {
            out.push_str(tail);
            rest = "";
        }
    }
    out.push_str(rest);
    out
}
