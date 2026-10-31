//! Durable run-stall watchdog (17.6/22.5) with real hung engine drivers, mock
//! model/tools, real temporary SQLite and an injected watchdog clock.
mod support;
use excalibur_engine::{Engine, Status, WATCHDOG_BLOCK_REASON, WATCHDOG_PAUSE_REASON};
use excalibur_journal::Journal;
use excalibur_models::{ModelError, ModelGateway};
use excalibur_orchestrator::{
    Agent, Control, Limits, Orchestrator, RunAuthority, WATCHDOG_PROGRESS_KINDS,
};
use excalibur_security::{Autonomy, Redactor, RunSecurity, Security};
use excalibur_store::{Database, DatabaseKind, Priority, ReadLane, StorageConfig};
use excalibur_types::runtime::*;
use serde_json::{Value, json};
use std::{
    collections::VecDeque,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};

#[derive(Clone, Copy)]
enum Reply {
    Hang,
    Call(&'static str),
    Done,
}
#[derive(Default)]
struct Script {
    replies: Mutex<VecDeque<Reply>>,
    model_entered: AtomicBool,
    /// Number of upcoming `prepare` calls that hang until `release`.
    hang_prepares: Mutex<usize>,
    release: tokio::sync::Notify,
    prepares: AtomicUsize,
    /// The next `prepare` blocks its worker thread synchronously (not at an
    /// await point) until `unblock` is set, so an abort cannot take effect.
    block_prepare_sync: AtomicBool,
    unblock: AtomicBool,
    executions: AtomicUsize,
    execute_entered: AtomicBool,
}
struct Model(Arc<Script>);
impl ModelGateway for Model {
    fn complete<'a>(
        &'a self,
        _: ModelRequest,
    ) -> BoxFuture<'a, std::result::Result<ModelResponse, ModelError>> {
        let reply = self
            .0
            .replies
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or(Reply::Done);
        self.0.model_entered.store(true, Ordering::SeqCst);
        Box::pin(async move {
            let message = match reply {
                Reply::Hang => std::future::pending().await,
                Reply::Done => Message::text("assistant", "Synthetic done"),
                Reply::Call(name) => Message {
                    tool_calls: vec![ToolCall {
                        id: format!("call-{name}"),
                        name: name.into(),
                        arguments: "{}".into(),
                    }],
                    ..Message::text("assistant", "")
                },
            };
            Ok(ModelResponse {
                message,
                usage: Usage::default(),
            })
        })
    }
}
struct Tools(Arc<Script>);
impl ToolRuntime for Tools {
    fn definitions(&self) -> Vec<ToolDefinition> {
        ["probe", "write"]
            .into_iter()
            .map(|name| ToolDefinition {
                name: name.into(),
                description: format!("Synthetic {name}"),
                input_schema: json!({"type":"object"}),
            })
            .collect()
    }
    fn scheduling<'a>(
        &'a self,
        _: &'a str,
        _: &'a ToolCall,
    ) -> BoxFuture<'a, Option<ToolScheduling>> {
        Box::pin(async {
            Some(ToolScheduling {
                resources: vec![],
                exclusive: false,
            })
        })
    }
    fn prepare<'a>(
        &'a self,
        _: &'a str,
        call: ToolCall,
    ) -> BoxFuture<'a, std::result::Result<PreparedTool, String>> {
        Box::pin(async move {
            let hang = {
                let mut remaining = self.0.hang_prepares.lock().unwrap();
                let hang = *remaining > 0;
                *remaining = remaining.saturating_sub(1);
                hang
            };
            self.0.prepares.fetch_add(1, Ordering::SeqCst);
            if self.0.block_prepare_sync.swap(false, Ordering::SeqCst) {
                while !self.0.unblock.load(Ordering::SeqCst) {
                    std::thread::sleep(Duration::from_millis(5));
                }
            }
            if hang {
                // Outside any control select: only an abort or an explicit
                // test release can stop this driver.
                self.0.release.notified().await;
            }
            let effect = if call.name == "write" {
                Effect::NonIdempotent
            } else {
                Effect::Pure
            };
            Ok(PreparedTool {
                call,
                effect,
                resources: vec![],
                exclusive: false,
                data: json!({}),
            })
        })
    }
    fn execute<'a>(
        &'a self,
        _: &'a str,
        _: &'a str,
        prepared: &'a PreparedTool,
    ) -> BoxFuture<'a, ToolResult> {
        Box::pin(async move {
            self.0.executions.fetch_add(1, Ordering::SeqCst);
            self.0.execute_entered.store(true, Ordering::SeqCst);
            if prepared.call.name == "write" {
                std::future::pending::<()>().await;
            }
            ToolResult {
                success: true,
                output: json!("synthetic probe"),
                full_output_blob: None,
            }
        })
    }
    fn reconcile<'a>(
        &'a self,
        _: &'a str,
        _: &'a str,
        _: &'a PreparedTool,
    ) -> BoxFuture<'a, Reconcile> {
        Box::pin(async { Reconcile::Uncertain("synthetic external effect".into()) })
    }
}
struct Authority;
impl RunAuthority for Authority {
    fn bind<'a>(&'a self, _: &'a Agent) -> BoxFuture<'a, std::result::Result<(), String>> {
        Box::pin(async { Ok(()) })
    }
}

