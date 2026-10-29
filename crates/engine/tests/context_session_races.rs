//! Deterministic races at context/extractor and instruction-registry boundaries.
use excalibur_context::{ContextInput, IncrementalSummary, SessionPolicy};
use excalibur_engine::{Budget, Criterion, DurableSink, Engine, RunSpec, Status};
use excalibur_journal::{
    Event, Journal,
    instructions::{InstructionOrigin, InstructionOriginKind, InstructionScope, InstructionStore},
};
use excalibur_models::{GatewayMetadata, ModelCallBound, ModelError, ModelGateway, ModelRole};
use excalibur_store::{Database, DatabaseKind, StorageConfig};
use excalibur_tools::{Access, LocalTools, ToolPolicy};
use excalibur_types::runtime::*;
use serde_json::{Value, json};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};
use tokio::sync::Notify;

#[derive(Default)]
struct Hold {
    entered: Notify,
    release: Notify,
    dropped: AtomicUsize,
}
struct DropSignal(Arc<Hold>);
impl Drop for DropSignal {
    fn drop(&mut self) {
        self.0.dropped.fetch_add(1, Ordering::SeqCst);
    }
}
struct Main {
    requests: Mutex<Vec<ModelRequest>>,
    hold_first: Option<Arc<Hold>>,
    stale_write: bool,
}
impl Main {
    fn immediate() -> Self {
        Self {
            requests: Mutex::new(Vec::new()),
            hold_first: None,
            stale_write: false,
        }
    }
}
impl ModelGateway for Main {
    fn request_time_override(&self, _: u64) -> Option<String> {
        Some("1700000000".into())
    }
    fn complete<'a>(
        &'a self,
        request: ModelRequest,
    ) -> BoxFuture<'a, Result<ModelResponse, ModelError>> {
        Box::pin(async move {
            let index = {
                let mut requests = self.requests.lock().unwrap();
                let index = requests.len();
                requests.push(request);
                index
            };
            if index == 0
                && let Some(hold) = &self.hold_first
            {
                let _drop = DropSignal(hold.clone());
                hold.entered.notify_one();
                hold.release.notified().await;
            }
            let mut message = Message::text("assistant", "Synthetic final response");
            if index == 0 && self.stale_write {
                message.content = "Old response before instruction revision".into();
                message.tool_calls.push(ToolCall {
                    id: "stale-write".into(),
                    name: "write_file".into(),
                    arguments: json!({"path":"old-response.txt","content":"stale effect"})
                        .to_string(),
                });
            }
            Ok(ModelResponse {
                message,
                usage: Usage {
                    input_tokens: 1,
                    output_tokens: 1,
                    estimated: true,
                    ..Default::default()
                },
            })
        })
    }
}
struct Extractor {
    hold: Option<Arc<Hold>>,
    calls: AtomicUsize,
}
impl ModelGateway for Extractor {
    fn metadata(&self) -> GatewayMetadata {
        GatewayMetadata {
            role: Some(ModelRole::Extractor),
            ..Default::default()
        }
    }
    fn call_bound(&self, _: &ModelRequest) -> Option<ModelCallBound> {
        Some(ModelCallBound {
            max_tokens: 1000,
            max_cost_microusd: 0,
        })
    }
    fn complete<'a>(&'a self, _: ModelRequest) -> BoxFuture<'a, Result<ModelResponse, ModelError>> {
        Box::pin(async move {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if let Some(hold) = &self.hold {
                let _drop = DropSignal(hold.clone());
                hold.entered.notify_one();
                std::future::pending::<()>().await;
            }
            let delta = IncrementalSummary {
                findings: vec!["Unique durable incremental finding".into()],
                ..Default::default()
            };
            Ok(ModelResponse {
                message: Message::text("assistant", serde_json::to_string(&delta).unwrap()),
                usage: Usage {
                    input_tokens: 10,
                    output_tokens: 2,
                    ..Default::default()
                },
            })
        })
    }
}
async fn fixture() -> (tempfile::TempDir, Database, Journal, Arc<LocalTools>) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("workspace");
    std::fs::create_dir(&root).unwrap();
    let db = Database::open(
        dir.path().join("main.db"),
        DatabaseKind::Main,
        StorageConfig::default(),
    )
    .unwrap();
    let journal = Journal::new(db.clone()).await.unwrap();
    let tools = Arc::new(
        LocalTools::new(
            db.clone(),
            root,
            ToolPolicy {
                read: Access::Allow,
                write: Access::Allow,
                exec: Access::Deny,
                ..Default::default()
            },
        )
        .await
        .unwrap(),
    );
    (dir, db, journal, tools)
}
fn spec(long: bool) -> RunSpec {
    let mut context = ContextInput::new("Synthetic core", "Literal original request", "1700000000");
    if long {
        for index in 0..100 {
            context.activity.push(Message::text(
                "user",
                format!(
                    "Previous turn {index}: {}",
                    "synthetic history ".repeat(100)
                ),
            ));
        }
    }
    RunSpec {
        context,
        budget: Budget {
            max_tokens: 1_000_000,
            ..Default::default()
        },
        criteria: vec![Criterion::FinalResponse { schema: None }],
    }
}
fn policy() -> SessionPolicy {
    SessionPolicy {
        compaction_cap: 10_000,
        recent_activity_tokens: 1000,
        ..Default::default()
    }
}
async fn controlled_extractor(cancel: bool) {
    let (_dir, db, journal, tools) = fixture().await;
    let hold = Arc::new(Hold::default());
    let main = Arc::new(Main::immediate());
    let extractor = Arc::new(Extractor {
        hold: Some(hold.clone()),
        calls: AtomicUsize::new(0),
    });
    let engine = Engine::new(journal.clone(), main.clone(), tools.clone())
        .with_context_policy(policy())
        .with_context_extractor(extractor.clone())
        .unwrap();
    let run = engine.create("held-extractor", spec(true)).await.unwrap();
    let driver = tokio::spawn({
        let engine = engine.clone();
        let run = run.clone();
        async move { engine.run(&run).await.unwrap() }
    });
    tokio::time::timeout(std::time::Duration::from_secs(10), hold.entered.notified())
        .await
        .expect("extractor never reached the deterministic hold");
    let snapshot = engine.load(&run).await.unwrap();
    let bound = snapshot.pending_context_summary.as_ref().unwrap()["bound"]["max_tokens"]
        .as_u64()
        .unwrap();
    assert_eq!(
        excalibur_engine::remaining_budget(&snapshot).max_tokens,
        snapshot.spec.budget.max_tokens - bound
    );
    let child_spec = spec(false);
    let child = engine
        .create(
            if cancel {
                "cancel-pending-child"
            } else {
                "pause-pending-child"
            },
            child_spec.clone(),
        )
        .await
        .unwrap();
    let error = excalibur_engine::reserve_child_budget(
        engine.journal(),
        &run,
        "pending-extractor-child",
        &child,
        child_spec.budget,
    )
    .await
    .unwrap_err();
    assert!(error.to_string().contains("unaccounted model call"));

    if cancel {
        engine.cancel(&run).await.unwrap();
    } else {
        engine.pause(&run).await.unwrap();
    }
    let stopped = tokio::time::timeout(std::time::Duration::from_secs(10), driver)
        .await
        .expect("extractor did not honor durable control")
        .unwrap();
    assert_eq!(
        stopped.status,
        if cancel {
            Status::Cancelled
        } else {
            Status::Paused
        }
    );
    assert_eq!(stopped.reason.as_deref(), Some("user"));
    assert_eq!(hold.dropped.load(Ordering::SeqCst), 1);
    assert!(main.requests.lock().unwrap().is_empty());
    let events = journal.replay(&run, 0).await.unwrap();
    assert_eq!(
        events
            .iter()
            .filter(|e| e.kind == "context_summary_started")
            .count(),
        1
    );
    assert_eq!(
        events
            .iter()
            .filter(|e| e.kind == "context_summary_completed")
            .count(),
        1
    );
    assert!(!events.iter().any(|e| e.kind == "model_dispatch"));
    assert!(stopped.usage.estimated);
    assert_eq!(stopped.usage.input_tokens, 1000);
    if cancel {
        assert!(engine.resume(&run).await.is_err());
    } else {
        engine.resume(&run).await.unwrap();
        assert_eq!(engine.run(&run).await.unwrap().status, Status::Completed);
        assert_eq!(
            extractor.calls.load(Ordering::SeqCst),
            1,
            "confirmed fallback/session must not repeat interrupted inference"
        );
    }
    tools.terminals().shutdown().await;
    db.shutdown().await.unwrap();
}
#[tokio::test]
async fn pause_drops_held_extractor_before_main_dispatch_and_resume_reuses_projection() {
    controlled_extractor(false).await;
}
#[tokio::test]
async fn cancel_drops_held_extractor_without_main_dispatch_or_terminal_resurrection() {
    controlled_extractor(true).await;
}
#[tokio::test]
async fn registry_change_during_held_main_response_blocks_old_unadmitted_file_effect() {
    let (dir, db, journal, tools) = fixture().await;
    let registry = InstructionStore::open(db.clone(), excalibur_vault::Redactor::default())
        .await
        .unwrap();
    let origin = InstructionOrigin {
        kind: InstructionOriginKind::Host,
        reference: "synthetic-host-instruction".into(),
    };
    let old = registry
        .register(
            "initial",
            None,
            InstructionScope::General,
            origin.clone(),
            "Initial literal instruction",
        )
        .await
        .unwrap();
    let hold = Arc::new(Hold::default());
    let main = Arc::new(Main {
        requests: Mutex::new(vec![]),
        hold_first: Some(hold.clone()),
        stale_write: true,
    });
    let engine = Engine::new(journal.clone(), main.clone(), tools.clone());
    let run = engine
        .create("instruction-race", spec(false))
        .await
        .unwrap();
    let driver = tokio::spawn({
        let engine = engine.clone();
        let run = run.clone();
        async move { engine.run(&run).await.unwrap() }
    });
    tokio::time::timeout(std::time::Duration::from_secs(10), hold.entered.notified())
        .await
        .expect("main request never reached hold");
    let literal = "NEW literal constraint: never write old-response.txt; finish read-only.";
    registry
        .revise("updated", None, &old.id, old.revision, origin, literal)
        .await
        .unwrap();
    hold.release.notify_one();
    let stopped = tokio::time::timeout(std::time::Duration::from_secs(10), driver)
        .await
        .expect("run did not consume instruction revision")
        .unwrap();
    assert_eq!(stopped.status, Status::Completed);
    assert!(!dir.path().join("workspace/old-response.txt").exists());
    {
        let requests = main.requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        assert!(
            requests[1]
                .messages
                .iter()
                .any(|m| m.content.contains(literal))
        );
    }
    let events = journal.replay(&run, 0).await.unwrap();
    assert!(events.iter().any(|e| e.kind == "directive_applied"));
    assert!(
        !events
            .iter()
            .any(|e| e.kind == "tool_intent" && e.payload.to_string().contains("stale-write"))
    );
    assert!(
        events
            .iter()
            .any(|e| e.kind == "tool_result" && e.payload["result"]["success"] == false)
    );
    tools.terminals().shutdown().await;
    db.shutdown().await.unwrap();
}
struct HoldCompletedSummary {
    entered: Notify,
}
impl DurableSink for HoldCompletedSummary {
    fn record<'a>(&'a self, _: &'a str, event: &'a Event) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async move {
            if event.kind == "context_summary_completed" {
                self.entered.notify_one();
                std::future::pending::<()>().await;
            }
            Ok(())
        })
    }
}
#[tokio::test]
async fn confirmed_valid_incremental_summary_is_reused_after_abort_before_context_projection() {
    let (_dir, db, journal, tools) = fixture().await;
    let main = Arc::new(Main::immediate());
    let extractor = Arc::new(Extractor {
        hold: None,
        calls: AtomicUsize::new(0),
    });
    let sink = Arc::new(HoldCompletedSummary {
        entered: Notify::new(),
    });
    let engine = Engine::new(journal.clone(), main.clone(), tools.clone())
        .with_context_policy(policy())
        .with_context_extractor(extractor.clone())
        .unwrap()
        .with_durable_sink(sink.clone());
    let run = engine
        .create("valid-summary-recovery", spec(true))
        .await
        .unwrap();
    let driver = tokio::spawn({
        let engine = engine.clone();
        let run = run.clone();
        async move { engine.run(&run).await.unwrap() }
    });
    tokio::time::timeout(std::time::Duration::from_secs(10), sink.entered.notified())
        .await
        .expect("summary receipt never committed");
    driver.abort();
    assert!(driver.await.unwrap_err().is_cancelled());
    let before = journal.replay(&run, 0).await.unwrap();
    assert!(before.iter().any(|e| e.kind == "context_summary_completed"));
    assert!(!before.iter().any(|e| e.kind == "context_prepared"));
    let reopened = Engine::new(journal.clone(), main.clone(), tools.clone())
        .with_context_extractor(extractor.clone())
        .unwrap();
    let finished = reopened.run(&run).await.unwrap();
    assert_eq!(finished.status, Status::Completed);
    assert_eq!(extractor.calls.load(Ordering::SeqCst), 1);
    assert_eq!(finished.model_calls, 2);
    assert_eq!(
        finished
            .context_session
            .as_ref()
            .unwrap()
            .summary()
            .findings,
        vec!["Unique durable incremental finding"]
    );
    let events = journal.replay(&run, 0).await.unwrap();
    assert_eq!(
        events
            .iter()
            .filter(|e| e.kind == "context_summary_started")
            .count(),
        1
    );
    assert_eq!(
        events
            .iter()
            .filter(|e| e.kind == "context_summary_completed")
            .count(),
        1
    );
    let projection = events
        .iter()
        .find(|e| e.kind == "context_prepared")
        .unwrap();
    assert_eq!(
        projection.payload["receipt"]["compaction"]["summary_status"],
        "incremental"
    );
    let actual = main.requests.lock().unwrap()[0].clone();
    let block = actual
        .messages
        .iter()
        .find_map(|m| {
            serde_json::from_str::<Value>(&m.content)
                .ok()
                .filter(|v| v.get("context_compaction").is_some())
        })
        .unwrap();
    assert_eq!(
        block["context_compaction"]["incremental_summary"]["findings"],
        json!(["Unique durable incremental finding"])
    );
    tools.terminals().shutdown().await;
    db.shutdown().await.unwrap();
}

