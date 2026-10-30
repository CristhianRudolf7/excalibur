//! Durable, principal-local agent trees, controls, mailboxes and admission.
#[cfg(test)]
extern crate self as excalibur_orchestrator;
mod agent_wait;
mod control;
mod gate;
mod messaging;
mod notifications;
mod profiles;
mod progress;
mod projects;
#[cfg(test)]
#[path = "../tests/support/mod.rs"]
mod recovery_test_support;
pub use notifications::{
    HostNotificationAuthority, HostNotificationBinding, HostNotificationTarget,
};
pub use profiles::CustomProfile;
pub mod recovery;
mod run_watchdog;
mod scheduling;
mod tools;
pub use gate::OrchestrationGate;
mod types;
use excalibur_engine::{Engine, Status};
use excalibur_store::{
    Database, Priority, ReadLane,
    rusqlite::{self, OptionalExtension},
};
use excalibur_types::{AgentId, runtime::BoxFuture};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};
use tokio::sync::{Mutex as AsyncMutex, Notify};
pub use tools::OrchestratorTools;
pub use types::*;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("orchestration storage: {0}")]
    Storage(#[from] excalibur_store::Error),
    #[error("orchestration engine: {0}")]
    Engine(#[from] excalibur_engine::Error),
    #[error("orchestration journal: {0}")]
    Journal(#[from] excalibur_journal::Error),
    #[error("orchestration serialization: {0}")]
    Json(#[from] serde_json::Error),
    #[error("invalid orchestration request: {0}")]
    Invalid(String),
    #[error("agent or project does not exist")]
    Missing,
    #[error("host capability binding failed: {0}")]
    Authority(String),
}
pub type Result<T> = std::result::Result<T, Error>;
/// The host must bind actual runtime permissions before any execution is admitted.
pub trait RunAuthority: Send + Sync {
    fn sanitize_durable(&self, value: &Value) -> std::result::Result<Value, String> {
        Ok(value.clone())
    }
    fn bind<'a>(&'a self, agent: &'a Agent) -> BoxFuture<'a, std::result::Result<(), String>>;
    /// Host context blocks (for example memory) added once while a delegation is prepared.
    /// The prepared delegation is stored durably, so replays reuse the same bytes.
    fn prepare_context<'a>(
        &'a self,
        _parent: &'a Agent,
        _delegation: &'a mut Delegation,
    ) -> BoxFuture<'a, std::result::Result<(), String>> {
        Box::pin(async { Ok(()) })
    }
    fn admission_capacity(&self) -> AdmissionCapacity {
        AdmissionCapacity::default()
    }
    fn live_status(&self, _run: &str) -> Vec<Value> {
        Vec::new()
    }
    fn interrupt_tool<'a>(
        &'a self,
        _run: &'a str,
    ) -> BoxFuture<'a, std::result::Result<bool, String>> {
        Box::pin(async { Ok(false) })
    }
    fn unload_idle<'a>(&'a self) -> BoxFuture<'a, std::result::Result<(), String>> {
        Box::pin(async { Ok(()) })
    }
    fn kill_largest_tool<'a>(
        &'a self,
        _run: &'a str,
    ) -> BoxFuture<'a, std::result::Result<bool, String>> {
        Box::pin(async { Ok(false) })
    }
}
#[derive(Clone)]
pub struct Orchestrator {
    inner: Arc<Inner>,
}
struct Inner {
    db: Database,
    engine: Arc<Engine>,
    limits: Limits,
    gate: Arc<AsyncMutex<()>>,
    authority: Mutex<Option<Arc<dyn RunAuthority>>>,
    running: AsyncMutex<
        BTreeMap<
            String,
            tokio::task::JoinHandle<excalibur_engine::Result<excalibur_engine::Snapshot>>,
        >,
    >,
    wake: Notify,
    fair_service: Mutex<BTreeMap<(u8, String), u64>>,
    incarnation: String,
    recovery: recovery::RecoveryLedger,
    runtime: Mutex<Option<tokio::runtime::Handle>>,
    tasks: Mutex<Option<excalibur_tasks::TaskStore>>,
}
fn digest(v: &[u8]) -> String {
    format!("{:x}", Sha256::digest(v))
}
fn now() -> i64 {
    excalibur_store::unix_seconds()
}
fn invalid(s: &str) -> Error {
    Error::Invalid(s.into())
}
impl Orchestrator {
    pub async fn open(db: Database, engine: Arc<Engine>, limits: Limits) -> Result<Self> {
        if limits.running == 0
            || limits.children_per_parent == 0
            || limits.ram_bytes == 0
            || limits.estimated_run_bytes == 0
            || limits.process_limit == 0
            || limits.provider_concurrency == 0
            || limits
                .project_weights
                .values()
                .any(|weight| *weight == 0 || *weight > 100)
        {
            return Err(invalid("positive resource limits required"));
        }
        if db.path() != engine.journal().database().path() {
            return Err(invalid(
                "engine and orchestrator must share principal storage",
            ));
        }
        let sql = include_str!("schema.sql");
        let hash = digest(sql.as_bytes());
        db.write(Priority::Journal, move |c| {
            c.execute_batch(sql)?;
            let known: Option<String> = c
                .query_row(
                    "SELECT checksum FROM orchestration_schema WHERE version=1",
                    [],
                    |r| r.get(0),
                )
                .optional()?;
            if known.as_ref().is_some_and(|v| v != &hash) {
                return Err(excalibur_store::Error::Invalid(
                    "orchestration schema checksum mismatch",
                ));
            }
            c.execute(
                "INSERT OR IGNORE INTO orchestration_schema VALUES(1,?1)",
                [hash],
            )?;
            let next = include_str!("schema2.sql");
            let next_hash = digest(next.as_bytes());
            let known: Option<String> = c
                .query_row(
                    "SELECT checksum FROM orchestration_schema WHERE version=2",
                    [],
                    |r| r.get(0),
                )
                .optional()?;
            if known.as_ref().is_some_and(|hash| hash != &next_hash) {
                return Err(excalibur_store::Error::Invalid(
                    "orchestration schema checksum mismatch",
                ));
            }
            c.execute_batch(next)?;
            if known.is_none(){c.execute("INSERT OR IGNORE INTO orchestration_admissions SELECT a.id,a.created FROM orchestration_agents a JOIN runs r ON r.id=a.run_id WHERE r.state IN ('running','verifying','recovering','waiting','paused') OR EXISTS(SELECT 1 FROM journal j WHERE j.run_id=a.run_id AND j.kind='model_request')",[])?;}
            c.execute(
                "INSERT OR IGNORE INTO orchestration_schema VALUES(2,?1)",
                [next_hash],
            )?;
            Ok(())
        })
        .await?;
        db.write(Priority::Journal, |c| {
            let sql = include_str!("host_notifications.sql");
            let hash = digest(sql.as_bytes());
            let known: Option<String> = c
                .query_row(
                    "SELECT checksum FROM orchestration_schema WHERE version=3",
                    [],
                    |r| r.get(0),
                )
                .optional()?;
            if known.as_ref().is_some_and(|old| old != &hash) {
                return Err(excalibur_store::Error::Invalid(
                    "host notification schema checksum mismatch",
                ));
            }
            c.execute_batch(sql)?;
            c.execute(
                "INSERT OR IGNORE INTO orchestration_schema VALUES(3,?1)",
                [hash],
            )?;
            Ok(())
        })
        .await?;
        run_watchdog::initialize(&db).await?;
        let recovery = recovery::RecoveryLedger::open(db.clone()).await?;
        Ok(Self {
            inner: Arc::new(Inner {
                db,
                engine,
                limits,
                gate: Arc::new(AsyncMutex::new(())),
                authority: Mutex::new(None),
                running: AsyncMutex::new(BTreeMap::new()),
                wake: Notify::new(),
                fair_service: Mutex::new(BTreeMap::new()),
                incarnation: AgentId::new().to_string(),
                recovery,
                runtime: Mutex::new(None),
                tasks: Mutex::new(None),
            }),
        })
    }
    pub fn with_authority(self, authority: Arc<dyn RunAuthority>) -> Self {
        *self.inner.authority.lock().unwrap() = Some(authority);
        self
    }
    pub fn with_tasks(self, tasks: excalibur_tasks::TaskStore) -> Self {
        *self.inner.tasks.lock().unwrap() = Some(tasks);
        self
    }
    pub fn with_runtime(self, handle: tokio::runtime::Handle) -> Self {
        *self.inner.runtime.lock().unwrap() = Some(handle);
        self
    }
    pub fn database(&self) -> &Database {
        &self.inner.db
    }
    pub fn engine(&self) -> &Arc<Engine> {
        &self.inner.engine
    }
    pub fn limits(&self) -> &Limits {
        &self.inner.limits
    }
    pub async fn notified(&self) {
        self.inner.wake.notified().await
    }
    pub async fn register_master(
        &self,
        name: &str,
        token: excalibur_security::CapabilityToken,
    ) -> Result<Agent> {
        self.register_master_inner(name, token, false).await
    }
    /// Trusted host startup reprovisioning only: preserve identity and frozen
    /// descendant tokens while replacing the root admission ceiling. No client
    /// or model tool exposes this operation. Call before starting dispatch.
    pub async fn rebind_master_at_startup(
        &self,
        name: &str,
        token: excalibur_security::CapabilityToken,
    ) -> Result<Agent> {
        self.register_master_inner(name, token, true).await
    }
    async fn register_master_inner(
        &self,
        name: &str,
        token: excalibur_security::CapabilityToken,
        startup: bool,
    ) -> Result<Agent> {
        token.validate().map_err(Error::Authority)?;
        if name.trim().is_empty() {
            return Err(invalid("master name required"));
        }
        let _guard = self.inner.gate.lock().await;
        if startup && !self.inner.running.lock().await.is_empty() {
            return Err(invalid(
                "master startup rebind requires dispatch to be stopped",
            ));
        }
        if let Some(mut known) = self.list_agents().await?.into_iter().find(|a| a.depth == 0) {
            if known.name != name || (!startup && known.token != token) {
                return Err(invalid("master identity already bound"));
            }
            if known.token != token {
                known.token = token;
                self.validate_durable(&serde_json::to_value(&known)?)?;
                let (id, body) = (known.id.clone(), serde_json::to_string(&known)?);
                self.inner
                    .db
                    .write(Priority::Journal, move |c| {
                        c.execute(
                            "UPDATE orchestration_agents SET body=?2 WHERE id=?1 AND depth=0",
                            rusqlite::params![id, body],
                        )?;
                        Ok(())
                    })
                    .await?;
            }
            return Ok(known);
        }
        let agent = Agent {
            id: AgentId::new().to_string(),
            parent_id: None,
            run_id: None,
            depth: 0,
            name: name.into(),
            profile: "master".into(),
            profile_id: None,
            scope: "general".into(),
            task: String::new(),
            origin_conversation: String::new(),
            token,
            budget: excalibur_engine::Budget {
                max_steps: 8,
                ..Default::default()
            },
            delegation: None,
        };
        self.insert_agent(&agent).await?;
        Ok(agent)
    }
    async fn insert_agent(&self, agent: &Agent) -> Result<()> {
        let a = agent.clone();
        let body = serde_json::to_string(&a)?;
        self.validate_durable(&serde_json::to_value(&a)?)?;
        self.inner.db.write(Priority::Journal,move|c|{
            c.execute("INSERT INTO agents(id,name,description,parent_id,level,scope_id,profile_id) VALUES(?1,?2,?3,?4,?5,?6,?7)",rusqlite::params![a.id,a.name,a.profile,a.parent_id,a.depth,a.scope,a.profile_id])?;
            c.execute("INSERT INTO orchestration_agents(id,parent_id,run_id,depth,body,created) VALUES(?1,?2,?3,?4,?5,?6)",rusqlite::params![a.id,a.parent_id,a.run_id,a.depth,body,now()])?;
            if let Some(run)=a.run_id {let changed=c.execute("UPDATE runs SET agent_id=?2,scope_id=?3,budget=?4 WHERE id=?1 AND (agent_id IS NULL OR agent_id=?2)",rusqlite::params![run,a.id,a.scope,serde_json::to_string(&a.budget).map_err(|_|excalibur_store::Error::Invalid("invalid agent budget"))?])?;if changed!=1{return Err(excalibur_store::Error::Invalid("run is already owned by another agent"));}}
            Ok(())}).await?;
        Ok(())
    }
    pub async fn agent(&self, id: &str) -> Result<Agent> {
        let id = id.to_owned();
        let text = self
            .inner
            .db
            .read(ReadLane::Interactive, move |c| {
                Ok(c.query_row(
                    "SELECT body FROM orchestration_agents WHERE id=?1",
                    [id],
                    |r| r.get::<_, String>(0),
                )
                .optional()?)
            })
            .await?
            .ok_or(Error::Missing)?;
        Ok(serde_json::from_str(&text)?)
    }
    pub async fn list_agents(&self) -> Result<Vec<Agent>> {
        let rows = self
            .inner
            .db
            .read(ReadLane::Interactive, move |c| {
                let mut q =
                    c.prepare("SELECT body FROM orchestration_agents ORDER BY created,id")?;
                Ok(q.query_map([], |r| r.get::<_, String>(0))?
                    .collect::<std::result::Result<Vec<_>, _>>()?)
            })
            .await?;
        rows.into_iter()
            .map(|v| Ok(serde_json::from_str(&v)?))
            .collect()
    }
    /// Hot-path projection excludes terminal history before decoding any agent
    /// specification or replaying its journal.
    pub async fn active_agents(&self) -> Result<Vec<Agent>> {
        let rows=self.inner.db.read(ReadLane::Interactive,|c|{let mut q=c.prepare("SELECT a.body FROM orchestration_agents a LEFT JOIN runs r ON r.id=a.run_id WHERE a.depth=0 OR r.state NOT IN ('completed','completed_with_pending','cancelled','failed') ORDER BY a.created,a.id")?;Ok(q.query_map([],|r|r.get::<_,String>(0))?.collect::<std::result::Result<Vec<_>,_>>()?)}).await?;
        rows.into_iter()
            .map(|body| Ok(serde_json::from_str(&body)?))
            .collect()
    }
    pub async fn agent_for_run(&self, run: &str) -> Result<Agent> {
        let run = run.to_owned();
        let row = self
            .inner
            .db
            .read(ReadLane::Interactive, move |c| {
                Ok(c.query_row(
                    "SELECT body FROM orchestration_agents WHERE run_id=?1",
                    [run],
                    |r| r.get::<_, String>(0),
                )
                .optional()?)
            })
            .await?
            .ok_or(Error::Missing)?;
        Ok(serde_json::from_str(&row)?)
    }
    pub async fn token_for_run(&self, run: &str) -> Result<excalibur_security::CapabilityToken> {
        Ok(self.agent_for_run(run).await?.token)
    }
    pub async fn spawn(&self, parent: &str, mut delegation: Delegation) -> Result<Agent> {
        let _guard = self.inner.gate.lock().await;
        let parent = self.agent(parent).await?;
        if parent.depth >= 2 {
            return Err(invalid("workers cannot create subagents"));
        }
        if delegation.operation.trim().is_empty()
            || delegation.operation.len() > 128
            || delegation.name.trim().is_empty()
            || delegation.goal.trim().is_empty()
            || delegation.deliverable.trim().is_empty()
        {
            return Err(invalid(
                "delegation identity, goal and deliverable required",
            ));
        }
        let custom_profile = if profiles::builtin_profile(&delegation.profile) {
            None
        } else {
            Some(self.custom_profile(&delegation.profile).await?)
        };
        let effective_profile = custom_profile
            .as_ref()
            .map(|p| p.base_profile.clone())
            .unwrap_or_else(|| delegation.profile.clone());
        delegation.token.validate().map_err(Error::Authority)?;
        if !parent.token.contains(&delegation.token)
            || !delegation.token.data_scopes.contains(&delegation.scope)
        {
            return Err(invalid("child capabilities or scope exceed parent"));
        }
        if matches!(
            effective_profile.as_str(),
            "planner" | "explorer" | "verifier" | "reviewer" | "researcher" | "security-auditor"
        ) && delegation.token.roots.iter().any(|r| r.write)
        {
            return Err(invalid("read-only profile cannot receive writable roots"));
        }
        if parent.depth == 1 && !delegation.spec.budget.fits_within(&parent.budget) {
            return Err(invalid("child budget exceeds parent"));
        }
        if parent.depth == 0 && effective_profile != "coordinator" {
            return Err(invalid("master delegates root work to a coordinator"));
        }
        if parent.depth == 1 && effective_profile == "coordinator" {
            return Err(invalid("level two cannot coordinate further children"));
        }
        if parent.depth == 1
            && parent.scope != delegation.scope
            && !parent.token.data_scopes.contains(&delegation.scope)
        {
            return Err(invalid(
                "cross-scope delegation requires explicit capability",
            ));
        }
        if delegation.priority > 3 {
            return Err(invalid("execution priority must be P1, P2 or P3"));
        }
        delegation.priority = delegation.priority.max(1).max(
            parent
                .delegation
                .as_ref()
                .map(|d| d.priority.max(1))
                .unwrap_or(1),
        );
        self.validate_durable(&serde_json::to_value(&delegation)?)?;
        if serde_json::to_vec(&delegation)?.len() > 8 * 1024 * 1024 {
            return Err(invalid("delegation exceeds 8 MiB"));
        }
        let request_hash = digest(&serde_json::to_vec(&(
            parent.id.clone(),
            delegation.clone(),
        ))?);
        let operation = delegation.operation.clone();
        let request = serde_json::to_string(&delegation)?;
        let id = AgentId::new().to_string();
        let (id, known) = self
            .inner
            .db
            .write(Priority::Journal, move |c| {
                let known: Option<(String, String)> = c
                    .query_row(
                        "SELECT agent_id,request_hash FROM orchestration_spawn WHERE operation=?1",
                        [&operation],
                        |r| Ok((r.get(0)?, r.get(1)?)),
                    )
                    .optional()?;
                if let Some((id, hash)) = known {
                    if hash != request_hash {
                        return Err(excalibur_store::Error::Invalid(
                            "delegation operation reused",
                        ));
                    }
                    return Ok((id, true));
                }
                c.execute(
                    "INSERT INTO orchestration_spawn VALUES(?1,?2,?3,?4)",
                    rusqlite::params![operation, request_hash, id, request],
                )?;
                Ok((id, false))
            })
            .await?;
        if known && let Ok(agent) = self.agent(&id).await {
            return Ok(agent);
        }
        if let Some(run) = &parent.run_id
            && !matches!(
                self.inner.engine.load(run).await?.status,
                Status::Ready | Status::Running | Status::Verifying
            )
        {
            return Err(invalid("parent cannot delegate in current state"));
        }
        // Preserve mandatory instructions, without inheriting parent's private reasoning/history.
        if let Some(run) = &parent.run_id {
            let instructions = self
                .inner
                .engine
                .journal()
                .context_instructions(run)
                .await?;
            for record in instructions.records() {
                if !delegation
                    .pinned_instructions
                    .contains(&record.literal_text)
                {
                    delegation
                        .pinned_instructions
                        .push(record.literal_text.clone());
                }
            }
            if delegation.fork_context {
                delegation.spec.context.activity = self.inner.engine.load(run).await?.history;
            }
        }
        delegation.spec.context.layers[3] = format!(
            "Goal: {}\nDeliverable: {}\nScope: {}\nInputs: {}\nMandatory instructions (literal):\n{}",
            delegation.goal,
            delegation.deliverable,
            delegation.scope,
            serde_json::to_string(&delegation.inputs)?,
            delegation.pinned_instructions.join("\n")
        );
        if let Some(profile) = &custom_profile {
            delegation.spec.context.layers[1].push_str(&format!(
                "\nCustom profile {} ({}):\n{}",
                profile.id, profile.name, profile.identity
            ));
        }
        delegation.spec.context.suggested_skills = delegation.suggested_skills.clone();
        if let Some(authority) = self.authority() {
            authority
                .prepare_context(&parent, &mut delegation)
                .await
                .map_err(Error::Authority)?;
        }
        for lane in 0..=3 {
            delegation.spec.context.layers[0] = delegation.spec.context.layers[0]
                .replace(&format!("\n[excalibur-host-lane:{lane}]\n"), "\n");
        }
        delegation.spec.context.layers[0].push_str(&format!(
            "\n[excalibur-host-lane:{}]\n",
            delegation.priority
        ));
        let operation = delegation.operation.clone();
        let prepared = serde_json::to_string(&delegation)?;
        let prepared = self
            .inner
            .db
            .write(Priority::Journal, move |c| {
                c.execute(
                    "INSERT OR IGNORE INTO orchestration_prepared VALUES(?1,?2)",
                    rusqlite::params![operation, prepared],
                )?;
                Ok(c.query_row(
                    "SELECT body FROM orchestration_prepared WHERE operation=?1",
                    [operation],
                    |r| r.get::<_, String>(0),
                )?)
            })
            .await?;
        delegation = serde_json::from_str(&prepared)?;
        let run = self
            .inner
            .engine
            .create(
                &format!("orchestrator:{}", delegation.operation),
                delegation.spec.clone(),
            )
            .await?;
        let instructions = excalibur_journal::instructions::InstructionStore::open(
            self.inner.db.clone(),
            Default::default(),
        )
        .await?;
        let mut scopes = Vec::new();
        if delegation.token.data_scopes.contains("general") {
            scopes.push(excalibur_journal::instructions::InstructionScope::General);
        }
        if let Some(project) = delegation.scope.strip_prefix("project:") {
            scopes.push(excalibur_journal::instructions::InstructionScope::Project(
                project.to_owned(),
            ));
        }
        scopes.push(excalibur_journal::instructions::InstructionScope::Task(
            delegation.task.clone(),
        ));
        instructions.bind_run(&run, None, scopes).await?;
        if let Some(parent_run) = &parent.run_id {
            let inherited = self
                .inner
                .engine
                .journal()
                .context_instructions(parent_run)
                .await?;
            let ids = inherited
                .records()
                .map(|r| r.id.clone())
                .collect::<Vec<_>>();
            instructions.delegate(parent_run, &run, &ids).await?;
        }
        if let Some(parent_run) = &parent.run_id {
            excalibur_engine::reserve_child_budget(
                self.inner.engine.journal(),
                parent_run,
                &id,
                &run,
                delegation.spec.budget.clone(),
            )
            .await?;
        }
        let agent = Agent {
            id,
            parent_id: Some(parent.id),
            run_id: Some(run),
            depth: parent.depth + 1,
            name: delegation.name.clone(),
            profile: effective_profile,
            profile_id: custom_profile.map(|p| p.id),
            scope: delegation.scope.clone(),
            task: delegation.task.clone(),
            origin_conversation: delegation.origin_conversation.clone(),
            token: delegation.token.clone(),
            budget: delegation.spec.budget.clone(),
            delegation: Some(delegation),
        };
        let tasks = self.inner.tasks.lock().unwrap().clone();
        if let Some(tasks) = tasks {
            if tasks.task(&agent.scope, &agent.task).await.is_err() {
                let card = json!({"original":agent.delegation.as_ref().map(|d|d.goal.clone()).unwrap_or_default(),"interpretation":agent.delegation.as_ref().map(|d|d.goal.clone()).unwrap_or_default(),"requirements":[{"id":"R1","kind":"functional","text":agent.delegation.as_ref().map(|d|d.deliverable.clone()).unwrap_or_default()}],"criteria":[{"id":"C1.1","requirement":"R1","statement":"Return the delegated deliverable under the assigned contract","method":"final_response"}],"assumptions":[],"open_questions":[],"pinned_instructions":agent.delegation.as_ref().map(|d|d.pinned_instructions.clone()).unwrap_or_default()});
                let requirement = format!("{}:requirement", agent.task);
                let criterion = format!("{}:criterion", agent.task);
                tasks.host_batch(&agent.scope,&format!("orchestrator-task:{}",agent.id),json!({"ops":[{"op":"create_task","id":agent.task,"title":agent.name,"data":{"request_card":card}},{"op":"create_requirement","id":requirement,"data":{"text":agent.delegation.as_ref().map(|d|d.deliverable.clone()).unwrap_or_default(),"kind":"functional"}},{"op":"create_criterion","id":criterion,"data":{"requirement_id":requirement,"statement":"Return a final response satisfying the delegated local output contract","method":"inspection","expected":"Nonempty final response validated against declared output schema","impact_area":[],"metadata":{"implicit_final_response":true}}},{"op":"link_requirement","task":agent.task,"requirement":requirement}]})).await.map_err(|e|Error::Authority(e.to_string()))?;
            }
            if let Some(parent_run) = self
                .agent(agent.parent_id.as_deref().unwrap_or(""))
                .await?
                .run_id
            {
                tasks
                    .bind_child(
                        &agent.task,
                        agent.run_id.as_deref().unwrap_or(""),
                        &parent_run,
                    )
                    .await
                    .map_err(|e| Error::Authority(e.to_string()))?;
            } else {
                tasks
                    .bind_root(&agent.task, agent.run_id.as_deref().unwrap_or(""))
                    .await
                    .map_err(|e| Error::Authority(e.to_string()))?;
            }
        }
        self.insert_agent(&agent).await?;
        self.inner.wake.notify_waiters();
        Ok(agent)
    }
    pub async fn status(&self, id: &str) -> Result<AgentStatus> {
        let agent = self.agent(id).await?;
        let pending = self.mailbox(id).await?;
        let mut out = AgentStatus {
            agent: agent.clone(),
            state: "available".into(),
            reason: None,
            queue_position: None,
            step: 0,
            model_in_flight: false,
            tools: vec![],
            last_progress: None,
            last_report: None,
            plan: json!({}),
            usage: json!({}),
            pending_messages: pending.len(),
            report_reminder: false,
        };
        if let Some(run) = &agent.run_id {
            let state = self.inner.engine.load(run).await?;
            out.state = label(&state.status).into();
            out.reason = state.reason.clone();
            out.step = state.step;
            out.usage = serde_json::to_value(&state.usage)?;
            out.model_in_flight = state.request.is_some() && state.response.is_none();
            let events = self.inner.engine.journal().replay(run, 0).await?;
            for event in &events {
                if matches!(
                    event.kind.as_str(),
                    "tool_result" | "check_recorded" | "step_completed"
                ) {
                    out.last_progress = Some(
                        json!({"kind":event.kind,"sequence":event.sequence,"payload":event.payload}),
                    );
                }
            }
            let query_run = run.clone();
            let tool_times=self.inner.db.read(ReadLane::Interactive,move|c| {
                let mut q=c.prepare("SELECT sequence,created_at FROM journal WHERE run_id=?1 AND kind='tool_intent' AND state='confirmed' ORDER BY sequence")?;
                Ok(q.query_map([query_run],|r|Ok((r.get::<_,u64>(0)?,r.get::<_,i64>(1)?)))?.collect::<std::result::Result<BTreeMap<_,_>,_>>()?)
            }).await?;
            out.tools=state.calls.iter().filter(|(_,call)|call.result.is_none()).filter_map(|(index,call)|call.prepared.as_ref().map(|p| {
                let sequence=events.iter().rev().find(|event|event.kind=="tool_intent"&&event.payload["index"].as_u64()==Some(*index as u64)).map(|event|event.sequence);
                let started=sequence.and_then(|sequence|tool_times.get(&sequence).copied()).unwrap_or(state.started_at);
                let mut summary=p.call.arguments.chars().take(512).collect::<String>();if summary.len()<p.call.arguments.len(){summary.push('…');}
                json!({"index":index,"name":p.call.name,"arguments_summary":summary,"elapsed_seconds":now().saturating_sub(started),"resources":p.resources})
            })).collect();
            if let Some(host) = self.authority() {
                out.tools.extend(host.live_status(run));
            }
            out.last_report = self.latest_report(id).await?;
            let last_report_step = out
                .last_report
                .as_ref()
                .and_then(|v| v["step"].as_u64())
                .unwrap_or(0);
            let last_report_at = out
                .last_report
                .as_ref()
                .and_then(|v| v["created"].as_i64())
                .unwrap_or(state.started_at);
            let progress_id = agent.id.clone();
            let progress=self.inner.db.read(ReadLane::Interactive,move|c|Ok(c.query_row("SELECT step,observed_at FROM orchestration_plan_progress WHERE agent_id=?1",[progress_id],|r|Ok((r.get::<_,u64>(0)?,r.get::<_,i64>(1)?))).optional()?)).await?;
            let baseline_step =
                progress.map_or(last_report_step, |(step, _)| step.max(last_report_step));
            let baseline_at = progress.map_or(last_report_at, |(_, at)| at.max(last_report_at));
            out.report_reminder =
                state.step.saturating_sub(baseline_step) >= 15 || now() - baseline_at >= 600;
            if state.status == Status::Ready {
                let active = self
                    .inner
                    .running
                    .lock()
                    .await
                    .keys()
                    .cloned()
                    .collect::<std::collections::BTreeSet<_>>();
                out.queue_position = self
                    .ready_order()
                    .await?
                    .iter()
                    .filter(|a| !active.contains(&a.id))
                    .position(|a| a.id == id)
                    .map(|v| v + 1);
            }
            let task = agent.task.clone();
            let scope = agent.scope.clone();
            out.plan = self
                .inner
                .db
                .read(ReadLane::Interactive, move |c| {
                    let mut q =
                        c.prepare("SELECT i.id,i.state FROM plan_items i LEFT JOIN plans p ON p.id=i.plan_id WHERE (i.task_id=?1 OR p.owner_task_id=?1) AND COALESCE(i.scope_id,p.scope_id,'general')=?2 ORDER BY i.id LIMIT 100")?;
                    let rows = q
                        .query_map(rusqlite::params![task,scope], |r| {
                            Ok(json!({"id":r.get::<_,String>(0)?,"state":r.get::<_,String>(1)?}))
                        })?
                        .collect::<std::result::Result<Vec<_>, _>>()?;
                    let done=rows.iter().filter(|i|matches!(i["state"].as_str(),Some("done"|"completed"))).count();Ok(json!({"done":done,"total":rows.len(),"items":rows}))
                })
                .await?;
        }
        Ok(out)
    }
    fn validate_durable(&self, value: &Value) -> Result<()> {
        if let Some(authority) = self.authority() {
            let sanitized = authority
                .sanitize_durable(value)
                .map_err(Error::Authority)?;
            if sanitized != *value {
                return Err(invalid(
                    "secret literal in durable orchestration input; use references",
                ));
            }
        }
        Ok(())
    }
    fn authority(&self) -> Option<Arc<dyn RunAuthority>> {
        self.inner.authority.lock().unwrap().clone()
    }
}
