use excalibur_context::ContextInput;
use excalibur_engine::*;
use excalibur_journal::Journal;
use excalibur_models::{ModelError, ModelGateway, StreamDelta, StreamSink};
use excalibur_store::{Database, DatabaseKind, StorageConfig};
use excalibur_types::runtime::*;
use serde_json::json;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};

struct FinalModel {
    calls: Arc<AtomicUsize>,
    requests: Arc<Mutex<Vec<ModelRequest>>>,
}
impl ModelGateway for FinalModel {
    fn complete<'a>(
        &'a self,
        request: ModelRequest,
    ) -> BoxFuture<'a, std::result::Result<ModelResponse, ModelError>> {
        Box::pin(async move {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.requests.lock().unwrap().push(request);
            Ok(ModelResponse {
                message: Message::text("assistant", "Synthetic deliverable"),
                usage: Usage {
                    input_tokens: 2,
                    output_tokens: 1,
                    ..Default::default()
                },
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
    ) -> BoxFuture<'a, std::result::Result<PreparedTool, String>> {
        Box::pin(async { Err("no tools".into()) })
    }
    fn execute<'a>(
        &'a self,
        _: &'a str,
        _: &'a str,
        _: &'a PreparedTool,
    ) -> BoxFuture<'a, ToolResult> {
        Box::pin(async { panic!("no admitted tool") })
    }
    fn reconcile<'a>(
        &'a self,
        _: &'a str,
        _: &'a str,
        _: &'a PreparedTool,
    ) -> BoxFuture<'a, Reconcile> {
        Box::pin(async { panic!("no admitted tool") })
    }
}
async fn fixture() -> (
    tempfile::TempDir,
    Database,
    Engine,
    Arc<AtomicUsize>,
    Arc<Mutex<Vec<ModelRequest>>>,
) {
    let dir = tempfile::tempdir().unwrap();
    let db = Database::open(
        dir.path().join("main.db"),
        DatabaseKind::Main,
        StorageConfig::default(),
    )
    .unwrap();
    let journal = Journal::new(db.clone()).await.unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let requests = Arc::new(Mutex::new(vec![]));
    let engine = Engine::new(
        journal,
        Arc::new(FinalModel {
            calls: calls.clone(),
            requests: requests.clone(),
        }),
        Arc::new(NoTools),
    );
    (dir, db, engine, calls, requests)
}
fn spec() -> RunSpec {
    RunSpec {
        context: ContextInput::new("Synthetic rules", "Synthetic task", "fixed"),
        budget: Budget::default(),
        criteria: vec![Criterion::FinalResponse { schema: None }],
    }
}