struct Fixture {
    dir: tempfile::TempDir,
    db: Database,
    host: Orchestrator,
    master: Agent,
    script: Arc<Script>,
}
async fn open(
    dir: tempfile::TempDir,
    script: Arc<Script>,
    boot_at: i64,
    autonomy: Autonomy,
) -> Fixture {
    let db = Database::open(
        dir.path().join("main.db"),
        DatabaseKind::Main,
        StorageConfig::default(),
    )
    .unwrap();
    let journal = Journal::new(db.clone()).await.unwrap();
    let engine = Arc::new(Engine::new(
        journal,
        Arc::new(Model(script.clone())),
        Arc::new(Tools(script.clone())),
    ));
    let host = Orchestrator::open(
        db.clone(),
        engine,
        Limits {
            running: 4,
            ..Default::default()
        },
    )
    .await
    .unwrap()
    .with_authority(Arc::new(Authority));
    let master = host
        .rebind_master_at_startup("Excalibur", support::token())
        .await
        .unwrap();
    let boot = host.recovery().begin(boot_at).await.unwrap();
    host.recover_startup(&boot, autonomy).await.unwrap();
    Fixture {
        dir,
        db,
        host,
        master,
        script,
    }
}
async fn fixture(replies: Vec<Reply>) -> Fixture {
    let script = Arc::new(Script {
        replies: Mutex::new(replies.into()),
        ..Default::default()
    });
    open(
        tempfile::tempdir().unwrap(),
        script,
        excalibur_store::unix_seconds(),
        Autonomy::Autonomous,
    )
    .await
}
async fn authorize(f: &Fixture, run: &str) {
    Security::new(f.db.clone(), support::token(), Redactor::default())
        .await
        .unwrap()
        .register_run(
            run,
            RunSecurity {
                parent: None,
                project: "general".into(),
                workspace: f.dir.path().canonicalize().unwrap(),
                token: support::token(),
                autonomy: vec![Autonomy::Autonomous],
                full_acknowledged: false,
                user_requested: true,
            },
            excalibur_store::unix_seconds(),
        )
        .await
        .unwrap();
}
async fn spawn(f: &Fixture, name: &str) -> (String, String) {
    let mut task = support::delegation(name, "general", 2000);
    task.profile = "coordinator".into();
    let agent = f.host.spawn(&f.master.id, task).await.unwrap();
    let run = agent.run_id.clone().unwrap();
    authorize(f, &run).await;
    (agent.id, run)
}
async fn until(flag: impl Fn() -> bool) {
    for _ in 0..1000 {
        if flag() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("condition was not reached");
}
/// The latest heartbeat or confirmed driver-progress timestamp of the run.
async fn base(db: &Database, run: &str) -> i64 {
    let r = run.to_owned();
    db.read(ReadLane::Interactive, move |c| {
        Ok(c.query_row(
            &format!("SELECT max(COALESCE((SELECT heartbeat_at FROM runs WHERE id=?1),0),COALESCE((SELECT MAX(created_at) FROM journal WHERE run_id=?1 AND (kind IN ({WATCHDOG_PROGRESS_KINDS}) OR (kind='state_change' AND json_extract(payload,'$.state')='running'))),0))"),
            [r],
            |row| row.get(0),
        )?)
    })
    .await
    .unwrap()
}
async fn last_intent(db: &Database, run: &str) -> i64 {
    let r = run.to_owned();
    db.read(ReadLane::Interactive, move |c| {
        Ok(c.query_row(
            "SELECT MAX(created_at) FROM journal WHERE run_id=?1 AND kind='tool_intent'",
            [r],
            |row| row.get(0),
        )?)
    })
    .await
    .unwrap()
}
async fn count(db: &Database, run: &str, kind: &str) -> u64 {
    let (r, k) = (run.to_owned(), kind.to_owned());
    db.read(ReadLane::Interactive, move |c| {
        Ok(c.query_row(
            "SELECT count(*) FROM journal WHERE run_id=?1 AND kind=?2",
            [r, k],
            |row| row.get(0),
        )?)
    })
    .await
    .unwrap()
}
async fn alerts(host: &Orchestrator, run: &str) -> Vec<(String, Value)> {
    host.notifications("conversation:test")
        .await
        .unwrap()
        .into_iter()
        .filter_map(|n| {
            let content: Value = serde_json::from_str(&n.content).ok()?;
            (content["kind"] == "run_watchdog" && content["run_id"] == run)
                .then_some((n.class, content))
        })
        .collect()
}
async fn settle(
    host: &Orchestrator,
    run: &str,
    at: i64,
) -> excalibur_orchestrator::WatchdogIncident {
    for _ in 0..1000 {
        host.watchdog_runs(at).await.unwrap();
        let incidents = host.watchdog_incidents(Some(run)).await.unwrap();
        if incidents.first().is_some_and(|i| i.notified) {
            return incidents[0].clone();
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("watchdog incident was not settled");
}

#[tokio::test]
async fn hung_model_is_interrupted_at_the_boundary_paused_and_alerted_once_across_reopen() {
    let f = fixture(vec![Reply::Hang]).await;
    let (agent, run) = spawn(&f, "hung-model").await;
    assert_eq!(f.host.tick().await.unwrap(), vec![agent.clone()]);
    until(|| f.script.model_entered.load(Ordering::SeqCst)).await;
    // The driver renewed the run heartbeat at its step boundary.
    let r = run.clone();
    let heartbeat: Option<i64> =
        f.db.read(ReadLane::Interactive, move |c| {
            Ok(
                c.query_row("SELECT heartbeat_at FROM runs WHERE id=?1", [r], |row| {
                    row.get(0)
                })?,
            )
        })
        .await
        .unwrap();
    assert!(heartbeat.is_some());
    let at = base(&f.db, &run).await;
    assert!(f.host.watchdog_runs(at + 599).await.unwrap().is_empty());
    assert!(f.host.watchdog_incidents(None).await.unwrap().is_empty());
    assert_eq!(
        f.host.watchdog_runs(at + 600).await.unwrap(),
        vec![agent.clone()]
    );
    let incident = settle(&f.host, &run, at + 601).await;
    assert_eq!(incident.action.as_deref(), Some("paused"));
    assert_eq!(incident.decision.as_deref(), Some("paused"));
    // The unknown provider charge is reserved conservatively; with no declared
    // call bound it exhausts the token budget, so the 17.3 assessment pauses.
    assert!(
        incident
            .reason
            .as_deref()
            .unwrap()
            .contains("remaining budget is insufficient")
    );
    // The live driver observed the durable pause and recorded the interruption.
    assert!(!incident.aborted);
    assert_eq!(count(&f.db, &run, "model_interrupted").await, 1);
    let state = f.host.engine().load(&run).await.unwrap();
    assert_eq!(state.status, Status::Paused);
    assert_eq!(state.reason.as_deref(), Some(WATCHDOG_PAUSE_REASON));
    assert!(state.recovery_note.unwrap().contains("run_watchdog"));
    // Concurrent passes and later passes neither interrupt nor alert again.
    let (a, b) = tokio::join!(
        f.host.watchdog_runs(at + 2000),
        f.host.watchdog_runs(at + 2000)
    );
    assert!(a.unwrap().is_empty() && b.unwrap().is_empty());
    let found = alerts(&f.host, &run).await;
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].0, "urgent");
    assert_eq!(found[0].1["decision"], "paused");
    assert_eq!(found[0].1["scope"], "general");
    let state_changes = count(&f.db, &run, "state_change").await;
    f.host.shutdown().await.unwrap();
    let Fixture { dir, db, host, .. } = f;
    drop(host);
    db.shutdown().await.unwrap();
    drop(db);
    let f = open(
        dir,
        Arc::new(Script::default()),
        excalibur_store::unix_seconds(),
        Autonomy::Autonomous,
    )
    .await;
    f.host.watchdog_runs(at + 3000).await.unwrap();
    assert_eq!(alerts(&f.host, &run).await.len(), 1);
    assert_eq!(f.host.watchdog_incidents(None).await.unwrap().len(), 1);
    // Startup preserved the watchdog-owned pause instead of resuming it.
    assert_eq!(count(&f.db, &run, "state_change").await, state_changes);
    assert_eq!(
        f.host.engine().load(&run).await.unwrap().reason.as_deref(),
        Some(WATCHDOG_PAUSE_REASON)
    );
    f.host.shutdown().await.unwrap();
    f.db.shutdown().await.unwrap();
}

#[tokio::test]
async fn non_cooperative_driver_is_aborted_reconciled_and_resumed_once_to_completion() {
    let f = fixture(vec![Reply::Call("probe"), Reply::Done]).await;
    *f.script.hang_prepares.lock().unwrap() = 1;
    let (agent, run) = spawn(&f, "hung-prepare").await;
    assert_eq!(f.host.tick().await.unwrap(), vec![agent.clone()]);
    until(|| f.script.prepares.load(Ordering::SeqCst) >= 1).await;
    let at = base(&f.db, &run).await;
    assert_eq!(
        f.host.watchdog_runs(at + 600).await.unwrap(),
        vec![agent.clone()]
    );
    // Inside the cooperative grace window the hung driver is not yet aborted.
    f.host.watchdog_runs(at + 601).await.unwrap();
    let pending = f.host.watchdog_incidents(Some(&run)).await.unwrap();
    assert!(!pending[0].aborted && pending[0].decision.is_none());
    let incident = settle(&f.host, &run, at + 606).await;
    assert!(incident.aborted);
    assert_eq!(incident.decision.as_deref(), Some("resumed"));
    let found = alerts(&f.host, &run).await;
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].0, "normal");
    assert_eq!(
        f.host.engine().load(&run).await.unwrap().status,
        Status::Ready
    );
    // The aborted task was not converted into a failure; the resumed run
    // reuses the recorded model response and completes.
    for _ in 0..1000 {
        f.host.tick().await.unwrap();
        if f.host.engine().load(&run).await.unwrap().status == Status::Completed {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let state = f.host.engine().load(&run).await.unwrap();
    assert_eq!(state.status, Status::Completed);
    assert_eq!(count(&f.db, &run, "model_response").await, 2);
    assert_eq!(f.script.executions.load(Ordering::SeqCst), 1);
    assert_eq!(count(&f.db, &run, "recovery_note").await, 1);
    f.host.shutdown().await.unwrap();
    f.db.shutdown().await.unwrap();
}

#[tokio::test]
async fn registered_tool_wait_is_exempt_then_uncertain_effect_blocks_without_retry() {
    let f = fixture(vec![Reply::Call("write")]).await;
    let (agent, run) = spawn(&f, "hung-write").await;
    assert_eq!(f.host.tick().await.unwrap(), vec![agent.clone()]);
    until(|| f.script.execute_entered.load(Ordering::SeqCst)).await;
    let at = base(&f.db, &run).await;
    let intent = last_intent(&f.db, &run).await;
    let timeout = f
        .host
        .engine()
        .load(&run)
        .await
        .unwrap()
        .spec
        .budget
        .tool_timeout_seconds as i64;
    // Ten minutes after the last progress, but still inside the registered
    // tool timeout plus ten minutes: not stalled.
    assert!(at <= intent + timeout);
    assert!(f.host.watchdog_runs(at + 600).await.unwrap().is_empty());
    assert!(
        f.host
            .watchdog_runs(intent + timeout + 599)
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        f.host.watchdog_runs(intent + timeout + 600).await.unwrap(),
        vec![agent.clone()]
    );
    let at = intent;
    let state = f.host.engine().load(&run).await.unwrap();
    assert_eq!(state.status, Status::Blocked);
    assert_eq!(state.reason.as_deref(), Some(WATCHDOG_BLOCK_REASON));
    let incident = settle(&f.host, &run, at + timeout + 606).await;
    assert!(incident.aborted);
    assert_eq!(incident.action.as_deref(), Some("blocked"));
    assert_eq!(incident.decision.as_deref(), Some("blocked"));
    let found = alerts(&f.host, &run).await;
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].0, "urgent");
    // The uncertain non-idempotent effect is neither retried nor resolved.
    for _ in 0..3 {
        f.host.tick().await.unwrap();
    }
    assert_eq!(f.script.executions.load(Ordering::SeqCst), 1);
    assert_eq!(count(&f.db, &run, "tool_intent").await, 1);
    assert_eq!(count(&f.db, &run, "tool_result").await, 0);
    assert_eq!(
        f.host.engine().load(&run).await.unwrap().status,
        Status::Blocked
    );
    f.host.shutdown().await.unwrap();
    f.db.shutdown().await.unwrap();
}

