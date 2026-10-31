//! Three-tree backend scenario with real confined C compilation, process I/O and
//! file edits. Pure read barriers stand in for future web/browser adapters; no
//! browser, external provider or eight-minute test suite is claimed.
use excalibur_context::ContextInput;
use excalibur_daemon::{DaemonActions, RuntimeSet, ScopedPlanning, WorkerTools};
use excalibur_engine::{Budget, Criterion, Engine, RunSpec, Status};
use excalibur_gateway::{AccessPolicy, Gateway, SharedBackend};
use excalibur_journal::Journal;
use excalibur_master::{
    Classification, IntakeDecider, IntakeKind, LexiconEntry, Master, MasterConfig, Preanalysis,
};
use excalibur_models::{ModelError, ModelGateway};
use excalibur_orchestrator::{
    Agent, Delegation, Limits, OrchestrationGate, Orchestrator, OrchestratorTools, Project,
    RunAuthority,
};
use excalibur_security::{Autonomy, CapabilityToken, Root, RunSecurity, SecuredTools, Security};
use excalibur_store::{Database, DatabaseKind};
use excalibur_tasks::{CriterionState, TaskStore};
use excalibur_tools::{
    Access, LocalTools, ToolPolicy,
    terminal::{SandboxConfig, prepare_project_boundary},
};
use excalibur_types::{CriterionId, RequirementId, runtime::*};
use excalibur_vault::Redactor;
use serde_json::{Value, json};
use std::{
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};
const TOKEN: &str = "synthetic-native-three-tree-fixture-token";
const CHECK_COMMAND: &str = "set -e; cc check.c -o /tmp/native-check; /tmp/native-check; printf 'check-native-started\\n'; while [ ! -f release-check ]; do sleep 0.02; done; printf 'check-native-finished\\n'";
const GENERAL_COMMAND: &str = "set -e; cc general.c -o /tmp/general; /tmp/general; printf 'general-native-started\\n'; while [ ! -f release-general ]; do sleep 0.02; done; printf 'general-native-finished\\n'";

