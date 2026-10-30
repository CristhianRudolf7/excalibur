mod support;
use excalibur_engine::{Engine, Status};
use excalibur_journal::Journal;
use excalibur_orchestrator::{Control, Limits, Orchestrator};
use excalibur_store::{Database, DatabaseKind, ReadLane, StorageConfig};
use excalibur_types::runtime::{Effect, PreparedTool, ToolCall};
use serde_json::json;
use std::sync::Arc;

async fn running(f: &support::Fixture, name: &str) -> (String, String, i64) {
    let a = f
        .host
        .spawn(&f.master.id, support::delegation(name, "general", 2000))
        .await
        .unwrap();
    let run = a.run_id.unwrap();
    let j = Journal::new(f.db.clone()).await.unwrap();
    j.set_state(&run, "running", None, "test-running")
        .await
        .unwrap();
    let at = progress(&f.db, &run).await;
    (a.id, run, at)
}
async fn progress(db: &Database, run: &str) -> i64 {
    let r = run.to_owned();
    db.read(ReadLane::Interactive, move |c| {
        Ok(c.query_row(
            "SELECT MAX(created_at) FROM journal WHERE run_id=?1",
            [r],
            |row| row.get(0),
        )?)
    })
    .await
    .unwrap()
}

#[tokio::test]
async fn boundary_replay_and_registered_notice_survive_reopen() {
    let f = support::fixture().await;
    let (agent, run, at) = running(&f, "stalled").await;
    assert!(f.host.watchdog_runs(at + 599).await.unwrap().is_empty());
    assert_eq!(
        f.host.watchdog_runs(at + 600).await.unwrap(),
        vec![agent.clone()]
    );
    let j = Journal::new(f.db.clone()).await.unwrap();
    assert_eq!(
        j.state(&run).await.unwrap(),
        ("paused".into(), Some("watchdog_stalled".into()))
    );
    let reopened = Orchestrator::open(
        f.db.clone(),
        Arc::new(Engine::new(j, support::model(), support::no_tools())),
        Limits::default(),
    )
    .await
    .unwrap();
    reopened.watchdog_runs(at + 900).await.unwrap();
    assert_eq!(
        reopened
            .notifications("conversation:test")
            .await
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        reopened.notifications("conversation:test").await.unwrap()[0].class,
        "urgent"
    );
}

#[tokio::test]
async fn waiting_user_pause_and_pending_tool_are_excluded() {
    let f = support::fixture().await;
    let (agent, run, at) = running(&f, "user-pause").await;
    f.host.control(&agent, Control::Pause).await.unwrap();
    let (_, waiting, _) = running(&f, "waiting").await;
    let j = Journal::new(f.db.clone()).await.unwrap();
    j.set_state(&waiting, "waiting", Some("children".into()), "wait-child")
        .await
        .unwrap();
    let (_, tool, _) = running(&f, "long-tool").await;
    j.append(&tool,"tool_intent","test:tool",json!({"index":0,"prepared":PreparedTool{call:ToolCall{id:"long".into(),name:"long".into(),arguments:json!({})},effect:Effect::Pure,resources:vec![],exclusive:false,data:json!({})}})).await.unwrap();
    assert!(f.host.watchdog_runs(at + 900).await.unwrap().is_empty());
    assert_eq!(j.state(&run).await.unwrap().1, Some("user".into()));
    assert_eq!(j.state(&tool).await.unwrap().0, "running");
    assert_eq!(j.state(&waiting).await.unwrap().0, "waiting");
}

#[tokio::test]
async fn stale_revision_and_forged_watchdog_event_cannot_claim_a_notice() {
    let f = support::fixture().await;
    let (_, run, at) = running(&f, "revision").await;
    let j = Journal::new(f.db.clone()).await.unwrap();
    let e = Engine::new(j.clone(), support::model(), support::no_tools());
    let sequence = e.load(&run).await.unwrap().last_sequence;
    j.append(&run, "step_heartbeat", "fresh", json!({"step":0}))
        .await
        .unwrap();
    assert!(!e.pause_stalled_revision(&run, sequence).await.unwrap());
    let seq = e.load(&run).await.unwrap().last_sequence;
    j.set_state_if_revision(
        &run,
        &["running"],
        seq,
        "paused",
        Some("watchdog_stalled".into()),
        &format!("watchdog-stalled:{seq}"),
    )
    .await
    .unwrap();
    f.host.watchdog_runs(at + 900).await.unwrap();
    assert!(
        f.host
            .notifications("conversation:test")
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(e.load(&run).await.unwrap().status, Status::Paused);
}
