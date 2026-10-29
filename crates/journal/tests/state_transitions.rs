use excalibur_journal::Journal;
use excalibur_store::{Database, DatabaseKind, Priority, ReadLane, StorageConfig};
use serde_json::json;
use std::sync::Arc;
async fn fixture() -> (tempfile::TempDir, Database, Journal) {
    let root = tempfile::tempdir().unwrap();
    let db = Database::open(
        root.path().join("main.db"),
        DatabaseKind::Main,
        StorageConfig::default(),
    )
    .unwrap();
    let journal = Journal::new(db.clone()).await.unwrap();
    (root, db, journal)
}
async fn projection(db: &Database, run: &str) -> (String, Option<String>) {
    let run = run.to_string();
    db.read(ReadLane::Interactive, move |c| {
        Ok(
            c.query_row("SELECT state,reason FROM runs WHERE id=?1", [run], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })?,
        )
    })
    .await
    .unwrap()
}
#[tokio::test]
async fn retransmitted_older_transition_key_cannot_roll_projection_back() {
    let (_root, db, journal) = fixture().await;
    let run = journal.create_run("stale-key", json!({})).await.unwrap();
    journal
        .set_state(&run, "paused", Some("user".into()), "pause:original")
        .await
        .unwrap();
    journal
        .set_state(&run, "running", None, "resume:original")
        .await
        .unwrap();
    let events = journal.replay(&run, 0).await.unwrap();
    journal
        .set_state(&run, "paused", Some("user".into()), "pause:original")
        .await
        .unwrap();
    assert_eq!(
        journal.replay(&run, 0).await.unwrap(),
        events,
        "retrying an immutable event is not a new transition"
    );
    assert_eq!(
        projection(&db, &run).await,
        ("running".into(), None),
        "projection must follow the newest event, not the retransmitted key"
    );
    assert!(
        journal
            .set_state(
                &run,
                "paused",
                Some("different payload".into()),
                "pause:original"
            )
            .await
            .is_err()
    );
    assert_eq!(projection(&db, &run).await, ("running".into(), None));
    db.shutdown().await.unwrap();
}
#[tokio::test]
async fn all_terminal_states_reject_exit_and_same_state_retransmission_is_a_noop() {
    let (_root, db, journal) = fixture().await;
    for terminal in ["cancelled", "completed", "completed_with_pending"] {
        let run = journal
            .create_run(&format!("terminal-{terminal}"), json!({}))
            .await
            .unwrap();
        journal
            .set_state(
                &run,
                terminal,
                Some("original terminal reason".into()),
                "terminal",
            )
            .await
            .unwrap();
        let before = journal.replay(&run, 0).await.unwrap();
        journal
            .set_state(
                &run,
                terminal,
                Some("attempted reason replacement".into()),
                "same-terminal-new-key",
            )
            .await
            .unwrap();
        assert_eq!(
            journal.replay(&run, 0).await.unwrap(),
            before,
            "a same-terminal request must not append a state change"
        );
        assert_eq!(
            projection(&db, &run).await,
            (terminal.into(), Some("original terminal reason".into()))
        );
        for target in ["running", "paused", "blocked"] {
            assert!(
                journal
                    .set_state(&run, target, None, &format!("forbidden-{target}"))
                    .await
                    .is_err()
            );
        }
        assert_eq!(journal.replay(&run, 0).await.unwrap(), before);
        assert_eq!(projection(&db, &run).await.0, terminal);
    }
    db.shutdown().await.unwrap();
}
#[tokio::test]
async fn cas_uses_latest_durable_state_and_repairs_a_stale_projection() {
    let (_root, db, journal) = fixture().await;
    let run = journal
        .create_run("stale-projection", json!({}))
        .await
        .unwrap();
    assert!(
        journal
            .set_state_if(
                &run,
                &["running"],
                "cancelled",
                Some("user".into()),
                "cancel"
            )
            .await
            .unwrap()
    );
    let before = journal.replay(&run, 0).await.unwrap();
    db.write(Priority::Normal, {
        let run = run.clone();
        move |c| {
            c.execute(
                "UPDATE runs SET state='running',reason=NULL WHERE id=?1",
                [run],
            )?;
            Ok(())
        }
    })
    .await
    .unwrap();
    assert!(
        !journal
            .set_state_if(
                &run,
                &["running"],
                "blocked",
                Some("stale automatic tool outcome".into()),
                "stale-block"
            )
            .await
            .unwrap()
    );
    assert_eq!(
        projection(&db, &run).await,
        ("cancelled".into(), Some("user".into()))
    );
    assert_eq!(journal.replay(&run, 0).await.unwrap(), before);
    assert!(
        !journal
            .set_state_if(&run, &["cancelled"], "running", None, "forbidden-resume")
            .await
            .unwrap()
    );
    assert_eq!(
        projection(&db, &run).await,
        ("cancelled".into(), Some("user".into()))
    );
    db.shutdown().await.unwrap();
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn independent_cancellation_and_completion_callers_have_one_atomic_terminal_winner() {
    let (_root, db, journal) = fixture().await;
    for n in 0..8 {
        let run = journal
            .create_run(&format!("cas-race-{n}"), json!({}))
            .await
            .unwrap();
        let barrier = Arc::new(tokio::sync::Barrier::new(2));
        let cancel = tokio::spawn({
            let journal = journal.clone();
            let run = run.clone();
            let barrier = barrier.clone();
            async move {
                barrier.wait().await;
                journal
                    .set_state_if(
                        &run,
                        &["running"],
                        "cancelled",
                        Some("user".into()),
                        "cancel",
                    )
                    .await
                    .unwrap()
            }
        });
        let complete = tokio::spawn({
            let journal = journal.clone();
            let run = run.clone();
            let barrier = barrier.clone();
            async move {
                barrier.wait().await;
                journal
                    .set_state_if(&run, &["running"], "completed", None, "complete")
                    .await
                    .unwrap()
            }
        });
        let cancelled = cancel.await.unwrap();
        let completed = complete.await.unwrap();
        assert_ne!(
            cancelled, completed,
            "exactly one compare-and-set may append its terminal transition"
        );
        let projected = projection(&db, &run).await;
        assert_eq!(
            projected,
            if cancelled {
                ("cancelled".into(), Some("user".into()))
            } else {
                ("completed".into(), None)
            }
        );
        let events = journal.replay(&run, 0).await.unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[1].payload["state"], projected.0);
    }
    db.shutdown().await.unwrap();
}

