mod support;
use excalibur_orchestrator::*;
use serde_json::json;
use support::*;
#[tokio::test]
async fn mail_is_durable_idempotent_and_normal_directives_preserve_user_pause() {
    let f = fixture().await;
    let p = f
        .host
        .spawn(&f.master.id, delegation("p", "general", 300))
        .await
        .unwrap();
    let c = f
        .host
        .spawn(&p.id, delegation("c", "general", 60))
        .await
        .unwrap();
    f.host.control(&c.id, Control::Pause).await.unwrap();
    let m = f
        .host
        .send_message(
            "m",
            &p.id,
            &c.id,
            "directive",
            json!({"literal":"Exact new requirement"}),
            "normal",
        )
        .await
        .unwrap();
    let duplicate = f
        .host
        .send_message(
            "m",
            &p.id,
            &c.id,
            "directive",
            json!({"literal":"Exact new requirement"}),
            "normal",
        )
        .await
        .unwrap();
    assert_eq!(m.id, duplicate.id);
    assert!(
        f.host
            .send_message(
                "m",
                &p.id,
                &c.id,
                "directive",
                json!({"literal":"Changed"}),
                "normal"
            )
            .await
            .is_err()
    );
    assert_eq!(f.host.deliver_mailbox(&c.id).await.unwrap(), 0);
    assert_eq!(f.host.status(&c.id).await.unwrap().state, "paused");
    let reopened = Orchestrator::open(f.db.clone(), f.host.engine().clone(), Limits::default())
        .await
        .unwrap();
    assert_eq!(reopened.mailbox(&c.id).await.unwrap().len(), 1);
    f.host.control(&c.id, Control::Resume).await.unwrap();
    assert_eq!(f.host.deliver_mailbox(&c.id).await.unwrap(), 1);
    assert_eq!(f.host.mailbox(&c.id).await.unwrap()[0].state, "delivered");
    f.host
        .engine()
        .run(c.run_id.as_ref().unwrap())
        .await
        .unwrap();
    f.host.deliver_mailbox(&c.id).await.unwrap();
    assert!(f.host.mailbox(&c.id).await.unwrap().is_empty());
}
#[tokio::test]
async fn scoped_directive_reaches_descendants_but_not_other_projects() {
    let f = fixture().await;
    let p = f
        .host
        .spawn(&f.master.id, delegation("p", "general", 300))
        .await
        .unwrap();
    let c = f
        .host
        .spawn(&p.id, delegation("c", "general", 60))
        .await
        .unwrap();
    let messages = f
        .host
        .propagate_directive(
            &f.master.id,
            DirectiveImpact {
                scopes: std::collections::BTreeSet::from(["general".into()]),
                instruction_revision: 2,
                text: "Keep literal mandatory bytes".into(),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(messages.len(), 2);
    assert!(messages.iter().any(|m| m.recipient == c.id));
    assert!(
        messages
            .iter()
            .all(|m| m.payload["literal"] == "Keep literal mandatory bytes")
    );
}

#[tokio::test]
async fn child_report_is_context_data_without_invalidating_instruction_revision() {
    let f = fixture().await;
    let p = f
        .host
        .spawn(&f.master.id, delegation("p", "general", 300))
        .await
        .unwrap();
    let c = f
        .host
        .spawn(&p.id, delegation("c", "general", 60))
        .await
        .unwrap();
    let run = p.run_id.as_ref().unwrap();
    let before = f.host.engine().load(run).await.unwrap().revision;
    f.host
        .send_message(
            "report",
            &c.id,
            &p.id,
            "report",
            json!({"summary":"Synthetic progress"}),
            "normal",
        )
        .await
        .unwrap();
    f.host.deliver_mailbox(&p.id).await.unwrap();
    let state = f.host.engine().load(run).await.unwrap();
    assert_eq!(state.revision, before);
    assert!(
        state
            .history
            .iter()
            .any(|m| m.content.contains("Synthetic progress"))
    );
}

#[tokio::test]
async fn question_to_master_preserves_payload_and_creates_one_urgent_notification() {
    let f = fixture().await;
    let p = f
        .host
        .spawn(&f.master.id, delegation("p", "general", 300))
        .await
        .unwrap();
    let payload = json!({"question":"Which synthetic destination?","blocking":true});
    let one = f
        .host
        .send_message(
            "question",
            &p.id,
            &f.master.id,
            "question",
            payload.clone(),
            "normal",
        )
        .await
        .unwrap();
    let two = f
        .host
        .send_message(
            "question",
            &p.id,
            &f.master.id,
            "question",
            payload.clone(),
            "normal",
        )
        .await
        .unwrap();
    assert_eq!(one.id, two.id);
    let notices = f.host.notifications("conversation:test").await.unwrap();
    assert_eq!(notices.len(), 1);
    assert_eq!(notices[0].class, "urgent");
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&notices[0].content).unwrap(),
        payload
    );
}

