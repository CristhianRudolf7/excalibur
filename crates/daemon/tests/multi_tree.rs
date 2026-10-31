//! Shared-host reproduction of the multi-project control path. Models are local
//! pending futures: no real inference, long test suite or research is performed.
use excalibur_context::ContextInput;
use excalibur_daemon::{DaemonActions, DaemonHost, HostOptions, RuntimeSet};
use excalibur_engine::{Budget, Criterion, RunSpec, Status};
use excalibur_master::{
    Classification, FastActions, Incoming, IntakeDecider, IntakeKind, LexiconEntry, Preanalysis,
};
use excalibur_models::{ModelError, ModelGateway};
use excalibur_orchestrator::{Agent, Delegation, Project};
use excalibur_security::Root;
use excalibur_store::{Database, DatabaseKind};
use excalibur_tasks::TaskStore;
use excalibur_types::runtime::*;
use excalibur_types::{CriterionId, RequirementId};
use excalibur_vault::Redactor;
use serde_json::json;
use std::{
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

#[derive(Default)]
struct PendingModels {
    master_calls: AtomicUsize,
    worker_calls: AtomicUsize,
}
impl ModelGateway for PendingModels {
    fn complete<'a>(
        &'a self,
        request: ModelRequest,
    ) -> BoxFuture<'a, Result<ModelResponse, ModelError>> {
        Box::pin(async move {
            if request
                .messages
                .iter()
                .any(|message| message.content.contains("Synthetic isolated work:"))
            {
                self.worker_calls.fetch_add(1, Ordering::SeqCst);
                std::future::pending().await
            } else {
                self.master_calls.fetch_add(1, Ordering::SeqCst);
                Ok(ModelResponse {
                    message: Message::text(
                        "assistant",
                        "Unexpected model path in a deterministic control fixture",
                    ),
                    usage: Usage::default(),
                })
            }
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
fn request(id: &str, text: &str) -> Incoming {
    Incoming {
        conversation: "telegram-main".into(),
        client_id: "synthetic-telegram".into(),
        client_msg_id: id.into(),
        text: text.into(),
        scope: None,
        timezone: "UTC".into(),
        channel: "test".into(),
        reply_to: None,
        attachments: vec![],
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
#[test]
fn parallel_project_trees_keep_status_corrections_and_pause_isolated() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("projects");
    let gastos = root.join("gastos");
    let thesis = root.join("tesis");
    std::fs::create_dir_all(&gastos).unwrap();
    std::fs::create_dir_all(&thesis).unwrap();
    let database = Database::open(
        temp.path().join("data/main.db"),
        DatabaseKind::Main,
        Default::default(),
    )
    .unwrap();
    let runtimes = RuntimeSet::new(2, 1).unwrap();
    let model = Arc::new(PendingModels::default());
    let decider = Arc::new(ScenarioDecider::default());
    let mut options = HostOptions::project(root.clone());
    options.projects = vec![
        project("gastos", gastos.clone()),
        project("tesis", thesis.clone()),
    ];
    options.decider = Some(decider.clone());
    options.lane_limits.concurrency = 10;
    let host = runtimes
        .block_on(DaemonHost::open(
            database.clone(),
            options,
            model.clone(),
            None,
            "synthetic-multi-tree-token-for-local-tests-only",
            Redactor::default(),
            runtimes.work(),
        ))
        .unwrap();
    runtimes.block_on(async {
        // The origin conversation exists before any task/notification is created.
        host.master.receive(request("initial","/status")).await.unwrap();
        let master=host.orchestrator.list_agents().await.unwrap().into_iter().find(|agent|agent.depth==0).unwrap();
        let gastos_parent=host.orchestrator.spawn(&master.id,delegation(&master,"gastos-parent","Gastos coordinator","coordinator","project:gastos",&gastos)).await.unwrap();
        let thesis_parent=host.orchestrator.spawn(&master.id,delegation(&master,"thesis-parent","Thesis coordinator","coordinator","project:tesis",&thesis)).await.unwrap();
        let lena=host.orchestrator.spawn(&gastos_parent.id,delegation(&gastos_parent,"gastos-tests","Lena","verifier","project:gastos",&gastos)).await.unwrap();
        let implementer=host.orchestrator.spawn(&gastos_parent.id,delegation(&gastos_parent,"gastos-implementation","CSV implementer","implementer","project:gastos",&gastos)).await.unwrap();
        let tasks=TaskStore::open(database.clone()).await.unwrap();
        let export_requirement=RequirementId::new().to_string();let export_criterion=CriterionId::new().to_string();
        tasks.batch("project:gastos","r4-seed",json!({"ops":[{"op":"create_requirement","id":export_requirement,"data":{"text":"Export PDF","state":"accepted"}},{"op":"create_criterion","id":export_criterion,"data":{"requirement_id":export_requirement,"statement":"Expected export format","method":"inspection","impact_area":["src/export/pdf.rs"]}},{"op":"link_requirement","task":lena.task,"requirement":export_requirement},{"op":"link_requirement","task":implementer.task,"requirement":export_requirement}]})).await.unwrap();
        let export_id=export_requirement.clone();let code=database.read(excalibur_store::ReadLane::Interactive,move|conn|Ok(conn.query_row("SELECT code FROM requirements WHERE id=?1",[&export_id],|row|row.get::<_,String>(0))?)).await.unwrap();assert_eq!(code,"R4");
        let third=host.orchestrator.spawn(&gastos_parent.id,delegation(&gastos_parent,"gastos-other","Other module","explorer","project:gastos",&gastos)).await.unwrap();
        let researcher_one=host.orchestrator.spawn(&thesis_parent.id,delegation(&thesis_parent,"thesis-one","Research one","researcher","project:tesis",&thesis)).await.unwrap();
        let researcher_two=host.orchestrator.spawn(&thesis_parent.id,delegation(&thesis_parent,"thesis-two","Research two","researcher","project:tesis",&thesis)).await.unwrap();
        let conversion=host.orchestrator.spawn(&master.id,delegation(&master,"conversion","Document conversion","coordinator","general",&root)).await.unwrap();
        assert_eq!(host.orchestrator.list_agents().await.unwrap().iter().filter(|agent|agent.depth>0).count(),8);
        for parent in [&gastos_parent,&thesis_parent] {host.orchestrator.request_wait_for_children(&parent.id).await.unwrap();}
        // Admission binds the real host security authority before invoking each
        // engine. Waiting parents leave the actor without making a model call.
        let started=host.orchestrator.tick().await.unwrap();assert_eq!(started.len(),8);
        tokio::time::timeout(Duration::from_secs(15),async {loop {if host.orchestrator.status(&gastos_parent.id).await.unwrap().state=="waiting" && host.orchestrator.status(&thesis_parent.id).await.unwrap().state=="waiting" {break;}tokio::task::yield_now().await;}}).await.unwrap();
        let mut reaped=0;tokio::time::timeout(Duration::from_secs(15),async {while reaped<2 {reaped+=host.orchestrator.reap().await.unwrap();tokio::task::yield_now().await;}}).await.unwrap();
        for parent in [&gastos_parent,&thesis_parent] {let waiting=host.orchestrator.engine().load(parent.run_id.as_ref().unwrap()).await.unwrap();assert_eq!(waiting.status,Status::Waiting);assert_eq!(waiting.model_calls,0);}
        assert!(host.orchestrator.tick().await.unwrap().is_empty());
        tokio::time::timeout(Duration::from_secs(15),async {loop {let mut running=true;for agent in [&lena,&implementer,&third,&researcher_one,&researcher_two,&conversion] {running &= host.orchestrator.status(&agent.id).await.unwrap().model_in_flight;}if running {break;}tokio::task::yield_now().await;}}).await.unwrap();
        // Capacity is counted from running executions, not live waiting parents.
        let mut overflow=Vec::new();for index in 0..3 {overflow.push(host.orchestrator.spawn(&master.id,delegation(&master,&format!("overflow-{index}"),&format!("Queued {index}"),"coordinator","general",&root)).await.unwrap());}
        assert_eq!(host.orchestrator.tick().await.unwrap().len(),2);
        tokio::time::timeout(Duration::from_secs(15),async {while model.worker_calls.load(Ordering::SeqCst)<8 {tokio::task::yield_now().await;}}).await.unwrap();let mut queued=0;for agent in &overflow {let status=host.orchestrator.status(&agent.id).await.unwrap();if status.state=="ready"&&status.queue_position.is_some(){assert_eq!(status.queue_position,Some(1));queued+=1;}}assert_eq!(queued,1);
        *decider.targets.lock().unwrap()=(lena.id.clone(),thesis_parent.id.clone());host.master.set_lexicon(vec![LexiconEntry{alias:"Lena".into(),id:lena.id.clone(),kind:"agent".into()},LexiconEntry{alias:"investigación de la tesis".into(),id:thesis_parent.id.clone(),kind:"agent".into()}]).unwrap();
        host.master.receive(request("natural-status","¿Qué está haciendo Lena?")).await.unwrap();assert_eq!(model.master_calls.load(Ordering::SeqCst),0);
        let history=host.master.history("telegram-main",None,128).await.unwrap();assert!(history.iter().any(|message|message.role=="assistant"&&message.text.contains("Lena")&&message.text.contains("running")));
        // The same task_db adapter used by the master persists R4 first, then
        // derives the exact task branches. No synthetic directive rows are seeded.
        let actions=DaemonActions::new(host.orchestrator.clone(),master.id.clone()).with_tasks(tasks.clone());actions.bind_master(host.master.clone());
        actions.invoke_scoped("telegram-main","project:gastos","task_db",json!({"ops":[{"op":"update_requirement","id":"R4","expected_version":1,"data":{"text":"Export CSV, not PDF"}}]}),"r4-correction").await.unwrap();
        for affected in [&lena,&implementer] {let messages=host.orchestrator.mailbox(&affected.id).await.unwrap();assert!(messages.iter().any(|message|message.kind=="directive"&&message.payload["literal"].as_str().unwrap().contains("Export CSV, not PDF")));}
        for unaffected in [&third,&thesis_parent,&researcher_one,&researcher_two,&conversion] {assert!(host.orchestrator.mailbox(&unaffected.id).await.unwrap().is_empty());}
        let (requirements,_)=tasks.requirements_and_criteria(&lena.task).await.unwrap();assert!(requirements.iter().any(|requirement|requirement.id==export_requirement&&requirement.text=="Export CSV, not PDF"));
        host.master.receive(request("natural-pause","Detén la investigación de la tesis.")).await.unwrap();assert_eq!(model.master_calls.load(Ordering::SeqCst),0);
        for paused in [&thesis_parent,&researcher_one,&researcher_two] {assert_eq!(host.orchestrator.status(&paused.id).await.unwrap().state,"paused");}
        for unaffected in [&lena,&implementer,&third,&conversion] {assert_eq!(host.orchestrator.status(&unaffected.id).await.unwrap().state,"running");}
        let waiting=host.orchestrator.engine().load(gastos_parent.run_id.as_ref().unwrap()).await.unwrap();assert_eq!(waiting.status,Status::Waiting);assert_eq!(waiting.model_calls,0);
        actions.detach();
    });
    runtimes.block_on(host.close()).unwrap();
    runtimes.shutdown(Duration::from_secs(3));
}
