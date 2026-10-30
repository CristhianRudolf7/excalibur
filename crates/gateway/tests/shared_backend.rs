use excalibur_engine::Engine;
use excalibur_gateway::{RpcBackend, SharedBackend};
use excalibur_journal::Journal;
use excalibur_master::{Master, MasterConfig, NoActions};
use excalibur_models::{ModelError, ModelGateway};
use excalibur_orchestrator::{Limits, Orchestrator, Project};
use excalibur_store::{Database, DatabaseKind, StorageConfig};
use excalibur_types::runtime::*;
use serde_json::{Value, json};
use std::sync::Arc;
struct Model;
impl ModelGateway for Model {
    fn complete(&self, _: ModelRequest) -> BoxFuture<'_, Result<ModelResponse, ModelError>> {
        Box::pin(async {
            Ok(ModelResponse {
                message: Message::text("assistant", "synthetic reply"),
                usage: Usage::default(),
            })
        })
    }
}
struct NoTools;
impl ToolRuntime for NoTools {
    fn definitions(&self) -> Vec<ToolDefinition> {
        vec![]
    }
    fn prepare<'a>(
        &'a self,
        _: &'a str,
        _: ToolCall,
    ) -> BoxFuture<'a, Result<PreparedTool, String>> {
        Box::pin(async { Err("disabled".into()) })
    }
    fn execute<'a>(
        &'a self,
        _: &'a str,
        _: &'a str,
        _: &'a PreparedTool,
    ) -> BoxFuture<'a, ToolResult> {
        Box::pin(async { ToolResult::error("disabled") })
    }
    fn reconcile<'a>(
        &'a self,
        _: &'a str,
        _: &'a str,
        _: &'a PreparedTool,
    ) -> BoxFuture<'a, Reconcile> {
        Box::pin(async { Reconcile::Uncertain("disabled".into()) })
    }
}
async fn fixture() -> (
    tempfile::TempDir,
    Database,
    Master,
    Orchestrator,
    SharedBackend,
) {
    let temp = tempfile::tempdir().unwrap();
    let project = temp.path().join("project");
    std::fs::create_dir(&project).unwrap();
    let db = Database::open(
        temp.path().join("main.db"),
        DatabaseKind::Main,
        StorageConfig::default(),
    )
    .unwrap();
    let model = Arc::new(Model);
    let master = Master::open(
        db.clone(),
        MasterConfig::default(),
        model.clone(),
        None,
        Arc::new(NoActions),
        excalibur_vault::Redactor::default(),
    )
    .await
    .unwrap();
    let engine = Arc::new(Engine::new(
        Journal::new(db.clone()).await.unwrap(),
        model,
        Arc::new(NoTools),
    ));
    let orchestrator = Orchestrator::open(db.clone(), engine, Limits::default())
        .await
        .unwrap();
    let backend = SharedBackend::new(master.clone(), orchestrator.clone())
        .with_project_roots(vec![project])
        .unwrap();
    (temp, db, master, orchestrator, backend)
}
#[tokio::test]
async fn actual_intake_is_idempotent_and_replay_and_scopes_survive_service_reopen() {
    let (_temp, db, master, orchestrator, backend) = fixture().await;
    let incoming = json!({"conversation":"conversation-a","client_id":"client","client_msg_id":"m1","text":"hello"});
    let first = backend
        .dispatch("conversation.send", incoming.clone())
        .await
        .unwrap();
    let second = backend
        .dispatch("conversation.send", incoming.clone())
        .await
        .unwrap();
    assert_eq!(first["message_id"], second["message_id"]);
    assert_eq!(second["duplicate"], true);
    master.shutdown().await;
    let model = Arc::new(Model);
    let reopened = Master::open(
        db.clone(),
        MasterConfig::default(),
        model,
        None,
        Arc::new(NoActions),
        excalibur_vault::Redactor::default(),
    )
    .await
    .unwrap();
    let backend = SharedBackend::new(reopened.clone(), orchestrator);
    let third = backend
        .dispatch("conversation.send", incoming)
        .await
        .unwrap();
    assert_eq!(first["message_id"], third["message_id"]);
    assert_eq!(third["duplicate"], true);
    let events = backend.events(0, 100).await.unwrap();
    let cursors = events
        .iter()
        .map(|event| event["cursor"].as_u64().unwrap())
        .collect::<Vec<_>>();
    assert!(cursors.windows(2).all(|v| v[0] < v[1]));
    let cursor = first["cursor"].as_u64().unwrap();
    assert!(
        backend
            .events(cursor, 100)
            .await
            .unwrap()
            .iter()
            .all(|event| event["cursor"].as_u64().unwrap() > cursor)
    );
    assert!(
        backend
            .dispatch(
                "conversation.scope",
                json!({"conversation":"conversation-a","scope":"project:missing"})
            )
            .await
            .is_err()
    );
    reopened.shutdown().await;
    db.shutdown().await.unwrap();
}
#[tokio::test]
async fn project_listing_is_lazy_paged_ignored_filtered_and_bounded_by_host_authority() {
    let (temp, db, master, orchestrator, backend) = fixture().await;
    let root = temp.path().join("project");
    std::fs::write(root.join(".gitignore"), "ignored.txt\n").unwrap();
    std::fs::write(root.join("ignored.txt"), "ignored").unwrap();
    std::fs::write(root.join("a.txt"), "a").unwrap();
    std::fs::write(root.join("b.txt"), "b").unwrap();
    std::fs::write(root.join(".env"), "private").unwrap();
    std::fs::create_dir(root.join("node_modules")).unwrap();
    for i in 0..500 {
        std::fs::write(root.join("node_modules").join(format!("{i}.js")), "x").unwrap();
    }
    let project = Project {
        id: "p".into(),
        name: "project".into(),
        aliases: vec![],
        description: String::new(),
        roots: vec![root.clone()],
        status: "active".into(),
        vcs: None,
        settings: Value::Null,
    };
    orchestrator.register_project(project).await.unwrap();
    let first = backend
        .dispatch("files.list", json!({"project":"p","limit":1}))
        .await
        .unwrap();
    assert_eq!(first["entries"].as_array().unwrap().len(), 1);
    assert_eq!(first["has_more"], true);
    let all = backend
        .dispatch("files.list", json!({"project":"p"}))
        .await
        .unwrap();
    let rows = all["entries"].as_array().unwrap();
    assert!(
        rows.iter()
            .all(|row| row["name"] != "ignored.txt" && row["name"] != ".env")
    );
    assert!(
        rows.iter()
            .any(|row| row["name"] == "node_modules" && row["collapsed"] == true)
    );
    let next = backend
        .dispatch(
            "files.list",
            json!({"project":"p","after":first["next"],"limit":1}),
        )
        .await
        .unwrap();
    assert_ne!(first["entries"][0]["name"], next["entries"][0]["name"]);
    db.write(excalibur_store::Priority::Normal,{let path=root.join("b.txt").to_string_lossy().to_string();move|c|{c.execute("INSERT INTO file_versions(id,path,run_id,tool_call_id) VALUES('version',?1,NULL,'call')",[path])?;Ok(())}}).await.unwrap();
    let filtered = backend
        .dispatch("files.list", json!({"project":"p","agent_only":true}))
        .await
        .unwrap();
    assert_eq!(filtered["entries"].as_array().unwrap().len(), 1);
    assert_eq!(filtered["entries"][0]["name"], "b.txt");
    assert!(
        backend
            .dispatch("files.list", json!({"project":"p","path":".."}))
            .await
            .is_err()
    );
    let mut outside = serde_json::to_value(orchestrator.project("p").await.unwrap()).unwrap();
    outside["id"] = json!("outside");
    outside["roots"] = json!([temp.path()]);
    assert!(backend.dispatch("project.register", outside).await.is_err());
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(temp.path(), root.join("escape")).unwrap();
        assert!(
            backend
                .dispatch("files.list", json!({"project":"p","path":"escape"}))
                .await
                .is_err()
        );
    }
    master.shutdown().await;
    db.shutdown().await.unwrap();
}

