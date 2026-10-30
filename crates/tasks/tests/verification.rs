use excalibur_journal::Journal;
use excalibur_store::{Database, DatabaseKind, ReadLane, StorageConfig};
use excalibur_tasks::{TaskStore, verification::*};
use excalibur_types::runtime::*;
use serde_json::json;
use std::{
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

struct SlowRuntime {
    executions: Arc<AtomicUsize>,
}
struct SteadyRuntime {
    delay_ms: u64,
    executions: Arc<AtomicUsize>,
}
impl ToolRuntime for SteadyRuntime {
    fn definitions(&self) -> Vec<ToolDefinition> {
        vec![]
    }
    fn prepare<'a>(
        &'a self,
        _run: &'a str,
        call: ToolCall,
    ) -> BoxFuture<'a, Result<PreparedTool, String>> {
        Box::pin(async move {
            Ok(PreparedTool {
                call,
                effect: Effect::NonIdempotent,
                resources: vec![],
                exclusive: false,
                data: json!({}),
            })
        })
    }
    fn execute<'a>(
        &'a self,
        _run: &'a str,
        _key: &'a str,
        _tool: &'a PreparedTool,
    ) -> BoxFuture<'a, ToolResult> {
        self.executions.fetch_add(1, Ordering::SeqCst);
        let delay = self.delay_ms;
        Box::pin(async move {
            tokio::time::sleep(Duration::from_millis(delay)).await;
            ToolResult::ok(json!({"exited": true, "exit_code": 0}))
        })
    }
    fn reconcile<'a>(
        &'a self,
        _run: &'a str,
        _key: &'a str,
        _tool: &'a PreparedTool,
    ) -> BoxFuture<'a, Reconcile> {
        Box::pin(async { Reconcile::Uncertain("synthetic stall".into()) })
    }
}
impl ToolRuntime for SlowRuntime {
    fn definitions(&self) -> Vec<ToolDefinition> {
        vec![]
    }
    fn prepare<'a>(
        &'a self,
        _run: &'a str,
        call: ToolCall,
    ) -> BoxFuture<'a, Result<PreparedTool, String>> {
        Box::pin(async move {
            Ok(PreparedTool {
                call,
                effect: Effect::NonIdempotent,
                resources: vec![],
                exclusive: false,
                data: json!({}),
            })
        })
    }
    fn execute<'a>(
        &'a self,
        _run: &'a str,
        _key: &'a str,
        _tool: &'a PreparedTool,
    ) -> BoxFuture<'a, ToolResult> {
        self.executions.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move {
            tokio::time::sleep(Duration::from_secs(30)).await;
            ToolResult::ok(json!({}))
        })
    }
    fn reconcile<'a>(
        &'a self,
        _run: &'a str,
        _key: &'a str,
        _tool: &'a PreparedTool,
    ) -> BoxFuture<'a, Reconcile> {
        Box::pin(async { Reconcile::Uncertain("synthetic stall".into()) })
    }
}

struct Fixture {
    _temp: tempfile::TempDir,
    db: Database,
    store: TaskStore,
    root: PathBuf,
    run: String,
    runner: CheckRunner,
    executions: Arc<AtomicUsize>,
}

async fn fixture() -> Fixture {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("project");
    std::fs::create_dir(&root).unwrap();
    let db = Database::open(
        temp.path().join("data/main.db"),
        DatabaseKind::Main,
        StorageConfig::default(),
    )
    .unwrap();
    let store = TaskStore::open(db.clone()).await.unwrap();
    let journal = Journal::new(db.clone()).await.unwrap();
    let run = journal
        .create_run("root", json!({"test": true}))
        .await
        .unwrap();
    store
        .host_batch(
            "general",
            "seed",
            json!({"ops":[
                {"op":"create_task","id":"task","title":"Synthetic work","data":{"request_card":{"original":"Check the synthetic source"},"metadata":{"workspace":root}}},
                {"op":"create_requirement","id":"req","data":{"text":"Synthetic source is correct","state":"accepted"}},
                {"op":"create_criterion","id":"criterion","data":{"requirement_id":"req","statement":"Synthetic check passes","method":"test","impact_area":[],"metadata":{"implicit_final_response":false}}},
                {"op":"link_requirement","task":"task","requirement":"req"},
                {"op":"create_check","id":"check","data":{"criterion_id":"criterion","definition":{"kind":"test","command":"synthetic-stall","timeout_ms":250}}},
            ]}),
        )
        .await
        .unwrap();
    store.bind_root("task", &run).await.unwrap();
    let executions = Arc::new(AtomicUsize::new(0));
    let runtime = Arc::new(SlowRuntime {
        executions: executions.clone(),
    });
    let runner =
        CheckRunner::new(runtime, root.clone()).with_attempt_grace(Duration::from_millis(300));
    Fixture {
        _temp: temp,
        db,
        store,
        root,
        run,
        runner,
        executions,
    }
}