struct HeldFinalGate {
    entered: Notify,
    release: Notify,
    calls: AtomicUsize,
}
struct EmptyGateGuard;
impl excalibur_engine::CompletionGateGuard for EmptyGateGuard {
    fn report(&self) -> excalibur_engine::GateAssessment {
        Default::default()
    }
}
impl excalibur_engine::CompletionGate for HeldFinalGate {
    fn inspect<'a>(
        &'a self,
        _: &'a str,
        _: &'a excalibur_engine::GateInput,
    ) -> BoxFuture<'a, Result<Box<dyn excalibur_engine::CompletionGateGuard>, String>> {
        Box::pin(async move {
            if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
                self.entered.notify_one();
                self.release.notified().await;
            }
            Ok(Box::new(EmptyGateGuard) as Box<dyn excalibur_engine::CompletionGateGuard>)
        })
    }
}
#[tokio::test]
async fn instruction_change_during_final_gate_requires_new_literal_context_before_terminal_commit()
{
    let (_dir, db, journal, tools) = fixture().await;
    let registry = InstructionStore::open(db.clone(), excalibur_vault::Redactor::default())
        .await
        .unwrap();
    let origin = InstructionOrigin {
        kind: InstructionOriginKind::Host,
        reference: "synthetic-host-policy".into(),
    };
    let old = registry
        .register(
            "gate-instruction",
            None,
            InstructionScope::General,
            origin.clone(),
            "Initial literal constraint",
        )
        .await
        .unwrap();
    let main = Arc::new(Main::immediate());
    let gate = Arc::new(HeldFinalGate {
        entered: Notify::new(),
        release: Notify::new(),
        calls: AtomicUsize::new(0),
    });
    let engine =
        Engine::new(journal.clone(), main.clone(), tools).with_completion_gate(gate.clone());
    let run = engine
        .create("registry-finalization-race", spec(false))
        .await
        .unwrap();
    let driver = tokio::spawn({
        let engine = engine.clone();
        let run = run.clone();
        async move { engine.run(&run).await.unwrap() }
    });
    tokio::time::timeout(std::time::Duration::from_secs(10), gate.entered.notified())
        .await
        .unwrap();
    let current = "NEW literal constraint that final verification must cover";
    registry
        .revise(
            "gate-instruction-update",
            None,
            &old.id,
            old.revision,
            origin,
            current,
        )
        .await
        .unwrap();
    gate.release.notify_one();
    let state = tokio::time::timeout(std::time::Duration::from_secs(20), driver)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(state.status, Status::Completed);
    {
        let requests = main.requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        assert!(
            requests[1]
                .messages
                .iter()
                .any(|m| m.content.contains(current))
        );
    }
    assert_eq!(gate.calls.load(Ordering::SeqCst), 2);
    db.shutdown().await.unwrap();
}
