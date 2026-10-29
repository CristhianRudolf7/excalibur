use excalibur_eval::{ExecOptions, Execution};
use excalibur_models::{ModelError, ModelGateway};
use excalibur_types::runtime::*;
use excalibur_vault::Redactor;
use serde_json::json;
use std::sync::{Arc, Mutex};
struct Script(Mutex<Vec<ModelResponse>>);
impl ModelGateway for Script {
    fn complete<'a>(
        &'a self,
        _request: ModelRequest,
    ) -> BoxFuture<'a, Result<ModelResponse, ModelError>> {
        Box::pin(async move {
            let mut answers = self.0.lock().unwrap();
            if answers.is_empty() {
                return Err(ModelError::Validation("script exhausted".into()));
            }
            Ok(answers.remove(0))
        })
    }
}
fn response(call: bool) -> ModelResponse {
    ModelResponse {
        message: if call {
            Message {
                role: "assistant".into(),
                content: String::new(),
                tool_calls: vec![ToolCall {
                    id: "check-proof".into(),
                    name: "task_db".into(),
                    arguments: json!({"ops":[{"op":"run_check","check":"proof"}]}).to_string(),
                }],
                tool_call_id: None,
            }
        } else {
            Message::text("assistant", "The requested proof is ready.")
        },
        usage: Usage {
            input_tokens: 9,
            output_tokens: 5,
            ..Default::default()
        },
    }
}
async fn setup(pass: bool) -> (tempfile::TempDir, Execution) {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("work");
    std::fs::create_dir(&root).unwrap();
    std::fs::write(
        root.join("answer.txt"),
        if pass {
            "synthetic expected"
        } else {
            "synthetic incomplete"
        },
    )
    .unwrap();
    let execution = Execution::prepare(
        ExecOptions::new(
            "Produce the expected answer file",
            &root,
            temp.path().join("logs"),
        ),
        Arc::new(Script(Mutex::new(
            std::iter::once(response(true))
                .chain((0..8).map(|_| response(false)))
                .collect(),
        ))),
        Redactor::default(),
    )
    .await
    .unwrap();
    execution.tasks.batch("episode","declare-proof",json!({"ops":[
        {"op":"create_requirement","id":"file-requirement","data":{"text":"answer.txt contains expected","state":"accepted"}},
        {"op":"create_criterion","id":"file-criterion","data":{"requirement_id":"file-requirement","statement":"The file contains expected","method":"test","expected":"expected","impact_area":["answer.txt"]}},
        {"op":"create_check","id":"proof","data":{"criterion_id":"file-criterion","definition":{"kind":"file_contains","path":"answer.txt","needle":"expected"}}},
        {"op":"link_requirement","task":"episode-task","requirement":"file-requirement"}
    ]})).await.unwrap();
    (temp, execution)
}
#[tokio::test]
async fn noninteractive_task_db_produces_real_evidence_and_a_projected_checklist() {
    let (temp, execution) = setup(true).await;
    let outcome = execution.run(None).await.unwrap();
    assert_eq!(outcome.status, "completed", "{outcome:?}");
    let readme = std::fs::read_to_string(temp.path().join("work/tests/README.md")).unwrap();
    assert!(
        readme.contains("[x]") && readme.contains("proof"),
        "{readme}"
    );
    // Backend records, rather than the README, remain the source of truth.
    let database = excalibur_store::Database::open(
        temp.path().join("logs/excalibur/main.db"),
        excalibur_store::DatabaseKind::Main,
        excalibur_store::StorageConfig::default(),
    )
    .unwrap();
    let task = excalibur_tasks::TaskStore::open(database.clone())
        .await
        .unwrap();
    let evidence = task.task_evidence("episode-task").await.unwrap();
    assert!(evidence.iter().any(|e| e.check_id == "proof"
        && e.trusted
        && e.output_blob.is_some()
        && e.result == excalibur_tasks::evidence::EvidenceResult::Pass));
    assert_eq!(
        task.task("episode", "episode-task")
            .await
            .unwrap()
            .derived_state,
        "completed"
    );
    database.shutdown().await.unwrap();
}
#[tokio::test]
async fn a_final_answer_cannot_override_a_failed_file_check() {
    let (_temp, execution) = setup(false).await;
    let outcome = execution.run(None).await.unwrap();
    assert!(
        matches!(
            outcome.status.as_str(),
            "completed_with_pending" | "blocked"
        ),
        "{outcome:?}"
    );
    assert!(
        outcome
            .reason
            .as_deref()
            .is_some_and(|r| r.contains("file-criterion") || r.contains("proof")),
        "{outcome:?}"
    );
}