#[tokio::test]
async fn every_wait_reason_requires_a_real_wake_and_preserves_structured_reports() {
    let (_dir, db, engine, calls, _) = fixture().await;
    for (reason_index, reason) in [
        WaitReason::Tool,
        WaitReason::User,
        WaitReason::Approval,
        WaitReason::Children,
        WaitReason::Agenda,
        WaitReason::Capability,
        WaitReason::Lease,
    ]
    .into_iter()
    .enumerate()
    {
        for (event_index, event) in [
            WakeEvent::ToolResult,
            WakeEvent::UserReply,
            WakeEvent::ApprovalDecision,
            WakeEvent::ChildSignal,
            WakeEvent::Directive,
            WakeEvent::Timer,
            WakeEvent::CapabilityUpdate,
            WakeEvent::LeaseReleased,
        ]
        .into_iter()
        .enumerate()
        {
            let run = engine
                .create(&format!("wait-{reason_index}-{event_index}"), spec())
                .await
                .unwrap();
            assert_eq!(engine.load(&run).await.unwrap().status, Status::Ready);
            assert!(engine.wait(&run, reason).await.is_err());
            engine
                .journal()
                .set_state_if(&run, &["ready"], "running", None, "fixture-admission")
                .await
                .unwrap();
            engine.wait(&run, reason).await.unwrap();
            let before = calls.load(Ordering::SeqCst);
            let waiting = engine.run(&run).await.unwrap();
            assert_eq!(waiting.status, Status::Waiting);
            assert_eq!(calls.load(Ordering::SeqCst), before);
            assert_eq!(waiting.delivery_report.unwrap().status, Status::Waiting);
            engine
                .wake(&run, event, "synthetic wake evidence")
                .await
                .unwrap();
            let done = engine.run(&run).await.unwrap();
            assert_eq!(done.status, Status::Completed);
            assert_eq!(calls.load(Ordering::SeqCst), before + 1);
            let report = serde_json::to_value(done.delivery_report.unwrap()).unwrap();
            for field in [
                "status",
                "reason",
                "summary",
                "deliverables",
                "evidence",
                "modified_files",
                "assumptions",
                "open_questions",
                "pending",
                "metrics",
            ] {
                assert!(report.get(field).is_some(), "{field}");
            }
            let states = engine
                .journal()
                .replay(&run, 0)
                .await
                .unwrap()
                .into_iter()
                .filter(|event| event.kind == "state_change")
                .map(|event| event.payload["state"].as_str().unwrap().to_owned())
                .collect::<Vec<_>>();
            for state in [
                "ready",
                "running",
                "waiting",
                "recovering",
                "verifying",
                "completed",
            ] {
                assert!(states.iter().any(|s| s == state), "{state}");
            }
            assert!(
                engine
                    .wake(&run, event, "cannot reopen terminal")
                    .await
                    .is_err()
            );
        }
    }
    db.shutdown().await.unwrap();
}
struct Gate {
    kind: GateIssueKind,
    blocked: bool,
    lock: Arc<tokio::sync::RwLock<()>>,
}
struct Guard {
    _lock: tokio::sync::OwnedRwLockReadGuard<()>,
    assessment: GateAssessment,
}
impl CompletionGateGuard for Guard {
    fn report(&self) -> GateAssessment {
        self.assessment.clone()
    }
}
impl CompletionGate for Gate {
    fn inspect<'a>(
        &'a self,
        _: &'a str,
        _: &'a GateInput,
    ) -> BoxFuture<'a, std::result::Result<Box<dyn CompletionGateGuard>, String>> {
        Box::pin(async move {
            let guard = Guard {
                _lock: self.lock.clone().read_owned().await,
                assessment: GateAssessment {
                    issues: vec![GateIssue {
                        id: "criterion-synthetic".into(),
                        kind: self.kind.clone(),
                        detail: "synthetic evidence missing".into(),
                        blocked: self.blocked,
                    }],
                    evidence: vec![DeliveryReference {
                        reference: "synthetic:evidence".into(),
                        description: "Synthetic observation".into(),
                    }],
                    assumptions: vec!["Synthetic assumption".into()],
                    open_questions: vec!["Synthetic question".into()],
                    ..Default::default()
                },
            };
            Ok(Box::new(guard) as Box<dyn CompletionGateGuard>)
        })
    }
    fn context<'a>(&'a self, _: &'a str) -> BoxFuture<'a, std::result::Result<String, String>> {
        Box::pin(async { Ok("Current synthetic micro-plan".into()) })
    }
}
#[tokio::test]
async fn two_failed_gates_are_honest_and_user_confirmation_uses_one_model_call() {
    for (kind, blocked, expected, attempts) in [
        (
            GateIssueKind::Criterion,
            false,
            Status::CompletedWithPending,
            2,
        ),
        (
            GateIssueKind::IndependentVerification,
            true,
            Status::Blocked,
            2,
        ),
        (
            GateIssueKind::UserConfirmation,
            false,
            Status::CompletedWithPending,
            1,
        ),
    ] {
        let (_dir, db, engine, calls, requests) = fixture().await;
        let engine = engine.with_completion_gate(Arc::new(Gate {
            kind,
            blocked,
            lock: Arc::new(tokio::sync::RwLock::new(())),
        }));
        let run = engine.create("gate", spec()).await.unwrap();
        let state = engine.run(&run).await.unwrap();
        assert_eq!(state.status, expected);
        assert_eq!(state.gate_attempts, attempts);
        assert_eq!(calls.load(Ordering::SeqCst), attempts as usize);
        assert!(requests.lock().unwrap().iter().all(|request| {
            serde_json::to_string(request)
                .unwrap()
                .contains("Current synthetic micro-plan")
        }));
        let report = state.delivery_report.unwrap();
        assert!(!report.pending.is_empty());
        assert_eq!(report.assumptions, vec!["Synthetic assumption"]);
        assert_eq!(report.evidence[0].reference, "synthetic:evidence");
        if expected == Status::CompletedWithPending {
            engine.run(&run).await.unwrap();
            assert_eq!(calls.load(Ordering::SeqCst), attempts as usize);
        }
        db.shutdown().await.unwrap();
    }
}
struct RepeatedStream(Arc<AtomicUsize>);
impl ModelGateway for RepeatedStream {
    fn call_bound(&self, _: &ModelRequest) -> Option<excalibur_models::ModelCallBound> {
        Some(excalibur_models::ModelCallBound {
            max_tokens: 1000,
            max_cost_microusd: 0,
        })
    }
    fn complete<'a>(
        &'a self,
        _: ModelRequest,
    ) -> BoxFuture<'a, std::result::Result<ModelResponse, ModelError>> {
        Box::pin(async { panic!("stream required") })
    }
    fn complete_stream<'a>(
        &'a self,
        _: ModelRequest,
        sink: StreamSink,
    ) -> BoxFuture<'a, std::result::Result<ModelResponse, ModelError>> {
        Box::pin(async move {
            self.0.fetch_add(1, Ordering::SeqCst);
            for _ in 0..10 {
                sink(StreamDelta::Text {
                    attempt: 0,
                    delta: "01234567890123456789012345678901234567890123456789".into(),
                });
            }
            std::future::pending().await
        })
    }
}
#[tokio::test]
async fn repeated_stream_interrupts_without_accepting_partial_responses_and_escalates() {
    let (_dir, db, base, _, _) = fixture().await;
    let calls = Arc::new(AtomicUsize::new(0));
    let engine = Engine::new(
        base.journal().clone(),
        Arc::new(RepeatedStream(calls.clone())),
        Arc::new(NoTools),
    )
    .with_stream_sink(Arc::new(|_| {}));
    let run = engine.create("repeat-stream", spec()).await.unwrap();
    let state = tokio::time::timeout(std::time::Duration::from_secs(5), engine.run(&run))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(state.status, Status::Blocked);
    assert_eq!(state.loops.detections, 3);
    assert_eq!(calls.load(Ordering::SeqCst), 3);
    assert!(state.usage.estimated);
    let events = engine.journal().replay(&run, 0).await.unwrap();
    assert_eq!(
        events
            .iter()
            .filter(|event| event.kind == "model_failure")
            .count(),
        3
    );
    assert!(!events.iter().any(|event| event.kind == "model_response"));
    assert!(
        events
            .iter()
            .filter(|event| event.kind == "loop_detection")
            .all(|event| event.payload["rule"] == "R5")
    );
    db.shutdown().await.unwrap();
}
#[tokio::test]
async fn child_reservations_are_atomic_reduce_parent_budget_and_settle_confirmed_usage() {
    let (_dir, db, engine, _, _) = fixture().await;
    let mut parent = spec();
    parent.budget.max_tokens = 1000;
    parent.budget.max_steps = 20;
    parent.budget.max_wall_seconds = 1000;
    parent.budget.max_tool_calls = 100;
    let run = engine.create("parent", parent).await.unwrap();
    let mut child_spec = spec();
    child_spec.budget = Budget {
        max_tokens: 800,
        max_steps: 3,
        max_wall_seconds: 10,
        max_tool_calls: 5,
        ..Default::default()
    };
    let a = engine.create("child-a", child_spec.clone()).await.unwrap();
    let b = engine.create("child-b", child_spec.clone()).await.unwrap();
    let (first, second) = tokio::join!(
        reserve_child_budget(engine.journal(), &run, "a", &a, child_spec.budget.clone()),
        reserve_child_budget(engine.journal(), &run, "b", &b, child_spec.budget.clone())
    );
    assert_ne!(first.is_ok(), second.is_ok());
    let (id, child) = if first.is_ok() { ("a", a) } else { ("b", b) };
    let reserved = engine.load(&run).await.unwrap();
    assert_eq!(remaining_budget(&reserved).max_tokens, 200);
    let other_parent = engine.create("other-parent", spec()).await.unwrap();
    assert!(
        reserve_child_budget(
            engine.journal(),
            &other_parent,
            "duplicate-child",
            &child,
            child_spec.budget.clone()
        )
        .await
        .is_err()
    );
    let mut unauthorized_expansion = child_spec.budget.clone();
    unauthorized_expansion.max_tokens += 1;
    assert!(
        engine
            .update_budget(&child, unauthorized_expansion)
            .await
            .is_err()
    );
    assert!(
        reserve_child_budget(engine.journal(), &run, id, &child, child_spec.budget)
            .await
            .is_ok()
    );
    let done = engine.run(&child).await.unwrap();
    assert_eq!(done.status, Status::Completed);
    assert!(
        reserve_child_budget(engine.journal(), &run, id, &child, done.spec.budget.clone())
            .await
            .is_ok()
    );
    let settled = settle_child_budget(engine.journal(), &run, id)
        .await
        .unwrap();
    assert_eq!(settled.tokens, 3);
    assert_eq!(
        remaining_budget(&engine.load(&run).await.unwrap()).max_tokens,
        997
    );
    assert_eq!(
        settle_child_budget(engine.journal(), &run, id)
            .await
            .unwrap()
            .tokens,
        3
    );
    db.shutdown().await.unwrap();
}

