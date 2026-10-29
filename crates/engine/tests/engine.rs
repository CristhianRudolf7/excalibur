mod common;
use common::*;
use excalibur_engine::{Engine, Status};
use excalibur_journal::Journal;
use excalibur_models::{ModelError, ModelGateway};
use excalibur_store::{Database, DatabaseKind, StorageConfig};
use excalibur_types::runtime::*;
use serde_json::json;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};
async fn fixture() -> (tempfile::TempDir, Database, Engine) {
    let dir = tempfile::tempdir().unwrap();
    let db = Database::open(
        dir.path().join("main.db"),
        DatabaseKind::Main,
        StorageConfig::default(),
    )
    .unwrap();
    let j = Journal::new(db.clone()).await.unwrap();
    let tools = Arc::new(Effects {
        root: dir.path().into(),
        effect: Effect::Idempotent,
        journal: j.clone(),
    });
    let engine = Engine::new(j, model(dir.path(), false, 1), tools);
    (dir, db, engine)
}
#[tokio::test]
async fn concurrent_drivers_and_operation_resubmission_do_not_repeat_a_completed_run() {
    let (dir, db, engine) = fixture().await;
    let spec = spec(false, 10);
    let (a, b) = tokio::join!(
        engine.create("same", spec.clone()),
        engine.create("same", spec.clone())
    );
    let run = a.unwrap();
    assert_eq!(run, b.unwrap());
    let (a, b) = tokio::join!(engine.run(&run), engine.run(&run));
    assert_eq!(a.unwrap().status, Status::Completed);
    assert_eq!(b.unwrap().status, Status::Completed);
    assert_eq!(count(&dir.path().join("effects")), 1);
    assert_eq!(count(&dir.path().join("models")), 2);
    assert_eq!(engine.create("same", spec).await.unwrap(), run);
    let mut altered = common::spec(false, 10);
    altered.context.layers[3] = "different task".into();
    assert!(engine.create("same", altered).await.is_err());
    db.shutdown().await.unwrap();
}
struct FinalOnly;
impl ModelGateway for FinalOnly {
    fn complete<'a>(&'a self, _: ModelRequest) -> BoxFuture<'a, Result<ModelResponse, ModelError>> {
        Box::pin(async {
            Ok(ModelResponse {
                message: Message::text("assistant", "Done without evidence"),
                usage: Usage::default(),
            })
        })
    }
}
#[tokio::test]
async fn final_claim_cannot_pass_unverified_criteria_and_gate_retries_are_bounded() {
    let (_dir, db, engine) = fixture().await;
    let engine = Engine::new(
        engine.journal().clone(),
        Arc::new(FinalOnly),
        engine_tools(engine.journal()),
    );
    let run = engine.create("gate", spec(false, 10)).await.unwrap();
    let result = engine.run(&run).await.unwrap();
    assert_eq!(result.status, Status::CompletedWithPending);
    assert_eq!(result.gate_attempts, 3);
    assert_eq!(
        engine
            .journal()
            .replay(&run, 0)
            .await
            .unwrap()
            .iter()
            .filter(|e| e.kind == "model_response")
            .count(),
        3
    );
    db.shutdown().await.unwrap();
}
fn engine_tools(journal: &Journal) -> Arc<dyn ToolRuntime> {
    Arc::new(Effects {
        root: journal.database().path().parent().unwrap().into(),
        effect: Effect::Pure,
        journal: journal.clone(),
    })
}
#[tokio::test]
async fn closing_keeps_definitions_disables_tools_and_budget_pause_preserves_progress() {
    let (dir, db, engine) = fixture().await;
    let run = engine.create("closing", spec(false, 1)).await.unwrap();
    let result = engine.run(&run).await.unwrap();
    assert_eq!(result.status, Status::CompletedWithPending);
    let req = result.request.unwrap();
    assert!(!req.tools_enabled);
    assert!(!req.tools.is_empty());
    assert_eq!(count(&dir.path().join("effects")), 0);
    let mut budget = spec(false, 10);
    budget.budget.max_tokens = 15;
    let run = engine.create("budget", budget.clone()).await.unwrap();
    let result = engine.run(&run).await.unwrap();
    assert_eq!(result.status, Status::Paused);
    assert_eq!(result.usage.input_tokens + result.usage.output_tokens, 15);
    assert_eq!(count(&dir.path().join("effects")), 1);
    assert_eq!(result.step, 1);
    let mut expanded = result.spec.budget;
    expanded.max_tokens = 1000;
    engine.update_budget(&run, expanded).await.unwrap();
    engine.resume(&run).await.unwrap();
    assert_eq!(engine.run(&run).await.unwrap().status, Status::Completed);
    assert_eq!(engine.create("budget", budget).await.unwrap(), run);
    assert_eq!(count(&dir.path().join("effects")), 1);
    let mut unpriced = spec(false, 10);
    unpriced.budget.max_cost_microusd = Some(10);
    assert!(engine.create("unpriced", unpriced).await.is_err());
    db.shutdown().await.unwrap();
}

