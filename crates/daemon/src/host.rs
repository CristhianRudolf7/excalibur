use crate::{
    DaemonActions, GitOptions, Lane, LaneLimits, LaneScheduler, ScopedPlanning, WorkerTools,
};
use excalibur_engine::Engine;
use excalibur_gateway::{AccessPolicy, Gateway, SharedBackend};
use excalibur_master::{Master, MasterConfig};
use excalibur_models::ModelGateway;
use excalibur_orchestrator::{Agent, Limits, Orchestrator, Project, RunAuthority};
use excalibur_security::{Autonomy, CapabilityToken, Root, RunSecurity, SecuredTools, Security};
use excalibur_store::{Database, ReadLane};
use excalibur_tools::{Access, LocalTools, ToolPolicy};
use excalibur_types::runtime::{BoxFuture, ToolRuntime};
use excalibur_vault::Redactor;
use std::{collections::BTreeSet, path::PathBuf, sync::Arc};

struct Authority {
    skills: Arc<crate::skills::SkillsHost>,
    memory: Option<Arc<crate::memory::MemoryHost>>,
    security: Security,
    tools: Arc<WorkerTools>,
    db: Database,
    redactor: Redactor,
    planning: Arc<ScopedPlanning>,
    autonomy: Autonomy,
    full_acknowledged: bool,
    lanes: crate::LaneScheduler,
    model: Arc<dyn ModelGateway>,
    process_limit: usize,
    fallback: Option<Arc<dyn ModelGateway>>,
}
impl RunAuthority for Authority {
    fn sanitize_durable(&self, value: &serde_json::Value) -> Result<serde_json::Value, String> {
        Ok(self.redactor.redact_json(value))
    }
    fn bind<'a>(&'a self, agent: &'a Agent) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async move {
            crate::project_scope::validate_agent(&self.db, agent).await?;
            let run = agent.run_id.as_ref().ok_or("worker has no run")?;
            let parent_agent = if let Some(parent) = &agent.parent_id {
                let parent = parent.clone();
                let body = self
                    .db
                    .read(ReadLane::Interactive, move |c| {
                        Ok(c.query_row(
                            "SELECT body FROM orchestration_agents WHERE id=?1",
                            [parent],
                            |r| r.get::<_, String>(0),
                        )?)
                    })
                    .await
                    .map_err(|e| e.to_string())?;
                Some(serde_json::from_str::<Agent>(&body).map_err(|_| "invalid parent metadata")?)
            } else {
                None
            };
            let parent = parent_agent.as_ref().and_then(|p| p.run_id.clone());
            let binding = self.tools.bind_agent(agent, parent_agent.as_ref()).await?;
            let mut effective = agent.clone();
            effective.token = binding.token.clone();
            let body = serde_json::to_string(&effective).map_err(|_| "invalid bound agent")?;
            let id = agent.id.clone();
            self.db
                .write(excalibur_store::Priority::Journal, move |c| {
                    c.execute(
                        "UPDATE orchestration_agents SET body=?2 WHERE id=?1",
                        (&id, &body),
                    )?;
                    Ok(())
                })
                .await
                .map_err(|e| e.to_string())?;
            let workspace = binding.workspace.clone();
            self.security
                .register_run(
                    run,
                    RunSecurity {
                        parent: parent.clone(),
                        project: agent.scope.clone(),
                        workspace: binding.workspace,
                        token: binding.token,
                        autonomy: vec![self.autonomy],
                        full_acknowledged: self.full_acknowledged,
                        user_requested: true,
                    },
                    excalibur_store::unix_seconds(),
                )
                .await
                .map_err(|e| e.to_string())?;
            self.planning.bind_agent(&effective, workspace).await?;
            if let Some(memory) = &self.memory {
                memory.bind_agent(&effective, parent).await?;
            }
            self.skills.bind_agent(&effective).await?;
            Ok(())
        })
    }
    fn prepare_context<'a>(
        &'a self,
        _parent: &'a Agent,
        delegation: &'a mut excalibur_orchestrator::Delegation,
    ) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async move {
            match &self.memory {
                Some(memory) => memory.prepare_context(delegation).await,
                None => Ok(()),
            }
        })
    }
    fn admission_capacity(&self) -> excalibur_orchestrator::AdmissionCapacity {
        let (provider_limit, provider_active) = self.lanes.worker_capacity();
        excalibur_orchestrator::AdmissionCapacity {
            process_limit: self.process_limit,
            processes_active: self.tools.active_processes(),
            provider_limit,
            provider_active,
        }
    }
    fn unload_idle(&self) -> BoxFuture<'_, Result<(), String>> {
        Box::pin(async move {
            self.tools.unload_idle().await?;
            self.model
                .unload_idle()
                .await
                .map_err(|_| "idle inference release failed")?;
            if let Some(model) = &self.fallback {
                model
                    .unload_idle()
                    .await
                    .map_err(|_| "idle fallback release failed")?;
            }
            Ok(())
        })
    }
    fn live_status(&self, run: &str) -> Vec<serde_json::Value> {
        self.tools.live_status(run)
    }
    fn interrupt_tool<'a>(&'a self, run: &'a str) -> BoxFuture<'a, Result<bool, String>> {
        Box::pin(async move { self.tools.interrupt_tool(run).await })
    }
    fn kill_largest_tool<'a>(&'a self, run: &'a str) -> BoxFuture<'a, Result<bool, String>> {
        Box::pin(async move { self.tools.kill_largest_tool(run).await })
    }
}
/// The trusted host supplies roots and credentials. Clients cannot widen this ceiling.
pub struct MasterToolOptions {
    pub secured_delegate: Arc<dyn ToolRuntime>,
    pub catalog: Arc<excalibur_tools::deferred::ToolCatalog>,
    pub authorized_run: String,
    pub scopes: BTreeSet<String>,
}
pub struct HostOptions {
    pub skills: crate::skills::SkillsOptions,
    pub root: PathBuf,
    pub cgroup_root: Option<PathBuf>,
    pub project_services:
        std::collections::BTreeMap<String, Arc<excalibur_tools::intelligence::ProjectIntelligence>>,
    pub projects: Vec<Project>,
    pub origins: BTreeSet<String>,
    pub limits: Limits,
    pub lane_limits: LaneLimits,
    pub master_tools: Option<MasterToolOptions>,
    pub master_config: MasterConfig,
    pub decider: Option<Arc<dyn excalibur_master::IntakeDecider>>,
    pub git: Option<GitOptions>,
    pub context_runtime: Option<tokio::runtime::Handle>,
    pub autonomy: Autonomy,
    pub full_acknowledged: bool,
    /// Long-term memory and documents; `None` keeps the host without memory tools.
    pub memory: Option<crate::memory::MemoryOptions>,
}
impl HostOptions {
    pub fn project(root: PathBuf) -> Self {
        Self {
            skills: Default::default(),
            root: root.clone(),
            cgroup_root: None,
            project_services: Default::default(),
            projects: vec![Project {
                id: "default".into(),
                name: "Default project".into(),
                aliases: vec![],
                description: String::new(),
                roots: vec![root],
                status: "active".into(),
                vcs: None,
                settings: serde_json::json!({}),
            }],
            origins: BTreeSet::new(),
            limits: Limits::default(),
            lane_limits: LaneLimits::default(),
            master_tools: None,
            master_config: MasterConfig::default(),
            decider: None,
            git: None,
            context_runtime: None,
            autonomy: Autonomy::Autonomous,
            full_acknowledged: false,
            memory: None,
        }
    }
}
pub struct DaemonHost {
    pub skills: Arc<crate::skills::SkillsHost>,
    pub master: Master,
    pub orchestrator: Orchestrator,
    pub gateway: Gateway,
    pub shutdown: Arc<tokio::sync::Notify>,
    pub lanes: LaneScheduler,
    pub telemetry: Arc<excalibur_telemetry::Telemetry>,
    actions: Arc<DaemonActions>,
    tools: Arc<WorkerTools>,
    db: Database,
    pub memory: Option<Arc<crate::memory::MemoryHost>>,
    boot: excalibur_orchestrator::recovery::BootRecord,
}
impl DaemonHost {
    /// Call on the interactive runtime, passing a *different* work runtime handle.
    pub async fn open(
        db: Database,
        options: HostOptions,
        model: Arc<dyn ModelGateway>,
        fallback: Option<Arc<dyn ModelGateway>>,
        token: &str,
        redactor: Redactor,
        work: tokio::runtime::Handle,
    ) -> Result<Self, String> {
        let recovery = excalibur_orchestrator::recovery::RecoveryLedger::open(db.clone())
            .await
            .map_err(|e| e.to_string())?;
        let boot = recovery
            .begin(excalibur_store::unix_seconds())
            .await
            .map_err(|e| e.to_string())?;
        db.write(excalibur_store::Priority::Interactive,|c|{let present:bool=c.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='orchestration_activity')",[],|r|r.get(0))?;if present{c.execute("UPDATE orchestration_activity SET connected=0",[])?;}Ok(())}).await.map_err(|e|e.to_string())?;
        let root = options
            .root
            .canonicalize()
            .map_err(|_| "workspace root unavailable")?;
        let mut scopes = BTreeSet::from(["general".to_string()]);
        for project in &options.projects {
            if project
                .roots
                .iter()
                .any(|p| p.canonicalize().map_or(true, |p| !p.starts_with(&root)))
            {
                return Err("project root exceeds host authority".into());
            }
            scopes.insert(format!("project:{}", project.id));
        }
        let memory_scopes: Vec<String> = scopes.iter().cloned().collect();
        let mut master_config = options.master_config.clone();
        for project in &options.projects {
            master_config.scope_context.entry(format!("project:{}",project.id)).or_insert_with(||serde_json::json!({"name":project.name,"description":project.description,"state":project.status}).to_string());
        }
        let context_runtime = options
            .context_runtime
            .clone()
            .unwrap_or_else(|| work.clone());
        excalibur_tools::terminal::prepare_project_boundary(&root)?;
        let policy = ToolPolicy {
            read: Access::Allow,
            write: Access::Allow,
            exec: Access::Allow,
            sandbox: Some(excalibur_tools::terminal::SandboxConfig::project(
                root.clone(),
            )),
            redactor: redactor.clone(),
            ..Default::default()
        };
        let local = Arc::new(LocalTools::new(db.clone(), root.clone(), policy.clone()).await?);
        let workers = Arc::new(
            WorkerTools::new(
                db.clone(),
                local,
                policy,
                options.projects.clone(),
                options.git.clone(),
            )
            .await?
            .with_cgroup_root(options.cgroup_root.clone())?
            .with_project_services(options.project_services.clone())?,
        );
        let managed = workers.managed_worktree_roots();
        let mut roots = vec![Root {
            path: root.clone(),
            write: true,
        }];
        for path in managed.values() {
            if !roots.iter().any(|r| r.path == *path) {
                roots.push(Root {
                    path: path.clone(),
                    write: true,
                });
            }
        }
        let ceiling = CapabilityToken {
            roots,
            processes: true,
            sandbox_profile: Some("project".into()),
            data_scopes: scopes,
            budget: 10000,
            external_actions: if options.git.is_some() {
                BTreeSet::from([
                    "tool:worktree".to_string(),
                    "tool:integration_queue".to_string(),
                ])
            } else {
                BTreeSet::new()
            },
            ..Default::default()
        };
        let security = Security::new(db.clone(), ceiling.clone(), redactor.clone())
            .await
            .map_err(|e| e.to_string())?;
        let secured: Arc<dyn ToolRuntime> =
            Arc::new(SecuredTools::new(workers.clone(), security.clone()));
        let planning =
            Arc::new(ScopedPlanning::new(secured.clone(), db.clone(), redactor.clone()).await?);
        let fallback_limits = options.lane_limits.clone();
        let lanes = LaneScheduler::new(options.lane_limits).map_err(String::from)?;
        let memory = match options.memory.clone() {
            Some(mut memory_options) => {
                if memory_options.principal != options.master_config.principal {
                    return Err("memory and master must use the same principal".into());
                }
                if memory_options.services.cheap_classifier.is_none() {
                    memory_options.services.cheap_classifier =
                        Some(Arc::new(excalibur_memory::ModelRole::new(
                            "memory-admission",
                            lanes.gateway(Lane::User, model.clone()),
                        )));
                }
                let store = excalibur_memory::MemoryStore::open(db.clone())
                    .await
                    .map_err(|e| e.to_string())?
                    .with_redactor(redactor.clone());
                Some(
                    crate::memory::MemoryHost::open(store, planning.clone(), memory_options)
                        .await?,
                )
            }
            None => None,
        };
        let coordination_delegate: Arc<dyn ToolRuntime> = match &memory {
            Some(memory) => memory.tools.clone(),
            None => planning.clone(),
        };
        let coordination = Arc::new(excalibur_orchestrator::OrchestratorTools::new(
            coordination_delegate,
        ));
        let mut deferred_entries = excalibur_tools::project::deferred_catalog();
        if memory.is_some() {
            deferred_entries.push(crate::memory::library_ingest_entry());
        }
        let catalog = Arc::new(excalibur_tools::deferred::ToolCatalog::new(
            deferred_entries,
        )?);
        let deferred = Arc::new(excalibur_tools::deferred::DeferredTools::new(
            coordination.clone(),
            catalog,
            excalibur_tools::deferred::CallerRole::Worker,
        )?);
        let skills = crate::skills::SkillsHost::open(
            db.clone(),
            deferred,
            &options.projects,
            options.skills.clone(),
            model.as_ref(),
        )
        .await?;
        let journal = excalibur_journal::Journal::new(db.clone())
            .await
            .map_err(|e| e.to_string())?;
        let tasks = excalibur_tasks::TaskStore::open(db.clone())
            .await
            .map_err(|e| e.to_string())?
            .with_redactor(redactor.clone());
        let telemetry = Arc::new(
            excalibur_telemetry::Telemetry::open(excalibur_telemetry::TelemetryConfig::new(
                db.path()
                    .parent()
                    .ok_or("data parent required")?
                    .join("daemon-logs"),
                excalibur_telemetry::RunMetadata {
                    profile: "daemon".into(),
                    provider: model.metadata().provider,
                    model: model.metadata().model,
                    ..Default::default()
                },
                redactor.clone(),
            ))
            .await
            .map_err(|e| e.to_string())?,
        );
        let task_gate = crate::task_gate::WorkerTaskGate {
            store: tasks.clone(),
            runtime: planning.clone(),
            workers: workers.clone(),
            model: lanes.gateway(Lane::User, model.clone()),
            redactor: redactor.clone(),
        };
        let gate = Arc::new(
            excalibur_orchestrator::OrchestrationGate::new().with_delegate(Arc::new(task_gate)),
        );
        let projection = Arc::new(crate::projection::DaemonProjection::new(
            telemetry.clone(),
            tasks.clone(),
        ));
        let engine = Arc::new(
            Engine::new(
                journal,
                lanes.gateway(Lane::User, model.clone()),
                skills.runtime.clone(),
            )
            .with_completion_gate(gate.clone())
            .with_durable_sink(projection.clone()),
        );
        projection.attach(&engine);
        let orchestrator = Orchestrator::open(db.clone(), engine, options.limits.clone())
            .await
            .map_err(|e| e.to_string())?
            .with_runtime(work.clone())
            .with_tasks(tasks.clone())
            .with_authority(Arc::new(Authority {
                skills: skills.clone(),
                memory: memory.clone(),
                security: security.clone(),
                tools: workers.clone(),
                db: db.clone(),
                redactor: redactor.clone(),
                planning: planning.clone(),
                autonomy: options.autonomy,
                full_acknowledged: options.full_acknowledged,
                lanes: lanes.clone(),
                model: model.clone(),
                process_limit: options.limits.process_limit,
                fallback: fallback.clone(),
            }));
        gate.attach(&orchestrator).map_err(|e| e.to_string())?;
        if let Some(memory) = &memory {
            memory.attach(&orchestrator);
        }
        coordination
            .attach(&orchestrator)
            .map_err(|e| e.to_string())?;
        let registration_roots = options
            .projects
            .iter()
            .flat_map(|project| project.roots.clone())
            .collect::<Vec<_>>();
        for project in options.projects {
            orchestrator
                .register_project(project)
                .await
                .map_err(|e| e.to_string())?;
        }
        let master_agent = orchestrator
            .rebind_master_at_startup("Master", ceiling)
            .await
            .map_err(|e| e.to_string())?;
        orchestrator
            .recover_startup(&boot, options.autonomy)
            .await
            .map_err(|e| e.to_string())?;
        let actions = Arc::new(
            DaemonActions::new(orchestrator.clone(), master_agent.id)
                .with_security(security.clone())
                .with_tasks(tasks)
                .with_redactor(redactor.clone())
                .with_worktree_roots(managed),
        );
        let fast_actions: Arc<dyn excalibur_master::FastActions> =
            if let Some(extension) = options.master_tools {
                Arc::new(excalibur_master::MasterFastTools::new(
                    actions.clone(),
                    extension.secured_delegate,
                    extension.catalog,
                    extension.authorized_run,
                    extension.scopes,
                )?)
            } else {
                actions.clone()
            };
        let fast_actions: Arc<dyn excalibur_master::FastActions> = match &memory {
            Some(memory) => Arc::new(crate::memory::MemoryFastActions::new(
                fast_actions,
                memory.clone(),
            )),
            None => fast_actions,
        };
        if let Some(memory) = &memory {
            memory.spawn_maintenance(
                &work,
                Some(lanes.gateway(Lane::Maintenance, model.clone())),
                memory_scopes.clone(),
            );
        }
        let fallback = if let Some(gateway) = fallback {
            let same_provider = model.metadata().provider.is_some()
                && model.metadata().provider == gateway.metadata().provider;
            let scheduler = if same_provider {
                lanes.clone()
            } else {
                LaneScheduler::new(fallback_limits).map_err(String::from)?
            };
            Some(scheduler.gateway(Lane::Master, gateway))
        } else {
            None
        };
        let master = Master::open(
            db.clone(),
            master_config,
            lanes.gateway(Lane::Master, model),
            fallback,
            fast_actions,
            redactor,
        )
        .await
        .map_err(|e| e.to_string())?;
        recovery
            .publish_summary(&boot.id, excalibur_store::unix_seconds())
            .await
            .map_err(|e| e.to_string())?;
        master.set_context_runtime(context_runtime);
        if let Some(decider) = options.decider {
            master.set_decider(Some(decider));
        }
        actions.bind_master(master.clone());
        actions.recover_task_directives().await?;
        master.retry_pending().await.map_err(|e| e.to_string())?;
        let shutdown = Arc::new(tokio::sync::Notify::new());
        let configuration = serde_json::json!({"roots":[root.clone()],"projects":orchestrator.projects().await.map_err(|e|e.to_string())?.into_iter().map(|p|serde_json::json!({"id":p.id,"name":p.name,"roots":p.roots,"state":p.status})).collect::<Vec<_>>(),"limits":orchestrator.limits(),"telemetry":"local per-run artifacts","autonomy":options.autonomy});
        let backend = SharedBackend::new(master.clone(), orchestrator.clone())
            .with_project_registration_roots(registration_roots)
            .map_err(|error| error.message)?
            .with_project_roots(vec![root])
            .map_err(|e| e.message)?
            .with_security(security)
            .with_telemetry(telemetry.clone())
            .with_configuration(configuration)
            .with_task_actions(actions.clone())
            .with_shutdown(shutdown.clone());
        let backend = match &memory {
            Some(memory) => backend.with_memory(excalibur_gateway::MemoryAccess {
                store: memory.store.clone(),
                principal: memory.options.principal.clone(),
                timezone: memory.options.timezone,
            }),
            None => backend,
        };
        let policy =
            AccessPolicy::new(token, options.origins, 16 * 1024 * 1024).map_err(|e| e.message)?;
        let gateway = Gateway::new(Arc::new(backend), db.clone(), policy);
        Ok(Self {
            skills,
            master,
            orchestrator,
            gateway,
            shutdown,
            lanes,
            telemetry,
            actions,
            tools: workers,
            db,
            memory,
            boot,
        })
    }
    pub fn control_actions(&self) -> Arc<DaemonActions> {
        self.actions.clone()
    }
    pub async fn recover_task_directives(&self) -> Result<usize, String> {
        self.actions.recover_task_directives().await
    }
    pub async fn close(self) -> Result<(), String> {
        self.close_with_outcome(true).await
    }
    pub async fn close_with_outcome(self, orderly: bool) -> Result<(), String> {
        self.skills.stop();
        if let Some(memory) = &self.memory {
            memory.stop();
        }
        self.gateway.close_connections();
        self.master.shutdown().await;
        self.actions.detach();
        let mut failure = self
            .orchestrator
            .shutdown()
            .await
            .err()
            .map(|e| e.to_string());
        if let Some(memory) = &self.memory {
            memory.detach();
        }
        // Storage failure must never skip process/resource cleanup.
        self.tools.shutdown().await;
        match self.orchestrator.active_agents().await {
            Ok(agents) => {
                for agent in agents {
                    if let Some(run) = agent.run_id {
                        match self.orchestrator.engine().load(&run).await {
                            Ok(snapshot) => {
                                if let Err(error) = self.telemetry.finalize(&run, &snapshot).await {
                                    failure.get_or_insert_with(|| error.to_string());
                                }
                            }
                            Err(error) => {
                                failure.get_or_insert_with(|| error.to_string());
                            }
                        }
                    }
                }
            }
            Err(error) => {
                failure.get_or_insert_with(|| error.to_string());
            }
        }
        if let Err(error) = self.telemetry.shutdown().await {
            failure.get_or_insert_with(|| error.to_string());
        }
        if orderly
            && failure.is_none()
            && let Err(error) = self
                .orchestrator
                .recovery()
                .clean_exit(&self.boot.id, excalibur_store::unix_seconds())
                .await
        {
            failure.get_or_insert_with(|| error.to_string());
        }
        if let Err(error) = self.db.shutdown().await {
            failure.get_or_insert_with(|| error.to_string());
        }
        match failure {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }
}
