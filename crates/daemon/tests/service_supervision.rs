use excalibur_daemon::services::{supervise, supervise_writer};
use excalibur_store::{Database, DatabaseKind, Priority, ReadLane};
use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::sync::{Notify, watch};
#[tokio::test]
async fn synchronous_and_async_service_panics_signal_shutdown_and_return_typed_failure() {
    for asynchronous in [false, true] {
        let shutdown = Arc::new(Notify::new());
        let (_stop, rx) = watch::channel(false);
        let handle = supervise(
            &tokio::runtime::Handle::current(),
            "fixture",
            shutdown.clone(),
            rx,
            move || {
                assert!(asynchronous, "synthetic factory panic");
                async {
                    panic!("synthetic iteration panic");
                    #[allow(unreachable_code)]
                    Ok(())
                }
            },
        );
        tokio::time::timeout(Duration::from_secs(5), shutdown.notified())
            .await
            .unwrap();
        assert!(handle.await.unwrap().unwrap_err().contains("panicked"));
    }
}
#[tokio::test(start_paused = true)]
async fn transient_iteration_failure_recovers_without_shutting_down_the_gateway() {
    let calls = Arc::new(AtomicUsize::new(0));
    let source = calls.clone();
    let shutdown = Arc::new(Notify::new());
    let (stop, rx) = watch::channel(false);
    let handle = supervise(
        &tokio::runtime::Handle::current(),
        "fixture",
        shutdown.clone(),
        rx,
        move || {
            let calls = source.clone();
            async move {
                if calls.fetch_add(1, Ordering::SeqCst) == 0 {
                    Err("synthetic transient".into())
                } else {
                    Ok(())
                }
            }
        },
    );
    tokio::task::yield_now().await;
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    tokio::time::advance(Duration::from_secs(2)).await;
    tokio::task::yield_now().await;
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert!(
        tokio::time::timeout(Duration::from_millis(1), shutdown.notified())
            .await
            .is_err()
    );
    stop.send(true).unwrap();
    handle.await.unwrap().unwrap();
}
#[tokio::test]
async fn writer_failure_signals_without_any_followup_write_and_keeps_confirmed_data() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("main.db");
    let db = Database::open(&path, DatabaseKind::Main, Default::default()).unwrap();
    db.write(Priority::Journal, |c| {
        c.execute("INSERT INTO principals(id) VALUES('confirmed')", [])?;
        Ok(())
    })
    .await
    .unwrap();
    let shutdown = Arc::new(Notify::new());
    let (_stop, rx) = watch::channel(false);
    let watcher = supervise_writer(
        &tokio::runtime::Handle::current(),
        db.clone(),
        shutdown.clone(),
        rx,
    );
    let failure = db
        .write::<(), _>(Priority::Journal, |_| panic!("synthetic writer failure"))
        .await;
    assert!(failure.is_err());
    tokio::time::timeout(Duration::from_secs(5), shutdown.notified())
        .await
        .unwrap();
    assert_eq!(
        watcher.await.unwrap().unwrap_err(),
        "principal_writer_failed"
    );
    let _ = db.shutdown().await;
    drop(db);
    let reopened = Database::open(path, DatabaseKind::Main, Default::default()).unwrap();
    let count: i64 = reopened
        .read(ReadLane::Interactive, |c| {
            Ok(c.query_row(
                "SELECT count(*) FROM principals WHERE id='confirmed'",
                [],
                |r| r.get(0),
            )?)
        })
        .await
        .unwrap();
    assert_eq!(count, 1);
    reopened.shutdown().await.unwrap();
}

#[tokio::test]
async fn stopping_the_host_cancels_an_iteration_waiting_on_an_unavailable_dependency() {
    let entered = Arc::new(Notify::new());
    let started = entered.clone();
    let (stop, rx) = watch::channel(false);
    let handle = supervise(
        &tokio::runtime::Handle::current(),
        "waiting fixture",
        Arc::new(Notify::new()),
        rx,
        move || {
            let started = started.clone();
            async move {
                started.notify_one();
                std::future::pending::<Result<(), String>>().await
            }
        },
    );
    entered.notified().await;
    stop.send(true).unwrap();
    tokio::time::timeout(Duration::from_secs(5), handle)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
}
