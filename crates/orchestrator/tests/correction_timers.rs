mod support;
use excalibur_engine::{Status, WaitReason};
use excalibur_orchestrator::*;
use serde_json::json;
use support::*;
#[tokio::test]
async fn directive_deadline_counts_running_time_not_queue_user_pause_or_restart_downtime() {
    let f = fixture().await;
    let p = f
        .host
        .spawn(&f.master.id, delegation("p", "general", 300))
        .await
        .unwrap();
    let run = p.run_id.as_ref().unwrap();
    let message = f
        .host
        .send_message(
            "queued-directive",
            &f.master.id,
            &p.id,
            "directive",
            json!({"literal":"Preserve exact instruction"}),
            "normal",
        )
        .await
        .unwrap();
    let t = message.created;
    assert!(
        f.host
            .enforce_directive_deadlines(t + 500)
            .await
            .unwrap()
            .is_empty()
    );
    f.host
        .engine()
        .journal()
        .set_state_if(run, &["ready"], "running", None, "start")
        .await
        .unwrap();
    f.host.deliver_mailbox_at(&p.id, t + 500).await.unwrap();
    assert!(
        f.host
            .enforce_directive_deadlines(t + 500)
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        f.host
            .enforce_directive_deadlines(t + 550)
            .await
            .unwrap()
            .is_empty()
    );
    f.host.engine().pause(run).await.unwrap();
    assert!(
        f.host
            .enforce_directive_deadlines(t + 550)
            .await
            .unwrap()
            .is_empty()
    );
    f.host.engine().resume(run).await.unwrap();
    assert!(
        f.host
            .engine()
            .journal()
            .set_state_if(run, &["ready"], "running", None, "fixture-resumed")
            .await
            .unwrap()
    );
    assert!(
        f.host
            .enforce_directive_deadlines(t + 1000)
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        f.host
            .enforce_directive_deadlines(t + 1069)
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        f.host.enforce_directive_deadlines(t + 1070).await.unwrap(),
        vec![p.id.clone()]
    );
    let q = f
        .host
        .spawn(&f.master.id, delegation("q", "general", 300))
        .await
        .unwrap();
    let qrun = q.run_id.as_ref().unwrap();
    f.host
        .engine()
        .journal()
        .set_state_if(qrun, &["ready"], "running", None, "q-running")
        .await
        .unwrap();
    let m = f
        .host
        .send_message(
            "restart-directive",
            &f.master.id,
            &q.id,
            "directive",
            json!({"literal":"Different instruction"}),
            "normal",
        )
        .await
        .unwrap();
    f.host.deliver_mailbox_at(&q.id, m.created).await.unwrap();
    let reopened = Orchestrator::open(f.db.clone(), f.host.engine().clone(), Limits::default())
        .await
        .unwrap()
        .with_authority(authority());
    assert!(
        reopened
            .enforce_directive_deadlines(m.created + 10000)
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        reopened
            .enforce_directive_deadlines(m.created + 10120)
            .await
            .unwrap(),
        vec![q.id]
    );
}
#[tokio::test]
async fn mandatory_question_never_defaults_and_reminds_daily_until_answered() {
    let f = fixture().await;
    let p = f
        .host
        .spawn(&f.master.id, delegation("p", "general", 300))
        .await
        .unwrap();
    let c = f
        .host
        .spawn(&p.id, delegation("c", "general", 30))
        .await
        .unwrap();
    let run = c.run_id.as_ref().unwrap();
    let message=f.host.send_message("external-question",&c.id,&p.id,"question",json!({"question":"Authorize the synthetic external destination?","reason_kind":"external","blocking":false,"default":"never silently use this"}),"normal").await.unwrap();
    assert_eq!(message.payload["blocking"], true);
    f.host
        .engine()
        .journal()
        .set_state_if(run, &["ready"], "running", None, "running")
        .await
        .unwrap();
    f.host.engine().wait(run, WaitReason::User).await.unwrap();
    assert!(
        f.host
            .service_question_deadlines(message.created + 900)
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        f.host
            .service_question_deadlines(message.created + 86400)
            .await
            .unwrap(),
        vec![message.id.clone()]
    );
    assert!(
        f.host
            .service_question_deadlines(message.created + 86400)
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        f.host.engine().load(run).await.unwrap().status,
        Status::Waiting
    );
    assert!(
        !f.host
            .engine()
            .journal()
            .replay(run, 0)
            .await
            .unwrap()
            .iter()
            .any(|event| event.kind == "assumption_recorded")
    );
    assert_eq!(
        f.host
            .service_question_deadlines(message.created + 172800)
            .await
            .unwrap(),
        vec![message.id.clone()]
    );
    f.host
        .send_message(
            "manual-answer",
            &p.id,
            &c.id,
            "answer",
            json!({"question_id":message.id,"text":"Explicit destination"}),
            "normal",
        )
        .await
        .unwrap();
    f.host.deliver_mailbox(&c.id).await.unwrap();
    assert!(
        f.host
            .service_question_deadlines(message.created + 259200)
            .await
            .unwrap()
            .is_empty()
    );
}
#[tokio::test]
async fn optional_default_requires_actual_idle_wait_and_is_durable_idempotent_context_data() {
    let f = fixture().await;
    let p = f
        .host
        .spawn(&f.master.id, delegation("p", "general", 300))
        .await
        .unwrap();
    let c = f
        .host
        .spawn(&p.id, delegation("c", "general", 30))
        .await
        .unwrap();
    let run = c.run_id.as_ref().unwrap();
    let payload = json!({"question":"Which reversible output label?","reason_kind":"optional_preference","blocking":true,"default":"synthetic-label"});
    let message = f
        .host
        .send_message(
            "optional",
            &c.id,
            &p.id,
            "question",
            payload.clone(),
            "normal",
        )
        .await
        .unwrap();
    assert_eq!(message.payload["blocking"], false);
    assert_eq!(
        f.host
            .send_message("optional", &c.id, &p.id, "question", payload, "normal")
            .await
            .unwrap()
            .id,
        message.id
    );
    assert!(
        f.host
            .service_question_deadlines(message.created + 1000)
            .await
            .unwrap()
            .is_empty()
    );
    f.host
        .engine()
        .journal()
        .set_state_if(run, &["ready"], "running", None, "running")
        .await
        .unwrap();
    assert!(
        f.host
            .service_question_deadlines(message.created + 1000)
            .await
            .unwrap()
            .is_empty()
    );
    f.host.engine().wait(run, WaitReason::User).await.unwrap();
    assert_eq!(
        f.host
            .service_question_deadlines(message.created + 1001)
            .await
            .unwrap(),
        vec![message.id.clone()]
    );
    assert_eq!(
        f.host.engine().load(run).await.unwrap().status,
        Status::Running
    );
    assert!(
        f.host
            .service_question_deadlines(message.created + 1002)
            .await
            .unwrap()
            .is_empty()
    );
    let events = f.host.engine().journal().replay(run, 0).await.unwrap();
    assert_eq!(
        events
            .iter()
            .filter(|event| event.kind == "assumption_recorded")
            .count(),
        1
    );
    assert!(
        events
            .iter()
            .any(|event| event.kind == "agent_message_received"
                && event.payload["content"]["assumption"] == true)
    );
}
