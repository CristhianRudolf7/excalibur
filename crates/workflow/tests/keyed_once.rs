mod common;

use common::{Recorder, clock, definition, open, tool};
use excalibur_types::runtime::Effect;
use excalibur_workflow::{FailureClass, Mode, Status, StepFailure};
use serde_json::json;

#[tokio::test]
async fn transient_retries_reuse_the_node_key_and_apply_once() {
    let tmp = tempfile::tempdir().unwrap();
    let at = clock(1_700_000_000);
    let (_db, wf) = open(&tmp.path().join("wf.db"), at.clone()).await;
    wf.publish(definition(
        "once",
        1,
        vec![tool("a", Effect::Keyed, None, false)],
    ))
    .await
    .unwrap();
    wf.start("inst1", "once", None, json!({}), Mode::Live)
        .await
        .unwrap();

    let rec = Recorder::default();
    rec.fail(
        "a",
        vec![
            Err(StepFailure {
                retry_after: Some(600),
                ..StepFailure::new(FailureClass::Transient, "provider 503")
            }),
            Err(StepFailure::new(FailureClass::Transient, "timeout")),
            Ok(json!({"ok": true})),
        ],
    );

    // First attempt fails transient with a Retry-After hint: backoff honours it.
    let status = wf.advance("inst1", &rec).await.unwrap();
    let until = match status {
        Status::Waiting { until } => until,
        other => panic!("expected backoff wait, got {other:?}"),
    };
    assert!(until >= 1_700_000_600, "hint not honoured: {until}");
    let key = rec.calls()[0].idempotency_key.clone();

    at.advance(700);
    let status = wf.advance("inst1", &rec).await.unwrap();
    assert!(matches!(status, Status::Waiting { .. }), "got {status:?}");

    at.advance(200);
    let status = wf.advance("inst1", &rec).await.unwrap();
    assert_eq!(status, Status::Completed);

    // Three executions, one key, one application: without the stable key the
    // retried effect would have applied three times.
    let calls = rec.calls();
    assert_eq!(calls.len(), 3);
    assert!(calls.iter().all(|c| c.idempotency_key == key));
    assert_eq!(key, wf.idempotency_key("inst1", "a"));
    assert_eq!(rec.applied_tools(), vec!["tool_a".to_string()]);

    let steps = wf.steps("inst1").await.unwrap();
    assert_eq!(steps.len(), 1);
    assert_eq!(steps[0].attempts, 3);
    assert_eq!(steps[0].phase, "done");

    let kinds: Vec<String> = wf
        .log("inst1")
        .await
        .unwrap()
        .into_iter()
        .map(|e| e.kind)
        .collect();
    assert_eq!(kinds.iter().filter(|k| *k == "failure").count(), 2);
    assert_eq!(kinds.iter().filter(|k| *k == "retry_scheduled").count(), 2);
}
