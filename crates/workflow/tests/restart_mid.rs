mod common;

use common::{Recorder, SCOPE, clock, definition, open, tool};
use excalibur_store::{Priority, rusqlite::params};
use excalibur_types::runtime::Effect;
use excalibur_workflow::{Mode, Status};
use serde_json::json;

#[tokio::test]
async fn reopen_after_completion_replays_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("wf.db");
    let def = || {
        definition(
            "pair",
            1,
            vec![
                tool("a", Effect::Idempotent, Some("b"), false),
                tool("b", Effect::Keyed, None, false),
            ],
        )
    };
    let rec1 = Recorder::default();
    {
        let (db, wf) = open(&path, clock(1_700_000_000)).await;
        wf.publish(def()).await.unwrap();
        wf.start("inst1", "pair", None, json!({}), Mode::Live)
            .await
            .unwrap();
        let status = wf.advance("inst1", &rec1).await.unwrap();
        assert_eq!(status, Status::Completed);
        assert_eq!(rec1.calls().len(), 2);
        drop(wf);
        drop(db);
    }
    // A new process handle on the same file sees the finished instance and
    // never re-executes its effects.
    {
        let (_db, wf) = open(&path, clock(1_700_000_000)).await;
        let snap = wf.snapshot("inst1").await.unwrap();
        assert_eq!(snap.status, Status::Completed);
        assert_eq!(snap.outputs.len(), 2);
        let rec2 = Recorder::default();
        let status = wf.advance("inst1", &rec2).await.unwrap();
        assert_eq!(status, Status::Completed);
        assert!(rec2.calls().is_empty());
    }
}

#[tokio::test]
async fn crash_between_intent_and_result_replays_with_same_key() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("wf.db");
    let (db, wf) = open(&path, clock(1_700_000_000)).await;
    wf.publish(definition(
        "solo",
        1,
        vec![tool("a", Effect::Keyed, None, false)],
    ))
    .await
    .unwrap();
    wf.start("inst9", "solo", None, json!({}), Mode::Live)
        .await
        .unwrap();

    // Simulate a kill after the intent write but before the result write.
    let key = wf.idempotency_key("inst9", "a");
    let scope = SCOPE.to_string();
    let key2 = key.clone();
    db.write(Priority::Journal, move |c| {
        c.execute(
            "INSERT INTO workflow_steps(scope,instance_id,node_id,seq,phase,idempotency_key,\
             attempts,logical_failures,output,wake_at,started_at,completed_at) \
             VALUES(?1,'inst9','a',1,'intent',?2,1,0,NULL,NULL,1700000000,NULL)",
            params![scope, key2],
        )?;
        c.execute(
            "UPDATE workflow_instances SET next_seq=2 WHERE scope=?1 AND id='inst9'",
            params![scope],
        )?;
        Ok(())
    })
    .await
    .unwrap();
    drop(wf);
    drop(db);

    // The restarted runtime replays the interrupted node with the same key, and
    // the keyed host applies the effect exactly once.
    let (_db2, wf2) = open(&path, clock(1_700_000_000)).await;
    let steps = wf2.steps("inst9").await.unwrap();
    assert_eq!(steps.len(), 1);
    assert_eq!(steps[0].phase, "intent");

    let rec = Recorder::default();
    let status = wf2.advance("inst9", &rec).await.unwrap();
    assert_eq!(status, Status::Completed);
    let calls = rec.calls();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].idempotency_key, key);
    assert_eq!(calls[0].attempt, 1);
    assert_eq!(rec.applied_tools(), vec!["tool_a".to_string()]);
}
