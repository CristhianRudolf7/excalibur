use excalibur_store::{Database, DatabaseKind, StorageConfig};
use excalibur_tasks::TaskStore;
use excalibur_types::{CriterionId, RequirementId, TaskId};
use serde_json::json;
#[tokio::test]
async fn request_card_reference_resolution_is_host_only_and_scope_checked_atomically() {
    let temp = tempfile::tempdir().unwrap();
    let db = Database::open(
        temp.path().join("main.db"),
        DatabaseKind::Main,
        StorageConfig::default(),
    )
    .unwrap();
    let store = TaskStore::open(db.clone()).await.unwrap();
    let task = TaskId::new();
    let req = RequirementId::from_ulid(task.ulid()).to_string();
    let criterion = CriterionId::from_ulid(task.ulid()).to_string();
    store.host_batch("general","refs",json!({"ops":[{"op":"create_requirement","id":req,"data":{"text":"Owned source"}},{"op":"create_criterion","id":criterion,"data":{"requirement_id":req,"statement":"Verified result","method":"inspection","expected":"result","impact_area":[]}}]})).await.unwrap();
    let create = json!({"ops":[{"op":"create_task","id":task.to_string(),"title":"Literal card","data":{"resolve_request_card_codes":true,"request_card":{"original":[{"text":"Keep this exact source"}],"requirements":[{"id":req}],"criteria":[{"id":criterion,"requirement_id":req}]}}}]});
    assert!(
        store
            .batch("general", "untrusted", create.clone())
            .await
            .is_err()
    );
    assert!(store.task("general", &task.to_string()).await.is_err());
    assert!(
        store
            .host_batch("project:other", "wrong-scope", create.clone())
            .await
            .is_err()
    );
    assert!(
        store
            .task("project:other", &task.to_string())
            .await
            .is_err()
    );
    let first = store
        .host_batch("general", "trusted", create.clone())
        .await
        .unwrap();
    assert_eq!(
        first,
        store
            .host_batch("general", "trusted", create)
            .await
            .unwrap()
    );
    let card = store
        .task("general", &task.to_string())
        .await
        .unwrap()
        .request_card;
    assert_eq!(card["requirements"][0]["code"], "R1");
    assert_eq!(card["criteria"][0]["code"], "C1.1");
    assert_eq!(card["original"][0]["text"], "Keep this exact source");
    db.shutdown().await.unwrap();
}
