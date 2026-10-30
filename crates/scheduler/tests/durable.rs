use excalibur_scheduler::{MissedPolicy, Schedule, Scheduler, Timing};
use excalibur_store::{Database, DatabaseKind};
use serde_json::json;

fn timer(id: &str, policy: MissedPolicy) -> Schedule {
    Schedule {
        id: id.into(),
        scope: "project:a".into(),
        timing: Timing::Every {
            first: 100,
            seconds: 10,
        },
        missed: policy,
        payload: json!({"message":"fixture"}),
    }
}
#[tokio::test]
async fn recovery_policies_and_bounded_backlog_are_durable() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("main.db");
    let db = Database::open(&path, DatabaseKind::Main, Default::default()).unwrap();
    let scheduler = Scheduler::open(db.clone(), "project:a".into())
        .await
        .unwrap();
    scheduler
        .create(timer("all", MissedPolicy::RunAll))
        .await
        .unwrap();
    assert_eq!(scheduler.tick(135, true, 2).await.unwrap(), 2);
    let first = scheduler.pending(100).await.unwrap();
    assert_eq!(
        first.iter().map(|f| f.scheduled_at).collect::<Vec<_>>(),
        [100, 110]
    );
    drop(scheduler);
    db.shutdown().await.unwrap();
    drop(db);
    let db = Database::open(&path, DatabaseKind::Main, Default::default()).unwrap();
    let scheduler = Scheduler::open(db.clone(), "project:a".into())
        .await
        .unwrap();
    assert_eq!(scheduler.tick(135, true, 2).await.unwrap(), 2);
    assert_eq!(scheduler.tick(135, true, 2).await.unwrap(), 0);
    let fires = scheduler.pending(100).await.unwrap();
    assert_eq!(fires.len(), 4);
    assert_eq!(fires[0].id, first[0].id);
    assert!(scheduler.acknowledge(fires[0].id.clone()).await.unwrap());
    assert!(!scheduler.acknowledge(fires[0].id.clone()).await.unwrap());
    scheduler
        .create(timer("skip", MissedPolicy::Skip))
        .await
        .unwrap();
    scheduler
        .create(timer("once", MissedPolicy::RunOnceNow))
        .await
        .unwrap();
    assert_eq!(scheduler.tick(135, true, 100).await.unwrap(), 1);
    assert_eq!(
        scheduler
            .pending(100)
            .await
            .unwrap()
            .iter()
            .filter(|f| f.schedule == "once")
            .count(),
        1
    );
    assert_eq!(scheduler.tick(140, false, 100).await.unwrap(), 3);
    drop(scheduler);
    db.shutdown().await.unwrap();
}
#[tokio::test]
async fn scopes_and_concurrent_ticks_cannot_duplicate_or_steal_deliveries() {
    let dir = tempfile::tempdir().unwrap();
    let db = Database::open(
        dir.path().join("main.db"),
        DatabaseKind::Main,
        Default::default(),
    )
    .unwrap();
    let a = Scheduler::open(db.clone(), "project:a".into())
        .await
        .unwrap();
    let b = Scheduler::open(db.clone(), "project:b".into())
        .await
        .unwrap();
    let definition = timer("same:id", MissedPolicy::RunAll);
    a.create(definition.clone()).await.unwrap();
    a.create(definition.clone()).await.unwrap();
    let mut changed = definition.clone();
    changed.payload = json!("changed");
    assert!(a.create(changed).await.is_err());
    assert!(b.create(definition).await.is_err());
    let (x, y) = tokio::join!(a.tick(120, false, 100), a.tick(120, false, 100));
    assert_eq!(x.unwrap() + y.unwrap(), 3);
    assert!(b.pending(100).await.unwrap().is_empty());
    let fires = a.pending(100).await.unwrap();
    assert!(!b.acknowledge(fires[0].id.clone()).await.unwrap());
    assert_eq!(a.pending(100).await.unwrap().len(), 3);
    drop((a, b));
    db.shutdown().await.unwrap();
}
#[tokio::test]
async fn exact_startup_time_is_not_skipped_and_overflow_finishes_timer() {
    let dir = tempfile::tempdir().unwrap();
    let db = Database::open(
        dir.path().join("main.db"),
        DatabaseKind::Main,
        Default::default(),
    )
    .unwrap();
    let a = Scheduler::open(db.clone(), "project:a".into())
        .await
        .unwrap();
    a.create(timer("exact", MissedPolicy::Skip)).await.unwrap();
    assert_eq!(a.tick(100, true, 100).await.unwrap(), 1);
    let mut terminal = timer("last", MissedPolicy::RunAll);
    terminal.timing = Timing::Every {
        first: i64::MAX,
        seconds: 1,
    };
    a.create(terminal).await.unwrap();
    // Use a separate scope to avoid enumerating the other timer's enormous backlog.
    let b = Scheduler::open(db.clone(), "project:b".into())
        .await
        .unwrap();
    let mut terminal = timer("last", MissedPolicy::RunAll);
    terminal.scope = "project:b".into();
    terminal.timing = Timing::Every {
        first: i64::MAX,
        seconds: 1,
    };
    b.create(terminal).await.unwrap();
    assert_eq!(b.tick(i64::MAX, false, 100).await.unwrap(), 1);
    assert_eq!(b.tick(i64::MAX, false, 100).await.unwrap(), 0);
    drop((a, b));
    db.shutdown().await.unwrap();
}