#[derive(Default)]
struct Barriers {
    edit_ready: AtomicBool,
    edit_cancelled: AtomicUsize,
    pdf_effects: AtomicUsize,
    csv_effects: AtomicUsize,
    immediate_ready: AtomicBool,
    cooperative_ready: AtomicBool,
    cooperative_cancelled: AtomicUsize,
}
struct HeldTools {
    delegate: Arc<ScopedPlanning>,
    barriers: Arc<Barriers>,
}
impl HeldTools {
    fn old_edit(tool: &PreparedTool) -> bool {
        tool.call.name == "edit_file" && tool.call.arguments.contains("PDF_PENDING")
    }
}
impl ToolRuntime for HeldTools {
    fn definitions(&self) -> Vec<ToolDefinition> {
        self.delegate.definitions()
    }
    fn run_definitions<'a>(
        &'a self,
        run: &'a str,
    ) -> BoxFuture<'a, Result<Option<Vec<ToolDefinition>>, String>> {
        self.delegate.run_definitions(run)
    }
    fn scheduling<'a>(
        &'a self,
        run: &'a str,
        call: &'a ToolCall,
    ) -> BoxFuture<'a, Option<ToolScheduling>> {
        self.delegate.scheduling(run, call)
    }
    fn prepare<'a>(
        &'a self,
        run: &'a str,
        call: ToolCall,
    ) -> BoxFuture<'a, Result<PreparedTool, String>> {
        self.delegate.prepare(run, call)
    }
    fn execute<'a>(
        &'a self,
        run: &'a str,
        key: &'a str,
        tool: &'a PreparedTool,
    ) -> BoxFuture<'a, ToolResult> {
        Box::pin(async move {
            // Admission barrier outside EpisodeTools' mutation guard: no file effect
            // or write lock exists yet, so this fixture adapter can cancel immediately.
            if Self::old_edit(tool) {
                self.barriers.edit_ready.store(true, Ordering::SeqCst);
                return std::future::pending().await;
            }
            if tool.call.name == "read_file" && tool.call.arguments.contains("immediate.txt") {
                self.barriers.immediate_ready.store(true, Ordering::SeqCst);
                return std::future::pending().await;
            }
            let result = self.delegate.execute(run, key, tool).await;
            if tool.call.name == "read_file" && tool.call.arguments.contains("cooperative.txt") {
                assert!(result.success, "{result:?}");
                self.barriers
                    .cooperative_ready
                    .store(true, Ordering::SeqCst);
                return std::future::pending().await;
            }
            if tool.call.name == "edit_file" && result.success {
                if tool.call.arguments.contains("PDF_PENDING") {
                    self.barriers.pdf_effects.fetch_add(1, Ordering::SeqCst);
                } else {
                    self.barriers.csv_effects.fetch_add(1, Ordering::SeqCst);
                }
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
            if Self::old_edit(tool) && self.barriers.edit_cancelled.load(Ordering::SeqCst) > 0 {
                return Reconcile::Done(ToolResult::error(
                    "fixture admission cancelled before any file effect",
                ));
            }
            self.delegate.reconcile(run, key, tool).await
        })
    }
    fn interrupt_execution<'a>(
        &'a self,
        run: &'a str,
        cancelled: bool,
        tool: Option<&'a PreparedTool>,
    ) -> BoxFuture<'a, Result<bool, String>> {
        Box::pin(async move {
            if let Some(tool) = tool {
                if Self::old_edit(tool) {
                    self.barriers.edit_cancelled.fetch_add(1, Ordering::SeqCst);
                    return Ok(true);
                }
                if tool.call.name == "read_file" && tool.call.arguments.contains("immediate.txt") {
                    return Ok(true);
                }
                if tool.call.name == "read_file" && tool.call.arguments.contains("cooperative.txt")
                {
                    self.barriers
                        .cooperative_cancelled
                        .fetch_add(1, Ordering::SeqCst);
                    return Ok(true);
                }
            }
            self.delegate
                .interrupt_execution(run, cancelled, tool)
                .await
        })
    }
    fn pending_approval<'a>(&'a self, run: &'a str) -> BoxFuture<'a, bool> {
        self.delegate.pending_approval(run)
    }
    fn approval_blocked<'a>(&'a self, run: &'a str) -> BoxFuture<'a, Option<String>> {
        self.delegate.approval_blocked(run)
    }
    fn result_requires_approval(&self, result: &ToolResult) -> bool {
        self.delegate.result_requires_approval(result)
    }
    fn sanitize_durable(&self, value: &Value) -> Result<Value, String> {
        self.delegate.sanitize_durable(value)
    }
    fn observe_untrusted<'a>(
        &'a self,
        run: &'a str,
        source: &'a str,
    ) -> BoxFuture<'a, Result<(), String>> {
        self.delegate.observe_untrusted(run, source)
    }
    fn live_status(&self, run: &str) -> Vec<Value> {
        self.delegate.live_status(run)
    }
    fn configure_execution_limits<'a>(
        &'a self,
        run: &'a str,
        limits: ExecutionLimits,
    ) -> BoxFuture<'a, Result<(), String>> {
        self.delegate.configure_execution_limits(run, limits)
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
        authorizer: Arc<dyn NetworkAuthorizer>,
    ) -> BoxFuture<'a, Result<(), String>> {
        self.delegate.configure_network_authorizer(run, authorizer)
    }
    fn execution_resources<'a>(
        &'a self,
        run: &'a str,
    ) -> BoxFuture<'a, Result<Option<ExecutionResources>, String>> {
        self.delegate.execution_resources(run)
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
}
struct Authority {
    workers: Arc<WorkerTools>,
    planning: Arc<ScopedPlanning>,
    security: Security,
}
impl RunAuthority for Authority {
    fn bind<'a>(&'a self, agent: &'a Agent) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async move {
            let binding = self.workers.bind_agent(agent, None).await?;
            self.security
                .register_run(
                    agent.run_id.as_deref().ok_or("run missing")?,
                    RunSecurity {
                        parent: None,
                        project: agent.scope.clone(),
                        workspace: binding.workspace.clone(),
                        token: binding.token.clone(),
                        autonomy: vec![Autonomy::Autonomous],
                        full_acknowledged: false,
                        user_requested: true,
                    },
                    excalibur_store::unix_seconds(),
                )
                .await
                .map_err(|e| e.to_string())?;
            let command = match agent.delegation.as_ref().map(|d| d.goal.as_str()) {
                Some("native:tests") => Some(CHECK_COMMAND),
                Some("native:conversion") => Some(GENERAL_COMMAND),
                _ => None,
            };
            if let Some(command) = command {
                let now = excalibur_store::unix_seconds();
                let action = excalibur_security::Action::Exec {
                    command: command.into(),
                };
                match self
                    .security
                    .authorize(agent.run_id.as_deref().unwrap(), &action, now)
                    .await
                    .map_err(|e| e.to_string())?
                {
                    excalibur_security::Decision::Ask { approval_id, .. } => {
                        self.security
                            .answer_approval(
                                &approval_id,
                                true,
                                excalibur_security::ApprovalScope::Task,
                                &format!("fixture-explicit-command:{}", agent.id),
                                now,
                            )
                            .await
                            .map_err(|e| e.to_string())?;
                    }
                    excalibur_security::Decision::Allow => {}
                    other => return Err(format!("fixture command denied: {other:?}")),
                }
            }
            let mut effective = agent.clone();
            effective.token = binding.token;
            self.planning
                .bind_agent(&effective, binding.workspace)
                .await
        })
    }
    fn live_status(&self, run: &str) -> Vec<Value> {
        self.workers.live_status(run)
    }
    fn interrupt_tool<'a>(&'a self, run: &'a str) -> BoxFuture<'a, Result<bool, String>> {
        Box::pin(async move { self.workers.interrupt_tool(run).await })
    }
}
struct WorkerModel {
    started: Mutex<std::collections::BTreeSet<String>>,
    csv: AtomicUsize,
    old_edit: AtomicUsize,
    barriers: Arc<Barriers>,
}
fn response(call: ToolCall) -> ModelResponse {
    ModelResponse {
        message: Message {
            role: "assistant".into(),
            content: String::new(),
            tool_calls: vec![call],
            tool_call_id: None,
            provider_state: None,
        },
        usage: Usage::default(),
    }
}
fn call(name: &str, args: Value) -> ToolCall {
    ToolCall {
        id: format!("native-{name}"),
        name: name.into(),
        arguments: args.to_string(),
    }
}
impl ModelGateway for WorkerModel {
    fn complete<'a>(
        &'a self,
        request: ModelRequest,
    ) -> BoxFuture<'a, Result<ModelResponse, ModelError>> {
        Box::pin(async move {
            let text = serde_json::to_string(&request.messages).unwrap();
            let task_text = request
                .messages
                .iter()
                .find(|message| message.content.starts_with("[L3]\n"))
                .map(|message| message.content.as_str())
                .unwrap_or("");
            if task_text.contains("native:implementation")
                && text.contains("Export CSV, not PDF")
                && self.barriers.csv_effects.load(Ordering::SeqCst) == 0
            {
                let mut csv = call(
                    "edit_file",
                    json!({"path":"src/export/pdf.rs","old_text":"BASE","new_text":"CSV"}),
                );
                csv.id = format!(
                    "native-csv-edit-{}",
                    self.csv.fetch_add(1, Ordering::SeqCst)
                );
                return Ok(response(csv));
            }
            let kind = [
                "implementation",
                "tests",
                "other",
                "thesis-one",
                "thesis-two",
                "conversion",
            ]
            .into_iter()
            .find(|kind| task_text.contains(&format!("native:{kind}")))
            .unwrap_or("parent");
            if !self.started.lock().unwrap().insert(kind.into()) {
                if kind == "implementation" && !self.barriers.edit_ready.load(Ordering::SeqCst) {
                    let mut edit = call(
                        "edit_file",
                        json!({"path":"src/export/pdf.rs","old_text":"BASE","new_text":"PDF_PENDING"}),
                    );
                    edit.id = format!(
                        "native-pdf-edit-{}",
                        self.old_edit.fetch_add(1, Ordering::SeqCst)
                    );
                    return Ok(response(edit));
                }
                return std::future::pending().await;
            }
            let tool = match kind {
                "implementation" => call("read_file", json!({"path":"src/export/pdf.rs"})),
                "tests" => call(
                    "task_db",
                    json!({"ops":[{"op":"run_check","check":"native-check"}]}),
                ),
                "conversion" => call(
                    "exec",
                    json!({"cmd":GENERAL_COMMAND,"yield_ms":250,"timeout_ms":60000}),
                ),
                "thesis-one" => call("read_file", json!({"path":"immediate.txt"})),
                "thesis-two" => call("read_file", json!({"path":"cooperative.txt"})),
                "other" => call("read_file", json!({"path":"notes.txt"})),
                _ => return std::future::pending().await,
            };
            Ok(response(tool))
        })
    }
}
#[derive(Default)]
struct RootModel(AtomicUsize);
impl ModelGateway for RootModel {
    fn complete<'a>(
        &'a self,
        request: ModelRequest,
    ) -> BoxFuture<'a, Result<ModelResponse, ModelError>> {
        Box::pin(async move {
            self.0.fetch_add(1, Ordering::SeqCst);
            if request
                .messages
                .iter()
                .any(|message| message.role == "tool")
            {
                return Ok(ModelResponse {
                    message: Message::text(
                        "assistant",
                        "Actualicé R4 a CSV; interrumpí la edición afectada y los tests nativos terminan antes de repetirse.",
                    ),
                    usage: Usage::default(),
                });
            }
            Ok(response(call(
                "task_db",
                json!({"ops":[{"op":"update_requirement","id":"R4","expected_version":1,"data":{"text":"Export CSV, not PDF"}}]}),
            )))
        })
    }
}
#[derive(Default)]
struct ScenarioDecider {
    targets: Mutex<(String, String)>,
}
impl IntakeDecider for ScenarioDecider {
    fn classify<'a>(
        &'a self,
        text: &'a str,
        _: &'a Preanalysis,
    ) -> BoxFuture<'a, Result<Classification, String>> {
        Box::pin(async move {
            let (lena, thesis) = self.targets.lock().unwrap().clone();
            let (kind, target) = if text.contains("Lena") {
                (IntakeKind::Status, lena)
            } else if text.contains("tesis") {
                (IntakeKind::Pause, thesis)
            } else {
                return Err("unclassified fixture input".into());
            };
            Ok(Classification {
                kind,
                confidence: 0.99,
                target: Some(target),
                scopes: vec![],
                literal_instructions: vec![],
                requires_clarification: false,
            })
        })
    }
}
fn project(id: &str, root: PathBuf) -> Project {
    Project {
        id: id.into(),
        name: id.into(),
        aliases: vec![],
        description: "Synthetic control fixture".into(),
        roots: vec![root],
        status: "active".into(),
        vcs: None,
        settings: json!({}),
    }
}
fn delegation(
    parent: &Agent,
    id: &str,
    name: &str,
    profile: &str,
    scope: &str,
    root: &Path,
) -> Delegation {
    let mut token = parent.token.clone();
    token
        .data_scopes
        .retain(|item| item == scope || item == "general");
    token.roots = vec![Root {
        path: root.into(),
        write: !matches!(profile, "researcher" | "verifier" | "explorer"),
    }];
    token.budget = if parent.depth == 0 { 1000 } else { 100 };
    if profile == "researcher" {
        token.processes = false;
    }
    Delegation {
        operation: id.into(),
        name: name.into(),
        profile: profile.into(),
        scope: scope.into(),
        task: format!("scenario:{id}"),
        origin_conversation: "telegram-main".into(),
        goal: format!("Synthetic isolated work: {id}"),
        deliverable: "Local fixture result".into(),
        inputs: vec![],
        pinned_instructions: vec![],
        suggested_skills: vec![],
        fork_context: false,
        token,
        spec: RunSpec {
            context: ContextInput::new("Keep project scopes isolated.", id, "fixed fixture time"),
            budget: Budget {
                max_steps: if parent.depth == 0 { 150 } else { 30 },
                // Workers must afford their own native L0 core catalog.
                max_tokens: if parent.depth == 0 { 100000 } else { 10000 },
                max_wall_seconds: if parent.depth == 0 { 3600 } else { 120 },
                max_tool_calls: if parent.depth == 0 { 500 } else { 30 },
                ..Default::default()
            },
            criteria: vec![Criterion::FinalResponse { schema: None }],
        },
        priority: 1,
    }
}
async fn rpc(
    client: &reqwest::Client,
    address: std::net::SocketAddr,
    method: &str,
    params: Value,
) -> Value {
    let response: Value = client
        .post(format!("http://{address}/rpc"))
        .bearer_auth(TOKEN)
        .timeout(Duration::from_secs(3))
        .json(&json!({"jsonrpc":"2.0","id":1,"method":method,"params":params}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(response.get("error").is_none(), "{response}");
    response["result"].clone()
}
async fn send(client: &reqwest::Client, address: std::net::SocketAddr, id: &str, text: &str) {
    rpc(client,address,"conversation.send",json!({"conversation":"telegram-main","client_id":"native-fixture","client_msg_id":id,"text":text,"scope":"project:gastos"})).await;
}
async fn approve_fixture_edit(
    client: &reqwest::Client,
    address: std::net::SocketAddr,
    agent: &Agent,
) {
    let rows = rpc(
        client,
        address,
        "approval.list",
        json!({"target":agent.id,"limit":20}),
    )
    .await;
    for entry in rows["approvals"].as_array().unwrap() {
        let id = entry["id"].as_str().unwrap();
        assert!(
            entry["request"].to_string().contains("WriteExact"),
            "unexpected fixture approval: {entry}"
        );
        rpc(client,address,"approval.respond",json!({"approval":id,"approved":true,"scope":"task","operation":format!("native-fixture-user:{id}")})).await;
    }
}
struct ReleaseOnDrop(PathBuf, PathBuf);
impl Drop for ReleaseOnDrop {
    fn drop(&mut self) {
        let _ = std::fs::write(&self.0, "release");
        let _ = std::fs::write(&self.1, "release");
    }
}
#[test]
fn native_three_tree_correction_preserves_the_graceful_check_and_unrelated_process() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("projects");
    let gastos = root.join("gastos");
    let thesis = root.join("tesis");
    std::fs::create_dir_all(gastos.join("src/export")).unwrap();
    std::fs::create_dir_all(&thesis).unwrap();
    std::fs::write(gastos.join("src/export/pdf.rs"), "BASE\n").unwrap();
    std::fs::write(gastos.join("notes.txt"), "unrelated gastos file").unwrap();
    std::fs::write(
        gastos.join("check.c"),
        r#"#include <stdio.h>
#include <string.h>
int main(void) {
    char data[32] = {0};
    FILE *source = fopen("src/export/pdf.rs", "r");
    if (!source || !fgets(data, sizeof(data), source)) return 2;
    fclose(source);
    if (strcmp(data, "BASE\n") != 0) return 3;
    puts("PDF native check OK");
    return 0;
}
"#,
    )
    .unwrap();
    std::fs::write(
        root.join("general.c"),
        "#include <stdio.h>\nint main(void){puts(\"general conversion OK\");return 0;}",
    )
    .unwrap();
    for name in ["immediate.txt", "cooperative.txt"] {
        std::fs::write(thesis.join(name), "synthetic read contract, no browser").unwrap();
    }
    prepare_project_boundary(&root).unwrap();
    let db = Database::open(
        temp.path().join("data/main.db"),
        DatabaseKind::Main,
        Default::default(),
    )
    .unwrap();
    let runtimes = RuntimeSet::new(4, 1).unwrap();
    let _release = ReleaseOnDrop(gastos.join("release-check"), root.join("release-general"));
    runtimes.block_on(async{
        let projects=vec![project("gastos",gastos.clone()),project("tesis",thesis.clone())];let policy=ToolPolicy{read:Access::Allow,write:Access::Allow,exec:Access::Allow,sandbox:Some(SandboxConfig::project(&root)),..Default::default()};let local=Arc::new(LocalTools::new(db.clone(),root.clone(),policy.clone()).await.unwrap());let workers=Arc::new(WorkerTools::new(db.clone(),local,policy,projects.clone(),None).await.unwrap());
        let token=CapabilityToken{roots:vec![Root{path:root.clone(),write:true}],processes:true,sandbox_profile:Some("project".into()),data_scopes:std::collections::BTreeSet::from(["general".into(),"project:gastos".into(),"project:tesis".into()]),budget:10000,..Default::default()};let security=Security::new(db.clone(),token.clone(),Redactor::default()).await.unwrap();let secured=Arc::new(SecuredTools::new(workers.clone(),security.clone()));let planning=Arc::new(ScopedPlanning::new(secured,db.clone(),Redactor::default()).await.unwrap());let barriers=Arc::new(Barriers::default());let held=Arc::new(HeldTools{delegate:planning.clone(),barriers:barriers.clone()});let coordination=Arc::new(OrchestratorTools::new(held));let tasks=TaskStore::open(db.clone()).await.unwrap();let model=Arc::new(WorkerModel{started:Mutex::new(Default::default()),csv:AtomicUsize::new(0),old_edit:AtomicUsize::new(0),barriers:barriers.clone()});
        let gate=Arc::new(OrchestrationGate::new().with_delegate(Arc::new(excalibur_daemon::task_gate::WorkerTaskGate{store:tasks.clone(),runtime:planning.clone(),workers:workers.clone(),model:model.clone(),redactor:Redactor::default()})));
        let catalog=Arc::new(excalibur_tools::deferred::ToolCatalog::new(excalibur_tools::project::deferred_catalog()).unwrap());let deferred=Arc::new(excalibur_tools::deferred::DeferredTools::new(coordination.clone(),catalog,excalibur_tools::deferred::CallerRole::Worker).unwrap());
        let engine=Arc::new(Engine::new(Journal::new(db.clone()).await.unwrap(),model,deferred).with_completion_gate(gate.clone()));let orch=Orchestrator::open(db.clone(),engine.clone(),Limits::default()).await.unwrap().with_runtime(runtimes.work()).with_tasks(tasks.clone()).with_authority(Arc::new(Authority{workers:workers.clone(),planning,security:security.clone()}));coordination.attach(&orch).unwrap();gate.attach(&orch).unwrap();for project in projects{orch.register_project(project).await.unwrap();}let master_agent=orch.register_master("Master",token).await.unwrap();
        let actions=Arc::new(DaemonActions::new(orch.clone(),master_agent.id.clone()).with_tasks(tasks.clone()).with_security(security.clone()));let root_model=Arc::new(RootModel::default());let master=Master::open(db.clone(),MasterConfig::default(),root_model.clone(),None,actions.clone(),Redactor::default()).await.unwrap();actions.bind_master(master.clone());let decider=Arc::new(ScenarioDecider::default());master.set_decider(Some(decider.clone()));
        let backend=Arc::new(SharedBackend::new(master.clone(),orch.clone()).with_security(security.clone()).with_task_actions(actions.clone()).with_project_roots(vec![root.clone()]).unwrap());let gateway=Gateway::new(backend,db.clone(),AccessPolicy::new(TOKEN,Default::default(),1024*1024).unwrap());let listener=tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();let address=listener.local_addr().unwrap();let server=tokio::spawn(async move{axum::serve(listener,gateway.router()).await.unwrap();});let client=reqwest::Client::new();send(&client,address,"initial","/status").await;
        // Establish the already-running General conversion before synthetic
        // indefinite read barriers. Unknown exec correctly takes the global
        // exclusive admission gate until its first background-session yield.
        let mut conversion_request=delegation(&master_agent,"conversion","Conversion","coordinator","general",root.as_path());conversion_request.goal="native:conversion".into();conversion_request.spec.context=ContextInput::new("Native fixture","native:conversion","fixed");
        let conversion=orch.spawn(&master_agent.id,conversion_request).await.unwrap();orch.tick().await.unwrap();
        tokio::time::timeout(Duration::from_secs(15),async {loop {
            let run=conversion.run_id.as_deref().unwrap();
            let confirmed=engine.journal().replay(run,0).await.unwrap().iter().any(|event|event.kind=="tool_result");
            let native_started=workers.live_status(run).iter().any(|session|session["tail"].as_str().is_some_and(|tail|tail.contains("general-native-started")));
            if confirmed&&native_started {break;}tokio::time::sleep(Duration::from_millis(10)).await;
        }}).await.unwrap();
        let gp=orch.spawn(&master_agent.id,delegation(&master_agent,"gastos-parent","Gastos coordinator","coordinator","project:gastos",&gastos)).await.unwrap();let tp=orch.spawn(&master_agent.id,delegation(&master_agent,"thesis-parent","Thesis coordinator","coordinator","project:tesis",&thesis)).await.unwrap();
        let mut ld=delegation(&gp,"tests","Lena","verifier","project:gastos",&gastos);ld.goal="native:tests".into();ld.spec.context=ContextInput::new("Scoped native test","native:tests","fixed");let lena=orch.spawn(&gp.id,ld).await.unwrap();let mut id=delegation(&gp,"implementation","CSV implementer","implementer","project:gastos",&gastos);id.goal="native:implementation".into();id.spec.context=ContextInput::new("Scoped native edit","native:implementation","fixed");let implementation=orch.spawn(&gp.id,id).await.unwrap();
        let requirement=RequirementId::new().to_string();let criterion=CriterionId::new().to_string();tasks.batch("project:gastos","native-r4",json!({"ops":[{"op":"create_requirement","id":requirement,"data":{"text":"Export PDF","state":"accepted"}},{"op":"create_criterion","id":criterion,"data":{"requirement_id":requirement,"statement":"Native PDF check","method":"test","impact_area":["src/export/pdf.rs"]}},{"op":"create_check","id":"native-check","data":{"criterion_id":criterion,"definition":{"kind":"command","command":CHECK_COMMAND,"timeout_ms":60000,"expected_exit":0}}},{"op":"link_requirement","task":lena.task,"requirement":requirement},{"op":"link_requirement","task":implementation.task,"requirement":requirement}]})).await.unwrap();
        let mut jobs=Vec::new();for (parent,operation,name,profile,scope,path) in [(&gp,"other","Other module","explorer","project:gastos",gastos.as_path()),(&tp,"thesis-one","Read researcher","researcher","project:tesis",thesis.as_path()),(&tp,"thesis-two","Cooperative researcher","researcher","project:tesis",thesis.as_path())]{let mut d=delegation(parent,operation,name,profile,scope,path);d.goal=format!("native:{operation}");d.spec.context=ContextInput::new("Native fixture",format!("native:{operation}"),"fixed");jobs.push(orch.spawn(&parent.id,d).await.unwrap());}jobs.push(conversion);
        for parent in [&gp,&tp]{orch.request_wait_for_children(&parent.id).await.unwrap();}orch.tick().await.unwrap();
        let readiness=tokio::time::timeout(Duration::from_secs(30),async{
            loop {
                approve_fixture_edit(&client,address,&implementation).await;
                orch.reap().await.unwrap();orch.tick().await.unwrap();
                let l=workers.live_status(lena.run_id.as_deref().unwrap());
                let g=workers.live_status(jobs[3].run_id.as_deref().unwrap());
                let check_started=l.iter().any(|session|session["tail"].as_str().is_some_and(|tail|tail.contains("check-native-started")));
                let general_started=g.iter().any(|session|session["tail"].as_str().is_some_and(|tail|tail.contains("general-native-started")));
                if barriers.edit_ready.load(Ordering::SeqCst)&&barriers.immediate_ready.load(Ordering::SeqCst)&&barriers.cooperative_ready.load(Ordering::SeqCst)&&check_started&&general_started {break;}
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }).await;
        if readiness.is_err(){
            eprintln!("barriers edit={} immediate={} cooperative={}",barriers.edit_ready.load(Ordering::SeqCst),barriers.immediate_ready.load(Ordering::SeqCst),barriers.cooperative_ready.load(Ordering::SeqCst));
            for agent in [&lena,&implementation,&jobs[0],&jobs[1],&jobs[2],&jobs[3]] {
                let run=agent.run_id.as_deref().unwrap();eprintln!("native state {} {:?}",agent.name,tokio::time::timeout(Duration::from_secs(1),engine.journal().state(run)).await);
            }
        }
        readiness.unwrap();
        assert_eq!(std::fs::read_to_string(gastos.join("src/export/pdf.rs")).unwrap(),"BASE\n");for parent in [&gp,&tp]{assert_eq!(engine.load(parent.run_id.as_deref().unwrap()).await.unwrap().status,Status::Waiting);}
        *decider.targets.lock().unwrap()=(lena.id.clone(),tp.id.clone());master.set_lexicon(vec![LexiconEntry{alias:"Lena".into(),id:lena.id.clone(),kind:"agent".into()},LexiconEntry{alias:"investigación de la tesis".into(),id:tp.id.clone(),kind:"agent".into()}]).unwrap();send(&client,address,"status","¿Qué está haciendo Lena?").await;assert_eq!(root_model.0.load(Ordering::SeqCst),0);let history=master.history("telegram-main",None,128).await.unwrap();assert!(history.iter().any(|message|message.role=="assistant"&&message.text.contains("Lena")&&!message.text.starts_with('{')));
        send(&client,address,"correction","Cambia R4: la exportación debe ser CSV, no PDF.").await;
        tokio::time::timeout(Duration::from_secs(20),async{loop{if tasks.requirements_and_criteria(&lena.task).await.unwrap().0.iter().any(|r|r.id==requirement&&r.version==2)&&orch.mailbox(&implementation.id).await.unwrap().iter().any(|message|message.kind=="directive")&&orch.mailbox(&lena.id).await.unwrap().iter().any(|message|message.kind=="directive"){break;}tokio::time::sleep(Duration::from_millis(10)).await;}}).await.unwrap();
        let edit_mail=orch.mailbox(&implementation.id).await.unwrap();assert!(edit_mail.iter().any(|m|m.kind=="directive"&&m.priority=="interrupt"));let check_mail=orch.mailbox(&lena.id).await.unwrap();assert!(check_mail.iter().any(|m|m.kind=="directive"&&m.priority=="normal"));for unaffected in [&jobs[0],&tp,&jobs[1],&jobs[2],&jobs[3]]{assert!(orch.mailbox(&unaffected.id).await.unwrap().is_empty());}
        orch.deliver_mailbox(&implementation.id).await.unwrap();orch.deliver_mailbox(&lena.id).await.unwrap();
        tokio::time::timeout(Duration::from_secs(20),async{loop{approve_fixture_edit(&client,address,&implementation).await;orch.reap().await.unwrap();orch.tick().await.unwrap();if barriers.csv_effects.load(Ordering::SeqCst)==1{break;}tokio::time::sleep(Duration::from_millis(10)).await;}}).await.unwrap();assert_eq!(barriers.pdf_effects.load(Ordering::SeqCst),0);assert!(barriers.edit_cancelled.load(Ordering::SeqCst)>0);assert_eq!(std::fs::read_to_string(gastos.join("src/export/pdf.rs")).unwrap(),"CSV\n");
        send(&client,address,"pause","Detén la investigación de la tesis.").await;for paused in [&tp,&jobs[1],&jobs[2]]{assert_eq!(orch.status(&paused.id).await.unwrap().state,"paused");}tokio::time::timeout(Duration::from_secs(5),async{while barriers.cooperative_cancelled.load(Ordering::SeqCst)==0{tokio::task::yield_now().await;}}).await.unwrap();
        for live in [&lena,&jobs[3]]{assert!(workers.live_status(live.run_id.as_deref().unwrap()).iter().any(|s|s["exited"]==false),"unrelated native process stopped");}
        std::fs::write(gastos.join("release-check"),"release").unwrap();tokio::time::timeout(Duration::from_secs(20),async{loop{if !tasks.task_evidence(&lena.task).await.unwrap().is_empty(){break;}tokio::time::sleep(Duration::from_millis(10)).await;}}).await.unwrap();let evidence=tasks.task_evidence(&lena.task).await.unwrap().into_iter().find(|e|e.check_id=="native-check").unwrap();assert!(evidence.trusted);assert_eq!(evidence.result,excalibur_tasks::EvidenceResult::Error);assert!(evidence.stale);assert_eq!(evidence.explanation,"affected files changed while check was running");assert!(!evidence.snapshot.fresh(evidence.snapshot.instruction_revision).await.unwrap(),"old BASE evidence must be stale for the actual CSV file");let output=db.read_blob(evidence.output_blob.unwrap().parse().unwrap(),1024*1024).await.unwrap();let observation:Value=serde_json::from_slice(&output).unwrap();assert_eq!(observation["success"],true);assert_eq!(observation["output"]["exit_code"],0);assert_eq!(observation["output"]["exited"],true);assert_eq!(observation["output"]["timed_out"],false);assert!(observation["output"]["error"].is_null());assert!(observation["output"]["output"].as_str().unwrap().contains("check-native-finished"));let (_,criteria)=tasks.requirements_and_criteria(&lena.task).await.unwrap();let c=criteria.iter().find(|c|c.id==criterion).unwrap();assert_ne!(tasks.assess_criterion(c,0).await.unwrap().state,CriterionState::Satisfied);
        assert!(workers.live_status(jobs[3].run_id.as_deref().unwrap()).iter().any(|s|s["exited"]==false));let history=master.history("telegram-main",None,128).await.unwrap();assert!(history.iter().any(|m|m.role=="assistant"&&m.text.contains("CSV")&&m.text.contains("tests nativos")));assert!(history.iter().any(|m|m.role=="assistant"&&m.text.contains("resume")&&m.text.contains("cancel")));
        std::fs::write(root.join("release-general"),"release").unwrap();server.abort();master.shutdown().await;actions.detach();orch.shutdown().await.unwrap();workers.shutdown().await;db.shutdown().await.unwrap();
    });
    runtimes.shutdown(Duration::from_secs(3));
}