#[tokio::test]
async fn task_correction_reaches_child_tasks_on_its_branch_only() {
    let f = fixture().await;
    let p = f
        .host
        .spawn(&f.master.id, delegation("p", "general", 300))
        .await
        .unwrap();
    let q = f
        .host
        .spawn(&f.master.id, delegation("q", "general", 300))
        .await
        .unwrap();
    let c = f
        .host
        .spawn(&p.id, delegation("c", "general", 60))
        .await
        .unwrap();
    let d = f
        .host
        .spawn(&q.id, delegation("d", "general", 60))
        .await
        .unwrap();
    let messages = f
        .host
        .propagate_directive(
            &f.master.id,
            DirectiveImpact {
                tasks: std::collections::BTreeSet::from([p.task.clone()]),
                instruction_revision: 3,
                text: "CSV, not PDF".into(),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(messages.len(), 2);
    assert!(messages.iter().any(|m| m.recipient == p.id));
    assert!(messages.iter().any(|m| m.recipient == c.id));
    assert!(
        !messages
            .iter()
            .any(|m| m.recipient == q.id || m.recipient == d.id)
    );
}

#[tokio::test]
async fn directive_deadline_escalates_at_120_seconds_or_two_completed_steps() {
    use excalibur_types::runtime::{Message, ModelResponse, Usage};
    let f = fixture().await;
    let p = f
        .host
        .spawn(&f.master.id, delegation("p", "general", 300))
        .await
        .unwrap();
    let run = p.run_id.as_ref().unwrap();
    f.host
        .engine()
        .journal()
        .set_state_if(run, &["ready"], "running", None, "fixture-running")
        .await
        .unwrap();
    let message = f
        .host
        .send_message(
            "deadline",
            &f.master.id,
            &p.id,
            "directive",
            json!({"literal":"Bounded correction"}),
            "normal",
        )
        .await
        .unwrap();
    assert!(
        f.host
            .enforce_directive_deadlines(message.created + 119)
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        f.host
            .enforce_directive_deadlines(message.created + 120)
            .await
            .unwrap(),
        vec![p.id.clone()]
    );
    assert_eq!(
        f.host.mailbox(&p.id).await.unwrap()[0].priority,
        "interrupt"
    );
    let q = f
        .host
        .spawn(&f.master.id, delegation("q", "general", 300))
        .await
        .unwrap();
    let run = q.run_id.as_ref().unwrap();
    f.host
        .engine()
        .journal()
        .set_state_if(run, &["ready"], "running", None, "fixture-running")
        .await
        .unwrap();
    let message = f
        .host
        .send_message(
            "step-deadline",
            &f.master.id,
            &q.id,
            "directive",
            json!({"literal":"Step correction"}),
            "normal",
        )
        .await
        .unwrap();
    for step in 0..2 {
        f.host.engine().journal().append(run,"model_response",&format!("fixture-response:{step}"),json!({"response":ModelResponse{message:Message::text("assistant","Synthetic step"),usage:Usage::default()}})).await.unwrap();
        f.host
            .engine()
            .journal()
            .append(
                run,
                "step_completed",
                &format!("fixture-step:{step}"),
                json!({}),
            )
            .await
            .unwrap();
        if step == 0 {
            assert!(
                f.host
                    .enforce_directive_deadlines(message.created)
                    .await
                    .unwrap()
                    .is_empty()
            );
        }
    }
    assert_eq!(
        f.host
            .enforce_directive_deadlines(message.created)
            .await
            .unwrap(),
        vec![q.id.clone()]
    );
    assert_eq!(
        f.host.mailbox(&q.id).await.unwrap()[0].priority,
        "interrupt"
    );
}

#[tokio::test]
async fn long_after_completion_tool_defers_deadline_until_confirmed_result() {
    use excalibur_types::runtime::{Effect, PreparedTool, ToolCall, ToolResult};
    let f = fixture().await;
    let p = f
        .host
        .spawn(&f.master.id, delegation("p", "general", 300))
        .await
        .unwrap();
    let run = p.run_id.as_ref().unwrap();
    f.host
        .engine()
        .journal()
        .set_state_if(run, &["ready"], "running", None, "fixture-running")
        .await
        .unwrap();
    let prepared = PreparedTool {
        call: ToolCall {
            id: "synthetic-atomic".into(),
            name: "fixture_atomic".into(),
            arguments: "{}".into(),
        },
        effect: Effect::Keyed,
        resources: vec![],
        exclusive: true,
        data: json!({"cancel":"after_completion","timeout_ms":300000}),
    };
    f.host
        .engine()
        .journal()
        .append(
            run,
            "tool_intent",
            "fixture-intent",
            json!({"index":0,"prepared":prepared}),
        )
        .await
        .unwrap();
    let message = f
        .host
        .send_message(
            "long-tool-correction",
            &f.master.id,
            &p.id,
            "directive",
            json!({"literal":"Change next operation"}),
            "normal",
        )
        .await
        .unwrap();
    assert!(
        f.host
            .enforce_directive_deadlines(message.created + 121)
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(f.host.status(&p.id).await.unwrap().state, "running");
    assert_eq!(f.host.mailbox(&p.id).await.unwrap()[0].priority, "normal");
    f.host.engine().journal().append(run,"tool_result","fixture-result",json!({"index":0,"name":"fixture_atomic","result":ToolResult::ok(json!({"synthetic":true}))})).await.unwrap();
    assert_eq!(
        f.host
            .enforce_directive_deadlines(message.created + 121)
            .await
            .unwrap(),
        vec![p.id]
    );
}
