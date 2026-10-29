use excalibur_journal::Journal;
use excalibur_store::{Database, DatabaseKind, StorageConfig};
use excalibur_tools::{Access, LocalTools, ToolPolicy, evaluation::EpisodeTools};
use excalibur_types::runtime::{ToolCall, ToolRuntime};
use serde_json::json;
use std::sync::Arc;
#[tokio::test]
async fn pending_initiative_approval_prevents_real_file_effects_but_keeps_planning_available() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("work");
    std::fs::create_dir(&root).unwrap();
    let db = Database::open(
        temp.path().join("data/main.db"),
        DatabaseKind::Main,
        StorageConfig::default(),
    )
    .unwrap();
    let local = Arc::new(
        LocalTools::new(
            db.clone(),
            root.clone(),
            ToolPolicy {
                read: Access::Allow,
                write: Access::Allow,
                ..Default::default()
            },
        )
        .await
        .unwrap(),
    );
    let tools = EpisodeTools::new(local, db.clone(), "episode")
        .await
        .unwrap()
        .with_planning_autonomy(3)
        .unwrap();
    let tasks = tools.task_store();
    tasks.host_batch("episode","initiative",json!({"ops":[{"op":"create_task","id":"initiative","title":"Synthetic initiative","data":{"level":"N3","request_card":{"original":"Make a controlled change"}}}]})).await.unwrap();
    let run = Journal::new(db.clone())
        .await
        .unwrap()
        .create_run("initiative", json!({}))
        .await
        .unwrap();
    tasks.bind_root("initiative", &run).await.unwrap();
    let denied = tools
        .prepare(
            &run,
            ToolCall {
                id: "write".into(),
                name: "write_file".into(),
                arguments: json!({"path":"forbidden.txt","content":"not authorized"}).to_string(),
            },
        )
        .await;
    assert!(denied.unwrap_err().contains("approval"));
    assert!(!root.join("forbidden.txt").exists());
    let planning=tools.prepare(&run,ToolCall{id:"plan".into(),name:"task_db".into(),arguments:json!({"ops":[{"op":"create_plan","id":"micro","data":{"kind":"micro","owner_task_id":"initiative"}}]}).to_string()}).await.unwrap();
    assert!(
        tools
            .execute(&run, "plan-operation", &planning)
            .await
            .success
    );
    db.shutdown().await.unwrap();
}
