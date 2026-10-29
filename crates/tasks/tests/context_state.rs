use excalibur_engine::CompletionGate;
use excalibur_journal::Journal;
use excalibur_store::{Database, DatabaseKind, StorageConfig};
use excalibur_tasks::{CheckRunner, TaskCompletionGate, TaskStore};
use excalibur_tools::{LocalTools, ToolPolicy};
use serde_json::json;
use std::sync::Arc;
#[tokio::test]
async fn deterministic_task_context_preserves_literal_sources_and_fresh_evidence() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("project");
    std::fs::create_dir(&workspace).unwrap();
    std::fs::write(workspace.join("proof.txt"), "OK\n").unwrap();
    let db = Database::open(
        temp.path().join("main.db"),
        DatabaseKind::Main,
        StorageConfig::default(),
    )
    .unwrap();
    let store = TaskStore::open(db.clone()).await.unwrap();
    let journal = Journal::new(db.clone()).await.unwrap();
    let run = journal.create_run("context-root", json!({})).await.unwrap();
    let literal = "  Literal user request.\nñ 🦀\n";
    store.host_batch("general","seed",json!({"ops":[
 {"op":"create_task","id":"task","title":"Objective","data":{"request_card":{"original":[{"msg_id":"message-1","text":literal,"private_reasoning":"do not promote this field"}],"open_questions":[{"id":"question-1","text":"Which branch?"}]},"metadata":{"workspace":workspace}}},
 {"op":"create_requirement","id":"requirement","data":{"text":"Preserve complete requirement"}},
 {"op":"create_criterion","id":"criterion","data":{"requirement_id":"requirement","statement":"Proof contains OK","method":"test","impact_area":["proof.txt"]}},
 {"op":"link_requirement","task":"task","requirement":"requirement"},
 {"op":"create_check","id":"check","data":{"criterion_id":"criterion","definition":{"kind":"file_contains","path":"proof.txt","needle":"OK"}}},
 {"op":"create_plan","id":"micro","data":{"kind":"micro","owner_task_id":"task","title":"Microplan","state":"active"}},
 {"op":"create_plan_item","id":"next","data":{"plan_id":"micro","title":"Next concrete action","state":"pending"}},
 {"op":"create_decision","id":"decision","data":{"statement":"Use deterministic facts","reason":"Do not trust a summary","alternatives":["Trust model prose"]}}
 ]})).await.unwrap();
    store.bind_root("task", &run).await.unwrap();
    let tools = Arc::new(
        LocalTools::new(db.clone(), workspace.clone(), ToolPolicy::default())
            .await
            .unwrap(),
    );
    let runner = CheckRunner::new(tools, workspace.clone());
    let gate = TaskCompletionGate::new(store.clone(), runner.clone());
    let first = gate.structured_context(&run).await.unwrap().unwrap();
    let originals: serde_json::Value = serde_json::from_str(&first.original_request).unwrap();
    assert_eq!(originals[0]["text"], literal);
    assert!(!first.original_request.contains("private_reasoning"));
    assert_eq!(first.requirements[0].text, "Preserve complete requirement");
    assert_eq!(first.criteria[0].state, "unverified");
    assert!(first.required_open_ids().contains("criterion"));
    assert!(first.required_open_ids().contains("question-1"));
    assert!(
        first
            .next_steps
            .iter()
            .any(|s| s.contains("Next concrete action"))
    );
    assert_eq!(first.decisions[0].id, "decision");
    runner
        .run_keyed(&store, "task", "check", &run, "proof-once")
        .await
        .unwrap();
    let passed = gate.structured_context(&run).await.unwrap().unwrap();
    assert_eq!(passed.criteria[0].state, "satisfied");
    std::fs::write(workspace.join("proof.txt"), "changed externally").unwrap();
    let stale = gate.structured_context(&run).await.unwrap().unwrap();
    assert_eq!(stale.criteria[0].state, "unverified");
    assert!(stale.required_open_ids().contains("criterion"));
    assert_eq!(stale.original_request, first.original_request);
    let unbound = journal.create_run("unbound", json!({})).await.unwrap();
    assert!(gate.structured_context(&unbound).await.unwrap().is_none());
    db.shutdown().await.unwrap();
}
