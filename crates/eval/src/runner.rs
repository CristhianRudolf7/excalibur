use crate::{
    ExecutionTools,
    profile::{FrozenProfile, ProfileOverrides, hash},
    skills::builtin_skills,
};
use excalibur_context::ContextInput;
use excalibur_engine::{Budget, Criterion, DurableSink, Engine, Snapshot, Status};
use excalibur_journal::{Event, Journal};
use excalibur_models::ModelGateway;
use excalibur_security::{
    Autonomy, CapabilityToken, Network, Root, RunSecurity, SecuredTools, Security,
};
use excalibur_store::{Database, DatabaseKind, StorageConfig};
use excalibur_telemetry::{RunMetadata, Telemetry, TelemetryConfig};
use excalibur_tools::{Access, LocalTools, ToolPolicy};
use excalibur_types::runtime::{BoxFuture, ToolRuntime};
use excalibur_vault::Redactor;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::Arc,
};
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionProfile {
    #[default]
    Project,
    Eval,
}
#[derive(Clone, Debug)]
pub struct ExecOptions {
    pub operation_id: String,
    pub request: String,
    pub workspace: PathBuf,
    pub logs_dir: PathBuf,
    pub profile: ExecutionProfile,
    pub autonomy: Autonomy,
    pub budget: Budget,
    /// Host-configured model window and context lifecycle policy.
    pub context_policy: excalibur_context::SessionPolicy,
    /// Trusted, explicitly approved server configurations; empty means no language servers.
    pub language_servers: BTreeMap<String, excalibur_code_intelligence::ServerConfig>,
    pub output_schema: Option<Value>,
    pub criteria: Vec<Criterion>,
    pub metadata: RunMetadata,
    pub profile_override: Option<ProfileOverrides>,
    pub task_network_allowed: bool,
    pub task_cpu_count: usize,
    pub container_runtime: Option<String>,
    pub memory_store: Option<PathBuf>,
    pub skills_store: Option<PathBuf>,
    pub credentials_store: Option<PathBuf>,
    pub required_inputs: BTreeMap<String, Option<String>>,
    pub blocked_reason: Option<String>,
    pub task_timeout_seconds: Option<u64>,
    pub extra_read_roots: Vec<PathBuf>,
    pub extra_write_roots: Vec<PathBuf>,
    pub cgroup: Option<PathBuf>,
    pub cgroup_scope: excalibur_telemetry::resources::ResourceScope,
}
impl ExecOptions {
    pub fn new(
        request: impl Into<String>,
        workspace: impl Into<PathBuf>,
        logs: impl Into<PathBuf>,
    ) -> Self {
        Self {
            operation_id: format!("exec-{}", excalibur_types::RunId::new()),
            request: request.into(),
            workspace: workspace.into(),
            logs_dir: logs.into(),
            profile: ExecutionProfile::Project,
            autonomy: Autonomy::Supervised,
            budget: Budget::default(),
            context_policy: excalibur_context::SessionPolicy::default(),
            language_servers: BTreeMap::new(),
            output_schema: None,
            criteria: vec![],
            metadata: RunMetadata::default(),
            profile_override: None,
            task_network_allowed: false,
            task_cpu_count: std::thread::available_parallelism().map_or(1, usize::from),
            container_runtime: None,
            memory_store: None,
            skills_store: None,
            credentials_store: None,
            required_inputs: BTreeMap::new(),
            blocked_reason: None,
            task_timeout_seconds: None,
            extra_read_roots: vec![],
            extra_write_roots: vec![],
            cgroup: None,
            cgroup_scope: excalibur_telemetry::resources::ResourceScope::Shared,
        }
    }
}
#[derive(Debug, thiserror::Error)]
pub enum ExecError {
    #[error("execution configuration: {0}")]
    Configuration(String),
    #[error("execution backend failed")]
    Backend,
}
pub type Result<T> = std::result::Result<T, ExecError>;
#[derive(Clone, Debug, Serialize)]
pub struct ExecOutcome {
    pub run_id: String,
    pub status: String,
    pub exit_code: i32,
    pub final_output: Option<String>,
    pub reason: Option<String>,
    pub logs_dir: PathBuf,
}
pub fn exit_code(status: &Status) -> i32 {
    match status {
        Status::Completed => 0,
        Status::CompletedWithPending => 2,
        Status::Blocked => 3,
        Status::Paused => 4,
        Status::Failed => 5,
        Status::Cancelled => 130,
        Status::Created
        | Status::Ready
        | Status::Running
        | Status::Waiting
        | Status::Verifying
        | Status::Recovering => 1,
    }
}
struct Projection(Arc<Telemetry>, excalibur_tasks::TaskStore);
impl DurableSink for Projection {
    fn record<'a>(
        &'a self,
        run: &'a str,
        event: &'a Event,
    ) -> BoxFuture<'a, std::result::Result<(), String>> {
        Box::pin(async move {
            self.1
                .observe_event(run, event)
                .await
                .map_err(|_| "task projection unavailable".to_string())?;
            self.0
                .record_event(run, event)
                .await
                .map(|_| ())
                .map_err(|_| "telemetry projection unavailable".into())
        })
    }
}
pub struct Execution {
    pub engine: Engine,
    pub run_id: String,
    pub telemetry: Arc<Telemetry>,
    pub tasks: excalibur_tasks::TaskStore,
    local: Arc<LocalTools>,
    project_monitor: excalibur_tools::monitor::ProjectMonitor,
    graph_db: Database,
    lsp: Arc<excalibur_code_intelligence::LspManager>,
    db: Database,
    logs: PathBuf,
    blocked: Option<String>,
}
fn config_error(error: impl ToString) -> ExecError {
    ExecError::Configuration(error.to_string())
}
fn prepare_data_directory(logs: &Path) -> Result<PathBuf> {
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(logs).map_err(|_| ExecError::Backend)?;
    let logs = logs.canonicalize().map_err(|_| ExecError::Backend)?;
    let data = logs.join("excalibur");
    if std::fs::symlink_metadata(&data).is_ok_and(|m| m.file_type().is_symlink() || !m.is_dir()) {
        return Err(config_error("episode data path must be a real directory"));
    }
    if data.exists()
        && std::fs::read_dir(&data)
            .map_err(|_| ExecError::Backend)?
            .next()
            .is_some()
    {
        return Err(config_error("episode data directory must be empty"));
    }
    for name in [
        "trajectory.json",
        "events.jsonl",
        "metrics.json",
        "agent-context.json",
    ] {
        if logs.join(name).exists() {
            return Err(config_error(
                "episode artifacts already exist; use a fresh trial directory",
            ));
        }
    }
    builder.create(&data).map_err(|_| ExecError::Backend)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&data, std::fs::Permissions::from_mode(0o700))
            .map_err(|_| ExecError::Backend)?;
    }
    let mut marker = std::fs::OpenOptions::new();
    marker.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        marker.mode(0o600);
    }
    marker
        .open(data.join("episode-owner"))
        .and_then(|file| file.sync_all())
        .map_err(|_| config_error("episode directory is already reserved"))?;
    Ok(data)
}
impl Execution {
    pub async fn prepare(
        mut options: ExecOptions,
        model: Arc<dyn ModelGateway>,
        redactor: Redactor,
    ) -> Result<Self> {
        let root = options
            .workspace
            .canonicalize()
            .map_err(|_| config_error("project directory is required and must exist"))?;
        if !root.is_dir() {
            return Err(config_error("project must be a directory"));
        }
        let frozen = if options.profile == ExecutionProfile::Eval {
            Some(
                FrozenProfile::evaluation(options.task_cpu_count, options.profile_override.clone())
                    .map_err(config_error)?,
            )
        } else {
            None
        };
        if let Some(p) = &frozen {
            p.validate_stores(
                options.memory_store.as_deref(),
                options.skills_store.as_deref(),
                options.credentials_store.as_deref(),
            )
            .map_err(config_error)?;
        }
        if options.profile != ExecutionProfile::Eval && options.profile_override.is_some() {
            return Err(config_error("profile overrides require eval"));
        }
        if options.blocked_reason.is_some()
            && !model.cost_is_known()
            && let Some(cost) = options.budget.max_cost_microusd.take()
        {
            options
                .metadata
                .extra
                .insert("rejected_budget_microusd".into(), json!(cost));
        }
        if let Some(overrides) = &options.profile_override
            && overrides
                .model_roles
                .values()
                .any(|m| Some(m) != model.metadata().model.as_ref())
        {
            return Err(config_error(
                "role overrides require registered host gateways; this execution uses a single model",
            ));
        }
        if redactor.contains_secret(options.request.as_bytes()) {
            return Err(config_error(
                "request contains secret material; use host-side secret references",
            ));
        }
        if let Some(timeout) = options.task_timeout_seconds {
            if timeout == 0 {
                return Err(config_error("task timeout must be positive"));
            }
            options.budget.max_wall_seconds = options
                .budget
                .max_wall_seconds
                .min((timeout.saturating_mul(9) / 10).max(1));
        }
        let data = prepare_data_directory(&options.logs_dir)?;
        let logs = data.parent().unwrap().to_path_buf();
        let db = Database::open(
            data.join("main.db"),
            DatabaseKind::Main,
            StorageConfig::default(),
        )
        .map_err(|_| ExecError::Backend)?;
        let journal = Journal::new(db.clone())
            .await
            .map_err(|_| ExecError::Backend)?;
        let mut sandbox = if options.profile == ExecutionProfile::Eval {
            let runtime = options.container_runtime.as_deref().ok_or_else(|| {
                config_error("eval requires an explicit external container runtime declaration")
            })?;
            let attestation =
                excalibur_sandbox::evaluation::EvalContainerAttestation::from_host_declaration(
                    runtime,
                )
                .map_err(config_error)?;
            excalibur_sandbox::SandboxConfig::evaluation(root.clone(), attestation)
        } else {
            excalibur_sandbox::SandboxConfig::project(root.clone())
        };
        if let Some(attestation) = &sandbox.eval_container {
            options
                .metadata
                .extra
                .insert("container_attestation".into(), attestation.metadata());
        }
        if options.profile != ExecutionProfile::Eval
            && (!options.extra_read_roots.is_empty() || !options.extra_write_roots.is_empty())
        {
            return Err(config_error(
                "extra container roots require the eval profile",
            ));
        }
        let mut capability_roots = vec![Root {
            path: root.clone(),
            write: true,
        }];
        for (paths, write) in [
            (&options.extra_read_roots, false),
            (&options.extra_write_roots, true),
        ] {
            for path in paths {
                let canonical = path
                    .canonicalize()
                    .map_err(|_| config_error("declared container root does not exist"))?;
                if canonical != *path {
                    return Err(config_error(
                        "declared container roots must be canonical absolute paths",
                    ));
                }
                sandbox.read_roots.push(canonical.clone());
                if write {
                    sandbox.write_roots.push(canonical.clone());
                }
                capability_roots.push(Root {
                    path: canonical,
                    write,
                });
            }
        }
        let mut protected = vec![data.clone()];
        if options.profile == ExecutionProfile::Eval {
            protected.push(
                std::env::current_exe()
                    .and_then(|p| p.canonicalize())
                    .map_err(|_| ExecError::Backend)?,
            );
        }
        sandbox.hidden_paths.extend(protected.clone());
        sandbox.cgroup_directory = options.cgroup.clone();
        sandbox.eval_network =
            options.profile == ExecutionProfile::Eval && options.task_network_allowed;
        if options.profile == ExecutionProfile::Project {
            excalibur_sandbox::prepare_project_boundary(&root).map_err(config_error)?;
        }
        let verifier_sandbox = sandbox.clone();
        let graph_db = Database::open(
            data.join("codegraph.db"),
            DatabaseKind::CodeGraph,
            StorageConfig::default(),
        )
        .map_err(config_error)?;
        let project_id = hash(&root);
        let graph = excalibur_code_intelligence::CodeGraph::open(graph_db.clone(), &project_id)
            .await
            .map_err(config_error)?
            .with_redactor(redactor.clone());
        let lsp = Arc::new(excalibur_code_intelligence::LspManager::with_options(
            Arc::new(excalibur_code_intelligence::ReadOnlySpawner::default()),
            redactor.clone(),
            std::time::Duration::from_secs(600),
        ));
        for (language, mut server) in options.language_servers.clone() {
            if server.root.canonicalize().map_err(config_error)? != root {
                return Err(config_error(
                    "language server project root differs from execution workspace",
                ));
            }
            server.hidden_paths.extend(protected.clone());
            server.hidden_paths.push(data.clone());
            lsp.register(&project_id, &language, server)
                .await
                .map_err(config_error)?;
        }
        let intelligence = Arc::new(
            excalibur_tools::intelligence::ProjectIntelligence::new(
                graph,
                project_id,
                root.clone(),
            )
            .map_err(config_error)?
            .with_lsp(lsp.clone()),
        );

        let local = Arc::new(
            LocalTools::new(
                db.clone(),
                root.clone(),
                ToolPolicy {
                    redactor: redactor.clone(),
                    sandbox: Some(sandbox),
                    protected_paths: protected.clone(),
                    write: Access::Allow,
                    exec: Access::Allow,
                    ..Default::default()
                },
            )
            .await
            .map_err(config_error)?
            .with_intelligence(intelligence)
            .map_err(config_error)?,
        );
        let token = CapabilityToken {
            roots: capability_roots,
            processes: true,
            sandbox_profile: Some(
                if options.profile == ExecutionProfile::Eval {
                    "eval"
                } else {
                    "project"
                }
                .into(),
            ),
            budget: options.budget.max_tool_calls,
            network: if options.profile == ExecutionProfile::Eval && options.task_network_allowed {
                Network::Full
            } else {
                Network::None
            },
            ..Default::default()
        };
        let security = Security::new(db.clone(), token.clone(), redactor.clone())
            .await
            .map_err(|_| ExecError::Backend)?
            .with_protected_paths(
                protected.clone(),
                vec![
                    PathBuf::from("/tests"),
                    PathBuf::from("/solution"),
                    PathBuf::from("/logs/verifier"),
                ],
            )
            .map_err(config_error)?
            .with_noninteractive_policy();
        let secured: Arc<dyn ToolRuntime> =
            Arc::new(SecuredTools::new(local.clone(), security.clone()));
        let episode =
            excalibur_tools::evaluation::EpisodeTools::new(secured.clone(), db.clone(), "episode")
                .await
                .map_err(config_error)?
                .with_workspace(root.clone())
                .with_redactor(redactor.clone())
                .with_planning_autonomy(if options.profile == ExecutionProfile::Eval {
                    3
                } else {
                    match options.autonomy {
                        Autonomy::Consultative => 0,
                        Autonomy::Supervised => 1,
                        Autonomy::Autonomous => 2,
                        Autonomy::Full => 3,
                    }
                })
                .map_err(config_error)?;
        let task_store = episode.task_store();
        let observer = local
            .project_intelligence()
            .map(|services| services as Arc<dyn excalibur_tools::monitor::ChangeObserver>);
        let project_monitor = excalibur_tools::monitor::ProjectMonitor::start(
            (*local).clone(),
            observer,
            vec![excalibur_tools::monitor::EvidenceTarget {
                tasks: task_store.clone(),
                scope: "episode".into(),
            }],
        )
        .await
        .map_err(config_error)?;

        let skill_mount = options
            .profile_override
            .as_ref()
            .and_then(|o| o.task_skills_dir.as_deref());
        let skills =
            crate::skills::with_task_skills(builtin_skills(), skill_mount).map_err(config_error)?;
        let tools: Arc<dyn ToolRuntime> = Arc::new(
            ExecutionTools::new(
                Arc::new(episode),
                frozen.clone(),
                options.task_network_allowed,
            )
            .with_skills(skills.clone()),
        );
        let tools: Arc<dyn ToolRuntime> = excalibur_web_adapter::harness::WebHarness::wrap_from_env(
            tools,
            options.task_network_allowed,
            options.profile == ExecutionProfile::Eval,
        );
        let available = tools
            .definitions()
            .into_iter()
            .map(|d| d.name)
            .collect::<std::collections::BTreeSet<_>>();
        let mut domains = excalibur_tools::project::deferred_catalog()
            .into_iter()
            .filter(|d| available.contains(&d.name))
            .collect::<Vec<_>>();
        if options.task_network_allowed && excalibur_web_adapter::harness::registration_enabled() {
            domains.extend(excalibur_web_adapter::harness::browser_catalog());
        }
        let domain_catalog =
            Arc::new(excalibur_tools::deferred::ToolCatalog::new(domains).map_err(config_error)?);
        let tools: Arc<dyn ToolRuntime> = Arc::new(
            excalibur_tools::deferred::DeferredTools::new(
                tools,
                domain_catalog,
                excalibur_tools::deferred::CallerRole::Worker,
            )
            .map_err(config_error)?,
        );
        let mut catalog = tools.definitions();
        catalog.extend(excalibur_engine::builtin_tool_definitions());
        catalog.sort_by(|a, b| a.name.cmp(&b.name));
        let tools_hash = hash(&catalog);
        let configuration = json!({"profile":options.profile,"execution_mode":if options.metadata.extra.get("replay").and_then(Value::as_bool)==Some(true){"replay"}else{"live"},"model_gateway":model.metadata(),"sampling":{"seed":options.metadata.seed,"temperature":options.metadata.temperature,"reasoning_effort":options.metadata.reasoning_effort},"noninteractive":true,"approval_policy":"deny_ungranted","frozen_profile":frozen,"autonomy":format!("{:?}",if options.profile==ExecutionProfile::Eval{Autonomy::Full}else{options.autonomy}),"budget":options.budget,"task_network_allowed":options.task_network_allowed,"task_cpu_count":options.task_cpu_count,"task_timeout_seconds":options.task_timeout_seconds,"read_roots":options.extra_read_roots,"write_roots":options.extra_write_roots,"cgroup_scope":options.cgroup_scope,"output_schema":options.output_schema,"criteria":options.criteria,"required_inputs":options.required_inputs,"container_runtime":options.container_runtime});
        options.metadata.instruction = options.request.clone();
        options.metadata.profile = if options.profile == ExecutionProfile::Eval {
            "eval"
        } else {
            "project"
        }
        .into();
        options.metadata.profile_version = frozen.as_ref().map(|p| p.version.clone());
        options.metadata.configuration_hash = Some(hash(&redactor.redact_json(&configuration)));
        options.metadata.configuration = configuration;
        if let Some(p) = &frozen {
            options
                .metadata
                .extra
                .insert("profile_hash".into(), json!(hash(p)));
        }
        let deferred_adapters: Vec<&str> = if excalibur_web_adapter::harness::registration_enabled()
        {
            vec!["browser"]
        } else {
            Vec::new()
        };
        options.metadata.extra.insert("capability_availability".into(),json!({"tools":catalog.iter().map(|d|&d.name).collect::<Vec<_>>(),"subagent_orchestrator":false,"durable_task_planning":true,"independent_task_verifier":true,"full_skill_catalog":false,"deferred_adapters":deferred_adapters,"rewards_source":"external_host_adapter_only"}));
        options.metadata.tool_catalog_hash = Some(tools_hash);
        options.metadata.skill_catalog_hash = Some(hash(&skills));
        options.metadata.environment = json!({"os":std::env::consts::OS,"arch":std::env::consts::ARCH,"cpus":options.task_cpu_count,"timezone":"UTC","language":frozen.as_ref().map(|p|p.language.as_str()).unwrap_or("en")});
        let sampling = json!({"seed":options.metadata.seed,"temperature":options.metadata.temperature,"reasoning_effort":options.metadata.reasoning_effort,"requested_only":true});
        let mut telemetry_config = TelemetryConfig::new(&data, options.metadata, redactor.clone());
        telemetry_config.artifact_root = Some(logs.clone());
        telemetry_config.resources.cgroup = options.cgroup.clone();
        telemetry_config.resources.cgroup_scope = options.cgroup_scope;
        let telemetry = Arc::new(
            Telemetry::open(telemetry_config)
                .await
                .map_err(|_| ExecError::Backend)?,
        );
        let (checklist_runtime, checklist_path): (Arc<dyn ToolRuntime>, PathBuf) =
            if options.profile == ExecutionProfile::Eval {
                let checklist_root = data.join("checklists");
                std::fs::create_dir_all(&checklist_root).map_err(|_| ExecError::Backend)?;
                let checklist = LocalTools::new(
                    db.clone(),
                    checklist_root,
                    ToolPolicy {
                        redactor: redactor.clone(),
                        read: Access::Allow,
                        write: Access::Allow,
                        exec: Access::Deny,
                        ..Default::default()
                    },
                )
                .await
                .map_err(config_error)?;
                (Arc::new(checklist), PathBuf::from("README.md"))
            } else {
                let folder = ["tests", "test", "scripts"]
                    .into_iter()
                    .find(|folder| root.join(folder).is_dir())
                    .unwrap_or("tests");
                let projection: Arc<dyn ToolRuntime> = if options.autonomy == Autonomy::Consultative
                {
                    Arc::new(
                        LocalTools::new(
                            db.clone(),
                            root.clone(),
                            ToolPolicy {
                                redactor: redactor.clone(),
                                protected_paths: protected.clone(),
                                read: Access::Allow,
                                write: Access::Deny,
                                exec: Access::Deny,
                                ..Default::default()
                            },
                        )
                        .await
                        .map_err(config_error)?,
                    )
                } else {
                    local.clone()
                };
                (projection, PathBuf::from(folder).join("README.md"))
            };
        let verifier = Arc::new(crate::verifier::GatewayVerifier {
            store: task_store.clone(),
            model: model.clone(),
            workspace: root.clone(),
            sandbox: verifier_sandbox,
            protected: protected.clone(),
            redactor: redactor.clone(),
        });
        let gate = excalibur_tasks::verification::TaskCompletionGate::new(
            task_store.clone(),
            excalibur_tasks::verification::CheckRunner::new(secured.clone(), root.clone())
                .with_exclusions(protected.clone()),
        )
        .with_verifier(verifier)
        .with_checklist(checklist_runtime, checklist_path);
        let engine = Engine::new(journal, model, tools)
            .with_context_policy(options.context_policy.clone())
            .with_completion_gate(Arc::new(gate))
            .with_durable_sink(Arc::new(Projection(telemetry.clone(), task_store.clone())));
        let mut context = ContextInput::new(
            "You are Excalibur, the execution coordinator for one isolated task. Never ask questions or request permission. Use conservative assumptions. Missing mandatory data blocks the task: call status with action=block and a reason instead of asking. Rejected operations are tool results: find an authorized alternative. Never read verifier tests or benchmark solutions; never change security or use undeclared services.",
            options.request.clone(),
            "UTC",
        );
        context.layers[0].push_str(&format!(
            " Response language: {}.",
            frozen.as_ref().map(|p| p.language.as_str()).unwrap_or("en")
        ));
        context.layers[1]=serde_json::to_string(&json!({"role":"coordinator","level":1,"sampling":sampling,"skills":skills.iter().map(|s|json!({"name":s.name,"group":s.group,"description":s.description})).collect::<Vec<_>>(),"profile":frozen})).map_err(|_|ExecError::Backend)?;
        context.layers[2] = format!(
            "Environment: {} / {}; UTC",
            std::env::consts::OS,
            std::env::consts::ARCH
        );
        context.reasoning_effort = sampling["reasoning_effort"].as_str().map(str::to_owned);
        context.output_schema = options.output_schema.clone();
        let mut criteria = options.criteria;
        criteria.push(Criterion::FinalResponse {
            schema: options.output_schema,
        });
        let run = engine
            .create(
                &options.operation_id,
                excalibur_engine::RunSpec {
                    context,
                    budget: options.budget.clone(),
                    criteria,
                },
            )
            .await
            .map_err(config_error)?;
        task_store.host_batch("episode", "bootstrap-execution", json!({"ops":[
            {"op":"create_task","id":"episode-task","title":"Execute the literal request","data":{"request_card":{"original":options.request,"interpretation":"Execute the literal request and report observable results","assumptions":[],"open_questions":[],"pinned_instructions":[]},"budget":options.budget,"metadata":{"workspace":root}}},
            {"op":"create_requirement","id":"episode-request","data":{"text":if options.request.trim().is_empty(){"Mandatory request input is missing"}else{options.request.as_str()},"state":"accepted","kind":"functional","priority":"must"}},
            {"op":"create_criterion","id":"episode-response","data":{"requirement_id":"episode-request","statement":"Return a final response satisfying the declared local output contract","method":"inspection","expected":"Nonempty final response validated against the host output schema, when supplied","impact_area":[],"metadata":{"implicit_final_response":true}}},
            {"op":"link_requirement","task":"episode-task","requirement":"episode-request"}
        ]})).await.map_err(config_error)?;
        task_store
            .bind_root("episode-task", &run)
            .await
            .map_err(config_error)?;
        security
            .register_run(
                &run,
                RunSecurity {
                    parent: None,
                    project: "episode".into(),
                    workspace: root,
                    token,
                    autonomy: vec![if options.profile == ExecutionProfile::Eval {
                        Autonomy::Full
                    } else {
                        options.autonomy
                    }],
                    full_acknowledged: options.profile == ExecutionProfile::Eval
                        || options.autonomy == Autonomy::Full,
                    user_requested: true,
                },
                excalibur_store::unix_seconds(),
            )
            .await
            .map_err(|_| ExecError::Backend)?;
        let blocked = options.blocked_reason.or_else(|| {
            if options.request.trim().is_empty() {
                Some("mandatory task request is missing".into())
            } else {
                options
                    .required_inputs
                    .iter()
                    .find(|(_, v)| v.as_ref().is_none_or(|s| s.trim().is_empty()))
                    .map(|(k, _)| format!("mandatory input is missing: {k}"))
            }
        });
        Ok(Self {
            engine,
            run_id: run,
            telemetry,
            tasks: task_store,
            local,
            project_monitor,
            graph_db,
            lsp,
            db,
            logs,
            blocked,
        })
    }
    pub async fn run(
        self,
        mut cancel: Option<tokio::sync::watch::Receiver<bool>>,
    ) -> Result<ExecOutcome> {
        let sampler = self
            .telemetry
            .start_sampler(&self.run_id)
            .await
            .map_err(|_| ExecError::Backend)?;
        let result = if let Some(reason) = &self.blocked {
            self.engine.block(&self.run_id, reason).await
        } else {
            tokio::select! {
             value=self.engine.run(&self.run_id)=>value.map(|_|()),
             _=async {if let Some(ref mut receiver)=cancel{while !*receiver.borrow(){if receiver.changed().await.is_err(){std::future::pending::<()>().await;}}}else{std::future::pending::<()>().await;}}=>self.engine.cancel(&self.run_id).await,
            }
        };
        if let Err(error) = result {
            self.engine
                .fail(&self.run_id, &error.to_string())
                .await
                .map_err(|_| ExecError::Backend)?;
        }
        self.project_monitor
            .scan_now()
            .await
            .map_err(config_error)?;
        self.project_monitor.stop().await;
        self.lsp.shutdown_all().await;
        self.local.terminals().shutdown().await;
        sampler.stop().await.map_err(|_| ExecError::Backend)?;
        self.engine
            .flush_events(&self.run_id)
            .await
            .map_err(|_| ExecError::Backend)?;
        let snapshot = self
            .engine
            .load(&self.run_id)
            .await
            .map_err(|_| ExecError::Backend)?;
        let mut final_snapshot = serde_json::to_value(&snapshot).map_err(|_| ExecError::Backend)?;
        final_snapshot["exit_code"] = json!(exit_code(&snapshot.status));
        if snapshot.status != Status::Completed {
            let mut cursor = 0;
            let mut class = None;
            loop {
                let events = self
                    .engine
                    .journal()
                    .entries(&self.run_id, cursor, 16)
                    .await
                    .map_err(|_| ExecError::Backend)?;
                if events.is_empty() {
                    break;
                }
                for event in events {
                    cursor = event.sequence;
                    if event.kind == "model_failure" {
                        class = event.payload["failure_class"].as_str().map(str::to_owned);
                    }
                }
            }
            if let Some(class) = &class {
                final_snapshot["failure_class"] = json!(class);
            }
            let failure = crate::connection::installed_failure(
                class.as_deref(),
                exit_code(&snapshot.status),
                snapshot.reason.as_deref(),
            );
            final_snapshot["harbor_error_type"] = json!(failure.exception_type);
            let directory = self.logs.join("excalibur");
            let mut file =
                tempfile::NamedTempFile::new_in(&directory).map_err(|_| ExecError::Backend)?;
            use std::io::Write;
            file.write_all(&serde_json::to_vec_pretty(&failure).map_err(|_| ExecError::Backend)?)
                .map_err(|_| ExecError::Backend)?;
            file.as_file().sync_all().map_err(|_| ExecError::Backend)?;
            file.persist(directory.join("failure.json"))
                .map_err(|_| ExecError::Backend)?;
            #[cfg(unix)]
            std::fs::File::open(&directory)
                .and_then(|f| f.sync_all())
                .map_err(|_| ExecError::Backend)?;
        }
        self.telemetry
            .finalize(&self.run_id, &final_snapshot)
            .await
            .map_err(|_| ExecError::Backend)?;
        let outcome = outcome(&self.run_id, &self.logs, &snapshot);
        match excalibur_models::ReplayTape::from_journal(self.engine.journal(), &self.run_id).await
        {
            Ok(tape) => tape
                .save(&self.logs.join("excalibur/replay.json"))
                .map_err(|_| ExecError::Backend)?,
            Err(error) => std::fs::write(
                self.logs.join("excalibur/replay-export.json"),
                serde_json::to_vec(&json!({"available":false,"reason":error.to_string()}))
                    .map_err(|_| ExecError::Backend)?,
            )
            .map_err(|_| ExecError::Backend)?,
        }
        self.graph_db
            .shutdown()
            .await
            .map_err(|_| ExecError::Backend)?;
        self.db.shutdown().await.map_err(|_| ExecError::Backend)?;
        Ok(outcome)
    }
}
fn outcome(run: &str, logs: &Path, s: &Snapshot) -> ExecOutcome {
    ExecOutcome {
        run_id: run.into(),
        status: serde_json::to_value(&s.status)
            .unwrap()
            .as_str()
            .unwrap()
            .to_owned(),
        exit_code: exit_code(&s.status),
        final_output: s.final_text.clone(),
        reason: s.reason.clone(),
        logs_dir: logs.into(),
    }
}
