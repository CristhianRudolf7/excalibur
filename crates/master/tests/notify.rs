use excalibur_master::*;
use excalibur_models::{ModelError, ModelGateway};
use excalibur_store::{Database, DatabaseKind, StorageConfig};
use excalibur_types::runtime::*;
use excalibur_vault::Redactor;
use serde_json::json;
use std::sync::Arc;
struct Unavailable;
impl ModelGateway for Unavailable {
    fn complete<'a>(
        &'a self,
        _: ModelRequest,
    ) -> BoxFuture<'a, std::result::Result<ModelResponse, ModelError>> {
        Box::pin(async { Err(ModelError::Provider) })
    }
}
async fn service() -> (tempfile::TempDir, Database, Master) {
    let dir = tempfile::tempdir().unwrap();
    let db = Database::open(
        dir.path().join("main.db"),
        DatabaseKind::Main,
        StorageConfig::default(),
    )
    .unwrap();
    let master = Master::open(
        db.clone(),
        MasterConfig::default(),
        Arc::new(Unavailable),
        None,
        Arc::new(NoActions),
        Redactor::default(),
    )
    .await
    .unwrap();
    master
        .receive(Incoming {
            conversation: "synthetic-conversation".into(),
            client_id: "synthetic".into(),
            client_msg_id: "initial".into(),
            text: "/status".into(),
            scope: None,
            timezone: "UTC".into(),
            channel: "local".into(),
            reply_to: None,
            attachments: Vec::new(),
        })
        .await
        .unwrap();
    (dir, db, master)
}
#[tokio::test]
async fn question_defaults_never_answer_mandatory_questions_and_reminders_are_daily() {
    let (_dir, db, master) = service().await;
    let conversation = "synthetic-conversation";
    let mandatory = master
        .ask(
            conversation,
            "Required recipient?",
            true,
            Some("Never choose this".into()),
            1000,
        )
        .await
        .unwrap();
    let optional = master
        .ask(
            conversation,
            "Use plain text?",
            false,
            Some("plain text".into()),
            1000,
        )
        .await
        .unwrap();
    assert!(master.tick_questions(1899).await.unwrap().is_empty());
    assert_eq!(master.tick_questions(1900).await.unwrap().len(), 1);
    let questions = master.questions(conversation).await.unwrap();
    assert!(
        questions
            .iter()
            .find(|q| q.id == mandatory)
            .unwrap()
            .answer
            .is_none()
    );
    assert_eq!(
        questions
            .iter()
            .find(|q| q.id == optional)
            .unwrap()
            .answer
            .as_deref(),
        Some("plain text")
    );
    master.tick_questions(1000 + 86400).await.unwrap();
    master.tick_questions(1000 + 86401).await.unwrap();
    assert_eq!(
        master
            .events(0, 128)
            .await
            .unwrap()
            .iter()
            .filter(|e| e.kind == "question_reminder")
            .count(),
        1
    );
    master
        .answer_question(conversation, &mandatory, "explicit recipient")
        .await
        .unwrap();
    master
        .answer_question(conversation, &mandatory, "explicit recipient")
        .await
        .unwrap();
    assert!(
        master
            .answer_question(conversation, &mandatory, "different recipient")
            .await
            .is_err()
    );
    assert!(
        master
            .tick_questions(1000 + 86400 * 3)
            .await
            .unwrap()
            .is_empty()
    );
    master.shutdown().await;
    db.shutdown().await.unwrap();
}
#[tokio::test]
async fn notice_delivery_is_durable_idempotent_and_normal_notices_do_not_call_models() {
    let (_dir, db, master) = service().await;
    let conversation = "synthetic-conversation";
    let cursor = master
        .publish_notice(
            "notice-one",
            conversation,
            json!({"state":"completed","task":"synthetic"}),
            "normal",
        )
        .await
        .unwrap();
    assert_eq!(
        master
            .publish_notice(
                "notice-one",
                conversation,
                json!({"state":"completed","task":"synthetic"}),
                "normal"
            )
            .await
            .unwrap(),
        cursor
    );
    assert!(
        master
            .publish_notice(
                "notice-one",
                conversation,
                json!({"state":"failed"}),
                "normal"
            )
            .await
            .is_err()
    );
    let events = master.events(0, 128).await.unwrap();
    assert_eq!(
        events.iter().filter(|e| e.kind == "notification").count(),
        1
    );
    assert!(!events.iter().any(|e| e.kind == "model_started"));
    master.shutdown().await;
    db.shutdown().await.unwrap();
}
#[tokio::test]
async fn a_frozen_scope_question_and_its_answer_do_not_follow_the_conversation_selector() {
    let (_dir, db, master) = service().await;
    let conversation = "synthetic-conversation";
    master
        .set_scope(conversation, "project:other")
        .await
        .unwrap();
    let question = master
        .ask_scoped(
            conversation,
            "project:original",
            "Which original-project format?",
            true,
            None,
            1000,
        )
        .await
        .unwrap();
    master
        .answer_question(conversation, &question, "frozen-scope-answer")
        .await
        .unwrap();
    let rows = db
        .read(excalibur_store::ReadLane::Interactive, move |conn| {
            let scope: String = conn.query_row(
                "SELECT scope_id FROM master_questions WHERE id=?1",
                [question],
                |row| row.get(0),
            )?;
            let answer_scope: String = conn.query_row(
                "SELECT scope_id FROM messages WHERE text LIKE '%frozen-scope-answer%'",
                [],
                |row| row.get(0),
            )?;
            Ok((scope, answer_scope))
        })
        .await
        .unwrap();
    assert_eq!(rows, ("project:original".into(), "project:original".into()));
    assert_eq!(
        master.conversation_scope(conversation).await.unwrap(),
        "project:other"
    );
    master.shutdown().await;
    db.shutdown().await.unwrap();
}
