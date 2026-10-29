use excalibur_context::ContextInput;
use excalibur_engine::{Budget, Criterion, Engine, RunSpec, Status};
use excalibur_journal::Journal;
use excalibur_models::{ModelError, ModelGateway};
use excalibur_store::{Database, DatabaseKind, ReadLane, StorageConfig};
use excalibur_types::runtime::*;
use serde_json::json;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use tokio::sync::Notify;
#[derive(Clone, Copy, Debug)]
enum Outcome {
    Success,
    Failure,
    Panic,
    Timeout,
}
#[derive(Clone, Copy)]
enum Control {
    Pause,
    Cancel,
}
struct Model {
    calls: AtomicUsize,
}
impl ModelGateway for Model {
    fn complete<'a>(&'a self, _: ModelRequest) -> BoxFuture<'a, Result<ModelResponse, ModelError>> {
        Box::pin(async move {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(ModelResponse {
                message: Message {
                    provider_state: None,
                    role: "assistant".into(),
                    content: String::new(),
                    tool_calls: vec![
                        ToolCall {
                            id: "first".into(),
                            name: "effect".into(),
                            arguments: "{}".into(),
                        },
                        ToolCall {
                            id: "later".into(),
                            name: "effect".into(),
                            arguments: "{}".into(),
                        },
                    ],
                    tool_call_id: None,
                },
                usage: Usage::default(),
            })
        })
    }
}
struct HeldTool {
    entered: Notify,
    release: Notify,
    started: AtomicUsize,
    finished: AtomicUsize,
    outcome: Outcome,
}
impl ToolRuntime for HeldTool {
    fn definitions(&self) -> Vec<ToolDefinition> {
        vec![ToolDefinition {
            name: "effect".into(),
            description: "Synthetic controlled non-idempotent effect".into(),
            input_schema: json!({"type":"object"}),
        }]
    }
    fn prepare<'a>(
        &'a self,
        run: &'a str,
        call: ToolCall,
    ) -> BoxFuture<'a, Result<PreparedTool, String>> {
        Box::pin(async move {
            Ok(PreparedTool {
                call,
                effect: Effect::NonIdempotent,
                resources: vec![Resource {
                    key: format!("control-fixture:{run}"),
                    write: true,
                }],
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
            self.started.fetch_add(1, Ordering::SeqCst);
            assert_eq!(
                tool.call.id, "first",
                "no later batch may execute after control admission"
            );
            self.entered.notify_one();
            if matches!(self.outcome, Outcome::Timeout) {
                std::future::pending::<()>().await;
                unreachable!();
            }
            self.release.notified().await;
            match self.outcome {
                Outcome::Success => {
                    self.finished.fetch_add(1, Ordering::SeqCst);
                    ToolResult::ok(json!({"observed":"synthetic complete"}))
                }
                Outcome::Failure => {
                    self.finished.fetch_add(1, Ordering::SeqCst);
                    ToolResult::error("synthetic fixture failure")
                }
                Outcome::Panic => panic!("synthetic held tool panic"),
                Outcome::Timeout => unreachable!(),
            }
        })
    }
    fn reconcile<'a>(
        &'a self,
        _: &'a str,
        _: &'a str,
        _: &'a PreparedTool,
    ) -> BoxFuture<'a, Reconcile> {
        Box::pin(async {
            Reconcile::Uncertain("synthetic effect cannot be safely repeated".into())
        })
    }
}
async fn control_case(control: Control, outcome: Outcome) {
    let root = tempfile::tempdir().unwrap();
    let db = Database::open(
        root.path().join("main.db"),
        DatabaseKind::Main,
        StorageConfig::default(),
    )
    .unwrap();
    let journal = Journal::new(db.clone()).await.unwrap();
    let model = Arc::new(Model {
        calls: AtomicUsize::new(0),
    });
    let tool = Arc::new(HeldTool {
        entered: Notify::new(),
        release: Notify::new(),
        started: AtomicUsize::new(0),
        finished: AtomicUsize::new(0),
        outcome,
    });
    let engine = Engine::new(journal.clone(), model.clone(), tool.clone());
    let spec = RunSpec {
        context: ContextInput::new(
            "Synthetic rules",
            "Exercise control priority",
            "2026-10-29T00:00:00Z",
        ),
        budget: Budget {
            tool_timeout_seconds: 1,
            max_steps: 10,
            ..Default::default()
        },
        criteria: vec![Criterion::ToolSuccess {
            name: "effect".into(),
        }],
    };
    let run = engine.create("control-race", spec).await.unwrap();
    let driver = tokio::spawn({
        let engine = engine.clone();
        let run = run.clone();
        async move { engine.run(&run).await.unwrap() }
    });
    tokio::time::timeout(std::time::Duration::from_secs(10), tool.entered.notified())
        .await
        .expect("fixture did not reach held effect");
    match control {
        Control::Pause => engine.pause(&run).await.unwrap(),
        Control::Cancel => engine.cancel(&run).await.unwrap(),
    };
    let expected = match control {
        Control::Pause => Status::Paused,
        Control::Cancel => Status::Cancelled,
    };
    let admitted = engine.load(&run).await.unwrap();
    assert_eq!(admitted.status, expected);
    assert_eq!(admitted.reason.as_deref(), Some("user"));
    tool.release.notify_one();
    let result = tokio::time::timeout(std::time::Duration::from_secs(10), driver)
        .await
        .expect("driver failed to observe controlled outcome")
        .unwrap();
    assert_eq!(
        result.status, expected,
        "automatic {outcome:?} processing must not overwrite user control"
    );
    assert_eq!(
        result.reason.as_deref(),
        Some("user"),
        "automatic outcome must preserve user control reason"
    );
    assert_eq!(tool.started.load(Ordering::SeqCst), 1);
    assert_eq!(model.calls.load(Ordering::SeqCst), 1);
    let events = journal.replay(&run, 0).await.unwrap();
    let confirmed = events
        .iter()
        .filter(|e| e.kind == "tool_result" && e.payload["index"] == 0)
        .collect::<Vec<_>>();
    if matches!(outcome, Outcome::Success | Outcome::Failure) {
        assert_eq!(
            confirmed.len(),
            1,
            "completed tool result must still be durable after control"
        );
        assert_eq!(
            confirmed[0].payload["result"]["success"],
            matches!(outcome, Outcome::Success)
        );
        assert_eq!(tool.finished.load(Ordering::SeqCst), 1);
    } else {
        assert!(
            confirmed.is_empty(),
            "interruption must not fabricate a completed effect"
        );
    }
    assert!(
        !events
            .iter()
            .any(|e| e.kind == "tool_result" && e.payload["index"] == 1)
    );
    let projected: (String, Option<String>) = db
        .read(ReadLane::Interactive, {
            let run = run.clone();
            move |c| {
                Ok(
                    c.query_row("SELECT state,reason FROM runs WHERE id=?1", [run], |r| {
                        Ok((r.get(0)?, r.get(1)?))
                    })?,
                )
            }
        })
        .await
        .unwrap();
    assert_eq!(
        projected.0,
        match control {
            Control::Pause => "paused",
            Control::Cancel => "cancelled",
        }
    );
    assert_eq!(projected.1.as_deref(), Some("user"));
    if matches!(control, Control::Cancel) {
        assert!(engine.resume(&run).await.is_err());
        assert!(
            engine
                .resolve_uncertain(
                    &run,
                    0,
                    ToolResult::ok(json!({})),
                    "explicit synthetic evidence"
                )
                .await
                .is_err()
        );
        assert_eq!(engine.run(&run).await.unwrap().status, Status::Cancelled);
        assert_eq!(model.calls.load(Ordering::SeqCst), 1);
    }
    db.shutdown().await.unwrap();
}
macro_rules! case {
    ($name:ident,$control:ident,$outcome:ident) => {
        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn $name() {
            control_case(Control::$control, Outcome::$outcome).await;
        }
    };
}
case!(
    pause_during_success_preserves_user_state_and_confirms_result,
    Pause,
    Success
);
case!(
    pause_during_failed_result_preserves_user_state_and_confirms_result,
    Pause,
    Failure
);
case!(pause_during_panic_preserves_user_state, Pause, Panic);
case!(pause_during_timeout_preserves_user_state, Pause, Timeout);
case!(
    cancel_during_success_stays_terminal_and_confirms_result,
    Cancel,
    Success
);
case!(
    cancel_during_failed_result_stays_terminal_and_confirms_result,
    Cancel,
    Failure
);
case!(cancel_during_panic_stays_terminal, Cancel, Panic);
case!(cancel_during_timeout_stays_terminal, Cancel, Timeout);