#[tokio::test]
async fn authenticated_question_answers_and_notification_preferences_use_shared_durable_services() {
    let (_temp, db, master, orchestrator, backend) = fixture().await;
    backend.dispatch("conversation.send",json!({"conversation":"questions","client_id":"client","client_msg_id":"q0","text":"hello"})).await.unwrap();
    for method in ["config.get", "config.read"] {
        assert_eq!(
            backend.dispatch(method, json!({})).await.unwrap_err().code,
            -32601
        );
    }
    assert_eq!(
        backend
            .dispatch(
                "conversation.retry_context",
                json!({"conversation":"questions"})
            )
            .await
            .unwrap()["retry_requested"],
        true
    );
    assert!(
        backend
            .dispatch(
                "conversation.retry_context",
                json!({"conversation":"unknown"})
            )
            .await
            .is_err()
    );
    let question = master
        .ask(
            "questions",
            "Which format?",
            true,
            Some("csv".into()),
            excalibur_store::unix_seconds(),
        )
        .await
        .unwrap();
    let params = json!({"conversation":"questions","question":question,"answer":"json"});
    assert_eq!(
        backend
            .dispatch("question.answer", params.clone())
            .await
            .unwrap()["answered"],
        true
    );
    backend.dispatch("question.answer", params).await.unwrap();
    assert!(
        backend
            .dispatch(
                "question.answer",
                json!({"conversation":"questions","question":question,"answer":"xml"})
            )
            .await
            .is_err()
    );
    let questions = backend
        .dispatch("question.list", json!({"conversation":"questions"}))
        .await
        .unwrap();
    assert!(questions.as_array().is_some());
    backend
        .dispatch(
            "conversation.activity",
            json!({"conversation":"questions","channel":"websocket","connected":true}),
        )
        .await
        .unwrap();
    backend.dispatch("notification.policy",json!({"conversation":"questions","policy":{"preferred_channel":"fixture","progress":true,"quiet_start_minute":60,"quiet_end_minute":120,"utc_offset_minutes":0,"urgent_during_quiet":true}})).await.unwrap();
    assert!(backend.dispatch("notification.policy",json!({"conversation":"questions","policy":{"preferred_channel":null,"progress":true,"quiet_start_minute":1440,"quiet_end_minute":120,"utc_offset_minutes":0,"urgent_during_quiet":true}})).await.is_err());
    assert!(
        backend
            .dispatch(
                "approval.respond",
                json!({"approval":"absent","approved":true,"operation":"authenticated"})
            )
            .await
            .is_err()
    );
    assert!(
        orchestrator
            .notifications("questions")
            .await
            .unwrap()
            .is_empty()
    );
    master.shutdown().await;
    db.shutdown().await.unwrap();
}

