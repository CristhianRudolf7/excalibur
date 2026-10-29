use excalibur_journal::Journal;
use excalibur_store::{Database, DatabaseKind, StorageConfig};
use serde_json::json;
#[tokio::test]
async fn expanded_states_enforce_admission_wait_and_recovery_transitions() {
    let dir = tempfile::tempdir().unwrap();
    let db = Database::open(
        dir.path().join("main.db"),
        DatabaseKind::Main,
        StorageConfig::default(),
    )
    .unwrap();
    let journal = Journal::new(db.clone()).await.unwrap();
    let run = journal
        .create_run("expanded", json!({"initial_state":"created"}))
        .await
        .unwrap();
    assert_eq!(journal.state(&run).await.unwrap().0, "created");
    assert!(
        !journal
            .set_state_if(&run, &["created"], "paused", None, "invalid-pause")
            .await
            .unwrap()
    );
    for (from, to) in [
        ("created", "ready"),
        ("ready", "running"),
        ("running", "waiting"),
        ("waiting", "recovering"),
        ("recovering", "ready"),
        ("ready", "running"),
        ("running", "verifying"),
    ] {
        assert!(
            journal
                .set_state_if(
                    &run,
                    &[from],
                    to,
                    None,
                    &format!(
                        "{from}-{to}-{}",
                        journal.entries(&run, 0, 100).await.unwrap().len()
                    )
                )
                .await
                .unwrap()
        );
    }
    let revision = journal
        .entries(&run, 0, 100)
        .await
        .unwrap()
        .last()
        .unwrap()
        .sequence;
    journal
        .append(
            &run,
            "directive_received",
            "directive:new:received",
            json!({"id":"new","text":"synthetic update"}),
        )
        .await
        .unwrap();
    assert!(
        !journal
            .set_state_if_revision(
                &run,
                &["verifying"],
                revision,
                "completed",
                None,
                "stale-completion"
            )
            .await
            .unwrap()
    );
    assert_eq!(journal.state(&run).await.unwrap().0, "verifying");
    assert!(
        journal
            .set_state_if(
                &run,
                &["verifying"],
                "cancelled",
                Some("user".into()),
                "cancel"
            )
            .await
            .unwrap()
    );
    assert!(
        !journal
            .set_state_if(&run, &["cancelled"], "ready", None, "cannot-reopen")
            .await
            .unwrap()
    );
    db.shutdown().await.unwrap();
}
