//! Network brokers can request an approval independently of a tool batch.
use excalibur_context::ContextInput;
use excalibur_engine::{Budget, Criterion, Engine, RunSpec, Status};
use excalibur_journal::Journal;
use excalibur_models::{ModelError, ModelGateway};
use excalibur_store::{Database, DatabaseKind, StorageConfig};
use excalibur_types::runtime::*;
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};
use tokio::sync::Notify;
struct Pending {
    pending: AtomicBool,
    expired: AtomicBool,
}
impl ToolRuntime for Pending {
    fn definitions(&self) -> Vec<ToolDefinition> {
        Vec::new()
    }
    fn pending_approval<'a>(&'a self, _: &'a str) -> BoxFuture<'a, bool> {
        Box::pin(async { self.pending.load(Ordering::SeqCst) })
    }
    fn approval_blocked<'a>(&'a self, _: &'a str) -> BoxFuture<'a, Option<String>> {
        Box::pin(async {
            self.expired
                .load(Ordering::SeqCst)
                .then(|| "approval expired".into())
        })
    }
    fn prepare<'a>(
        &'a self,
        _: &'a str,
        _: ToolCall,
    ) -> BoxFuture<'a, Result<PreparedTool, String>> {
        Box::pin(async { panic!("no tool may be prepared") })
    }
    fn execute<'a>(
        &'a self,
        _: &'a str,
        _: &'a str,
        _: &'a PreparedTool,
    ) -> BoxFuture<'a, ToolResult> {
        Box::pin(async { panic!("no tool may run") })
    }
    fn reconcile<'a>(
        &'a self,
        _: &'a str,
        _: &'a str,
        _: &'a PreparedTool,
    ) -> BoxFuture<'a, Reconcile> {
        Box::pin(async { panic!("no tool needs recovery") })
    }
}
struct Model {
    calls: AtomicUsize,
    entered: Notify,
    release: Notify,
}
impl ModelGateway for Model {
    fn complete<'a>(&'a self, _: ModelRequest) -> BoxFuture<'a, Result<ModelResponse, ModelError>> {
        Box::pin(async {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.entered.notify_one();
            self.release.notified().await;
            Ok(ModelResponse {
                message: Message::text("assistant", "Verified synthetic result"),
                usage: Usage::default(),
            })
        })
    }
}
async fn fixture(
    pending: bool,
    expired: bool,
) -> (
    tempfile::TempDir,
    Database,
    Engine,
    Arc<Pending>,
    Arc<Model>,
    String,
) {
    let root = tempfile::tempdir().unwrap();
    let db = Database::open(
        root.path().join("main.db"),
        DatabaseKind::Main,
        StorageConfig::default(),
    )
    .unwrap();
    let journal = Journal::new(db.clone()).await.unwrap();
    let tools = Arc::new(Pending {
        pending: AtomicBool::new(pending),
        expired: AtomicBool::new(expired),
    });
    let model = Arc::new(Model {
        calls: AtomicUsize::new(0),
        entered: Notify::new(),
        release: Notify::new(),
    });
    let engine = Engine::new(journal, model.clone(), tools.clone());
    let run = engine
        .create(
            "out-of-band-approval",
            RunSpec {
                context: ContextInput::new("Policy", "Finish verified work", "fixed"),
                budget: Budget::default(),
                criteria: vec![Criterion::HostCheck { id: "done".into() }],
            },
        )
        .await
        .unwrap();
    engine
        .record_check(&run, "done", true, "Synthetic backend evidence")
        .await
        .unwrap();
    (root, db, engine, tools, model, run)
}
#[tokio::test]
async fn existing_external_approval_or_expiry_stops_before_any_inference() {
    for expired in [false, true] {
        let (_root, db, engine, _tools, model, run) = fixture(true, expired).await;
        let state = engine.run(&run).await.unwrap();
        assert_eq!(
            state.status,
            if expired {
                Status::Blocked
            } else {
                Status::Paused
            }
        );
        assert_eq!(
            state.reason.as_deref(),
            Some(if expired {
                "approval expired"
            } else {
                "approval required"
            })
        );
        assert_eq!(model.calls.load(Ordering::SeqCst), 0);
        assert!(
            !engine
                .journal()
                .replay(&run, 0)
                .await
                .unwrap()
                .iter()
                .any(|e| e.kind == "model_request")
        );
        db.shutdown().await.unwrap();
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn approval_during_inference_prevents_completion_and_resume_reuses_confirmed_response() {
    let (_root, db, engine, tools, model, run) = fixture(false, false).await;
    let driver = tokio::spawn({
        let engine = engine.clone();
        let run = run.clone();
        async move { engine.run(&run).await.unwrap() }
    });
    tokio::time::timeout(std::time::Duration::from_secs(10), model.entered.notified())
        .await
        .unwrap();
    tools.pending.store(true, Ordering::SeqCst);
    model.release.notify_one();
    let state = driver.await.unwrap();
    assert_eq!(state.status, Status::Paused);
    assert_eq!(model.calls.load(Ordering::SeqCst), 1);
    let events = engine.journal().replay(&run, 0).await.unwrap();
    assert_eq!(
        events.iter().filter(|e| e.kind == "model_response").count(),
        1
    );
    assert!(!events.iter().any(|e| e.kind == "gate_report"));
    tools.pending.store(false, Ordering::SeqCst);
    engine.resume(&run).await.unwrap();
    assert_eq!(engine.run(&run).await.unwrap().status, Status::Completed);
    assert_eq!(model.calls.load(Ordering::SeqCst), 1);
    db.shutdown().await.unwrap();
}