#[tokio::test]
async fn approval_response_wakes_only_its_waiting_run_and_never_reopens_user_pause() {
    use excalibur_engine::{Budget, Criterion, RunSpec, Status, WaitReason};
    use excalibur_security::{Autonomy, CapabilityToken, Root, RunSecurity, Security};
    let (temp, db, master, orchestrator, backend) = fixture().await;
    let root = temp.path().join("project").canonicalize().unwrap();
    let ceiling = CapabilityToken {
        roots: vec![Root {
            path: root.clone(),
            write: true,
        }],
        data_scopes: ["general".into()].into(),
        budget: 100,
        ..Default::default()
    };
    let security = Security::new(
        db.clone(),
        ceiling.clone(),
        excalibur_vault::Redactor::default(),
    )
    .await
    .unwrap();
    let engine = orchestrator.engine();
    let run = engine
        .create(
            "approval-run",
            RunSpec {
                context: excalibur_context::ContextInput::new("rules", "task", "fixed"),
                budget: Budget::default(),
                criteria: vec![Criterion::FinalResponse { schema: None }],
            },
        )
        .await
        .unwrap();
    let base = CapabilityToken {
        budget: 10,
        ..ceiling.clone()
    };
    let now = excalibur_store::unix_seconds();
    security
        .register_run(
            &run,
            RunSecurity {
                parent: None,
                project: "p".into(),
                workspace: root,
                token: base,
                autonomy: vec![Autonomy::Supervised],
                full_acknowledged: false,
                user_requested: true,
            },
            now,
        )
        .await
        .unwrap();
    let grant = CapabilityToken {
        budget: 20,
        ..ceiling.clone()
    };
    let approval = security
        .request_grant(&run, &grant, now + 1000, "temporary budget grant", now)
        .await
        .unwrap();
    engine
        .journal()
        .set_state_if(&run, &["ready"], "running", None, "fixture-admission")
        .await
        .unwrap();
    engine.wait(&run, WaitReason::Approval).await.unwrap();
    let backend = backend.with_security(security.clone());
    security.redactor().register_secret(
        &excalibur_vault::SecretValue::new(b"temporary budget grant".to_vec()).unwrap(),
    );
    let listed = backend
        .dispatch("approval.list", json!({"limit":1}))
        .await
        .unwrap();
    assert_eq!(listed["approvals"].as_array().unwrap().len(), 1);
    assert_eq!(listed["approvals"][0]["id"], approval);
    assert_eq!(listed["approvals"][0]["run_id"], run);
    assert_eq!(listed["approvals"][0]["run_state"], "waiting");
    assert_eq!(
        listed["approvals"][0]["request"]["action"]["External"]["kind"],
        "capability_grant"
    );
    assert!(!listed.to_string().contains("temporary budget grant"));
    assert_eq!(
        security.effective_token(&run, now).await.unwrap().budget,
        10
    );
    assert_eq!(engine.load(&run).await.unwrap().status, Status::Waiting);
    assert!(
        backend
            .dispatch("approval.list", json!({"limit":101}))
            .await
            .is_err()
    );
    assert!(
        backend
            .dispatch("approval.list", json!({"limit":0}))
            .await
            .is_err()
    );
    let response =
        json!({"approval":approval,"approved":true,"operation":"authenticated-response"});
    assert_eq!(
        backend
            .dispatch("approval.respond", response.clone())
            .await
            .unwrap(),
        true
    );
    assert_eq!(engine.load(&run).await.unwrap().status, Status::Running);
    assert_eq!(
        backend
            .dispatch("approval.respond", response)
            .await
            .unwrap(),
        false
    );
    let other = security
        .request_grant(
            &run,
            &CapabilityToken {
                budget: 30,
                ..ceiling.clone()
            },
            now + 1000,
            "different temporary budget",
            now,
        )
        .await
        .unwrap();
    engine.pause(&run).await.unwrap();
    let extra = security
        .request_grant(
            &run,
            &CapabilityToken {
                budget: 40,
                ..ceiling
            },
            now + 1000,
            "second paused request",
            now,
        )
        .await
        .unwrap();
    let first_page = backend
        .dispatch("approval.list", json!({"limit":1}))
        .await
        .unwrap();
    assert_eq!(first_page["has_more"], true);
    let second_page = backend
        .dispatch(
            "approval.list",
            json!({"limit":1,"after":first_page["next"]}),
        )
        .await
        .unwrap();
    assert_eq!(second_page["has_more"], false);
    let actual = [
        first_page["approvals"][0]["id"].as_str().unwrap(),
        second_page["approvals"][0]["id"].as_str().unwrap(),
    ]
    .into_iter()
    .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(
        actual,
        [other.as_str(), extra.as_str()].into_iter().collect()
    );
    let paused_listing = backend.dispatch("approval.list", json!({})).await.unwrap();
    assert!(
        paused_listing["approvals"]
            .as_array()
            .unwrap()
            .iter()
            .any(|approval| approval["id"] == other)
    );
    assert_eq!(paused_listing["approvals"][0]["run_state"], "paused");
    assert_eq!(engine.load(&run).await.unwrap().status, Status::Paused);
    backend
        .dispatch(
            "approval.respond",
            json!({"approval":other,"approved":true,"operation":"later-response"}),
        )
        .await
        .unwrap();
    assert_eq!(engine.load(&run).await.unwrap().status, Status::Paused);
    master.shutdown().await;
    db.shutdown().await.unwrap();
}

