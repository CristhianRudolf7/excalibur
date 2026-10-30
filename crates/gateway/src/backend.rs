//! The transport exposes existing shared services; it has no agent loop of its own.
use crate::{RpcBackend, RpcError};
use excalibur_master::{Incoming, Master};
use excalibur_orchestrator::{Control, Delegation, Orchestrator, Project};
use excalibur_store::{
    ReadLane,
    rusqlite::{self, OptionalExtension},
};
use excalibur_types::runtime::BoxFuture;
use serde::Serialize;
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Component, Path, PathBuf},
    sync::Arc,
};

#[derive(Clone)]
pub struct SharedBackend {
    master: Master,
    task_actions: Option<Arc<dyn excalibur_master::FastActions>>,
    orchestrator: Orchestrator,
    project_roots: Vec<PathBuf>,
    project_registration_roots: Option<BTreeSet<PathBuf>>,
    project_dirs: Vec<Arc<cap_std::fs::Dir>>,
    shutdown: Option<Arc<tokio::sync::Notify>>,
    security: Option<excalibur_security::Security>,
    telemetry: Option<Arc<excalibur_telemetry::Telemetry>>,
    configuration: Option<Value>,
    pub(crate) memory: Option<crate::memory::MemoryAccess>,
}
impl SharedBackend {
    pub fn new(master: Master, orchestrator: Orchestrator) -> Self {
        Self {
            master,
            task_actions: None,
            orchestrator,
            project_roots: Vec::new(),
            project_registration_roots: None,
            project_dirs: Vec::new(),
            shutdown: None,
            security: None,
            telemetry: None,
            configuration: None,
            memory: None,
        }
    }
    /// Expose `memory.*` over the shared memory store for one principal.
    pub fn with_memory(mut self, access: crate::memory::MemoryAccess) -> Self {
        self.memory = Some(access);
        self
    }
    /// Only the trusted host supplies this ceiling, never a JSON-RPC parameter.
    pub fn with_project_roots(mut self, roots: Vec<PathBuf>) -> Result<Self, RpcError> {
        self.project_roots = roots
            .into_iter()
            .map(|root| {
                root.canonicalize()
                    .map_err(|_| RpcError::invalid("project root is unavailable"))
            })
            .collect::<Result<_, _>>()?;
        if self.project_roots.iter().any(|root| !root.is_dir()) {
            return Err(RpcError::invalid("project roots must be directories"));
        }
        self.project_dirs = self
            .project_roots
            .iter()
            .map(|root| {
                cap_std::fs::Dir::open_ambient_dir(root, cap_std::ambient_authority())
                    .map(Arc::new)
                    .map_err(|_| RpcError::invalid("project root cannot be opened"))
            })
            .collect::<Result<_, _>>()?;
        Ok(self)
    }
    /// A live daemon's exact provisioned roots are immutable until host reprovisioning.
    pub fn with_project_registration_roots(
        mut self,
        roots: Vec<PathBuf>,
    ) -> Result<Self, RpcError> {
        let roots = roots
            .into_iter()
            .map(|root| {
                root.canonicalize()
                    .map_err(|_| RpcError::invalid("provisioned project root is unavailable"))
            })
            .collect::<Result<BTreeSet<_>, _>>()?;
        if roots.iter().any(|root| !root.is_dir()) {
            return Err(RpcError::invalid(
                "provisioned project roots must be directories",
            ));
        }
        self.project_registration_roots = Some(roots);
        Ok(self)
    }
    pub fn with_telemetry(mut self, telemetry: Arc<excalibur_telemetry::Telemetry>) -> Self {
        self.telemetry = Some(telemetry);
        self
    }
    pub fn with_configuration(mut self, configuration: Value) -> Self {
        self.configuration = Some(configuration);
        self
    }
    pub fn with_task_actions(mut self, actions: Arc<dyn excalibur_master::FastActions>) -> Self {
        self.task_actions = Some(actions);
        self
    }
    pub fn with_security(mut self, security: excalibur_security::Security) -> Self {
        self.security = Some(security);
        self
    }
    pub fn with_shutdown(mut self, notify: Arc<tokio::sync::Notify>) -> Self {
        self.shutdown = Some(notify);
        self
    }
    fn permits_root(&self, root: &Path) -> bool {
        self.project_roots
            .iter()
            .any(|ceiling| root.starts_with(ceiling))
    }
    async fn checked_project(&self, id: &str) -> Result<Project, RpcError> {
        let project = self
            .orchestrator
            .project(id)
            .await
            .map_err(|_| RpcError::invalid("unknown project"))?;
        for root in &project.roots {
            let canonical = root
                .canonicalize()
                .map_err(|_| RpcError::invalid("project root unavailable"))?;
            if !self.permits_root(&canonical) {
                return Err(RpcError::invalid(
                    "project lies outside host root authority",
                ));
            }
        }
        Ok(project)
    }
    pub(crate) async fn validate_scope(&self, scope: &str) -> Result<(), RpcError> {
        if scope == "general" {
            return Ok(());
        }
        if let Some(project) = scope.strip_prefix("project:") {
            self.checked_project(project).await?;
            if self.project_registration_roots.is_some() {
                let master = self
                    .orchestrator
                    .active_agents()
                    .await
                    .map_err(|_| RpcError::internal())?
                    .into_iter()
                    .find(|agent| agent.depth == 0)
                    .ok_or_else(|| RpcError::invalid("host scope authority is unavailable"))?;
                if !master.token.data_scopes.contains(scope) {
                    return Err(RpcError::invalid(
                        "project scope is not provisioned by the host",
                    ));
                }
            }
            return Ok(());
        }
        // Task and conversation scopes can only be used by their own persisted contracts.
        Err(RpcError::invalid(
            "client scope must be general or a registered authorized project",
        ))
    }
    async fn dispatch_inner(&self, method: &str, params: Value) -> Result<Value, RpcError> {
        match method {
            "conversation.send" => {
                let message: Incoming = serde_json::from_value(params)
                    .map_err(|_| RpcError::invalid("invalid conversation message"))?;
                if let Some(scope) = &message.scope {
                    self.validate_scope(scope).await?;
                } else if self.project_registration_roots.is_some() {
                    // A persisted selector from a previous host configuration
                    // cannot bypass the current scope ceiling by omitting scope.
                    match self.master.conversation_scope(&message.conversation).await {
                        Ok(scope) => self.validate_scope(&scope).await?,
                        Err(excalibur_master::Error::Invalid(_)) => {
                            self.validate_scope("general").await?
                        }
                        Err(_) => return Err(RpcError::internal()),
                    }
                }
                value(
                    self.master
                        .receive(message)
                        .await
                        .map_err(|_| RpcError::invalid("message rejected"))?,
                )
            }
            "conversation.list" => value(
                self.master
                    .conversations(limit(&params, 100, 512)?)
                    .await
                    .map_err(|_| RpcError::internal())?,
            ),
            "conversation.history" => value(
                self.master
                    .history(
                        text(&params, "conversation")?,
                        params
                            .get("after")
                            .and_then(Value::as_str)
                            .map(str::to_owned),
                        limit(&params, 100, 512)?,
                    )
                    .await
                    .map_err(|_| RpcError::invalid("unknown conversation"))?,
            ),
            "conversation.subscribe" => {
                if let Some(conversation) = params.get("conversation").and_then(Value::as_str) {
                    self.master
                        .history(conversation, None, 1)
                        .await
                        .map_err(|_| RpcError::invalid("unknown conversation"))?;
                }
                if params.get("cursor").is_some_and(|cursor| !cursor.is_u64()) {
                    return Err(RpcError::invalid("cursor must be unsigned"));
                }
                Ok(
                    json!({"subscribed":true,"cursor":params.get("cursor").and_then(Value::as_u64).unwrap_or(0)}),
                )
            }
            "conversation.activity" => {
                let conversation = text(&params, "conversation")?;
                self.master
                    .history(conversation, None, 1)
                    .await
                    .map_err(|_| RpcError::invalid("unknown conversation"))?;
                self.orchestrator
                    .connection_activity(
                        conversation,
                        params
                            .get("channel")
                            .and_then(Value::as_str)
                            .unwrap_or("websocket"),
                        params
                            .get("connected")
                            .and_then(Value::as_bool)
                            .unwrap_or(true),
                        excalibur_store::unix_seconds(),
                    )
                    .await
                    .map_err(|_| RpcError::internal())?;
                Ok(json!({"recorded":true}))
            }
            "notification.list" => value(
                self.orchestrator
                    .notifications(text(&params, "conversation")?)
                    .await
                    .map_err(|_| RpcError::internal())?,
            ),
            "notification.ack" => {
                self.orchestrator
                    .acknowledge_notification(text(&params, "notification")?)
                    .await
                    .map_err(|_| RpcError::invalid("unknown notification"))?;
                Ok(json!({"acknowledged":true}))
            }
            "notification.policy" => {
                let conversation = text(&params, "conversation")?;
                self.master
                    .history(conversation, None, 1)
                    .await
                    .map_err(|_| RpcError::invalid("unknown conversation"))?;
                let policy = serde_json::from_value(
                    params
                        .get("policy")
                        .cloned()
                        .ok_or_else(|| RpcError::invalid("policy required"))?,
                )
                .map_err(|_| RpcError::invalid("invalid notification policy"))?;
                self.orchestrator
                    .set_notification_policy(conversation, policy)
                    .await
                    .map_err(|_| RpcError::invalid("notification policy rejected"))?;
                Ok(json!({"updated":true}))
            }
            "conversation.scope" => {
                let scope = text(&params, "scope")?;
                self.validate_scope(scope).await?;
                self.master
                    .set_scope(text(&params, "conversation")?, scope)
                    .await
                    .map_err(|_| RpcError::invalid("scope change rejected"))?;
                Ok(json!({"scope":scope}))
            }
            "conversation.stop" => {
                self.master
                    .stop_turn(text(&params, "conversation")?)
                    .await
                    .map_err(|_| RpcError::invalid("unknown conversation"))?;
                Ok(json!({"stopped":true,"background_runs":"unchanged"}))
            }
            "events.replay" => value(
                self.events(
                    params.get("cursor").and_then(Value::as_u64).unwrap_or(0),
                    limit(&params, 100, 1024)?,
                )
                .await?,
            ),
            "run.list" => {
                let mut rows = Vec::new();
                for agent in self
                    .orchestrator
                    .list_agents()
                    .await
                    .map_err(|_| RpcError::internal())?
                {
                    if agent.depth > 0 {
                        rows.push(
                            self.orchestrator
                                .status(&agent.id)
                                .await
                                .map_err(|_| RpcError::internal())?,
                        );
                    }
                }
                value(rows)
            }
            "metrics.query" => {
                let agent = self
                    .orchestrator
                    .agent(&self.agent_target(text(&params, "target")?).await?)
                    .await
                    .map_err(|_| RpcError::invalid("unknown run"))?;
                let run = agent
                    .run_id
                    .ok_or_else(|| RpcError::invalid("target has no engine run"))?;
                value(
                    self.telemetry
                        .as_ref()
                        .ok_or_else(RpcError::not_found)?
                        .metrics(&run)
                        .await
                        .map_err(|_| RpcError::invalid("metrics are unavailable for this run"))?,
                )
            }
            "config.get" | "config.read" => {
                self.configuration.clone().ok_or_else(RpcError::not_found)
            }
            "conversation.retry_context" => {
                self.master
                    .retry_context(text(&params, "conversation")?)
                    .await
                    .map_err(|_| RpcError::invalid("context retry rejected"))?;
                Ok(json!({"retry_requested":true}))
            }
            "run.status" => value(
                self.orchestrator
                    .status(&self.agent_target(text(&params, "target")?).await?)
                    .await
                    .map_err(|_| RpcError::invalid("unknown run"))?,
            ),
            "run.pause" | "run.resume" | "run.interrupt" | "run.cancel" | "run.redirect" => {
                let agent = self.agent_target(text(&params, "target")?).await?;
                if method == "run.interrupt"
                    && params.get("tool").and_then(Value::as_bool) == Some(true)
                {
                    return value(
                        self.orchestrator
                            .interrupt_tool(&agent)
                            .await
                            .map_err(|_| RpcError::invalid("tool interruption rejected"))?,
                    );
                }
                let control = match method {
                    "run.pause" => Control::Pause,
                    "run.resume" => Control::Resume,
                    "run.interrupt" => Control::Interrupt,
                    "run.cancel" => Control::Cancel {
                        confirmed: params
                            .get("confirmed")
                            .and_then(Value::as_bool)
                            .unwrap_or(false),
                    },
                    _ => Control::Redirect {
                        text: text(&params, "text")?.to_owned(),
                    },
                };
                value(
                    self.orchestrator
                        .control_keyed(text(&params, "operation")?, &agent, control)
                        .await
                        .map_err(|_| RpcError::invalid("control rejected"))?,
                )
            }
            "run.spawn" => {
                let parent = text(&params, "parent")?;
                let delegation: Delegation = serde_json::from_value(
                    params
                        .get("delegation")
                        .cloned()
                        .ok_or_else(|| RpcError::invalid("delegation required"))?,
                )
                .map_err(|_| RpcError::invalid("invalid delegation"))?;
                self.validate_scope(&delegation.scope).await?;
                self.master
                    .conversation_scope(&delegation.origin_conversation)
                    .await
                    .map_err(|_| RpcError::invalid("origin conversation is unavailable"))?;
                value(
                    self.orchestrator
                        .spawn(parent, delegation)
                        .await
                        .map_err(|_| RpcError::invalid("delegation rejected"))?,
                )
            }
            "project.list" => {
                let projects = self
                    .orchestrator
                    .projects()
                    .await
                    .map_err(|_| RpcError::internal())?;
                value(
                    projects
                        .into_iter()
                        .filter(|project| project.roots.iter().all(|root| self.permits_root(root)))
                        .collect::<Vec<_>>(),
                )
            }
            "project.get" => value(self.checked_project(text(&params, "project")?).await?),
            "project.register" => {
                let mut project: Project = serde_json::from_value(params)
                    .map_err(|_| RpcError::invalid("invalid project"))?;
                for root in &mut project.roots {
                    *root = root
                        .canonicalize()
                        .map_err(|_| RpcError::invalid("project root unavailable"))?;
                    if !self.permits_root(root) {
                        return Err(RpcError::invalid("project root exceeds host authority"));
                    }
                }
                if let Some(provisioned) = &self.project_registration_roots {
                    let requested = project.roots.iter().cloned().collect::<BTreeSet<_>>();
                    if !requested.is_subset(provisioned) {
                        return Err(RpcError::invalid(
                            "new project roots require host provisioning and restart",
                        ));
                    }
                    match self.orchestrator.project(&project.id).await {
                        Ok(existing) => {
                            if existing.roots.into_iter().collect::<BTreeSet<_>>() != requested {
                                return Err(RpcError::invalid(
                                    "project root remapping requires host reprovisioning",
                                ));
                            }
                        }
                        Err(excalibur_orchestrator::Error::Missing) => {}
                        Err(_) => return Err(RpcError::internal()),
                    }
                }
                self.orchestrator
                    .register_project(project)
                    .await
                    .map_err(|_| RpcError::invalid("project registration rejected"))?;
                Ok(json!({"registered":true}))
            }
            "project.resolve" => {
                let path = Path::new(text(&params, "path")?)
                    .canonicalize()
                    .map_err(|_| RpcError::invalid("path unavailable"))?;
                if !self.permits_root(&path) {
                    return Err(RpcError::invalid("path exceeds host authority"));
                }
                value(
                    self.orchestrator
                        .resolve_path(
                            &path,
                            params.get("run_project").and_then(Value::as_str),
                            params.get("mentioned_project").and_then(Value::as_str),
                        )
                        .await
                        .map_err(|_| RpcError::internal())?,
                )
            }
            "files.list" => self.list_files(&params).await,
            "task.batch" => {
                let scope = text(&params, "scope")?;
                self.validate_scope(scope).await?;
                let actions = self.task_actions.as_ref().ok_or_else(RpcError::not_found)?;
                actions
                    .invoke_scoped(
                        params
                            .get("conversation")
                            .and_then(Value::as_str)
                            .unwrap_or(""),
                        scope,
                        "task_db",
                        params
                            .get("batch")
                            .cloned()
                            .ok_or_else(|| RpcError::invalid("batch required"))?,
                        text(&params, "operation")?,
                    )
                    .await
                    .map_err(|_| RpcError::invalid("task batch rejected"))
            }
            "task.get" => {
                let scope = text(&params, "scope")?;
                self.validate_scope(scope).await?;
                let tasks = excalibur_tasks::TaskStore::open(self.orchestrator.database().clone())
                    .await
                    .map_err(|_| RpcError::internal())?
                    .with_redactor(
                        self.security
                            .as_ref()
                            .map(excalibur_security::Security::redactor)
                            .unwrap_or_default(),
                    );
                value(
                    tasks
                        .task(scope, text(&params, "task")?)
                        .await
                        .map_err(|_| RpcError::invalid("task unavailable in scope"))?,
                )
            }
            "question.answer" => {
                self.master
                    .answer_question(
                        text(&params, "conversation")?,
                        text(&params, "question")?,
                        text(&params, "answer")?,
                    )
                    .await
                    .map_err(|_| RpcError::invalid("question answer rejected"))?;
                Ok(json!({"answered":true}))
            }
            "question.list" => value(
                self.master
                    .questions(text(&params, "conversation")?)
                    .await
                    .map_err(|_| RpcError::invalid("unknown conversation"))?,
            ),
            "approval.list" => {
                let security = self.security.as_ref().ok_or_else(RpcError::not_found)?;
                let count = limit(&params, 20, 100)?;
                let target = match params.get("target") {
                    Some(Value::String(target)) => {
                        let agent = self
                            .orchestrator
                            .agent(&self.agent_target(target).await?)
                            .await
                            .map_err(|_| RpcError::invalid("unknown run"))?;
                        Some(
                            agent
                                .run_id
                                .ok_or_else(|| RpcError::invalid("target has no engine run"))?,
                        )
                    }
                    None | Some(Value::Null) => None,
                    _ => return Err(RpcError::invalid("invalid approval target")),
                };
                let (after_time, after_id) = match params.get("after") {
                    None | Some(Value::Null) => (None, String::new()),
                    Some(after) => {
                        let at = after
                            .get("created_at")
                            .and_then(Value::as_i64)
                            .filter(|at| *at >= 0)
                            .ok_or_else(|| RpcError::invalid("invalid approval cursor"))?;
                        let id = text(after, "id")?.to_owned();
                        (Some(at), id)
                    }
                };
                let now = excalibur_store::unix_seconds();
                let mut rows=self.orchestrator.database().read(ReadLane::Interactive,move|connection|{
                    let mut query=connection.prepare("SELECT a.id,a.run_id,a.project,a.action_hash,a.created_at,a.expires_at,a.scope,a.request,COALESCE((SELECT json_extract(j.payload,'$.state') FROM journal j WHERE j.run_id=a.run_id AND j.kind='state_change' AND j.state='confirmed' ORDER BY j.sequence DESC LIMIT 1),r.state) FROM security_approvals a JOIN runs r ON r.id=a.run_id WHERE a.status='pending' AND a.expires_at>?1 AND (?2 IS NULL OR a.created_at>?2 OR a.created_at=?2 AND a.id>?3) AND (?4 IS NULL OR a.run_id=?4) ORDER BY a.created_at,a.id LIMIT ?5")?;
                    Ok(query.query_map(rusqlite::params![now,after_time,after_id,target,count+1],|row|Ok(json!({"id":row.get::<_,String>(0)?,"run_id":row.get::<_,String>(1)?,"project":row.get::<_,String>(2)?,"action_hash":row.get::<_,String>(3)?,"created_at":row.get::<_,i64>(4)?,"expires_at":row.get::<_,i64>(5)?,"proposed_scope":row.get::<_,String>(6)?.to_ascii_lowercase(),"request":serde_json::from_str::<Value>(&row.get::<_,String>(7)?).map_err(|error|rusqlite::Error::FromSqlConversionFailure(7,rusqlite::types::Type::Text,Box::new(error)))?,"run_state":row.get::<_,String>(8)?})))?.collect::<std::result::Result<Vec<_>,_>>()?)
                }).await.map_err(|_|RpcError::internal())?;
                let has_more = rows.len() > count as usize;
                rows.truncate(count as usize);
                let next = has_more.then(|| {
                    let last = rows.last().unwrap();
                    json!({"created_at":last["created_at"],"id":last["id"]})
                });
                Ok(security
                    .redactor()
                    .redact_json(&json!({"approvals":rows,"has_more":has_more,"next":next})))
            }
            "approval.respond" => {
                let security = self.security.as_ref().ok_or_else(RpcError::not_found)?;
                let approved = params
                    .get("approved")
                    .and_then(Value::as_bool)
                    .ok_or_else(|| RpcError::invalid("approved boolean required"))?;
                let scope = match params
                    .get("scope")
                    .and_then(Value::as_str)
                    .unwrap_or("once")
                {
                    "once" => excalibur_security::ApprovalScope::Once,
                    "task" => excalibur_security::ApprovalScope::Task,
                    "project" => excalibur_security::ApprovalScope::Project,
                    _ => return Err(RpcError::invalid("invalid approval scope")),
                };
                let approval = text(&params, "approval")?;
                let operation = text(&params, "operation")?;
                let changed = security
                    .answer_approval(
                        approval,
                        approved,
                        scope,
                        operation,
                        excalibur_store::unix_seconds(),
                    )
                    .await
                    .map_err(|_| RpcError::invalid("approval response rejected"))?;
                let key = approval.to_owned();
                let record=self.orchestrator.database().read(ReadLane::Interactive,move|conn|Ok(conn.query_row("SELECT run_id,response_id,status FROM security_approvals WHERE id=?1",[key],|row|Ok((row.get::<_,String>(0)?,row.get::<_,Option<String>>(1)?,row.get::<_,String>(2)?))).optional()?)).await.map_err(|_|RpcError::internal())?;
                if let Some((run, response, status)) = record
                    && response.as_deref() == Some(operation)
                    && matches!(status.as_str(), "approved" | "rejected")
                {
                    let engine = self.orchestrator.engine();
                    let state = engine.load(&run).await.map_err(|_| RpcError::internal())?;
                    if state.status == excalibur_engine::Status::Waiting
                        && state.reason.as_deref() == Some("approval")
                        && engine
                            .wake(
                                &run,
                                excalibur_engine::WakeEvent::ApprovalDecision,
                                approval,
                            )
                            .await
                            .is_err()
                    {
                        let current = engine.load(&run).await.map_err(|_| RpcError::internal())?;
                        if current.status == excalibur_engine::Status::Waiting
                            && current.reason.as_deref() == Some("approval")
                        {
                            return Err(RpcError::internal());
                        }
                    }
                    // Tool admission produces a system pause rather than the
                    // host's structured waiting state. This method re-reads the
                    // reason using a revision CAS without waiting for live
                    // inference; a newer user pause is never resumed.
                    engine
                        .resume_after_approval(&run)
                        .await
                        .map_err(|_| RpcError::internal())?;
                }
                value(changed)
            }
            "daemon.stop" => {
                let Some(notify) = &self.shutdown else {
                    return Err(RpcError::not_found());
                };
                let notify = notify.clone();
                tokio::spawn(async move {
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                    notify.notify_one();
                });
                Ok(json!({"stopping":true}))
            }
            method if method.starts_with("memory.") => self.memory_method(method, params).await,
            // No interfaces to unbuilt skills/agenda or arbitrary configuration mutation.
            _ => Err(RpcError::not_found()),
        }
    }
    async fn agent_target(&self, target: &str) -> Result<String, RpcError> {
        if self.orchestrator.agent(target).await.is_ok() {
            return Ok(target.to_owned());
        }
        self.orchestrator
            .agent_for_run(target)
            .await
            .map(|agent| agent.id)
            .map_err(|_| RpcError::invalid("unknown run or agent"))
    }
    async fn list_files(&self, params: &Value) -> Result<Value, RpcError> {
        let project = self.checked_project(text(params, "project")?).await?;
        let root_index = params.get("root").and_then(Value::as_u64).unwrap_or(0) as usize;
        let root = project
            .roots
            .get(root_index)
            .ok_or_else(|| RpcError::invalid("unknown root"))?
            .canonicalize()
            .map_err(|_| RpcError::invalid("root unavailable"))?;
        let relative = params.get("path").and_then(Value::as_str).unwrap_or("");
        if Path::new(relative)
            .components()
            .any(|c| !matches!(c, Component::Normal(_) | Component::CurDir))
        {
            return Err(RpcError::invalid("relative directory required"));
        }
        if sensitive_path(Path::new(relative)) {
            return Err(RpcError::invalid("protected directory"));
        }
        let relative = PathBuf::from(relative);
        let folder = root
            .join(&relative)
            .canonicalize()
            .map_err(|_| RpcError::invalid("directory unavailable"))?;
        if !folder.starts_with(&root) || !folder.is_dir() {
            return Err(RpcError::invalid("directory escapes project"));
        }
        let authority = self
            .project_roots
            .iter()
            .zip(&self.project_dirs)
            .find(|(ceiling, _)| root.starts_with(ceiling))
            .ok_or_else(|| RpcError::invalid("root exceeds authority"))?;
        let project_dir = Arc::new(
            authority
                .1
                .open_dir(nonempty(
                    root.strip_prefix(authority.0)
                        .map_err(|_| RpcError::internal())?,
                ))
                .map_err(|_| RpcError::invalid("project root unavailable"))?,
        );
        let directory = project_dir
            .open_dir(nonempty(&relative))
            .map_err(|_| RpcError::invalid("directory escapes project"))?;
        let after = params
            .get("after")
            .and_then(Value::as_str)
            .map(str::to_owned);
        let max = limit(params, 100, 500)? as usize;
        let only_agent = params
            .get("agent_only")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let root_text = folder.to_string_lossy().to_string();
        let edited = if only_agent {
            self.orchestrator.database().read(ReadLane::Interactive,move|c|{
            let present:bool=c.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE name='file_versions' AND type='table')",[],|r|r.get(0))?;
            if !present{return Ok(BTreeSet::new());}let mut q=c.prepare("SELECT DISTINCT ?2 || CASE instr(substr(path,length(?2)+1),'/') WHEN 0 THEN substr(path,length(?2)+1) ELSE substr(path,length(?2)+1,instr(substr(path,length(?2)+1),'/')-1) END FROM file_versions WHERE path LIKE ?1 ESCAPE '\\' ORDER BY 1")?;
            let pattern=format!("{}/%",root_text.replace('\\',"\\\\").replace('%',"\\%").replace('_',"\\_"));
            Ok(q.query_map([pattern,format!("{root_text}/")],|r|r.get::<_,String>(0))?.collect::<std::result::Result<BTreeSet<_>,_>>()?)
        }).await.map_err(|_|RpcError::internal())?
        } else {
            BTreeSet::new()
        };
        tokio::task::spawn_blocking(move||{
            // Keep only the next page and one lookahead, irrespective of directory size.
            let mut entries = BTreeMap::new();
            let mut matchers = Vec::new();
            let mut ancestor = PathBuf::new();
            // Repository excludes are lowest precedence; nested .gitignore files
            // are anchored at their own directory and override ancestor rules.
            let mut ignore_paths=vec![PathBuf::from(".git/info/exclude"), PathBuf::from(".gitignore")];
            for component in relative.components(){if let Component::Normal(part)=component{ancestor.push(part);ignore_paths.push(ancestor.join(".gitignore"));}}
            for ignore_path in ignore_paths {
                if let Ok(metadata)=project_dir.metadata(&ignore_path) {
                    if metadata.len()>1024*1024{return Err(RpcError::invalid("ignore file exceeds size limit"));}
                    if let Ok(content)=project_dir.read_to_string(&ignore_path){
                        let base = if ignore_path == Path::new(".git/info/exclude") { root.clone() } else { root.join(ignore_path.parent().unwrap_or(Path::new(""))) };
                        let mut builder=ignore::gitignore::GitignoreBuilder::new(base);
                        for line in content.lines(){builder.add_line(Some(root.join(&ignore_path)),line).map_err(|_|RpcError::invalid("invalid ignore pattern"))?;}
                        matchers.push(builder.build().map_err(|_|RpcError::invalid("invalid ignore rules"))?);
                    }
                }
            }
            for item in directory.entries().map_err(|_|RpcError::invalid("directory listing failed"))? {
                let entry=item.map_err(|_|RpcError::invalid("directory listing failed"))?;
                let Some(name)=entry.file_name().to_str().map(str::to_owned) else {continue;};
                let path=relative.join(&name);
                if sensitive_path(&path){continue;}
                let metadata=directory.symlink_metadata(&name).map_err(|_|RpcError::invalid("entry unavailable"))?;
                let is_directory=metadata.is_dir();
                let absolute_path=root.join(&path);
                let mut ignored=false;
                for matcher in &matchers {
                    let matched=matcher.matched_path_or_any_parents(&absolute_path,is_directory);
                    if matched.is_ignore(){ignored=true;}else if matched.is_whitelist(){ignored=false;}
                }
                if ignored {continue;}
                // Protective mountpoints are host metadata, not generated project
                // files. Preserve nonempty CI/config directories as user content.
                if is_directory && matches!(name.as_str(), ".agents"|".codex"|".github"|".gitlab"|".circleci"|".config")
                    && directory.open_dir(&name).ok().and_then(|d|d.entries().ok()).is_some_and(|mut items|items.next().is_none()) {continue;}
                let absolute=absolute_path.to_string_lossy().to_string();
                if only_agent && !edited.contains(&absolute) && !(is_directory&&edited.iter().any(|p|Path::new(p).starts_with(&absolute_path))) {continue;}
                if after.as_ref().is_some_and(|after|name<=*after){continue;}
                entries.insert(name.clone(),json!({"name":name,"path":path,"kind":if is_directory{"directory"}else if metadata.file_type().is_symlink(){"symlink"}else{"file"},"collapsed":is_directory,"agent_modified":edited.contains(&absolute),"size":metadata.len()}));
                if entries.len()>max+1{entries.pop_last();}
            }
            let has_more=entries.len()>max;let mut entries=entries.into_values().collect::<Vec<_>>();entries.truncate(max);let next=has_more.then(||entries.last().unwrap()["name"].clone());Ok(json!({"entries":entries,"next":next,"has_more":has_more}))
        }).await.map_err(|_|RpcError::internal())?
    }
}
fn nonempty(path: &Path) -> &Path {
    if path.as_os_str().is_empty() {
        Path::new(".")
    } else {
        path
    }
}
fn sensitive_path(path: &Path) -> bool {
    path.components().any(|component| {
        let name = component.as_os_str().to_string_lossy().to_ascii_lowercase();
        name == ".git"
            || name == ".excalibur"
            || name == ".ssh"
            || name == ".aws"
            || name == ".env"
            || name.starts_with(".env.")
            || name == "credentials"
            || name == "credentials.json"
            || name.ends_with(".pem")
            || name.ends_with(".key")
    })
}
pub(crate) fn text<'a>(params: &'a Value, key: &str) -> Result<&'a str, RpcError> {
    params
        .get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty() && value.len() <= 4096)
        .ok_or_else(|| RpcError::invalid(format!("{key} is required")))
}
pub(crate) fn limit(params: &Value, default: u32, max: u32) -> Result<u32, RpcError> {
    let n = params
        .get("limit")
        .map_or(Some(default as u64), Value::as_u64)
        .filter(|n| *n > 0 && *n <= max as u64)
        .ok_or_else(|| RpcError::invalid("invalid page limit"))?;
    Ok(n as u32)
}
fn value<T: Serialize>(value: T) -> Result<Value, RpcError> {
    serde_json::to_value(value).map_err(|_| RpcError::internal())
}
impl RpcBackend for SharedBackend {
    fn dispatch<'a>(
        &'a self,
        method: &'a str,
        params: Value,
    ) -> BoxFuture<'a, Result<Value, RpcError>> {
        Box::pin(self.dispatch_inner(method, params))
    }
    fn events(&self, after: u64, limit: u32) -> BoxFuture<'_, Result<Vec<Value>, RpcError>> {
        Box::pin(async move {
            self.master.events(after,limit).await.map_err(|_|RpcError::internal())?.into_iter().map(|event|Ok(json!({"cursor":event.sequence,"conversation":event.conversation_id,"type":event.kind,"payload":event.payload}))).collect()
        })
    }
}
