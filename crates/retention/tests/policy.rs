//! §4.7 time-based classes against a manual clock.
use excalibur_journal::Journal;
use excalibur_retention::{HoldReason, RetentionPolicy, RetentionSweeper};
use excalibur_store::{Database, DatabaseKind, Priority, ReadLane, StorageConfig};
use excalibur_telemetry::{RunMetadata, Telemetry, TelemetryConfig};
use excalibur_vault::Redactor;
use serde_json::json;
use std::sync::{
    Arc,
    atomic::{AtomicI64, Ordering},
};

const DAY: i64 = 86_400;

async fn finished_run(journal: &Journal, key: &str, state: &str) -> String {
    let run = journal.create_run(key, json!({"task": key})).await.unwrap();
    journal
        .append(&run, "tool_result", "tool:1", json!({"ok": true}))
        .await
        .unwrap();
    if state != "running" {
        journal.set_state(&run, state, None, "end").await.unwrap();
    }
    run
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn expiry_classes_follow_the_injected_clock() {
    let dir = tempfile::tempdir().unwrap();
    let main = Database::open(
        dir.path().join("excalibur.db"),
        DatabaseKind::Main,
        StorageConfig::default(),
    )
    .unwrap();
    let journal = Journal::new(main.clone()).await.unwrap();
    let start = excalibur_store::unix_seconds();
    let mut config = TelemetryConfig::new(
        dir.path().join("telemetry"),
        RunMetadata::default(),
        Redactor::default(),
    );
    config.clock = Arc::new(move || start * 1000);
    let telemetry = Telemetry::open(config).await.unwrap();
    let done = finished_run(&journal, "done", "completed").await;
    let held = finished_run(&journal, "held", "failed").await;
    let active = finished_run(&journal, "active", "running").await;
    telemetry
        .register_run(&done, RunMetadata::default())
        .await
        .unwrap();
    telemetry.reconcile(&journal, &done).await.unwrap();
    let detailed: i64 = telemetry
        .database()
        .write(Priority::Normal, move |c| {
            c.execute("INSERT INTO traces(id,run_id,name,started_at,created_at) VALUES('t1','r','span',?1,?1)", [start])?;
            c.execute("INSERT INTO metrics(run_id,name,value,created_at) VALUES('r','tokens',3,?1)", [start])?;
            c.execute("INSERT INTO metric_daily(day,name,value) VALUES('2026-10-30','tokens',3)", [])?;
            Ok(c.query_row("SELECT count(*) FROM events", [], |r| r.get(0))?)
        })
        .await
        .unwrap();
    assert!(detailed >= 3);

    let now = Arc::new(AtomicI64::new(start + 10 * DAY));
    let clock = now.clone();
    let sweeper = RetentionSweeper::open(
        main.clone(),
        Some(telemetry.clone()),
        RetentionPolicy::default(),
        Arc::new(move || clock.load(Ordering::SeqCst)),
    )
    .await
    .unwrap();
    sweeper.hold_run(&held, HoldReason::Replay).await.unwrap();

    let early = sweeper.run_once().await.unwrap();
    assert!(early.journal_compaction_due.is_empty() && early.journal_held.is_empty());
    assert_eq!(early.telemetry_events_expired, 0);

    now.store(start + 31 * DAY, Ordering::SeqCst);
    let month = sweeper.run_once().await.unwrap();
    assert_eq!(month.journal_compaction_due, vec![done.clone()]);
    assert_eq!(month.journal_held, vec![held.clone()]);
    assert!(!month.journal_compaction_due.contains(&active));
    assert_eq!(month.telemetry_events_expired, 0, "telemetry keeps 90 days");

    now.store(start + 91 * DAY, Ordering::SeqCst);
    let quarter = sweeper.run_once().await.unwrap();
    assert_eq!(quarter.telemetry_events_expired as i64, detailed);
    assert_eq!(
        (
            quarter.telemetry_traces_expired,
            quarter.telemetry_metrics_expired
        ),
        (1, 1)
    );
    assert_eq!(quarter.telemetry_runs_reexported, 1);
    let left: (i64, i64, i64) = telemetry
        .database()
        .read(ReadLane::Interactive, |c| {
            Ok((
                c.query_row("SELECT count(*) FROM events", [], |r| r.get(0))?,
                c.query_row("SELECT count(*) FROM telemetry_offsets", [], |r| r.get(0))?,
                c.query_row("SELECT count(*) FROM metric_daily", [], |r| r.get(0))?,
            ))
        })
        .await
        .unwrap();
    assert_eq!(left, (0, 0, 1), "daily aggregates are kept indefinitely");
    let exported = std::fs::read(
        dir.path()
            .join("telemetry/runs")
            .join(&done)
            .join("events.jsonl"),
    )
    .unwrap();
    assert!(exported.is_empty(), "re-export drops expired detail");
    // Reconciling the journal must not re-ingest deliberately expired events.
    telemetry.reconcile(&journal, &done).await.unwrap();
    let reingested: i64 = telemetry
        .database()
        .read(ReadLane::Interactive, |c| {
            Ok(c.query_row("SELECT count(*) FROM events", [], |r| r.get(0))?)
        })
        .await
        .unwrap();
    assert_eq!(
        reingested, 0,
        "expired telemetry came back through reconcile"
    );
    let again = sweeper.run_once().await.unwrap();
    assert_eq!(again.telemetry_events_expired, 0);
    sweeper.release_hold(&held).await.unwrap();
    let released = sweeper.run_once().await.unwrap();
    assert_eq!(released.journal_compaction_due.len(), 2);
    let guards: i64 = telemetry
        .database()
        .read(ReadLane::Interactive, |c| {
            Ok(c.query_row(
                "SELECT count(*) FROM sqlite_schema WHERE type='trigger' AND name LIKE 'telemetry_%_no_%'",
                [],
                |r| r.get(0),
            )?)
        })
        .await
        .unwrap();
    assert_eq!(guards, 4, "append-only telemetry guards are restored");
    telemetry.shutdown().await.unwrap();
    main.shutdown().await.unwrap();
}