#[test]
#[ignore = "subprocess fixture invoked by killed_dispatch_keeps_the_same_fire_key"]
fn crash_child() {
    let root = std::path::PathBuf::from(std::env::var_os("EXCALIBUR_TIMER_CRASH_ROOT").unwrap());
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let db =
            Database::open(root.join("main.db"), DatabaseKind::Main, Default::default()).unwrap();
        let scheduler = Scheduler::open(db, "project:a".into()).await.unwrap();
        let mut definition = timer("single", MissedPolicy::RunOnceNow);
        definition.timing = Timing::At(100);
        scheduler.create(definition).await.unwrap();
        scheduler.tick(110, true, 100).await.unwrap();
        let fire = scheduler.pending(100).await.unwrap().remove(0);
        std::fs::write(root.join("ready"), &fire.id).unwrap();
        std::fs::File::open(root.join("ready"))
            .unwrap()
            .sync_all()
            .unwrap();
        // Parent kills us between durable dispatch preparation and acknowledgment.
        std::future::pending::<()>().await;
    });
}

#[tokio::test]
async fn killed_dispatch_keeps_the_same_fire_key() {
    let dir = tempfile::tempdir().unwrap();
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--ignored", "--exact", "crash_child"])
        .env("EXCALIBUR_TIMER_CRASH_ROOT", dir.path())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    while !dir.path().join("ready").exists() {
        if std::time::Instant::now() >= deadline || child.try_wait().unwrap().is_some() {
            let _ = child.kill();
            let _ = child.wait();
            panic!("child did not prepare its durable fire");
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    child.kill().unwrap();
    child.wait().unwrap();
    let key = std::fs::read_to_string(dir.path().join("ready")).unwrap();
    let db = Database::open(
        dir.path().join("main.db"),
        DatabaseKind::Main,
        Default::default(),
    )
    .unwrap();
    let scheduler = Scheduler::open(db.clone(), "project:a".into())
        .await
        .unwrap();
    assert_eq!(scheduler.tick(200, true, 100).await.unwrap(), 0);
    let fires = scheduler.pending(100).await.unwrap();
    assert_eq!(fires.len(), 1);
    assert_eq!(fires[0].id, key);
    assert!(scheduler.acknowledge(key).await.unwrap());
    drop(scheduler);
    db.shutdown().await.unwrap();
}

#[tokio::test]
async fn rejects_nonprincipal_storage_and_invalid_definitions() {
    let dir = tempfile::tempdir().unwrap();
    let telemetry = Database::open(
        dir.path().join("telemetry.db"),
        DatabaseKind::Telemetry,
        Default::default(),
    )
    .unwrap();
    assert!(
        Scheduler::open(telemetry.clone(), "project:a".into())
            .await
            .is_err()
    );
    telemetry.shutdown().await.unwrap();
    let db = Database::open(
        dir.path().join("main.db"),
        DatabaseKind::Main,
        Default::default(),
    )
    .unwrap();
    assert!(Scheduler::open(db.clone(), String::new()).await.is_err());
    let scheduler = Scheduler::open(db.clone(), "project:a".into())
        .await
        .unwrap();
    let mut invalid = timer("zero", MissedPolicy::RunAll);
    invalid.timing = Timing::Every {
        first: 0,
        seconds: 0,
    };
    assert!(scheduler.create(invalid).await.is_err());
    assert!(scheduler.tick(-1, false, 100).await.is_err());
    assert!(scheduler.tick(1, false, 0).await.is_err());
    assert!(scheduler.pending(1001).await.is_err());
    assert!(scheduler.pending(100).await.unwrap().is_empty());
    drop(scheduler);
    db.shutdown().await.unwrap();
}
