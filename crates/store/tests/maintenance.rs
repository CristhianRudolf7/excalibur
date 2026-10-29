use excalibur_store::{
    Database, DatabaseKind, MaintenancePolicy, MaintenanceWindow, StorageConfig,
    maintenance_scheduler_stats,
};
use std::time::Duration;
static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
#[test]
fn low_activity_policy_is_explicit_delayed_and_handles_midnight_windows() {
    let _serial = SERIAL.blocking_lock();
    let policy = MaintenancePolicy::default();
    for minute in 0..1440 {
        assert!(!policy.permits(Duration::ZERO, minute * 60));
        assert_eq!(
            policy.permits(Duration::from_secs(300), minute * 60),
            (180..240).contains(&minute)
        );
    }
    let window = MaintenanceWindow {
        start_minute_utc: 1380,
        end_minute_utc: 60,
    };
    assert!(window.contains(23 * 3600));
    assert!(window.contains(30 * 60));
    assert!(!window.contains(60 * 60));
    assert!(!MaintenancePolicy::manual().permits(Duration::from_secs(86400), 3 * 3600));
}
#[cfg(target_os = "linux")]
fn rss_kib() -> u64 {
    std::fs::read_to_string("/proc/self/status")
        .unwrap()
        .lines()
        .find_map(|line| {
            line.strip_prefix("VmRSS:")
                .and_then(|v| v.split_whitespace().next())
                .and_then(|v| v.parse().ok())
        })
        .unwrap()
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn multiple_projects_share_one_scheduler_and_release_registrations() {
    let _serial = SERIAL.lock().await;
    let dir = tempfile::tempdir().unwrap();
    let config = StorageConfig {
        read_connections: 2,
        ..Default::default()
    };
    let main = Database::open(
        dir.path().join("excalibur.db"),
        DatabaseKind::Main,
        config.clone(),
    )
    .unwrap();
    let baseline = maintenance_scheduler_stats();
    assert_eq!(baseline.worker_threads, 1);
    #[cfg(target_os = "linux")]
    let before = (
        std::fs::read_dir("/proc/self/task").unwrap().count(),
        rss_kib(),
    );
    let mut databases = Vec::new();
    for n in 0..12 {
        let db = Database::open(
            dir.path().join(format!("projects/{n}/codegraph.db")),
            DatabaseKind::CodeGraph,
            config.clone(),
        )
        .unwrap();
        db.set_maintenance_allowed(false);
        databases.push(db);
    }
    let stats = maintenance_scheduler_stats();
    assert_eq!(stats.registered, baseline.registered + 12);
    assert_eq!(stats.worker_threads, 1);
    assert_eq!(stats.runs, baseline.runs);
    assert!(!dir.path().join("backups").exists());
    #[cfg(target_os = "linux")]
    {
        let after = (
            std::fs::read_dir("/proc/self/task").unwrap().count(),
            rss_kib(),
        );
        println!(
            "projects=12 added_threads={} rss_delta_kib={} shared_maintenance_workers=1 startup_maintenance_runs=0",
            after.0.saturating_sub(before.0),
            after.1.saturating_sub(before.1)
        );
        assert!(
            after.0 <= before.0 + 12,
            "only the dedicated writer is added per file"
        );
    }
    for db in databases {
        db.shutdown().await.unwrap();
    }
    assert_eq!(
        maintenance_scheduler_stats().registered,
        baseline.registered
    );
    main.shutdown().await.unwrap();
    assert_eq!(
        maintenance_scheduler_stats().registered,
        baseline.registered - 1
    );
}

#[test]
fn cancelled_queued_backup_does_not_publish_and_shutdown_drains_work() {
    use std::future::Future;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .max_blocking_threads(1)
        .enable_all()
        .build()
        .unwrap();
    let serial = runtime.block_on(SERIAL.lock());
    runtime.block_on(async {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open_with_maintenance(
            dir.path().join("excalibur.db"),
            DatabaseKind::Main,
            StorageConfig::default(),
            MaintenancePolicy::manual(),
        )
        .unwrap();
        let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let blocker = tokio::task::spawn_blocking(move || {
            entered_tx.send(()).unwrap();
            release_rx.recv().unwrap();
        });
        entered_rx.await.unwrap();
        let mut backup = Box::pin(db.backup(excalibur_store::unix_seconds()));
        assert!(
            std::future::poll_fn(|cx| std::task::Poll::Ready(backup.as_mut().poll(cx)))
                .await
                .is_pending()
        );
        drop(backup);
        release_tx.send(()).unwrap();
        blocker.await.unwrap();
        db.shutdown().await.unwrap();
        assert!(!dir.path().join("backups").exists());
    });
    drop(runtime);
    drop(serial);
}