struct HoldingGate {
    started: Arc<tokio::sync::Notify>,
    first: std::sync::atomic::AtomicBool,
    lock: Arc<tokio::sync::RwLock<()>>,
    active: Arc<std::sync::atomic::AtomicBool>,
    interrupted: Arc<AtomicUsize>,
}
struct InspectionLife(Arc<std::sync::atomic::AtomicBool>);
impl Drop for InspectionLife {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}
impl CompletionGate for HoldingGate {
    fn interrupt<'a>(
        &'a self,
        _: &'a str,
        _: bool,
    ) -> BoxFuture<'a, std::result::Result<(), String>> {
        Box::pin(async {
            assert!(
                !self.active.load(Ordering::SeqCst),
                "inspection must drop before child cleanup"
            );
            self.interrupted.fetch_add(1, Ordering::SeqCst);
            Ok(())
        })
    }
    fn inspect<'a>(
        &'a self,
        _: &'a str,
        _: &'a GateInput,
    ) -> BoxFuture<'a, std::result::Result<Box<dyn CompletionGateGuard>, String>> {
        Box::pin(async move {
            if self.first.swap(false, Ordering::SeqCst) {
                self.active.store(true, Ordering::SeqCst);
                let _life = InspectionLife(self.active.clone());
                self.started.notify_one();
                std::future::pending::<()>().await;
            }
            Ok(Box::new(Guard {
                _lock: self.lock.clone().read_owned().await,
                assessment: GateAssessment::default(),
            }) as Box<dyn CompletionGateGuard>)
        })
    }
}
#[tokio::test]
async fn durable_pause_interrupts_a_held_verifier_and_resume_reuses_the_confirmed_response() {
    let (_dir, db, base, calls, _) = fixture().await;
    let started = Arc::new(tokio::sync::Notify::new());
    let interrupted = Arc::new(AtomicUsize::new(0));
    let engine = base.with_completion_gate(Arc::new(HoldingGate {
        started: started.clone(),
        first: std::sync::atomic::AtomicBool::new(true),
        lock: Arc::new(tokio::sync::RwLock::new(())),
        active: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        interrupted: interrupted.clone(),
    }));
    let run = engine.create("held-gate", spec()).await.unwrap();
    let driver = tokio::spawn({
        let engine = engine.clone();
        let run = run.clone();
        async move { engine.run(&run).await.unwrap() }
    });
    started.notified().await;
    assert!(
        engine
            .journal()
            .set_state_if(
                &run,
                &["verifying"],
                "paused",
                Some("user".into()),
                "subtree-pause-fixture"
            )
            .await
            .unwrap()
    );
    let paused = tokio::time::timeout(std::time::Duration::from_secs(2), driver)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(paused.status, Status::Paused);
    assert_eq!(interrupted.load(Ordering::SeqCst), 1);
    assert!(paused.delivery_report.is_some());
    assert!(paused.response.is_some());
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    engine.resume(&run).await.unwrap();
    assert_eq!(engine.run(&run).await.unwrap().status, Status::Completed);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    db.shutdown().await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn gate_wall_deadline_drops_pending_verification_and_runs_cleanup() {
    let (_dir, db, base, calls, _) = fixture().await;
    let started = Arc::new(tokio::sync::Notify::new());
    let interrupted = Arc::new(AtomicUsize::new(0));
    let engine = base.with_completion_gate(Arc::new(HoldingGate {
        started: started.clone(),
        first: std::sync::atomic::AtomicBool::new(true),
        lock: Arc::new(tokio::sync::RwLock::new(())),
        active: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        interrupted: interrupted.clone(),
    }));
    let mut spec = spec();
    spec.budget.max_wall_seconds = 2;
    let run = engine.create("gate-deadline", spec).await.unwrap();
    let driver = tokio::spawn({
        let engine = engine.clone();
        let run = run.clone();
        async move { engine.run(&run).await.unwrap() }
    });
    started.notified().await;
    tokio::time::advance(std::time::Duration::from_secs(3)).await;
    let state = driver.await.unwrap();
    assert_eq!(state.status, Status::Paused);
    assert!(state.reason.unwrap().contains("wall time budget"));
    assert!(state.delivery_report.unwrap().urgent);
    assert_eq!(interrupted.load(Ordering::SeqCst), 1);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    db.shutdown().await.unwrap();
}
#[tokio::test]
async fn budget_allocation_cycles_are_rejected_by_the_same_writer_transaction() {
    let (_dir, db, engine, _, _) = fixture().await;
    let mut parent = spec();
    parent.budget = Budget {
        max_tokens: 100,
        max_steps: 3,
        max_wall_seconds: 3600,
        max_tool_calls: 5,
        ..Default::default()
    };
    let mut child = parent.clone();
    child.budget.max_wall_seconds = 10;
    let a = engine.create("cycle-a", parent.clone()).await.unwrap();
    let b = engine.create("cycle-b", child.clone()).await.unwrap();
    reserve_child_budget(engine.journal(), &a, "first", &b, child.budget)
        .await
        .unwrap();
    let before_a = engine.journal().replay(&a, 0).await.unwrap();
    let before_b = engine.journal().replay(&b, 0).await.unwrap();
    // Exercise the writer's DAG guard directly: a reverse allocation through
    // the engine would already fail its strictly decreasing wall-time budget.
    let error = engine
        .journal()
        .append_child_budget_reservation(
            &b,
            before_b.last().unwrap().sequence,
            "allocation:cycle",
            json!({
                "allocation_id":"cycle",
                "child_sequence":before_a.last().unwrap().sequence,
                "allocation":ChildBudgetAllocation {
                    child_run:a.clone(), budget:parent.budget, settled:None
                }
            }),
        )
        .await
        .unwrap_err();
    assert!(matches!(
        error,
        excalibur_journal::Error::Storage(excalibur_store::Error::Invalid(
            "child budget allocation would create a cycle"
        ))
    ));
    assert_eq!(engine.journal().replay(&a, 0).await.unwrap(), before_a);
    assert_eq!(engine.journal().replay(&b, 0).await.unwrap(), before_b);
    db.shutdown().await.unwrap();
}

#[tokio::test]
async fn child_wall_budget_cannot_exceed_parent_even_before_any_elapsed_time() {
    let (_dir, db, engine, _, _) = fixture().await;
    let mut parent = spec();
    parent.budget.max_wall_seconds = 10;
    let mut child = parent.clone();
    child.budget.max_wall_seconds = 11;
    let a = engine.create("wall-parent", parent).await.unwrap();
    let b = engine.create("wall-child", child.clone()).await.unwrap();
    let before_a = engine.journal().replay(&a, 0).await.unwrap();
    let before_b = engine.journal().replay(&b, 0).await.unwrap();
    assert!(matches!(
        reserve_child_budget(engine.journal(), &a, "excess-wall", &b, child.budget).await,
        Err(Error::Invalid(
            "child budget exceeds unallocated parent budget"
        ))
    ));
    assert_eq!(engine.journal().replay(&a, 0).await.unwrap(), before_a);
    assert_eq!(engine.journal().replay(&b, 0).await.unwrap(), before_b);
    db.shutdown().await.unwrap();
}

struct ResourceTools {
    processes: usize,
    sessions: usize,
}
impl ToolRuntime for ResourceTools {
    fn definitions(&self) -> Vec<ToolDefinition> {
        vec![]
    }
    fn configure_execution_limits<'a>(
        &'a self,
        _: &'a str,
        _: ExecutionLimits,
    ) -> BoxFuture<'a, std::result::Result<(), String>> {
        Box::pin(async { Ok(()) })
    }
    fn execution_resources<'a>(
        &'a self,
        _: &'a str,
    ) -> BoxFuture<'a, std::result::Result<Option<ExecutionResources>, String>> {
        Box::pin(async {
            Ok(Some(ExecutionResources {
                processes: self.processes as u64,
                terminal_sessions: self.sessions as u64,
            }))
        })
    }
    fn prepare<'a>(
        &'a self,
        _: &'a str,
        _: ToolCall,
    ) -> BoxFuture<'a, std::result::Result<PreparedTool, String>> {
        Box::pin(async { Err("no tools".into()) })
    }
    fn execute<'a>(
        &'a self,
        _: &'a str,
        _: &'a str,
        _: &'a PreparedTool,
    ) -> BoxFuture<'a, ToolResult> {
        Box::pin(async { panic!("no tools") })
    }
    fn reconcile<'a>(
        &'a self,
        _: &'a str,
        _: &'a str,
        _: &'a PreparedTool,
    ) -> BoxFuture<'a, Reconcile> {
        Box::pin(async { panic!("no tools") })
    }
}
#[tokio::test]
async fn managed_process_and_terminal_budgets_pause_before_inference_with_urgent_report() {
    for (processes, sessions) in [(1, 0), (0, 1)] {
        let (_dir, db, base, calls, requests) = fixture().await;
        let engine = Engine::new(
            base.journal().clone(),
            Arc::new(FinalModel {
                calls: calls.clone(),
                requests,
            }),
            Arc::new(ResourceTools {
                processes,
                sessions,
            }),
        );
        let mut spec = spec();
        spec.budget.max_processes = Some(1);
        spec.budget.max_terminal_sessions = Some(1);
        let run = engine.create("resource-budget", spec).await.unwrap();
        let state = engine.run(&run).await.unwrap();
        assert_eq!(state.status, Status::Paused);
        assert!(state.reason.unwrap().contains("budget"));
        assert!(state.delivery_report.unwrap().urgent);
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        let mut expanded = engine.load(&run).await.unwrap().spec.budget;
        expanded.max_processes = Some(2);
        expanded.max_terminal_sessions = Some(2);
        engine.update_budget(&run, expanded).await.unwrap();
        engine.resume(&run).await.unwrap();
        assert_eq!(engine.run(&run).await.unwrap().status, Status::Completed);
        db.shutdown().await.unwrap();
    }
}