#[tokio::test]
async fn writes_invalidate_host_evidence_instead_of_accepting_stale_success() {
    let (_dir, db, engine) = fixture().await;
    let mut request = spec(false, 10);
    request.criteria = vec![excalibur_engine::Criterion::HostCheck {
        id: "verified".into(),
    }];
    let run = engine.create("freshness", request).await.unwrap();
    engine
        .record_check(
            &run,
            "verified",
            true,
            "synthetic check before the mutation",
        )
        .await
        .unwrap();
    let state = engine.run(&run).await.unwrap();
    assert_eq!(state.status, Status::CompletedWithPending);
    assert!(state.host_checks.is_empty());
    db.shutdown().await.unwrap();
}
struct NoteModel;
impl ModelGateway for NoteModel {
    fn complete<'a>(&'a self, r: ModelRequest) -> BoxFuture<'a, Result<ModelResponse, ModelError>> {
        Box::pin(async move {
            let n = r.messages.iter().filter(|m| m.role == "tool").count();
            let mut message = Message::text("assistant", "Completed after twenty effects");
            if n < 21 {
                let (name, args) = if n == 0 {
                    (
                        "leave_continuation_note",
                        json!({"note":"Continue from the exact durable state"}),
                    )
                } else {
                    assert!(
                        r.messages
                            .last()
                            .unwrap()
                            .content
                            .contains("Continue from the exact durable state")
                    );
                    ("effect", json!({}))
                };
                message.tool_calls = vec![ToolCall {
                    id: format!("c{n}"),
                    name: name.into(),
                    arguments: args.to_string(),
                }];
            }
            Ok(ModelResponse {
                message,
                usage: Usage::default(),
            })
        })
    }
}
#[tokio::test]
async fn continuation_and_checkpoint_reconstruct_exact_history_after_reopening() {
    let (dir, db, base) = fixture().await;
    let engine = Engine::new(
        base.journal().clone(),
        Arc::new(NoteModel),
        engine_tools(base.journal()),
    );
    drop(base);
    let run = engine.create("checkpoint", spec(false, 30)).await.unwrap();
    let result = engine.run(&run).await.unwrap();
    assert_eq!(result.status, Status::Completed);
    assert_eq!(result.step, 21);
    assert_eq!(result.continuation, "Continue from the exact durable state");
    assert!(
        engine
            .journal()
            .latest_checkpoint(&run)
            .await
            .unwrap()
            .is_some()
    );
    let encoded = serde_json::to_value(&result).unwrap();
    db.shutdown().await.unwrap();
    drop(engine);
    drop(db);
    let db = Database::open(
        dir.path().join("main.db"),
        DatabaseKind::Main,
        StorageConfig::default(),
    )
    .unwrap();
    let j = Journal::new(db.clone()).await.unwrap();
    let engine = Engine::new(j.clone(), Arc::new(NoteModel), engine_tools(&j));
    assert_eq!(
        serde_json::to_value(engine.run(&run).await.unwrap()).unwrap(),
        encoded
    );
    db.shutdown().await.unwrap();
}
struct BatchModel {
    calls: Mutex<usize>,
}
impl ModelGateway for BatchModel {
    fn complete<'a>(
        &'a self,
        _r: ModelRequest,
    ) -> BoxFuture<'a, Result<ModelResponse, ModelError>> {
        Box::pin(async move {
            let mut n = self.calls.lock().unwrap();
            let mut m = Message::text("assistant", "verified");
            if *n == 0 {
                m.tool_calls = (0..2)
                    .map(|i| ToolCall {
                        id: format!("c{i}"),
                        name: "probe".into(),
                        arguments: json!({"resource":i}).to_string(),
                    })
                    .collect();
            }
            *n += 1;
            Ok(ModelResponse {
                message: m,
                usage: Usage::default(),
            })
        })
    }
}
struct Parallel {
    barrier: tokio::sync::Barrier,
    entered: AtomicUsize,
    order: Mutex<Vec<String>>,
}
impl ToolRuntime for Parallel {
    fn scheduling<'a>(
        &'a self,
        run: &'a str,
        call: &'a ToolCall,
    ) -> BoxFuture<'a, Option<ToolScheduling>> {
        Box::pin(async move {
            self.prepare(run, call.clone())
                .await
                .ok()
                .map(|tool| ToolScheduling::from_prepared(&tool))
        })
    }

    fn definitions(&self) -> Vec<ToolDefinition> {
        vec![ToolDefinition {
            name: "probe".into(),
            description: "barrier-backed parallel fixture".into(),
            input_schema: json!({"type":"object"}),
        }]
    }
    fn prepare<'a>(
        &'a self,
        _run: &'a str,
        call: ToolCall,
    ) -> BoxFuture<'a, Result<PreparedTool, String>> {
        Box::pin(async move {
            let v: serde_json::Value = serde_json::from_str(&call.arguments).unwrap();
            Ok(PreparedTool {
                resources: vec![Resource {
                    key: format!("fixture:{}", v["resource"]),
                    write: true,
                }],
                call,
                effect: Effect::Pure,
                exclusive: false,
                data: json!({}),
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
            self.entered.fetch_add(1, Ordering::SeqCst);
            self.barrier.wait().await;
            self.order.lock().unwrap().push(tool.call.id.clone());
            ToolResult::ok(json!({"id":tool.call.id}))
        })
    }
    fn reconcile<'a>(
        &'a self,
        _: &'a str,
        _: &'a str,
        _: &'a PreparedTool,
    ) -> BoxFuture<'a, Reconcile> {
        Box::pin(async { Reconcile::Retry })
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn independent_resources_execute_in_parallel_but_history_retains_model_order() {
    let (_dir, db, base) = fixture().await;
    let tools = Arc::new(Parallel {
        barrier: tokio::sync::Barrier::new(2),
        entered: AtomicUsize::new(0),
        order: Mutex::new(Vec::new()),
    });
    let engine = Engine::new(
        base.journal().clone(),
        Arc::new(BatchModel {
            calls: Mutex::new(0),
        }),
        tools.clone(),
    );
    let mut spec = spec(false, 10);
    spec.criteria = vec![excalibur_engine::Criterion::ToolSuccess {
        name: "probe".into(),
    }];
    let run = engine.create("parallel", spec).await.unwrap();
    let result = tokio::time::timeout(std::time::Duration::from_secs(10), engine.run(&run))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(result.status, Status::Completed);
    assert_eq!(tools.entered.load(Ordering::SeqCst), 2);
    let ids: Vec<_> = result
        .history
        .iter()
        .filter_map(|m| m.tool_call_id.clone())
        .collect();
    assert_eq!(ids, vec!["c0", "c1"]);
    db.shutdown().await.unwrap();
}

