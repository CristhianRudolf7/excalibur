mod support;
use excalibur_engine::Engine;
use excalibur_orchestrator::*;
use excalibur_types::runtime::*;
use serde_json::json;
use std::sync::Arc;
use support::*;
#[tokio::test]
async fn normal_notice_members_release_together_at_fixed_five_second_boundary() {
    let f = fixture().await;
    let p = f
        .host
        .spawn(&f.master.id, delegation("p", "general", 300))
        .await
        .unwrap();
    f.host
        .notify("one", &p.id, "normal", "One", 10)
        .await
        .unwrap();
    f.host
        .notify("two", &p.id, "normal", "Two", 14)
        .await
        .unwrap();
    assert!(f.host.pending_deliveries(14).await.unwrap().is_empty());
    assert_eq!(f.host.pending_deliveries(15).await.unwrap().len(), 2);
}
#[tokio::test]
async fn iana_quiet_hours_follow_winter_and_summer_clock_changes() {
    for (instant, quiet) in [
        ("2026-01-15T01:30:00Z", true),
        ("2026-07-15T00:30:00Z", true),
        ("2026-01-15T00:30:00Z", false),
        ("2026-07-15T01:30:00Z", false),
    ] {
        let f = fixture().await;
        let p = f
            .host
            .spawn(&f.master.id, delegation("p", "general", 300))
            .await
            .unwrap();
        f.host
            .set_notification_policy(
                "conversation:test",
                NotificationPolicy {
                    timezone: Some("Europe/Berlin".into()),
                    quiet_start_minute: Some(120),
                    quiet_end_minute: Some(180),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        let at = chrono::DateTime::parse_from_rfc3339(instant)
            .unwrap()
            .timestamp();
        f.host
            .notify("normal", &p.id, "normal", "Done", at - 10)
            .await
            .unwrap();
        assert_eq!(
            f.host.pending_deliveries(at).await.unwrap().is_empty(),
            quiet,
            "{instant}"
        );
    }
    let f = fixture().await;
    assert!(
        f.host
            .set_notification_policy(
                "conversation:test",
                NotificationPolicy {
                    timezone: Some("Invented/Nowhere".into()),
                    ..Default::default()
                }
            )
            .await
            .is_err()
    );
}
#[tokio::test]
async fn status_bounds_tool_arguments_uses_real_intent_age_and_report_reminder_reaches_model_context()
 {
    let f = fixture().await;
    let adapter = Arc::new(OrchestratorTools::new(no_tools()));
    let engine = Arc::new(Engine::new(
        f.host.engine().journal().clone(),
        model(),
        adapter.clone(),
    ));
    let host = Orchestrator::open(f.db.clone(), engine, Limits::default())
        .await
        .unwrap()
        .with_authority(authority());
    adapter.attach(&host).unwrap();
    let p = host
        .spawn(&f.master.id, delegation("p", "general", 300))
        .await
        .unwrap();
    let run = p.run_id.as_ref().unwrap();
    host.engine()
        .journal()
        .set_state_if(run, &["ready"], "running", None, "running")
        .await
        .unwrap();
    for step in 0..15 {
        host.engine().journal().append(run,"model_response",&format!("response:{step}"),json!({"response":ModelResponse{message:Message::text("assistant","Synthetic progress"),usage:Usage::default()}})).await.unwrap();
        host.engine()
            .journal()
            .append(run, "step_completed", &format!("step:{step}"), json!({}))
            .await
            .unwrap();
    }
    adapter.begin_step(run, 15).await.unwrap();
    adapter.begin_step(run, 15).await.unwrap();
    let events = host.engine().journal().replay(run, 0).await.unwrap();
    assert_eq!(
        events
            .iter()
            .filter(|event| event.kind == "agent_message_received"
                && event.payload["content"]["reminder"].is_string())
            .count(),
        1
    );
    host.engine().run(run).await.unwrap();
    assert!(
        host.engine()
            .journal()
            .replay(run, 0)
            .await
            .unwrap()
            .iter()
            .any(|event| event.kind == "model_request"
                && event
                    .payload
                    .to_string()
                    .contains("Update your plan or send a brief progress report"))
    );
    let q = host
        .spawn(&f.master.id, delegation("q", "general", 300))
        .await
        .unwrap();
    let qrun = q.run_id.as_ref().unwrap();
    let prepared = PreparedTool {
        call: ToolCall {
            id: "long-arguments".into(),
            name: "fixture".into(),
            arguments: "z".repeat(3000),
        },
        effect: Effect::Pure,
        resources: vec![],
        exclusive: true,
        data: json!({}),
    };
    let copy = qrun.clone();
    let payload = json!({"index":0,"prepared":prepared}).to_string();
    f.db.write(excalibur_store::Priority::Journal,move|c| {
        c.execute("INSERT INTO journal(id,run_id,sequence,kind,payload,state,idempotency_key,created_at) SELECT 'synthetic-age-intent',?1,COALESCE(MAX(sequence),0)+1,'tool_intent',?2,'confirmed','intent',unixepoch()-185 FROM journal WHERE run_id=?1",excalibur_store::rusqlite::params![copy,payload])?;Ok(())
    }).await.unwrap();
    let status = host.status(&q.id).await.unwrap();
    assert!(
        status.tools[0]["arguments_summary"]
            .as_str()
            .unwrap()
            .chars()
            .count()
            <= 513
    );
    assert!(status.tools[0].get("arguments").is_none());
    assert!(status.tools[0]["elapsed_seconds"].as_i64().unwrap() >= 185);
    let prepared = PreparedTool {
        call: ToolCall {
            id: "blob-arguments".into(),
            name: "fixture".into(),
            arguments: "z".repeat(70000),
        },
        effect: Effect::Pure,
        resources: vec![],
        exclusive: true,
        data: json!({}),
    };
    host.engine()
        .journal()
        .append(
            qrun,
            "tool_intent",
            "blob-intent",
            json!({"index":1,"prepared":prepared}),
        )
        .await
        .unwrap();
    let copy = qrun.clone();
    assert!(f.db.read(excalibur_store::ReadLane::Interactive,move|c|Ok(c.query_row("SELECT payload IS NULL AND payload_blob IS NOT NULL FROM journal WHERE run_id=?1 AND idempotency_key='blob-intent'",[copy],|r|r.get::<_,bool>(0))?)).await.unwrap());
    let status = host.status(&q.id).await.unwrap();
    assert_eq!(status.tools.len(), 2);
    assert!(
        status
            .tools
            .iter()
            .all(|tool| tool["arguments_summary"].as_str().unwrap().chars().count() <= 513)
    );
}

#[tokio::test]
async fn real_plan_revision_resets_report_cadence_at_the_next_safe_step() {
    let f = fixture().await;
    let _tasks = excalibur_tasks::TaskStore::open(f.db.clone())
        .await
        .unwrap();
    let adapter = Arc::new(OrchestratorTools::new(no_tools()));
    let engine = Arc::new(Engine::new(
        f.host.engine().journal().clone(),
        model(),
        adapter.clone(),
    ));
    let host = Orchestrator::open(f.db.clone(), engine, Limits::default())
        .await
        .unwrap()
        .with_authority(authority());
    adapter.attach(&host).unwrap();
    let p = host
        .spawn(&f.master.id, delegation("p", "general", 300))
        .await
        .unwrap();
    let task = p.task.clone();
    f.db.write(excalibur_store::Priority::Journal, move |c| {
        c.execute(
            "INSERT INTO tasks(id,title,scope_id) VALUES(?1,'Synthetic plan task','general')",
            [&task],
        )?;
        c.execute(
            "INSERT INTO plans(id,owner_task_id,scope_id) VALUES('synthetic-plan',?1,'general')",
            [task],
        )?;
        Ok(())
    })
    .await
    .unwrap();
    let run = p.run_id.as_ref().unwrap();
    adapter.begin_step(run, 0).await.unwrap();
    assert!(
        host.engine()
            .journal()
            .set_state_if(run, &["ready"], "running", None, "running")
            .await
            .unwrap()
    );
    for step in 0..30 {
        if step == 15 {
            f.db.write(excalibur_store::Priority::Journal,|c|{c.execute("UPDATE plans SET version=version+1,updated_at=unixepoch() WHERE id='synthetic-plan'",[])?;Ok(())}).await.unwrap();
            adapter.begin_step(run, 15).await.unwrap();
            assert!(!host.status(&p.id).await.unwrap().report_reminder);
        }
        host.engine().journal().append(run,"model_response",&format!("plan-response:{step}"),json!({"response":ModelResponse{message:Message::text("assistant","Synthetic step"),usage:Usage::default()}})).await.unwrap();
        host.engine()
            .journal()
            .append(
                run,
                "step_completed",
                &format!("plan-step:{step}"),
                json!({}),
            )
            .await
            .unwrap();
    }
    assert!(host.status(&p.id).await.unwrap().report_reminder);
    adapter.begin_step(run, 30).await.unwrap();
    assert_eq!(
        host.engine()
            .journal()
            .replay(run, 0)
            .await
            .unwrap()
            .iter()
            .filter(|event| event.kind == "agent_message_received"
                && event.payload["content"]["reminder"].is_string())
            .count(),
        1
    );
}
