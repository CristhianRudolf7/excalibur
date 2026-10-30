use excalibur_scheduler::{
    MissedPolicy, Schedule, Scheduler, Timing,
    calendar::{Cron, local_to_utc},
};
use excalibur_store::{Database, DatabaseKind};
use serde_json::json;
fn utc(s: &str) -> i64 {
    s.parse::<jiff::Timestamp>().unwrap().as_second()
}
#[test]
fn dst_gaps_use_first_valid_and_folds_use_first_occurrence() {
    assert_eq!(
        local_to_utc("2026-03-08T02:30:00", "America/New_York").unwrap(),
        utc("2026-03-08T07:00:00Z")
    );
    assert_eq!(
        local_to_utc("2026-11-01T01:30:00", "America/New_York").unwrap(),
        utc("2026-11-01T05:30:00Z")
    );
    assert_eq!(
        local_to_utc("2026-10-04T02:15:00", "Australia/Lord_Howe").unwrap(),
        utc("2026-10-03T15:30:00Z")
    );
    assert_eq!(
        local_to_utc("2011-12-30T12:00:00", "Pacific/Apia").unwrap(),
        utc("2011-12-30T10:00:00Z")
    );
    assert!(local_to_utc("2026-03-08T02:30:00.1", "America/New_York").is_err());
}
#[test]
fn cron_never_repeats_fold_and_handles_seconds_leap_days_and_day_union() {
    let cron = Cron::parse("30 1 * * *").unwrap();
    assert_eq!(
        cron.next_after(utc("2026-11-01T05:00:00Z"), "America/New_York")
            .unwrap(),
        utc("2026-11-01T05:30:00Z")
    );
    assert_eq!(
        cron.next_after(utc("2026-11-01T05:30:00Z"), "America/New_York")
            .unwrap(),
        utc("2026-11-02T06:30:00Z")
    );
    assert_eq!(
        cron.latest_at(utc("2026-11-01T06:15:00Z"), "America/New_York")
            .unwrap(),
        utc("2026-11-01T05:30:00Z")
    );
    assert_eq!(
        Cron::parse("*/15 * * * * *")
            .unwrap()
            .next_after(utc("2026-01-01T00:00:00Z"), "UTC")
            .unwrap(),
        utc("2026-01-01T00:00:15Z")
    );
    assert_eq!(
        Cron::parse("0 0 29 2 *")
            .unwrap()
            .next_after(utc("2096-02-29T00:00:00Z"), "UTC")
            .unwrap(),
        utc("2104-02-29T00:00:00Z")
    );
    assert_eq!(
        Cron::parse("0 0 1 * 1")
            .unwrap()
            .next_after(utc("2026-01-02T00:00:00Z"), "UTC")
            .unwrap(),
        utc("2026-01-05T00:00:00Z")
    );
    assert_eq!(
        Cron::parse("0 0 */2 * *")
            .unwrap()
            .next_after(utc("2026-01-01T00:00:00Z"), "UTC")
            .unwrap(),
        utc("2026-01-03T00:00:00Z")
    );
    assert!(
        Cron::parse("0 0 31 2 *")
            .unwrap()
            .next_after(utc("2026-01-01T00:00:00Z"), "UTC")
            .is_err()
    );
    for value in [
        "*/0 * * * *",
        "0 24 * * *",
        "0 0 0 * *",
        "0 0 * 13 *",
        "* * *",
        "1-0 * * * *",
    ] {
        assert!(Cron::parse(value).is_err(), "{value}");
    }
}
#[tokio::test]
async fn durable_calendar_recovery_and_timezone_change_preserve_old_fires() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("db");
    let db = Database::open(&path, DatabaseKind::Main, Default::default()).unwrap();
    let a = Scheduler::open(db.clone(), "project:a".into())
        .await
        .unwrap();
    let b = Scheduler::open(db.clone(), "project:b".into())
        .await
        .unwrap();
    let start = utc("2026-03-07T00:00:00Z");
    a.create(Schedule {
        id: "wake".into(),
        scope: "project:a".into(),
        timing: Timing::Cron {
            expression: "30 2 * * *".into(),
            timezone: "America/New_York".into(),
            not_before: start,
        },
        missed: MissedPolicy::RunAll,
        payload: json!("wake"),
    })
    .await
    .unwrap();
    assert_eq!(
        a.tick(utc("2026-03-08T07:00:00Z"), true, 1).await.unwrap(),
        1
    );
    assert_eq!(
        a.tick(utc("2026-03-08T07:00:00Z"), true, 1).await.unwrap(),
        1
    );
    assert_eq!(
        a.pending(10)
            .await
            .unwrap()
            .iter()
            .map(|f| f.scheduled_at)
            .collect::<Vec<_>>(),
        [utc("2026-03-07T07:30:00Z"), utc("2026-03-08T07:00:00Z")]
    );
    let change = a
        .change_timezone(
            "move".into(),
            "wake".into(),
            "Europe/Berlin".into(),
            utc("2026-03-08T08:00:00Z"),
        )
        .await
        .unwrap();
    assert_eq!(change.next, utc("2026-03-09T01:30:00Z"));
    assert_eq!(
        a.change_timezone(
            "move".into(),
            "wake".into(),
            "Europe/Berlin".into(),
            utc("2026-03-10T00:00:00Z")
        )
        .await
        .unwrap(),
        change
    );
    assert!(
        a.change_timezone("move".into(), "wake".into(), "UTC".into(), start)
            .await
            .is_err()
    );
    assert!(b.timezone_changes(10).await.unwrap().is_empty());
    assert!(!b.acknowledge_timezone_change("move".into()).await.unwrap());
    assert_eq!(a.pending(10).await.unwrap().len(), 2);
    drop((a, b));
    db.shutdown().await.unwrap();
    drop(db);
    let db = Database::open(&path, DatabaseKind::Main, Default::default()).unwrap();
    let a = Scheduler::open(db.clone(), "project:a".into())
        .await
        .unwrap();
    assert_eq!(a.timezone_changes(10).await.unwrap(), [change]);
    assert!(a.acknowledge_timezone_change("move".into()).await.unwrap());
    assert!(!a.acknowledge_timezone_change("move".into()).await.unwrap());
    drop(a);
    db.shutdown().await.unwrap();
}
#[tokio::test]
async fn calendar_skip_exact_due_and_run_once_coalesce_without_fold_duplicates() {
    let dir = tempfile::tempdir().unwrap();
    let db = Database::open(
        dir.path().join("db"),
        DatabaseKind::Main,
        Default::default(),
    )
    .unwrap();
    let a = Scheduler::open(db.clone(), "a".into()).await.unwrap();
    for (id, missed) in [
        ("skip", MissedPolicy::Skip),
        ("once", MissedPolicy::RunOnceNow),
    ] {
        a.create(Schedule {
            id: id.into(),
            scope: "a".into(),
            timing: Timing::Cron {
                expression: "30 1 * * *".into(),
                timezone: "America/New_York".into(),
                not_before: utc("2026-10-31T00:00:00Z"),
            },
            missed,
            payload: json!(id),
        })
        .await
        .unwrap();
    }
    assert_eq!(
        a.tick(utc("2026-11-01T05:30:00Z"), true, 10).await.unwrap(),
        2
    );
    assert_eq!(
        a.tick(utc("2026-11-01T06:30:00Z"), true, 10).await.unwrap(),
        0
    );
    assert_eq!(a.pending(10).await.unwrap().len(), 2);
    drop(a);
    db.shutdown().await.unwrap();
}