struct OneTool(ToolCall);
impl ModelGateway for OneTool {
    fn complete<'a>(&'a self, r: ModelRequest) -> BoxFuture<'a, Result<ModelResponse, ModelError>> {
        Box::pin(async move {
            let mut message = Message::text("assistant", "Output retrieved");
            if !r.messages.iter().any(|m| m.role == "tool") {
                message.tool_calls = vec![self.0.clone()];
            }
            Ok(ModelResponse {
                message,
                usage: Usage::default(),
            })
        })
    }
}
#[tokio::test]
async fn full_output_windows_preserve_exact_bytes_and_invalid_arguments_are_rejected() {
    let (_dir, db, base) = fixture().await;
    let staged = db.blobs().prepare("éX".as_bytes()).unwrap();
    let hash = db
        .attach_blob(staged, "fixture".into(), "output".into(), "full".into())
        .await
        .unwrap();
    let call = ToolCall {
        id: "output".into(),
        name: "get_tool_output".into(),
        arguments: format!("{{hash:'{}',offset:1,limit:1,}}", hash.as_str()),
    };
    let engine = Engine::new(
        base.journal().clone(),
        Arc::new(OneTool(call)),
        engine_tools(base.journal()),
    );
    let mut request = spec(false, 10);
    request.criteria = vec![excalibur_engine::Criterion::ToolOutput {
        name: "get_tool_output".into(),
        pointer: "/bytes_base64".into(),
        expected: json!("qQ=="),
    }];
    let run = engine.create("window", request).await.unwrap();
    let state = engine.run(&run).await.unwrap();
    assert_eq!(state.status, Status::Completed);
    let result: ToolResult = serde_json::from_str(
        &state
            .history
            .iter()
            .find(|m| m.role == "tool")
            .unwrap()
            .content,
    )
    .unwrap();
    assert!(result.output["text"].is_null());
    assert_eq!(result.output["offset"], 1);
    assert_eq!(result.output["next_offset"], 2);
    let call = ToolCall {
        id: "invalid".into(),
        name: "get_tool_output".into(),
        arguments: json!({"hash":hash.as_str(),"offset":-1}).to_string(),
    };
    let engine = Engine::new(
        base.journal().clone(),
        Arc::new(OneTool(call)),
        engine_tools(base.journal()),
    );
    let mut request = spec(false, 10);
    request.criteria = vec![excalibur_engine::Criterion::ToolSuccess {
        name: "get_tool_output".into(),
    }];
    let run = engine.create("invalid-window", request).await.unwrap();
    let state = engine.run(&run).await.unwrap();
    assert_eq!(state.status, Status::CompletedWithPending);
    assert!(
        state
            .history
            .iter()
            .any(|m| m.role == "tool" && m.content.contains("nonnegative integers"))
    );
    db.shutdown().await.unwrap();
}

