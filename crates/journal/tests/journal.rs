use excalibur_journal::Journal;
use excalibur_store::{Database, DatabaseKind, Priority, StorageConfig};
use serde_json::json;
async fn setup() -> (tempfile::TempDir, Database, Journal) {
    let dir = tempfile::tempdir().unwrap();
    let db = Database::open(
        dir.path().join("main.db"),
        DatabaseKind::Main,
        StorageConfig::default(),
    )
    .unwrap();
    let journal = Journal::new(db.clone()).await.unwrap();
    (dir, db, journal)
}
#[tokio::test]
async fn concurrent_creation_and_append_are_idempotent_ordered_and_immutable() {
    let (_dir, db, journal) = setup().await;
    let (a, b) = tokio::join!(
        journal.create_run("operation", json!({"value":1})),
        journal.create_run("operation", json!({"value":1}))
    );
    let run = a.unwrap();
    assert_eq!(run, b.unwrap());
    assert!(
        journal
            .create_run("operation", json!({"value":2}))
            .await
            .is_err()
    );
    let mut jobs = Vec::new();
    for i in 0..100 {
        let j = journal.clone();
        let r = run.clone();
        jobs.push(tokio::spawn(async move {
            j.append(&r, "test", &format!("event:{i}"), json!({"index":i}))
                .await
                .unwrap()
        }));
    }
    for job in jobs {
        job.await.unwrap();
    }
    let events = journal.replay(&run, 0).await.unwrap();
    assert_eq!(events.len(), 101);
    assert!(
        events
            .windows(2)
            .all(|p| p[0].sequence + 1 == p[1].sequence)
    );
    assert_eq!(
        journal
            .append(&run, "test", "event:0", json!({"index":0}))
            .await
            .unwrap(),
        events.iter().find(|e| e.key == "event:0").unwrap().sequence
    );
    assert!(
        journal
            .append(&run, "test", "event:0", json!({"index":999}))
            .await
            .is_err()
    );
    for sql in ["UPDATE journal SET payload='{}'", "DELETE FROM journal"] {
        assert!(
            db.write(Priority::Normal, move |c| {
                c.execute(sql, [])?;
                Ok(())
            })
            .await
            .is_err()
        );
    }
    assert_eq!(journal.replay(&run, 0).await.unwrap().len(), 101);
    assert_eq!(db.stats().busy_retries, 0);
    db.shutdown().await.unwrap();
}
#[tokio::test]
async fn blob_backed_payload_survives_reopen_and_replays_exactly() {
    let (dir, db, journal) = setup().await;
    let run = journal.create_run("large", json!({})).await.unwrap();
    let payload = json!({"output":"synthetic content".repeat(10000)});
    journal
        .append(&run, "tool_result", "output", payload.clone())
        .await
        .unwrap();
    db.shutdown().await.unwrap();
    drop(journal);
    drop(db);
    let db = Database::open(
        dir.path().join("main.db"),
        DatabaseKind::Main,
        StorageConfig::default(),
    )
    .unwrap();
    let journal = Journal::new(db.clone()).await.unwrap();
    assert_eq!(
        journal.entries(&run, 1, 1).await.unwrap()[0].payload,
        payload
    );
    db.shutdown().await.unwrap();
}