#[tokio::test]
async fn timed_out_check_expires_attempt_and_never_repeats_implicitly() {
    let f = fixture().await;
    assert!(f.root.is_dir());
    let err = f
        .runner
        .run_keyed(&f.store, "task", "check", &f.run, "op-timeout")
        .await
        .expect_err("stalled check must time out")
        .to_string();
    assert!(
        err.contains("timed out") && !err.contains("retry failed"),
        "expected a single-shot timeout error, got: {err}"
    );
    // The inner deadline fires with live timers after the execution started,
    // so the attempt expires and fails fast instead of dispatching again.
    assert_eq!(f.executions.load(Ordering::SeqCst), 1);
    // The terminal marking is detached from the failure path; wait for it.
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    let states = loop {
        let states: Vec<String> = f
            .db
            .read(ReadLane::Interactive, {
                move |c| {
                    let mut q = c.prepare(
                        "SELECT state FROM task_check_attempts WHERE operation_key LIKE '%:op-timeout%' ORDER BY operation_key",
                    )?;
                    Ok(q
                        .query_map([], |r| r.get(0))?
                        .collect::<std::result::Result<Vec<_>, _>>()?)
                }
            })
            .await
            .unwrap();
        if states == ["expired".to_string()] || tokio::time::Instant::now() > deadline {
            break states;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    };
    assert_eq!(states, vec!["expired".to_string()]);
    // The completion gate no longer mistakes the abandoned attempt for
    // in-flight work.
    let pending: Vec<String> = f
        .db
        .read(ReadLane::Interactive, {
            let run = f.run.clone();
            move |c| {
                let mut q = c.prepare(
                    "SELECT a.check_id FROM task_check_attempts a WHERE a.run_id=?1 AND a.state='intent'",
                )?;
                Ok(q
                    .query_map([run], |r| r.get(0))?
                    .collect::<std::result::Result<Vec<_>, _>>()?)
            }
        })
        .await
        .unwrap();
    assert!(pending.is_empty(), "unexpected pending checks: {pending:?}");
    // Repeating the same operation key goes through reconcile and is refused
    // instead of dispatching the command again.
    let repeat = f
        .runner
        .run_keyed(&f.store, "task", "check", &f.run, "op-timeout")
        .await
        .expect_err("repeat must not dispatch implicitly")
        .to_string();
    assert!(
        repeat.contains("explicit new attempt required"),
        "expected an explicit-attempt error, got: {repeat}"
    );
    // The repeat went through reconcile, which refused without dispatching
    // the command again: still exactly the single execution from the first run.
    assert_eq!(f.executions.load(Ordering::SeqCst), 1);
    f.db.shutdown().await.unwrap();
}

#[tokio::test]
async fn check_longer_than_grace_completes_once_within_its_timeout() {
    // A fixed setup grace must not cut a legitimate check that runs longer
    // than the grace but below its declared timeout: execution is bounded by
    // deadline plus grace, and a dispatched attempt is never retried.
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("project");
    std::fs::create_dir(&root).unwrap();
    let db = Database::open(
        temp.path().join("data/main.db"),
        DatabaseKind::Main,
        StorageConfig::default(),
    )
    .unwrap();
    let store = TaskStore::open(db.clone()).await.unwrap();
    let journal = Journal::new(db.clone()).await.unwrap();
    let run = journal
        .create_run("root", json!({"test": true}))
        .await
        .unwrap();
    store
        .host_batch(
            "general",
            "seed",
            json!({"ops":[
                {"op":"create_task","id":"task","title":"Synthetic work","data":{"request_card":{"original":"Check the synthetic source"},"metadata":{"workspace":root}}},
                {"op":"create_requirement","id":"req","data":{"text":"Synthetic source is correct","state":"accepted"}},
                {"op":"create_criterion","id":"criterion","data":{"requirement_id":"req","statement":"Synthetic check passes","method":"test","impact_area":[],"metadata":{"implicit_final_response":false}}},
                {"op":"create_check","id":"check","data":{"criterion_id":"criterion","definition":{"kind":"test","command":"synthetic-slow","timeout_ms":5000}}},
                {"op":"link_requirement","task":"task","requirement":"req"}
            ]}),
        )
        .await
        .unwrap();
    store.bind_root("task", &run).await.unwrap();
    let executions = Arc::new(AtomicUsize::new(0));
    let runtime = Arc::new(SteadyRuntime {
        delay_ms: 600,
        executions: executions.clone(),
    });
    let runner =
        CheckRunner::new(runtime, root.clone()).with_attempt_grace(Duration::from_millis(300));
    let evidence = runner
        .run_keyed(&store, "task", "check", &run, "op-long")
        .await
        .expect("a check below its declared timeout must complete");
    assert_eq!(
        evidence.result,
        excalibur_tasks::evidence::EvidenceResult::Pass
    );
    assert_eq!(
        executions.load(Ordering::SeqCst),
        1,
        "a dispatched check must run exactly once"
    );
    db.shutdown().await.unwrap();
}

#[tokio::test]
async fn wedged_setup_before_any_timer_fails_bounded_with_no_attempt_row() {
    let f = fixture().await;
    // Hold the single Interactive read permit. The attempt setup parks on
    // its first read before any timer exists; the end-to-end setup bound
    // must fire, expire (a no-op: no row was ever inserted), and fail
    // bounded instead of parking forever.
    let (started_tx, started_rx) = std::sync::mpsc::channel::<()>();
    let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
    let holder_db = f.db.clone();
    let holder = tokio::spawn(async move {
        holder_db
            .read(ReadLane::Interactive, move |_| {
                started_tx.send(()).unwrap();
                let _ = release_rx.recv_timeout(Duration::from_secs(10));
                Ok::<(), excalibur_store::Error>(())
            })
            .await
    });
    // Wait for permit ownership without blocking the runtime: the holder
    // needs a poll to acquire, and the permit is free, so yields suffice.
    let acquired_by = tokio::time::Instant::now() + Duration::from_secs(5);
    while started_rx.try_recv().is_err() {
        assert!(
            tokio::time::Instant::now() < acquired_by,
            "read permit holder never started"
        );
        tokio::task::yield_now().await;
    }
    let err = f
        .runner
        .run_keyed(&f.store, "task", "check", &f.run, "op-wedge")
        .await
        .expect_err("wedged setup must fail bounded")
        .to_string();
    assert!(
        err.contains("timed out") && err.contains("retry failed"),
        "expected a setup-bound retry error, got: {err}"
    );
    // Setup never reached the runtime and never inserted a row: nothing
    // executed, and there is nothing to expire or reconcile.
    assert_eq!(f.executions.load(Ordering::SeqCst), 0);
    release_tx.send(()).unwrap();
    holder.await.unwrap().unwrap();
    let rows: Vec<String> =
        f.db.read(ReadLane::Interactive, {
            let run = f.run.clone();
            move |c| {
                let mut q =
                    c.prepare("SELECT operation_key FROM task_check_attempts WHERE run_id=?1")?;
                Ok(q.query_map([run], |r| r.get(0))?
                    .collect::<std::result::Result<Vec<_>, _>>()?)
            }
        })
        .await
        .unwrap();
    assert!(rows.is_empty(), "unexpected attempt rows: {rows:?}");
    f.db.shutdown().await.unwrap();
}

#[tokio::test]
async fn superseded_verifier_attempts_expire_without_touching_completed_work() {
    let f = fixture().await;
    let seed = |key: &str, run: &str, state: &str| {
        let (key, run, state) = (key.to_owned(), run.to_owned(), state.to_owned());
        let db = f.db.clone();
        async move {
            db.write(
                excalibur_store::Priority::Journal,
                move |c| {
                    c.execute(
                        "INSERT INTO task_check_attempts(operation_key,task_id,check_id,run_id,prepared,snapshot,producer_snapshot,state) VALUES(?1,'task','check',?2,'{}','{}','{}',?3)",
                        excalibur_store::rusqlite::params![key, run, state],
                    )?;
                    Ok(())
                },
            )
            .await
            .unwrap();
        }
    };
    let other = Journal::new(f.db.clone())
        .await
        .unwrap()
        .create_run("sibling", json!({"test": true}))
        .await
        .unwrap();
    seed("abandoned:one", &f.run, "intent").await;
    seed("finished:two", &f.run, "completed").await;
    seed("other:three", &other, "intent").await;
    f.store.expire_abandoned_attempts(&f.run).await.unwrap();
    let states: std::collections::BTreeMap<String, String> =
        f.db.read(ReadLane::Interactive, {
            move |c| {
                let mut q = c.prepare("SELECT operation_key,state FROM task_check_attempts")?;
                Ok(q.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
                    .collect::<std::result::Result<Vec<_>, _>>()?
                    .into_iter()
                    .collect())
            }
        })
        .await
        .unwrap();
    assert_eq!(
        states.get("abandoned:one").map(String::as_str),
        Some("expired")
    );
    assert_eq!(
        states.get("finished:two").map(String::as_str),
        Some("completed")
    );
    assert_eq!(
        states.get("other:three").map(String::as_str),
        Some("intent")
    );
    // The completion gate stops mistaking the superseded attempt for
    // in-flight work while the unrelated run keeps its own intention.
    let pending: Vec<String> = f
        .db
        .read(ReadLane::Interactive, {
            let run = f.run.clone();
            move |c| {
                let mut q = c.prepare(
                    "SELECT a.check_id FROM task_check_attempts a WHERE a.run_id=?1 AND a.state='intent'",
                )?;
                Ok(q
                    .query_map([run], |r| r.get(0))?
                    .collect::<std::result::Result<Vec<_>, _>>()?)
            }
        })
        .await
        .unwrap();
    assert!(pending.is_empty(), "unexpected pending checks: {pending:?}");
    f.db.shutdown().await.unwrap();
}