struct GateObserver {
    lock: Arc<tokio::sync::RwLock<()>>,
    observed: Arc<AtomicUsize>,
}
impl Observer for GateObserver {
    fn reached(&self, phase: Boundary, _: &str, _: u64, _: Option<&str>) {
        if phase == Boundary::GateReportConfirmed {
            assert!(
                self.lock.try_write().is_err(),
                "evidence mutation must stay excluded through the completion CAS"
            );
            self.observed.fetch_add(1, Ordering::SeqCst);
        }
    }
}
#[tokio::test]
async fn external_evidence_guard_is_retained_until_the_terminal_transaction_finishes() {
    let (_dir, db, base, _, _) = fixture().await;
    let lock = Arc::new(tokio::sync::RwLock::new(()));
    let observed = Arc::new(AtomicUsize::new(0));
    let engine = base
        .with_completion_gate(Arc::new(Gate {
            kind: GateIssueKind::UserConfirmation,
            blocked: false,
            lock: lock.clone(),
        }))
        .with_observer(Arc::new(GateObserver {
            lock: lock.clone(),
            observed: observed.clone(),
        }));
    let run = engine.create("guard", spec()).await.unwrap();
    assert_eq!(
        engine.run(&run).await.unwrap().status,
        Status::CompletedWithPending
    );
    assert_eq!(observed.load(Ordering::SeqCst), 1);
    assert!(lock.try_write().is_ok());
    db.shutdown().await.unwrap();
}