#[tokio::test]
async fn effect_admission_checks_durable_control_instead_of_a_stale_projection() {
    let (_root, db, journal) = fixture().await;
    let run = journal
        .create_run("guarded-intents", json!({}))
        .await
        .unwrap();
    assert!(journal.is_running(&run).await.unwrap());
    assert!(
        journal
            .append_when_running(&run, "tool_intent", "first", json!({"index":0}))
            .await
            .unwrap()
            .is_some()
    );
    for (state, key) in [("paused", "pause"), ("cancelled", "cancel")] {
        journal
            .set_state(&run, state, Some("user".into()), key)
            .await
            .unwrap();
        let before = journal.replay(&run, 0).await.unwrap();
        db.write(Priority::Normal, {
            let run = run.clone();
            move |c| {
                c.execute(
                    "UPDATE runs SET state='running',reason=NULL WHERE id=?1",
                    [run],
                )?;
                Ok(())
            }
        })
        .await
        .unwrap();
        assert!(!journal.is_running(&run).await.unwrap());
        assert_eq!(
            journal
                .append_when_running(&run, "tool_intent", "forbidden", json!({"index":1}))
                .await
                .unwrap(),
            None
        );
        assert_eq!(journal.replay(&run, 0).await.unwrap(), before);
        // A finished admitted effect may still confirm its observed result.
        journal
            .append(
                &run,
                "tool_result",
                &format!("result-{key}"),
                json!({"observed":true}),
            )
            .await
            .unwrap();
    }
    db.shutdown().await.unwrap();
}
