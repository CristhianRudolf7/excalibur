mod common;

use common::{Recorder, approval, clock, definition, open, tool};
use excalibur_types::runtime::Effect;
use excalibur_workflow::{Error, Mode, Status};
use serde_json::json;

fn gated() -> excalibur_workflow::WorkflowDefinition {
    definition(
        "gated",
        1,
        vec![
            tool("a", Effect::Idempotent, Some("m"), true),
            approval("m", Some("b")),
            tool("b", Effect::Keyed, None, false),
        ],
    )
}

#[tokio::test]
async fn approval_gate_blocks_until_decided() {
    let tmp = tempfile::tempdir().unwrap();
    let (_db, wf) = open(&tmp.path().join("wf.db"), clock(1_700_000_000)).await;
    wf.publish(gated()).await.unwrap();
    wf.start("inst1", "gated", None, json!({}), Mode::Live)
        .await
        .unwrap();

    let rec = Recorder::default();
    let status = wf.advance("inst1", &rec).await.unwrap();
    assert!(
        matches!(status, Status::WaitingApproval { ref node } if node == "m"),
        "got {status:?}"
    );
    assert_eq!(rec.applied_tools(), vec!["tool_a".to_string()]);

    // Deciding the wrong node is rejected; the gate stays shut.
    let wrong = wf.decide_approval("inst1", "b", true, "tester").await;
    assert!(matches!(wrong, Err(Error::NotAllowed(_))), "got {wrong:?}");

    wf.decide_approval("inst1", "m", true, "tester")
        .await
        .unwrap();
    let status = wf.advance("inst1", &rec).await.unwrap();
    assert_eq!(status, Status::Completed);
    assert_eq!(
        rec.applied_tools(),
        vec!["tool_a".to_string(), "tool_b".to_string()]
    );

    let steps = wf.steps("inst1").await.unwrap();
    let gate = steps.iter().find(|s| s.node_id == "m").unwrap();
    assert_eq!(gate.phase, "done");
    assert_eq!(
        gate.output,
        Some(json!({"approved": true, "decided_by": "tester"}))
    );
}

#[tokio::test]
async fn rejection_aborts_and_compensates_in_reverse() {
    let tmp = tempfile::tempdir().unwrap();
    let (_db, wf) = open(&tmp.path().join("wf.db"), clock(1_700_000_000)).await;
    wf.publish(gated()).await.unwrap();
    wf.start("inst2", "gated", None, json!({}), Mode::Live)
        .await
        .unwrap();

    let rec = Recorder::default();
    let status = wf.advance("inst2", &rec).await.unwrap();
    assert!(
        matches!(status, Status::WaitingApproval { .. }),
        "got {status:?}"
    );

    wf.decide_approval("inst2", "m", false, "tester")
        .await
        .unwrap();
    let status = wf.advance("inst2", &rec).await.unwrap();
    let reason = match status {
        Status::Aborted { reason } => reason,
        other => panic!("expected abort after rejection, got {other:?}"),
    };
    assert!(reason.contains("rejected"), "{reason}");

    // The saga undid the completed tool step with its compensation key.
    let calls = rec.calls();
    let undo = calls
        .iter()
        .find(|c| c.tool == "undo_a")
        .expect("no compensation ran");
    assert!(undo.compensation);
    assert_eq!(
        undo.idempotency_key,
        format!("{}:compensate", wf.idempotency_key("inst2", "a"))
    );
    assert_eq!(wf.steps("inst2").await.unwrap()[0].phase, "compensated");

    // Repeating the recorded decision is an idempotent replay; a different
    // value conflicts with the immutable recorded decision.
    let snap = wf
        .decide_approval("inst2", "m", false, "tester")
        .await
        .unwrap();
    assert!(matches!(snap, Status::Aborted { .. }));
    let again = wf.decide_approval("inst2", "m", true, "tester").await;
    assert!(matches!(again, Err(Error::Conflict)), "got {again:?}");
}