#[tokio::test]
async fn unknown_origin_conversation_is_rejected_before_any_delegation_intent() {
    let (_temp, db, master, orchestrator, backend) = fixture().await;
    let token = excalibur_security::CapabilityToken {
        data_scopes: ["general".into()].into(),
        budget: 10,
        ..Default::default()
    };
    let parent = orchestrator
        .register_master("Master", token.clone())
        .await
        .unwrap();
    let delegation = excalibur_orchestrator::Delegation {
        operation: "unknown-origin".into(),
        name: "Worker".into(),
        profile: "coordinator".into(),
        scope: "general".into(),
        task: "task".into(),
        origin_conversation: "missing-conversation".into(),
        goal: "Synthetic work".into(),
        deliverable: "Synthetic result".into(),
        inputs: vec![],
        pinned_instructions: vec![],
        suggested_skills: vec![],
        fork_context: false,
        token,
        spec: excalibur_engine::RunSpec {
            context: excalibur_context::ContextInput::new("rules", "task", "fixed"),
            budget: Default::default(),
            criteria: vec![excalibur_engine::Criterion::FinalResponse { schema: None }],
        },
        priority: 1,
    };
    let error = backend
        .dispatch(
            "run.spawn",
            json!({"parent":parent.id,"delegation":delegation}),
        )
        .await
        .unwrap_err();
    assert_eq!(error.message, "origin conversation is unavailable");
    let intents = db
        .read(excalibur_store::ReadLane::Interactive, |connection| {
            Ok(
                connection.query_row("SELECT count(*) FROM orchestration_spawn", [], |row| {
                    row.get::<_, u64>(0)
                })?,
            )
        })
        .await
        .unwrap();
    assert_eq!(intents, 0);
    backend.dispatch("conversation.send",json!({"conversation":"known-origin","client_id":"fixture","client_msg_id":"origin","text":"Synthetic origin"})).await.unwrap();
    let mut valid = delegation;
    valid.origin_conversation = "known-origin".into();
    let created = backend
        .dispatch("run.spawn", json!({"parent":parent.id,"delegation":valid}))
        .await
        .unwrap();
    assert_eq!(
        backend
            .dispatch("metrics.query", json!({"target":created["id"]}))
            .await
            .unwrap_err()
            .code,
        -32601
    );
    master.shutdown().await;
    db.shutdown().await.unwrap();
}