struct ReviewingModel {
    verifier_requests: std::sync::atomic::AtomicUsize,
}
impl ModelGateway for ReviewingModel {
    fn complete<'a>(
        &'a self,
        request: ModelRequest,
    ) -> BoxFuture<'a, Result<ModelResponse, ModelError>> {
        Box::pin(async move {
            if request
                .messages
                .first()
                .is_some_and(|m| m.content.contains("You are the independent task verifier"))
            {
                self.verifier_requests
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                assert!(
                    !request.messages.iter().any(|m| m.role == "assistant"
                        && m.content.contains("private-executor-reasoning"))
                );
                let payload: serde_json::Value =
                    serde_json::from_str(&request.messages[1].content).unwrap();
                let criteria=payload["request"]["criteria"].as_array().unwrap().iter().map(|criterion| {
                    let evidence=payload["independent_evidence"].as_array().unwrap().iter().filter(|e|e["criterion_id"]==criterion["id"]).map(|e|e["id"].clone()).collect::<Vec<_>>();
                    json!({"criterion_id":criterion["id"],"passed":true,"explanation":"Independent declared checks pass","evidence_ids":evidence})
                }).collect::<Vec<_>>();
                return Ok(ModelResponse {
                    message: Message::text(
                        "assistant",
                        json!({"criteria":criteria,"uncovered_requests":[],"request_covered":true})
                            .to_string(),
                    ),
                    usage: Usage {
                        input_tokens: 11,
                        output_tokens: 7,
                        ..Default::default()
                    },
                });
            }
            Ok(ModelResponse {
                message: Message::text("assistant", "All five declared file checks are ready."),
                usage: Usage {
                    input_tokens: 9,
                    output_tokens: 5,
                    ..Default::default()
                },
            })
        })
    }
}
#[tokio::test]
async fn five_requirement_task_launches_a_read_only_verifier_and_charges_its_budget() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("work");
    std::fs::create_dir(&root).unwrap();
    std::fs::write(root.join("answer.txt"), "expected").unwrap();
    let model = Arc::new(ReviewingModel {
        verifier_requests: std::sync::atomic::AtomicUsize::new(0),
    });
    let mut options = ExecOptions::new(
        "Check five observable file requirements",
        &root,
        temp.path().join("logs"),
    );
    options.autonomy = excalibur_security::Autonomy::Full;
    let execution = Execution::prepare(options, model.clone(), Redactor::default())
        .await
        .unwrap();
    let current = execution
        .tasks
        .task("episode", "episode-task")
        .await
        .unwrap();
    let mut ops = vec![
        json!({"op":"update_task","id":"episode-task","expected_version":current.version,"data":{"level":"N2"}}),
    ];
    for n in 0..4 {
        ops.extend([
        json!({"op":"create_requirement","id":format!("r{n}"),"data":{"text":format!("File requirement {n}"),"state":"accepted"}}),
        json!({"op":"create_criterion","id":format!("c{n}"),"data":{"requirement_id":format!("r{n}"),"statement":"File contains expected","method":"test","impact_area":["answer.txt"]}}),
        json!({"op":"create_check","id":format!("k{n}"),"data":{"criterion_id":format!("c{n}"),"definition":if n==3 {json!({"kind":"test","command":"test -r answer.txt","timeout_ms":10000,"expected_exit":0})} else {json!({"kind":"file_contains","path":"answer.txt","needle":"expected"})}}}),
        json!({"op":"link_requirement","task":"episode-task","requirement":format!("r{n}")})
    ]);
    }
    execution
        .tasks
        .batch("episode", "large-contract", json!({"ops":ops}))
        .await
        .unwrap();
    let outcome = execution.run(None).await.unwrap();
    assert_eq!(outcome.status, "completed", "{outcome:?}");
    assert_eq!(
        model
            .verifier_requests
            .load(std::sync::atomic::Ordering::SeqCst),
        1
    );
    let db = excalibur_store::Database::open(
        temp.path().join("logs/excalibur/main.db"),
        excalibur_store::DatabaseKind::Main,
        excalibur_store::StorageConfig::default(),
    )
    .unwrap();
    let journal = excalibur_journal::Journal::new(db.clone()).await.unwrap();
    let events = journal.replay(&outcome.run_id, 0).await.unwrap();
    assert!(
        events
            .iter()
            .any(|event| event.kind == "child_budget_settled"),
        "{events:?}"
    );
    db.shutdown().await.unwrap();
}

