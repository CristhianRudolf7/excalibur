use excalibur_journal::Journal;
use excalibur_security::{
    Action, Autonomy, CapabilityToken, Decision, Redactor, Root, RunSecurity, Security,
};
use excalibur_store::{Database, DatabaseKind, Priority, ReadLane, StorageConfig};
use excalibur_types::runtime::FinalizationState;
use serde_json::json;
use std::{
    future::{Future, poll_fn},
    task::Poll,
};
struct Fixture {
    _temp: tempfile::TempDir,
    db: Database,
    journal: Journal,
    first: Security,
    second: Security,
    run: String,
    root: std::path::PathBuf,
}
async fn fixture() -> Fixture {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    let db = Database::open(
        root.join("main.db"),
        DatabaseKind::Main,
        StorageConfig::default(),
    )
    .unwrap();
    let journal = Journal::new(db.clone()).await.unwrap();
    let run = journal
        .create_run("synthetic-finalization", json!({}))
        .await
        .unwrap();
    let token = CapabilityToken {
        roots: vec![Root {
            path: root.clone(),
            write: true,
        }],
        budget: 10,
        ..Default::default()
    };
    let first = Security::new(db.clone(), token.clone(), Redactor::default())
        .await
        .unwrap();
    first
        .register_run(
            &run,
            RunSecurity {
                parent: None,
                project: "synthetic".into(),
                workspace: root.clone(),
                token: token.clone(),
                autonomy: vec![Autonomy::Consultative],
                full_acknowledged: false,
                user_requested: true,
            },
            100,
        )
        .await
        .unwrap();
    let second = Security::new(db.clone(), token, Redactor::default())
        .await
        .unwrap();
    Fixture {
        _temp: temp,
        db,
        journal,
        first,
        second,
        run,
        root,
    }
}
async fn audits(db: &Database) -> i64 {
    db.read(ReadLane::Interactive, |c| {
        Ok(c.query_row("SELECT count(*) FROM audit", [], |row| row.get(0))?)
    })
    .await
    .unwrap()
}
#[tokio::test]
async fn independent_security_instances_share_the_guard_and_no_request_can_follow_terminal_commit()
{
    let f = fixture().await;
    let held = f.first.finalization_guard(&f.run, 100).await.unwrap();
    assert_eq!(held.state(), FinalizationState::Ready);
    let before = audits(&f.db).await;
    let mut competing = Box::pin(f.second.finalization_guard(&f.run, 100));
    assert!(
        poll_fn(|context| Poll::Ready(matches!(competing.as_mut().poll(context), Poll::Pending)))
            .await
    );
    // The second guard must stop at the shared lock, before its maintenance write.
    // A same-priority FIFO barrier detects a wrongly independent lock without sleeps.
    f.db.write(Priority::Journal, |connection| {
        connection.execute_batch("SELECT 1")?;
        Ok(())
    })
    .await
    .unwrap();
    assert_eq!(audits(&f.db).await, before);
    f.journal
        .set_state(&f.run, "completed", None, "synthetic-terminal")
        .await
        .unwrap();
    drop(held);
    drop(competing.await.unwrap());
    // Tamper the projection to prove the terminal journal remains authoritative.
    let run = f.run.clone();
    f.db.write(Priority::Normal, move |connection| {
        connection.execute("UPDATE runs SET state='running' WHERE id=?1", [run])?;
        Ok(())
    })
    .await
    .unwrap();
    assert!(
        f.second
            .authorize(
                &f.run,
                &Action::Write {
                    path: f.root.join("late-file")
                },
                101
            )
            .await
            .is_err()
    );
    let pending: i64 =
        f.db.read(ReadLane::Interactive, |connection| {
            Ok(connection.query_row(
                "SELECT count(*) FROM security_approvals WHERE status='pending'",
                [],
                |row| row.get(0),
            )?)
        })
        .await
        .unwrap();
    assert_eq!(pending, 0);
    f.db.shutdown().await.unwrap();
}
#[tokio::test]
async fn guard_preserves_pending_and_expired_approval_facts_and_terminal_runs_cannot_renew() {
    let f = fixture().await;
    let Decision::Ask { approval_id, .. } = f
        .second
        .authorize(
            &f.run,
            &Action::Write {
                path: f.root.join("needs-permission"),
            },
            100,
        )
        .await
        .unwrap()
    else {
        panic!("consultative write must ask")
    };
    let held = f.first.finalization_guard(&f.run, 100).await.unwrap();
    assert_eq!(held.state(), FinalizationState::Pending);
    drop(held);
    let held = f
        .first
        .finalization_guard(&f.run, 100 + 7 * 86400 + 1)
        .await
        .unwrap();
    assert_eq!(
        held.state(),
        FinalizationState::Blocked("approval expired".into())
    );
    drop(held);
    f.journal
        .set_state(
            &f.run,
            "cancelled",
            Some("synthetic user cancellation".into()),
            "synthetic-cancel",
        )
        .await
        .unwrap();
    assert!(
        f.second
            .renew_approval(
                &approval_id,
                "new-authenticated-instruction",
                100 + 7 * 86400 + 2
            )
            .await
            .is_err()
    );
    f.db.shutdown().await.unwrap();
}