struct RepeatInvalid(AtomicUsize);
impl ModelGateway for RepeatInvalid {
    fn complete<'a>(&'a self, _: ModelRequest) -> BoxFuture<'a, Result<ModelResponse, ModelError>> {
        Box::pin(async move {
            let n = self.0.fetch_add(1, Ordering::SeqCst);
            let mut message = Message::text("assistant", "");
            message.tool_calls = vec![ToolCall {
                id: format!("bad{n}"),
                name: "invalid".into(),
                arguments: "{}".into(),
            }];
            Ok(ModelResponse {
                message,
                usage: Usage::default(),
            })
        })
    }
}
struct InvalidTool;
impl ToolRuntime for InvalidTool {
    fn definitions(&self) -> Vec<ToolDefinition> {
        vec![ToolDefinition {
            name: "invalid".into(),
            description: "Input rejection fixture".into(),
            input_schema: json!({"type":"object"}),
        }]
    }
    fn prepare<'a>(
        &'a self,
        _: &'a str,
        _: ToolCall,
    ) -> BoxFuture<'a, Result<PreparedTool, String>> {
        Box::pin(async { Err("synthetic rejected input".into()) })
    }
    fn execute<'a>(
        &'a self,
        _: &'a str,
        _: &'a str,
        _: &'a PreparedTool,
    ) -> BoxFuture<'a, ToolResult> {
        Box::pin(async { panic!("unadmitted effect must never execute") })
    }
    fn reconcile<'a>(
        &'a self,
        _: &'a str,
        _: &'a str,
        _: &'a PreparedTool,
    ) -> BoxFuture<'a, Reconcile> {
        Box::pin(async { panic!("unadmitted effect has no recovery") })
    }
}
#[tokio::test]
async fn repeated_failed_steps_pause_without_effects_or_unbounded_model_calls() {
    let (_dir, db, base) = fixture().await;
    let model = Arc::new(RepeatInvalid(AtomicUsize::new(0)));
    let engine = Engine::new(base.journal().clone(), model.clone(), Arc::new(InvalidTool));
    let mut request = spec(false, 10);
    request.criteria = vec![excalibur_engine::Criterion::ToolSuccess {
        name: "invalid".into(),
    }];
    let run = engine.create("loop", request).await.unwrap();
    let state = engine.run(&run).await.unwrap();
    assert_eq!(state.status, Status::Paused);
    assert_eq!(state.step, 3);
    assert_eq!(state.tool_calls, 3);
    assert_eq!(model.0.load(Ordering::SeqCst), 3);
    assert!(state.reason.unwrap().contains("identical unsuccessful"));
    assert!(
        !base
            .journal()
            .replay(&run, 0)
            .await
            .unwrap()
            .iter()
            .any(|event| event.kind == "tool_intent")
    );
    db.shutdown().await.unwrap();
}
struct TimeoutOnce {
    inner: Effects,
    hold: std::sync::atomic::AtomicBool,
}
impl ToolRuntime for TimeoutOnce {
    fn definitions(&self) -> Vec<ToolDefinition> {
        self.inner.definitions()
    }
    fn prepare<'a>(
        &'a self,
        run: &'a str,
        call: ToolCall,
    ) -> BoxFuture<'a, Result<PreparedTool, String>> {
        self.inner.prepare(run, call)
    }
    fn execute<'a>(
        &'a self,
        run: &'a str,
        key: &'a str,
        tool: &'a PreparedTool,
    ) -> BoxFuture<'a, ToolResult> {
        Box::pin(async move {
            let result = self.inner.execute(run, key, tool).await;
            if self.hold.swap(false, Ordering::SeqCst) {
                std::future::pending::<()>().await;
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
        self.inner.reconcile(run, key, tool)
    }
}
#[tokio::test]
async fn keyed_timeout_keeps_intent_and_resumes_with_same_key_without_duplicating_effect() {
    let (dir, db, base) = fixture().await;
    let tools = Arc::new(TimeoutOnce {
        inner: Effects {
            root: dir.path().into(),
            effect: Effect::Keyed,
            journal: base.journal().clone(),
        },
        hold: std::sync::atomic::AtomicBool::new(true),
    });
    let engine = Engine::new(base.journal().clone(), model(dir.path(), false, 1), tools);
    let mut request = spec(false, 10);
    request.budget.tool_timeout_seconds = 1;
    let run = engine.create("keyed-timeout", request).await.unwrap();
    let state = engine.run(&run).await.unwrap();
    assert_eq!(state.status, Status::Paused);
    assert!(state.reason.unwrap().contains("retryable"));
    assert_eq!(count(&dir.path().join("effects")), 1);
    engine.resume(&run).await.unwrap();
    assert_eq!(engine.run(&run).await.unwrap().status, Status::Completed);
    assert_eq!(count(&dir.path().join("effects")), 1);
    assert_eq!(count(&dir.path().join("executions")), 2);
    assert_eq!(count(&dir.path().join("models")), 2);
    db.shutdown().await.unwrap();
}