struct WaitingReviewer {
    started: tokio::sync::Notify,
    dropped: std::sync::atomic::AtomicBool,
}
struct Dropped<'a>(&'a std::sync::atomic::AtomicBool);
impl Drop for Dropped<'_> {
    fn drop(&mut self) {
        self.0.store(true, std::sync::atomic::Ordering::SeqCst);
    }
}
impl ModelGateway for WaitingReviewer {
    fn complete<'a>(
        &'a self,
        request: ModelRequest,
    ) -> BoxFuture<'a, Result<ModelResponse, ModelError>> {
        Box::pin(async move {
            if request
                .messages
                .first()
                .is_some_and(|m| m.content.contains("You are the independent task verifier"))
            {
                let _dropped = Dropped(&self.dropped);
                self.started.notify_one();
                std::future::pending::<()>().await;
            }
            Ok(response(false))
        })
    }
}
#[tokio::test]
async fn pausing_a_parent_interrupts_its_inflight_independent_model_call() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("work");
    std::fs::create_dir(&root).unwrap();
    let model = Arc::new(WaitingReviewer {
        started: tokio::sync::Notify::new(),
        dropped: std::sync::atomic::AtomicBool::new(false),
    });
    let execution = Execution::prepare(
        ExecOptions::new("Return a reviewed answer", root, temp.path().join("logs")),
        model.clone(),
        Redactor::default(),
    )
    .await
    .unwrap();
    let task = execution
        .tasks
        .task("episode", "episode-task")
        .await
        .unwrap();
    execution.tasks.host_batch("episode","request-review",json!({"ops":[{"op":"update_task","id":task.id,"expected_version":task.version,"data":{"metadata":{"user_requested_verification":true}}}]})).await.unwrap();
    let engine = execution.engine.clone();
    let run = execution.run_id.clone();
    let worker = tokio::spawn(execution.run(None));
    tokio::time::timeout(std::time::Duration::from_secs(20), model.started.notified())
        .await
        .expect("verifier starts");
    engine.pause(&run).await.unwrap();
    let outcome = tokio::time::timeout(std::time::Duration::from_secs(20), worker)
        .await
        .expect("controlled verifier returns")
        .unwrap()
        .unwrap();
    assert_eq!(outcome.status, "paused", "{outcome:?}");
    assert!(model.dropped.load(std::sync::atomic::Ordering::SeqCst));
    drop(engine);
    let db = excalibur_store::Database::open(
        temp.path().join("logs/excalibur/main.db"),
        excalibur_store::DatabaseKind::Main,
        excalibur_store::StorageConfig::default(),
    )
    .unwrap();
    let parent = run.clone();
    let states=db.read(excalibur_store::ReadLane::Interactive,move|c|{let mut s=c.prepare("SELECT r.state FROM runs r JOIN task_run_links l ON l.run_id=r.id WHERE l.parent_run_id=?1")?;Ok(s.query_map([parent],|r|r.get::<_,String>(0))?.collect::<Result<Vec<_>,_>>()?)}).await.unwrap();
    assert_eq!(
        states,
        vec!["paused"],
        "actual verifier child must share the parent's pause"
    );
    db.shutdown().await.unwrap();
}