#[tokio::test]
async fn user_control_and_stale_revisions_win_over_the_watchdog() {
    let f = fixture(vec![Reply::Hang]).await;
    let (agent, run) = spawn(&f, "user-pause").await;
    assert_eq!(f.host.tick().await.unwrap(), vec![agent.clone()]);
    until(|| f.script.model_entered.load(Ordering::SeqCst)).await;
    let state = f.host.engine().load(&run).await.unwrap();
    // A newer journal fact makes an older observed revision stale.
    f.host
        .engine()
        .journal()
        .append(&run, "loop_detection", "fixture:newer-fact", json!({}))
        .await
        .unwrap();
    assert!(
        f.host
            .engine()
            .interrupt_stalled_revision(&run, state.last_sequence)
            .await
            .unwrap()
            .is_none()
    );
    f.host.control(&agent, Control::Pause).await.unwrap();
    let at = base(&f.db, &run).await;
    for _ in 0..3 {
        assert!(f.host.watchdog_runs(at + 5000).await.unwrap().is_empty());
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(f.host.watchdog_incidents(None).await.unwrap().is_empty());
    assert!(alerts(&f.host, &run).await.is_empty());
    assert_eq!(
        f.host.engine().load(&run).await.unwrap().reason.as_deref(),
        Some("user")
    );
    f.host.shutdown().await.unwrap();
    f.db.shutdown().await.unwrap();
}

#[tokio::test]
async fn watchdog_schema_is_additive_and_tampering_fails_closed() {
    let f = fixture(vec![]).await;
    let columns: Vec<String> =
        f.db.read(ReadLane::Interactive, |c| {
            let mut q =
                c.prepare("SELECT name FROM pragma_table_info('orchestration_run_watchdog')")?;
            Ok(q.query_map([], |r| r.get(0))?
                .collect::<std::result::Result<Vec<_>, _>>()?)
        })
        .await
        .unwrap();
    for column in [
        "action",
        "decision",
        "reason",
        "aborted",
        "notified",
        "stalled_since",
    ] {
        assert!(columns.iter().any(|c| c == column));
    }
    // Reopening applies nothing twice.
    let engine = f.host.engine().clone();
    Orchestrator::open(f.db.clone(), engine.clone(), Limits::default())
        .await
        .unwrap();
    f.db.write(Priority::Journal, |c| {
        c.execute(
            "UPDATE orchestration_run_watchdog_schema SET checksum='tampered' WHERE version=2",
            [],
        )?;
        Ok(())
    })
    .await
    .unwrap();
    assert!(
        Orchestrator::open(f.db.clone(), engine, Limits::default())
            .await
            .is_err()
    );
    f.host.shutdown().await.unwrap();
    f.db.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_run_is_resumed_automatically_at_most_once() {
    let f = fixture(vec![Reply::Call("probe"), Reply::Done]).await;
    *f.script.hang_prepares.lock().unwrap() = 2;
    let (agent, run) = spawn(&f, "twice-stalled").await;
    assert_eq!(f.host.tick().await.unwrap(), vec![agent.clone()]);
    until(|| f.script.prepares.load(Ordering::SeqCst) >= 1).await;
    let at = base(&f.db, &run).await;
    assert_eq!(
        f.host.watchdog_runs(at + 600).await.unwrap(),
        vec![agent.clone()]
    );
    let first = settle(&f.host, &run, at + 606).await;
    assert_eq!(first.decision.as_deref(), Some("resumed"));
    // The redispatched driver hangs in the same place again.
    for _ in 0..1000 {
        if !f.host.tick().await.unwrap().is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    until(|| f.script.prepares.load(Ordering::SeqCst) >= 2).await;
    let again = base(&f.db, &run).await;
    assert_eq!(
        f.host.watchdog_runs(again + 600).await.unwrap(),
        vec![agent.clone()]
    );
    let second = settle(&f.host, &run, again + 606).await;
    assert_ne!(second.sequence, first.sequence);
    assert!(second.aborted);
    assert_eq!(second.decision.as_deref(), Some("paused"));
    assert!(
        second
            .reason
            .as_deref()
            .unwrap()
            .contains("an earlier watchdog incident already resumed this run")
    );
    let state = f.host.engine().load(&run).await.unwrap();
    assert_eq!(state.status, Status::Paused);
    assert_eq!(state.reason.as_deref(), Some(WATCHDOG_PAUSE_REASON));
    assert_eq!(alerts(&f.host, &run).await.len(), 2);
    f.host.shutdown().await.unwrap();
    f.db.shutdown().await.unwrap();
}

/// The process stops after the durable interruption and before settlement;
/// settlement in the next process applies the 17.3 downtime and autonomy rules.
async fn restart_before_settlement(downtime: i64, autonomy: Autonomy) -> (String, Status) {
    let f = fixture(vec![Reply::Call("probe"), Reply::Done]).await;
    *f.script.hang_prepares.lock().unwrap() = 1;
    let (agent, run) = spawn(&f, "restart").await;
    assert_eq!(f.host.tick().await.unwrap(), vec![agent.clone()]);
    until(|| f.script.prepares.load(Ordering::SeqCst) >= 1).await;
    let at = base(&f.db, &run).await;
    assert_eq!(
        f.host.watchdog_runs(at + 600).await.unwrap(),
        vec![agent.clone()]
    );
    // Let the observed driver stop on its own, without another watchdog pass.
    f.script.release.notify_one();
    for _ in 0..1000 {
        if f.host.reap().await.unwrap() > 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let pending = f.host.watchdog_incidents(Some(&run)).await.unwrap();
    assert_eq!(pending.len(), 1);
    assert!(pending[0].decision.is_none() && !pending[0].aborted);
    f.host.shutdown().await.unwrap();
    let Fixture { dir, db, host, .. } = f;
    drop(host);
    db.shutdown().await.unwrap();
    drop(db);
    let boot_at = excalibur_store::unix_seconds() + downtime;
    let f = open(dir, Arc::new(Script::default()), boot_at, autonomy).await;
    // Startup preserved the watchdog-owned pause for the watchdog to settle.
    assert_eq!(
        f.host.engine().load(&run).await.unwrap().reason.as_deref(),
        Some(WATCHDOG_PAUSE_REASON)
    );
    let incident = settle(&f.host, &run, boot_at + 1).await;
    assert_eq!(alerts(&f.host, &run).await.len(), 1);
    let status = f.host.engine().load(&run).await.unwrap().status;
    f.host.shutdown().await.unwrap();
    f.db.shutdown().await.unwrap();
    (
        format!(
            "{}: {}",
            incident.decision.unwrap(),
            incident.reason.unwrap_or_default()
        ),
        status,
    )
}

#[tokio::test]
async fn settlement_after_restart_applies_downtime_and_autonomy_rules() {
    let (decision, status) = restart_before_settlement(60, Autonomy::Autonomous).await;
    assert!(decision.starts_with("resumed"), "{decision}");
    assert_eq!(status, Status::Ready);
    let (decision, status) = restart_before_settlement(7 * 3600, Autonomy::Autonomous).await;
    assert!(decision.starts_with("paused"), "{decision}");
    assert!(
        decision.contains("interruption lasted six hours or more"),
        "{decision}"
    );
    assert_eq!(status, Status::Paused);
    let (decision, status) = restart_before_settlement(60, Autonomy::Supervised).await;
    assert!(decision.starts_with("paused"), "{decision}");
    assert!(
        decision.contains("host autonomy is below autonomous"),
        "{decision}"
    );
    assert_eq!(status, Status::Paused);
}

#[tokio::test]
async fn an_intent_without_a_confirmed_interruption_is_closed_without_alert() {
    let f = fixture(vec![]).await;
    let (agent, run) = spawn(&f, "not-applied").await;
    let sequence = f.host.engine().load(&run).await.unwrap().last_sequence;
    // Durable state after a loss between the host intent row and the CAS.
    let (r, a) = (run.clone(), agent.clone());
    f.db.write(Priority::Journal, move |c| {
        c.execute(
            "INSERT INTO orchestration_run_watchdog(run_id,sequence,agent_id,observed_at) VALUES(?1,?2,?3,0)",
            excalibur_store::rusqlite::params![r, sequence, a],
        )?;
        Ok(())
    })
    .await
    .unwrap();
    f.host
        .watchdog_runs(excalibur_store::unix_seconds())
        .await
        .unwrap();
    let incident = &f.host.watchdog_incidents(Some(&run)).await.unwrap()[0];
    assert_eq!(incident.decision.as_deref(), Some("not_applied"));
    assert!(incident.notified && incident.action.is_none());
    assert!(alerts(&f.host, &run).await.is_empty());
    assert_ne!(
        f.host.engine().load(&run).await.unwrap().reason.as_deref(),
        Some(WATCHDOG_PAUSE_REASON)
    );
    f.host.shutdown().await.unwrap();
    f.db.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_user_resume_after_give_up_is_not_failed_by_the_late_cancellation() {
    let f = fixture(vec![Reply::Call("probe"), Reply::Done]).await;
    f.script.block_prepare_sync.store(true, Ordering::SeqCst);
    let (agent, run) = spawn(&f, "sync-stuck").await;
    assert_eq!(f.host.tick().await.unwrap(), vec![agent.clone()]);
    until(|| f.script.prepares.load(Ordering::SeqCst) >= 1).await;
    let at = base(&f.db, &run).await;
    assert_eq!(
        f.host.watchdog_runs(at + 600).await.unwrap(),
        vec![agent.clone()]
    );
    f.host.watchdog_runs(at + 605).await.unwrap();
    assert!(f.host.watchdog_incidents(Some(&run)).await.unwrap()[0].aborted);
    // The aborted task is stuck in synchronous code: give up after 60 s.
    let incident = settle(&f.host, &run, at + 661).await;
    assert_eq!(incident.decision.as_deref(), Some("paused"));
    assert!(
        incident
            .reason
            .unwrap()
            .contains("did not stop after abort")
    );
    // The user resumes; this waits for the run boundary the old task holds.
    let engine = f.host.engine().clone();
    let target = run.clone();
    let resume = tokio::spawn(async move { engine.resume(&target).await });
    tokio::time::sleep(Duration::from_millis(50)).await;
    f.script.unblock.store(true, Ordering::SeqCst);
    resume.await.unwrap().unwrap();
    for _ in 0..1000 {
        if f.host.reap().await.unwrap() > 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    // The late cancellation of the aborted task does not fail the resumed run.
    assert_eq!(
        f.host.engine().load(&run).await.unwrap().status,
        Status::Ready
    );
    for _ in 0..1000 {
        f.host.tick().await.unwrap();
        if f.host.engine().load(&run).await.unwrap().status == Status::Completed {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(
        f.host.engine().load(&run).await.unwrap().status,
        Status::Completed
    );
    f.host.shutdown().await.unwrap();
    f.db.shutdown().await.unwrap();
}

#[tokio::test]
async fn downtime_is_measured_from_the_stall_reference_not_detection() {
    // Detection is delayed by seven hours (for example a suspended host):
    // the stall began long before the incident was observed.
    let f = fixture(vec![Reply::Call("probe"), Reply::Done]).await;
    *f.script.hang_prepares.lock().unwrap() = 1;
    let (agent, run) = spawn(&f, "late-detection").await;
    assert_eq!(f.host.tick().await.unwrap(), vec![agent.clone()]);
    until(|| f.script.prepares.load(Ordering::SeqCst) >= 1).await;
    let at = base(&f.db, &run).await + 7 * 3600;
    assert_eq!(f.host.watchdog_runs(at).await.unwrap(), vec![agent.clone()]);
    let incident = settle(&f.host, &run, at + 6).await;
    assert_eq!(incident.decision.as_deref(), Some("paused"));
    assert!(
        incident
            .reason
            .unwrap()
            .contains("interruption lasted six hours or more")
    );
    f.host.shutdown().await.unwrap();
    f.db.shutdown().await.unwrap();
}