#[test]
fn civil_inputs_cannot_silently_discard_offsets_or_fractional_seconds() {
    for local in [
        "2026-01-01T00:00:00Z",
        "2026-01-01T00:00:00+02:00",
        "2026-01-01T00:00:00[UTC]",
        "2026-01-01T00:00:00.001",
        "2026-01-01T00:00:00.000000001",
    ] {
        assert!(local_to_utc(local, "UTC").is_err(), "{local}");
    }
    assert!(local_to_utc("2026-01-01T00:00:00", "invented/nowhere").is_err());
}
#[tokio::test]
async fn concurrent_calendar_ticks_and_zone_changes_never_drop_due_work() {
    let dir = tempfile::tempdir().unwrap();
    let db = Database::open(
        dir.path().join("db"),
        DatabaseKind::Main,
        Default::default(),
    )
    .unwrap();
    let a = Scheduler::open(db.clone(), "a".into()).await.unwrap();
    let start = utc("2026-01-01T00:00:00Z");
    a.create(Schedule {
        id: "s".into(),
        scope: "a".into(),
        timing: Timing::Cron {
            expression: "* * * * *".into(),
            timezone: "UTC".into(),
            not_before: start,
        },
        missed: MissedPolicy::RunAll,
        payload: json!(null),
    })
    .await
    .unwrap();
    assert!(
        a.change_timezone(
            "tooearly".into(),
            "s".into(),
            "Europe/Berlin".into(),
            start + 120
        )
        .await
        .is_err()
    );
    let (x, y) = tokio::join!(a.tick(start + 120, true, 10), a.tick(start + 120, true, 10));
    assert_eq!(x.unwrap() + y.unwrap(), 3);
    assert_eq!(a.pending(10).await.unwrap().len(), 3);
    let note = a
        .change_timezone(
            "move".into(),
            "s".into(),
            "Europe/Berlin".into(),
            start + 120,
        )
        .await
        .unwrap();
    assert!(a.acknowledge_timezone_change("move".into()).await.unwrap());
    assert_eq!(
        a.change_timezone(
            "move".into(),
            "s".into(),
            "Europe/Berlin".into(),
            start + 86400
        )
        .await
        .unwrap(),
        note
    );
    assert!(a.timezone_changes(10).await.unwrap().is_empty());
    assert_eq!(a.pending(10).await.unwrap().len(), 3);
    drop(a);
    db.shutdown().await.unwrap();
}
#[tokio::test]
async fn version_one_timers_survive_additive_calendar_migration() {
    use sha2::{Digest, Sha256};
    let dir = tempfile::tempdir().unwrap();
    let db = Database::open(
        dir.path().join("db"),
        DatabaseKind::Main,
        Default::default(),
    )
    .unwrap();
    let schema = include_str!("../src/schema.sql");
    let digest = format!("{:x}", Sha256::digest(schema.as_bytes()));
    db.write(excalibur_store::Priority::Journal, move |c| {
        c.execute_batch(
            "CREATE TABLE scheduler_schema(version INTEGER PRIMARY KEY,checksum TEXT NOT NULL)",
        )?;
        c.execute_batch(schema)?;
        c.execute("INSERT INTO scheduler_schema VALUES(1,?1)", [digest])?;
        let definition = serde_json::to_string(&Schedule {
            id: "old".into(),
            scope: "a".into(),
            timing: Timing::At(100),
            missed: MissedPolicy::RunOnceNow,
            payload: json!("original"),
        })
        .unwrap();
        c.execute(
            "INSERT INTO scheduler_timers VALUES('a','old',?1,100)",
            [definition],
        )?;
        Ok(())
    })
    .await
    .unwrap();
    let a = Scheduler::open(db.clone(), "a".into()).await.unwrap();
    assert_eq!(a.tick(200, true, 10).await.unwrap(), 1);
    assert_eq!(a.pending(10).await.unwrap()[0].payload, json!("original"));
    assert_eq!(a.tick(300, true, 10).await.unwrap(), 0);
    assert!(
        a.change_timezone("restore".into(), "old".into(), "UTC".into(), 300)
            .await
            .is_err()
    );
    drop(a);
    db.shutdown().await.unwrap();
}