struct ApprovalModel {
    calls: std::sync::atomic::AtomicUsize,
}
impl ModelGateway for ApprovalModel {
    fn complete(&self, request: ModelRequest) -> BoxFuture<'_, Result<ModelResponse, ModelError>> {
        self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let done = request
            .messages
            .iter()
            .any(|message| message.role == "tool");
        Box::pin(async move {
            Ok(ModelResponse {
                message: if done {
                    Message::text("assistant", "Wrote the approved file.")
                } else {
                    Message {
                        role: "assistant".into(),
                        content: String::new(),
                        tool_calls: vec![ToolCall {
                            id: "approval-write".into(),
                            name: "write_file".into(),
                            arguments: json!({"path":"approved.txt","content":"approved once"})
                                .to_string(),
                        }],
                        tool_call_id: None,
                        provider_state: None,
                    }
                },
                usage: Usage::default(),
            })
        })
    }
}
struct ApprovalWrite {
    root: std::path::PathBuf,
    executions: std::sync::atomic::AtomicUsize,
}
impl ToolRuntime for ApprovalWrite {
    fn definitions(&self) -> Vec<ToolDefinition> {
        vec![ToolDefinition {
            name: "write_file".into(),
            description: "Write a single fixture file".into(),
            input_schema: json!({"type":"object","properties":{"path":{"const":"approved.txt"},"content":{"const":"approved once"}},"required":["path","content"],"additionalProperties":false}),
        }]
    }
    fn prepare<'a>(
        &'a self,
        _: &'a str,
        call: ToolCall,
    ) -> BoxFuture<'a, Result<PreparedTool, String>> {
        Box::pin(async move {
            let args: Value =
                serde_json::from_str(&call.arguments).map_err(|_| "invalid fixture arguments")?;
            if call.name != "write_file"
                || args["path"] != "approved.txt"
                || args["content"] != "approved once"
            {
                return Err("fixture target differs from approved operation".into());
            }
            Ok(PreparedTool {
                call,
                effect: Effect::Keyed,
                resources: vec![Resource {
                    key: self.root.join("approved.txt").to_string_lossy().to_string(),
                    write: true,
                }],
                exclusive: false,
                data: args,
            })
        })
    }
    fn execute<'a>(
        &'a self,
        _: &'a str,
        _: &'a str,
        tool: &'a PreparedTool,
    ) -> BoxFuture<'a, ToolResult> {
        Box::pin(async move {
            self.executions
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if tool.data["path"] != "approved.txt" || tool.data["content"] != "approved once" {
                return ToolResult::error("fixture target changed");
            }
            match std::fs::write(self.root.join("approved.txt"), b"approved once") {
                Ok(()) => ToolResult::ok(json!({"written":true})),
                Err(_) => ToolResult::error("fixture write failed"),
            }
        })
    }
    fn reconcile<'a>(
        &'a self,
        _: &'a str,
        _: &'a str,
        _: &'a PreparedTool,
    ) -> BoxFuture<'a, Reconcile> {
        Box::pin(async move {
            match std::fs::read(self.root.join("approved.txt")) {
                Ok(bytes) if bytes == b"approved once" => {
                    Reconcile::Done(ToolResult::ok(json!({"written":true})))
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Reconcile::Retry,
                _ => Reconcile::Uncertain("fixture file differs".into()),
            }
        })
    }
}
struct ApprovalFixture {
    _temp: tempfile::TempDir,
    db: Database,
    master: Master,
    engine: Arc<Engine>,
    backend: SharedBackend,
    security: excalibur_security::Security,
    model: Arc<ApprovalModel>,
    tools: Arc<ApprovalWrite>,
    run: String,
    approval: String,
}
async fn actual_approval_pause() -> ApprovalFixture {
    use excalibur_security::{
        Autonomy, CapabilityToken, Root, Rule, RuleEffect, RunSecurity, SecuredTools, Security,
    };
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("project");
    std::fs::create_dir(&root).unwrap();
    let db = Database::open(
        temp.path().join("main.db"),
        DatabaseKind::Main,
        StorageConfig::default(),
    )
    .unwrap();
    let token = CapabilityToken {
        roots: vec![Root {
            path: root.canonicalize().unwrap(),
            write: true,
        }],
        data_scopes: ["general".into()].into(),
        budget: 100,
        ..Default::default()
    };
    let security = Security::new(
        db.clone(),
        token.clone(),
        excalibur_vault::Redactor::default(),
    )
    .await
    .unwrap();
    let tools = Arc::new(ApprovalWrite {
        root: root.clone(),
        executions: Default::default(),
    });
    let model = Arc::new(ApprovalModel {
        calls: Default::default(),
    });
    let secured: Arc<dyn ToolRuntime> =
        Arc::new(SecuredTools::new(tools.clone(), security.clone()));
    let engine = Arc::new(Engine::new(
        Journal::new(db.clone()).await.unwrap(),
        model.clone(),
        secured,
    ));
    let run = engine
        .create(
            "actual-approval-pause",
            excalibur_engine::RunSpec {
                context: excalibur_context::ContextInput::new(
                    "Never bypass approval.",
                    "Write approved.txt only after authorization.",
                    "fixed",
                ),
                budget: Default::default(),
                criteria: vec![excalibur_engine::Criterion::FinalResponse { schema: None }],
            },
        )
        .await
        .unwrap();
    let now = excalibur_store::unix_seconds();
    security
        .register_run(
            &run,
            RunSecurity {
                parent: None,
                project: "fixture".into(),
                workspace: root,
                token,
                autonomy: vec![Autonomy::Supervised],
                full_acknowledged: false,
                user_requested: true,
            },
            now,
        )
        .await
        .unwrap();
    security
        .add_rule(
            Rule {
                id: "ask-write".into(),
                action: "write".into(),
                pattern: "**".into(),
                effect: RuleEffect::Ask,
                run: None,
                project: None,
                expires_at: None,
            },
            true,
            now,
        )
        .await
        .unwrap();
    let paused = engine.run(&run).await.unwrap();
    assert_eq!(paused.status, excalibur_engine::Status::Paused);
    assert_eq!(paused.reason.as_deref(), Some("approval required"));
    assert_eq!(
        tools.executions.load(std::sync::atomic::Ordering::SeqCst),
        0
    );
    assert_eq!(model.calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    let run_copy = run.clone();
    let approval = db
        .read(excalibur_store::ReadLane::Interactive, move |connection| {
            Ok(connection.query_row(
                "SELECT id FROM security_approvals WHERE run_id=?1 AND status='pending'",
                [run_copy],
                |row| row.get::<_, String>(0),
            )?)
        })
        .await
        .unwrap();
    let master = Master::open(
        db.clone(),
        MasterConfig::default(),
        Arc::new(Model),
        None,
        Arc::new(NoActions),
        excalibur_vault::Redactor::default(),
    )
    .await
    .unwrap();
    let orchestrator = Orchestrator::open(db.clone(), engine.clone(), Limits::default())
        .await
        .unwrap();
    let backend = SharedBackend::new(master.clone(), orchestrator).with_security(security.clone());
    ApprovalFixture {
        _temp: temp,
        db,
        master,
        engine,
        backend,
        security,
        model,
        tools,
        run,
        approval,
    }
}
#[tokio::test]
async fn tool_produced_system_approval_pause_resumes_the_exact_write_once_or_reblocks_denial() {
    use std::sync::atomic::Ordering;
    for approved in [true, false] {
        let fixture = actual_approval_pause().await;
        let args = json!({"approval":fixture.approval,"approved":approved,"operation":"actual-user-response"});
        assert_eq!(
            fixture
                .backend
                .dispatch("approval.respond", args.clone())
                .await
                .unwrap(),
            true
        );
        assert_eq!(
            fixture.engine.load(&fixture.run).await.unwrap().status,
            excalibur_engine::Status::Ready
        );
        let result = fixture.engine.run(&fixture.run).await.unwrap();
        if approved {
            assert_eq!(result.status, excalibur_engine::Status::Completed);
            assert_eq!(fixture.tools.executions.load(Ordering::SeqCst), 1);
            assert_eq!(
                std::fs::read(fixture.tools.root.join("approved.txt")).unwrap(),
                b"approved once"
            );
            assert_eq!(
                fixture.engine.run(&fixture.run).await.unwrap().status,
                excalibur_engine::Status::Completed
            );
            assert_eq!(fixture.tools.executions.load(Ordering::SeqCst), 1);
        } else {
            assert_eq!(result.status, excalibur_engine::Status::Blocked);
            assert_eq!(fixture.tools.executions.load(Ordering::SeqCst), 0);
            assert!(!fixture.tools.root.join("approved.txt").exists());
            assert_eq!(fixture.model.calls.load(Ordering::SeqCst), 1);
        }
        assert_eq!(
            fixture
                .backend
                .dispatch("approval.respond", args)
                .await
                .unwrap(),
            false
        );
        fixture.master.shutdown().await;
        fixture.db.shutdown().await.unwrap();
    }
}
#[tokio::test]
async fn a_new_user_pause_wins_over_an_in_flight_system_approval_response() {
    let fixture = actual_approval_pause().await;
    let gate = fixture.engine.journal().lock_run(&fixture.run).await;
    let user_pause = fixture.engine.pause(&fixture.run);
    tokio::pin!(user_pause);
    assert!(futures_util::poll!(&mut user_pause).is_pending());
    let backend = fixture.backend.clone();
    let approval = fixture.approval.clone();
    let decision = tokio::spawn(async move {
        backend
            .dispatch(
                "approval.respond",
                json!({"approval":approval,"approved":true,"operation":"racing-user-response"}),
            )
            .await
    });
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if !fixture
                .security
                .pending_approval(&fixture.run, excalibur_store::unix_seconds())
                .await
                .unwrap()
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    drop(gate);
    user_pause.await.unwrap();
    decision.await.unwrap().unwrap();
    let state = fixture.engine.load(&fixture.run).await.unwrap();
    assert_eq!(state.status, excalibur_engine::Status::Paused);
    assert_eq!(state.reason.as_deref(), Some("user"));
    assert_eq!(
        fixture
            .tools
            .executions
            .load(std::sync::atomic::Ordering::SeqCst),
        0
    );
    assert!(!fixture.tools.root.join("approved.txt").exists());
    fixture.master.shutdown().await;
    fixture.db.shutdown().await.unwrap();
}
