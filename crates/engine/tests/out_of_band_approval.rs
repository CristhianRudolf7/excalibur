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
    decide_during_pending_read: AtomicBool,
}
impl ToolRuntime for Pending {
    fn definitions(&self) -> Vec<ToolDefinition> {
        Vec::new()
    }
    fn pending_approval<'a>(&'a self, _: &'a str) -> BoxFuture<'a, bool> {
        Box::pin(async {
            let pending = self.pending.load(Ordering::SeqCst);
            if self
                .decide_during_pending_read
                .swap(false, Ordering::SeqCst)
            {
                self.pending.store(false, Ordering::SeqCst);
            }
            pending
        })
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
        decide_during_pending_read: AtomicBool::new(false),
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn automatic_approval_resume_never_waits_for_live_inference_or_reopens_a_user_pause() {
    let (_root, db, engine, _tools, model, run) = fixture(false, false).await;
    let driver = tokio::spawn({
        let engine = engine.clone();
        let run = run.clone();
        async move { engine.run(&run).await.unwrap() }
    });
    model.entered.notified().await;
    assert!(
        !tokio::time::timeout(
            std::time::Duration::from_secs(5),
            engine.resume_after_approval(&run)
        )
        .await
        .unwrap()
        .unwrap()
    );
    engine.pause(&run).await.unwrap();
    assert!(
        !tokio::time::timeout(
            std::time::Duration::from_secs(5),
            engine.resume_after_approval(&run)
        )
        .await
        .unwrap()
        .unwrap()
    );
    model.release.notify_one();
    assert_eq!(driver.await.unwrap().reason.as_deref(), Some("user"));
    assert_eq!(engine.load(&run).await.unwrap().status, Status::Paused);
    db.shutdown().await.unwrap();
}

#[tokio::test]
async fn automatic_system_resume_queues_without_waiting_on_the_old_driver_boundary() {
    let (_root, db, engine, tools, _model, run) = fixture(true, false).await;
    assert_eq!(engine.run(&run).await.unwrap().status, Status::Paused);
    tools.pending.store(false, Ordering::SeqCst);
    let old_driver_boundary = engine.journal().lock_run(&run).await;
    assert!(
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            engine.resume_after_approval(&run)
        )
        .await
        .unwrap()
        .unwrap()
    );
    assert_eq!(engine.load(&run).await.unwrap().status, Status::Ready);
    engine.pause(&run).await.unwrap();
    assert!(!engine.resume_after_approval(&run).await.unwrap());
    assert_eq!(
        engine.load(&run).await.unwrap().reason.as_deref(),
        Some("user")
    );
    drop(old_driver_boundary);
    db.shutdown().await.unwrap();
}

#[tokio::test]
async fn approval_committed_between_pending_read_and_pause_cannot_leave_a_lost_wakeup() {
    let (_root, db, engine, tools, model, run) = fixture(true, false).await;
    tools
        .decide_during_pending_read
        .store(true, Ordering::SeqCst);
    let state = engine.run(&run).await.unwrap();
    assert_eq!(state.status, Status::Ready);
    assert_eq!(engine.load(&run).await.unwrap().status, Status::Ready);
    assert_eq!(model.calls.load(Ordering::SeqCst), 0);
    assert!(!tools.pending.load(Ordering::SeqCst));
    db.shutdown().await.unwrap();
}