struct DirectiveGateObserver {
    started: Arc<tokio::sync::Notify>,
    release: Arc<std::sync::Barrier>,
    first: std::sync::atomic::AtomicBool,
}
struct FollowupDirectiveProjection {
    journal: Journal,
    once: std::sync::atomic::AtomicBool,
}
impl DurableSink for FollowupDirectiveProjection {
    fn record<'a>(
        &'a self,
        run: &'a str,
        event: &'a excalibur_journal::Event,
    ) -> BoxFuture<'a, std::result::Result<(), String>> {
        Box::pin(async move {
            if event.kind == "directive_applied"
                && event.payload["id"] == "first"
                && self.once.swap(false, Ordering::SeqCst)
            {
                self.journal
                    .append(
                        run,
                        "directive_received",
                        "directive:second:received",
                        json!({"id":"second","text":"Projected second instruction"}),
                    )
                    .await
                    .map_err(|e| e.to_string())?;
            }
            Ok(())
        })
    }
}
impl Observer for DirectiveGateObserver {
    fn reached(&self, phase: Boundary, _: &str, _: u64, _: Option<&str>) {
        if phase == Boundary::GateReportConfirmed && self.first.swap(false, Ordering::SeqCst) {
            self.started.notify_one();
            self.release.wait();
        }
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_directive_arriving_after_the_gate_report_forces_a_fresh_response_before_completion() {
    let (_dir, db, base, calls, requests) = fixture().await;
    let started = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(std::sync::Barrier::new(2));
    let engine = base.with_observer(Arc::new(DirectiveGateObserver {
        started: started.clone(),
        release: release.clone(),
        first: std::sync::atomic::AtomicBool::new(true),
    }));
    let run = engine.create("late-directive", spec()).await.unwrap();
    let driver = tokio::spawn({
        let engine = engine.clone();
        let run = run.clone();
        async move { engine.run(&run).await.unwrap() }
    });
    started.notified().await;
    engine
        .journal()
        .append(
            &run,
            "directive_received",
            "directive:late:received",
            json!({"id":"late","text":"New synthetic requirement"}),
        )
        .await
        .unwrap();
    release.wait();
    let done = driver.await.unwrap();
    assert_eq!(done.status, Status::Completed);
    assert_eq!(done.revision, 1);
    assert_eq!(done.request_revision, 1);
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert!(
        serde_json::to_string(requests.lock().unwrap().last().unwrap())
            .unwrap()
            .contains("New synthetic requirement")
    );
    db.shutdown().await.unwrap();
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn projected_followup_directive_is_applied_before_reissuing_an_obsolete_final_response() {
    let (_dir, db, base, calls, requests) = fixture().await;
    let started = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(std::sync::Barrier::new(2));
    let projection = Arc::new(FollowupDirectiveProjection {
        journal: base.journal().clone(),
        once: std::sync::atomic::AtomicBool::new(true),
    });
    let engine =
        base.with_durable_sink(projection)
            .with_observer(Arc::new(DirectiveGateObserver {
                started: started.clone(),
                release: release.clone(),
                first: std::sync::atomic::AtomicBool::new(true),
            }));
    let run = engine.create("projected-directive", spec()).await.unwrap();
    let worker = tokio::spawn({
        let engine = engine.clone();
        let run = run.clone();
        async move { engine.run(&run).await.unwrap() }
    });
    started.notified().await;
    engine
        .journal()
        .append(
            &run,
            "directive_received",
            "directive:first:received",
            json!({"id":"first","text":"First instruction"}),
        )
        .await
        .unwrap();
    release.wait();
    let done = tokio::time::timeout(std::time::Duration::from_secs(5), worker)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(done.status, Status::Completed);
    assert_eq!(done.revision, 2);
    assert_eq!(done.request_revision, 2);
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert!(
        serde_json::to_string(requests.lock().unwrap().last().unwrap())
            .unwrap()
            .contains("Projected second instruction")
    );
    db.shutdown().await.unwrap();
}
#[tokio::test]
async fn confirmed_model_failure_after_a_cut_requires_explicit_accounting_before_new_inference() {
    let (_dir, db, engine, calls, _) = fixture().await;
    let run = engine.create("failure-cut", spec()).await.unwrap();
    engine
        .journal()
        .set_state_if(&run, &["ready"], "running", None, "fixture-start")
        .await
        .unwrap();
    let request = excalibur_context::compile(&spec().context).unwrap().request;
    engine
        .journal()
        .append(
            &run,
            "model_request",
            "model:0:request",
            json!({"request":request,"revision":0}),
        )
        .await
        .unwrap();
    engine
        .journal()
        .append(&run, "model_dispatch", "model:0:dispatch", json!({}))
        .await
        .unwrap();
    engine
        .journal()
        .append(
            &run,
            "model_failure",
            "model:0:failure",
            json!({"failure_class":"rate_limit","error":"synthetic safe failure"}),
        )
        .await
        .unwrap();
    let paused = engine.run(&run).await.unwrap();
    assert_eq!(paused.status, Status::Paused);
    assert!(paused.reason.unwrap().contains("already confirmed"));
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    engine
        .resolve_model_failure(
            &run,
            Usage {
                input_tokens: 10,
                ..Default::default()
            },
            "synthetic host receipt after interruption",
        )
        .await
        .unwrap();
    let done = engine.run(&run).await.unwrap();
    assert_eq!(done.status, Status::Completed);
    assert_eq!(done.usage.input_tokens, 12);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    db.shutdown().await.unwrap();
}
#[tokio::test]
async fn host_failure_before_inference_has_a_durable_terminal_report() {
    let (_dir, db, engine, calls, _) = fixture().await;
    let run = engine.create("setup-failure", spec()).await.unwrap();
    engine
        .fail(&run, "synthetic unrecoverable setup error")
        .await
        .unwrap();
    let state = engine.load(&run).await.unwrap();
    assert_eq!(state.status, Status::Failed);
    assert_eq!(state.delivery_report.unwrap().status, Status::Failed);
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    db.shutdown().await.unwrap();
}

struct ParentAdmission {
    allowed: Arc<std::sync::atomic::AtomicBool>,
    lock: Arc<tokio::sync::RwLock<()>>,
}
impl CompletionGate for ParentAdmission {
    fn admission<'a>(
        &'a self,
        _: &'a str,
    ) -> BoxFuture<'a, std::result::Result<AdmissionState, String>> {
        Box::pin(async {
            Ok(if self.allowed.load(Ordering::SeqCst) {
                AdmissionState::Ready
            } else {
                AdmissionState::Paused("parent subtree is paused".into())
            })
        })
    }
    fn inspect<'a>(
        &'a self,
        _: &'a str,
        _: &'a GateInput,
    ) -> BoxFuture<'a, std::result::Result<Box<dyn CompletionGateGuard>, String>> {
        Box::pin(async {
            Ok(Box::new(Guard {
                _lock: self.lock.clone().read_owned().await,
                assessment: GateAssessment::default(),
            }) as Box<dyn CompletionGateGuard>)
        })
    }
}
#[tokio::test]
async fn parent_admission_fence_prevents_child_inference_until_parent_resumes() {
    let (_dir, db, base, calls, _) = fixture().await;
    let allowed = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let engine = base.with_completion_gate(Arc::new(ParentAdmission {
        allowed: allowed.clone(),
        lock: Arc::new(tokio::sync::RwLock::new(())),
    }));
    let run = engine.create("admission-fence", spec()).await.unwrap();
    assert_eq!(engine.run(&run).await.unwrap().status, Status::Paused);
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    engine.resume(&run).await.unwrap();
    assert_eq!(engine.run(&run).await.unwrap().status, Status::Paused);
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    allowed.store(true, Ordering::SeqCst);
    engine.resume(&run).await.unwrap();
    assert_eq!(engine.run(&run).await.unwrap().status, Status::Completed);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    db.shutdown().await.unwrap();
}
#[tokio::test]
async fn concurrent_child_expansion_and_reservation_cannot_bypass_the_parent_cap() {
    let (_dir, db, engine, _, _) = fixture().await;
    let parent = engine.create("expansion-parent", spec()).await.unwrap();
    let mut child = spec();
    child.budget = Budget {
        max_tokens: 100,
        max_steps: 3,
        max_wall_seconds: 10,
        max_tool_calls: 5,
        ..Default::default()
    };
    let run = engine
        .create("expansion-child", child.clone())
        .await
        .unwrap();
    let mut larger = child.budget.clone();
    larger.max_tokens += 1;
    let (reserved, expanded) = tokio::join!(
        reserve_child_budget(
            engine.journal(),
            &parent,
            "race",
            &run,
            child.budget.clone()
        ),
        engine.update_budget(&run, larger)
    );
    assert!(reserved.is_err() || expanded.is_err());
    if reserved.is_ok() {
        assert_eq!(engine.load(&run).await.unwrap().spec.budget.max_tokens, 100);
    }
    db.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_durable_task_binding_cannot_use_legacy_completion_without_its_task_gate() {
    let (_dir, db, engine, calls, _) = fixture().await;
    let run = engine.create("missing-task-gate", spec()).await.unwrap();
    db.write(excalibur_store::Priority::Normal, {
        let run = run.clone();
        move |c| {
            c.execute_batch(
                "CREATE TABLE task_run_links(run_id TEXT PRIMARY KEY,parent_run_id TEXT)",
            )?;
            c.execute("INSERT INTO task_run_links(run_id) VALUES(?1)", [run])?;
            Ok(())
        }
    })
    .await
    .unwrap();
    let state = engine.run(&run).await.unwrap();
    assert_eq!(state.status, Status::Blocked);
    assert!(state.reason.unwrap().contains("task completion adapter"));
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert!(state.delivery_report.is_some());
    db.shutdown().await.unwrap();
}
