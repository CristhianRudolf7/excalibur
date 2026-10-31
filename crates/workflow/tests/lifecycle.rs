mod common;

use common::{Recorder, clock, definition, open, tool};
use excalibur_types::runtime::Effect;
use excalibur_workflow::{Mode, Node, NodeKind, RetryPolicy, Scenario, Status, WorkflowDefinition};
use serde_json::json;

fn chain() -> WorkflowDefinition {
    definition(
        "chain",
        1,
        vec![
            tool("a", Effect::Idempotent, Some("pause"), false),
            Node {
                id: "pause".into(),
                kind: NodeKind::Wait {
                    seconds: 60,
                    next: Some("b".into()),
                },
            },
            tool("b", Effect::Keyed, None, false),
        ],
    )
}

#[tokio::test]
async fn live_chain_runs_tools_and_wait_to_completion() {
    let tmp = tempfile::tempdir().unwrap();
    let at = clock(1_700_000_000);
    let (_db, wf) = open(&tmp.path().join("wf.db"), at.clone()).await;
    wf.publish(chain()).await.unwrap();

    let snap = wf
        .start("inst1", "chain", None, json!({"goal": "demo"}), Mode::Live)
        .await
        .unwrap();
    assert_eq!(snap.cursor.as_deref(), Some("a"));

    let rec = Recorder::default();
    let status = wf.advance("inst1", &rec).await.unwrap();
    let until = match status {
        Status::Waiting { until } => until,
        other => panic!("expected a durable wait after a, got {other:?}"),
    };
    assert_eq!(until, 1_700_000_060);
    assert_eq!(rec.applied_tools(), vec!["tool_a".to_string()]);

    // The wait is durable: advancing before the clock passes stays parked.
    let status = wf.advance("inst1", &rec).await.unwrap();
    assert!(matches!(status, Status::Waiting { .. }));
    assert_eq!(rec.calls().len(), 1);

    at.advance(61);
    let status = wf.advance("inst1", &rec).await.unwrap();
    assert_eq!(status, Status::Completed);
    assert_eq!(
        rec.applied_tools(),
        vec!["tool_a".to_string(), "tool_b".to_string()]
    );

    // One idempotency key per node, stable across the whole run.
    let calls = rec.calls();
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0].idempotency_key, wf.idempotency_key("inst1", "a"));
    assert_eq!(calls[1].idempotency_key, wf.idempotency_key("inst1", "b"));
    assert_ne!(calls[0].idempotency_key, calls[1].idempotency_key);

    let snap = wf.snapshot("inst1").await.unwrap();
    assert_eq!(snap.status, Status::Completed);
    assert!(snap.cursor.is_none());
    // The finished wait records a `waited` output alongside the tool outputs.
    assert_eq!(snap.outputs.len(), 3);
    assert_eq!(snap.outputs["pause"], json!({"waited": true}));

    let kinds: Vec<String> = wf
        .log("inst1")
        .await
        .unwrap()
        .into_iter()
        .map(|e| e.kind)
        .collect();
    assert!(kinds.contains(&"started".to_string()));
    assert!(kinds.contains(&"wait".to_string()));
    assert!(kinds.contains(&"wait_done".to_string()));
    assert_eq!(kinds.last().unwrap(), "result");
}

#[tokio::test]
async fn dry_run_uses_declared_outputs_and_never_executes() {
    let tmp = tempfile::tempdir().unwrap();
    let (_db, wf) = open(&tmp.path().join("wf.db"), clock(1_700_000_000)).await;
    wf.publish(chain()).await.unwrap();
    wf.start("dry1", "chain", None, json!({}), Mode::DryRun)
        .await
        .unwrap();

    let status = wf.simulate("dry1", &Scenario::default()).await.unwrap();
    assert_eq!(status, Status::Completed);
    let snap = wf.snapshot("dry1").await.unwrap();
    assert_eq!(snap.outputs["a"], json!({"simulated": "a"}));

    // A tool with no declared dry-run behaviour fails closed instead of running.
    let bare = definition(
        "bare",
        1,
        vec![Node {
            id: "x".into(),
            kind: NodeKind::Tool {
                tool: "real_tool".into(),
                input: json!({}),
                effect: Effect::Idempotent,
                compensation: None,
                dry_run: None,
                retry: RetryPolicy::default(),
                next: None,
            },
        }],
    );
    wf.publish(bare).await.unwrap();
    wf.start("dry2", "bare", None, json!({}), Mode::DryRun)
        .await
        .unwrap();
    let status = wf.simulate("dry2", &Scenario::default()).await.unwrap();
    assert!(matches!(status, Status::Failed { .. }), "got {status:?}");
}

#[tokio::test]
async fn definition_validation_rejects_bad_graphs() {
    let dup = definition(
        "dup",
        1,
        vec![
            tool("a", Effect::Pure, None, false),
            tool("a", Effect::Pure, None, false),
        ],
    );
    assert!(dup.validate().is_err());

    let missing = definition(
        "missing",
        1,
        vec![tool("a", Effect::Pure, Some("ghost"), false)],
    );
    assert!(missing.validate().is_err());

    let cyclic = definition(
        "cyclic",
        1,
        vec![
            tool("a", Effect::Pure, Some("b"), false),
            tool("b", Effect::Pure, Some("a"), false),
        ],
    );
    assert!(cyclic.validate().is_err());

    let pure_comp = definition("pure", 1, vec![tool("a", Effect::Pure, None, true)]);
    assert!(pure_comp.validate().is_err());
}

#[tokio::test]
async fn describe_renders_textual_flow() {
    let text = chain().describe();
    assert!(text.contains("workflow chain v1 start a"), "{text}");
    assert!(text.contains("a [tool] -> pause"), "{text}");
    assert!(text.contains("pause [wait] -> b"), "{text}");
    assert!(text.contains("b [tool] -> (end)"), "{text}");
}