struct FinalModel {
    calls: AtomicUsize,
}
impl ModelGateway for FinalModel {
    fn complete<'a>(&'a self, _: ModelRequest) -> BoxFuture<'a, Result<ModelResponse, ModelError>> {
        Box::pin(async move {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(ModelResponse {
                message: Message::text("assistant", "Synthetic final report"),
                usage: Usage::default(),
            })
        })
    }
}
struct GateBarrier {
    boundary: excalibur_engine::Boundary,
    entered: Notify,
    release: (std::sync::Mutex<bool>, std::sync::Condvar),
}
impl excalibur_engine::Observer for GateBarrier {
    fn reached(&self, phase: excalibur_engine::Boundary, _: &str, _: u64, _: Option<&str>) {
        if phase == self.boundary {
            self.entered.notify_one();
            let mut released = self.release.0.lock().unwrap();
            while !*released {
                released = self.release.1.wait(released).unwrap();
            }
        }
    }
}
async fn final_gate_case(control: Control) {
    let root = tempfile::tempdir().unwrap();
    let db = Database::open(
        root.path().join("main.db"),
        DatabaseKind::Main,
        StorageConfig::default(),
    )
    .unwrap();
    let journal = Journal::new(db.clone()).await.unwrap();
    let model = Arc::new(FinalModel {
        calls: AtomicUsize::new(0),
    });
    let tool = Arc::new(HeldTool {
        entered: Notify::new(),
        release: Notify::new(),
        started: AtomicUsize::new(0),
        finished: AtomicUsize::new(0),
        outcome: Outcome::Success,
    });
    let gate = Arc::new(GateBarrier {
        boundary: excalibur_engine::Boundary::GateReportConfirmed,
        entered: Notify::new(),
        release: (std::sync::Mutex::new(false), std::sync::Condvar::new()),
    });
    let engine =
        Engine::new(journal.clone(), model.clone(), tool.clone()).with_observer(gate.clone());
    let run = engine
        .create(
            "gate-control",
            RunSpec {
                context: ContextInput::new(
                    "Synthetic rules",
                    "Finish verified work",
                    "2026-10-29T00:00:00Z",
                ),
                budget: Budget::default(),
                criteria: vec![Criterion::HostCheck {
                    id: "synthetic-complete".into(),
                }],
            },
        )
        .await
        .unwrap();
    engine
        .record_check(
            &run,
            "synthetic-complete",
            true,
            "Synthetic backend evidence",
        )
        .await
        .unwrap();
    let driver = tokio::spawn({
        let engine = engine.clone();
        let run = run.clone();
        async move { engine.run(&run).await.unwrap() }
    });
    tokio::time::timeout(std::time::Duration::from_secs(10), gate.entered.notified())
        .await
        .expect("final gate did not reach its durable boundary");
    let command = match control {
        Control::Pause => engine.pause(&run).await,
        Control::Cancel => engine.cancel(&run).await,
    };
    *gate.release.0.lock().unwrap() = true;
    gate.release.1.notify_all();
    command.unwrap();
    let state = driver.await.unwrap();
    assert_eq!(
        state.status,
        match control {
            Control::Pause => Status::Paused,
            Control::Cancel => Status::Cancelled,
        }
    );
    assert_eq!(state.reason.as_deref(), Some("user"));
    assert!(
        state.gate_report.is_empty(),
        "criterion really passed before the competing control"
    );
    assert_eq!(model.calls.load(Ordering::SeqCst), 1);
    assert_eq!(tool.started.load(Ordering::SeqCst), 0);
    let events = journal.replay(&run, 0).await.unwrap();
    assert!(
        !events
            .iter()
            .any(|e| e.kind == "state_change" && e.payload["state"] == "completed")
    );
    db.shutdown().await.unwrap();
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancelled_after_confirmed_successful_gate_cannot_be_overwritten_by_completion() {
    final_gate_case(Control::Cancel).await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn user_pause_after_confirmed_successful_gate_cannot_be_overwritten_by_completion() {
    final_gate_case(Control::Pause).await;
}

async fn intent_admission_case(control: Control) {
    let root = tempfile::tempdir().unwrap();
    let db = Database::open(
        root.path().join("main.db"),
        DatabaseKind::Main,
        StorageConfig::default(),
    )
    .unwrap();
    let journal = Journal::new(db.clone()).await.unwrap();
    let model = Arc::new(Model {
        calls: AtomicUsize::new(0),
    });
    let tool = Arc::new(HeldTool {
        entered: Notify::new(),
        release: Notify::new(),
        started: AtomicUsize::new(0),
        finished: AtomicUsize::new(0),
        outcome: Outcome::Success,
    });
    let gate = Arc::new(GateBarrier {
        boundary: excalibur_engine::Boundary::BeforeToolIntent,
        entered: Notify::new(),
        release: (std::sync::Mutex::new(false), std::sync::Condvar::new()),
    });
    let engine =
        Engine::new(journal.clone(), model.clone(), tool.clone()).with_observer(gate.clone());
    let run = engine
        .create(
            "intent-control",
            RunSpec {
                context: ContextInput::new(
                    "Synthetic rules",
                    "Hold admission",
                    "2026-10-29T00:00:00Z",
                ),
                budget: Budget::default(),
                criteria: vec![Criterion::ToolSuccess {
                    name: "effect".into(),
                }],
            },
        )
        .await
        .unwrap();
    let driver = tokio::spawn({
        let engine = engine.clone();
        let run = run.clone();
        async move { engine.run(&run).await.unwrap() }
    });
    tokio::time::timeout(std::time::Duration::from_secs(10), gate.entered.notified())
        .await
        .expect("preparation did not reach intent admission boundary");
    let command = match control {
        Control::Pause => engine.pause(&run).await,
        Control::Cancel => engine.cancel(&run).await,
    };
    *gate.release.0.lock().unwrap() = true;
    gate.release.1.notify_all();
    command.unwrap();
    let state = driver.await.unwrap();
    assert_eq!(
        state.status,
        match control {
            Control::Pause => Status::Paused,
            Control::Cancel => Status::Cancelled,
        }
    );
    assert_eq!(state.reason.as_deref(), Some("user"));
    assert_eq!(tool.started.load(Ordering::SeqCst), 0);
    assert_eq!(model.calls.load(Ordering::SeqCst), 1);
    let events = journal.replay(&run, 0).await.unwrap();
    assert!(
        !events
            .iter()
            .any(|event| matches!(event.kind.as_str(), "tool_intent" | "tool_result")),
        "control committed between preparation and intent must prevent effect admission"
    );
    db.shutdown().await.unwrap();
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancel_between_preparation_and_intent_prevents_any_new_effect() {
    intent_admission_case(Control::Cancel).await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pause_between_preparation_and_intent_prevents_any_new_effect() {
    intent_admission_case(Control::Pause).await;
}
