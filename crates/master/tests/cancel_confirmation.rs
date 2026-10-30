use excalibur_master::*;
use excalibur_models::{ModelError, ModelGateway};
use excalibur_store::{Database, DatabaseKind, Priority, ReadLane};
use excalibur_types::runtime::*;
use excalibur_vault::Redactor;
use serde_json::{Value, json};
use std::{
    collections::BTreeSet,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
};
struct NoModel;
impl ModelGateway for NoModel {
    fn complete(
        &self,
        _: ModelRequest,
    ) -> BoxFuture<'_, std::result::Result<ModelResponse, ModelError>> {
        Box::pin(async { Err(ModelError::Provider) })
    }
}
#[derive(Default)]
struct Actions {
    lose_result: AtomicBool,
    effects: Mutex<BTreeSet<String>>,
    calls: AtomicUsize,
}
impl FastActions for Actions {
    fn execute<'a>(
        &'a self,
        _: &'a str,
        _: &'a str,
        target: Option<&'a str>,
        _: &'a str,
    ) -> BoxFuture<'a, std::result::Result<Value, String>> {
        Box::pin(async move {
            if std::env::var_os("EXCALIBUR_CANCEL_INTENT_CUT").is_some() {
                std::process::exit(77);
            }
            Ok(
                json!({"confirmation_required":true,"target":target.unwrap_or("worker"),"scope":"general"}),
            )
        })
    }
    fn confirm_cancel<'a>(
        &'a self,
        _: &'a str,
        _: &'a str,
        _: &'a str,
        key: &'a str,
    ) -> BoxFuture<'a, std::result::Result<Value, String>> {
        Box::pin(async move {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.effects.lock().unwrap().insert(key.into());
            if self.lose_result.load(Ordering::SeqCst) {
                Err("synthetic effect-before-receipt interruption".into())
            } else {
                Ok(json!({"cancelled":true}))
            }
        })
    }
}
fn incoming(id: &str, text: &str, reply: Option<&str>) -> Incoming {
    Incoming {
        conversation: "origin".into(),
        client_id: "human".into(),
        client_msg_id: id.into(),
        text: text.into(),
        scope: None,
        timezone: "UTC".into(),
        channel: "test".into(),
        reply_to: reply.map(str::to_owned),
        attachments: vec![],
    }
}
async fn open(db: Database, actions: Arc<Actions>, principal: &str) -> Master {
    Master::open(
        db,
        MasterConfig {
            principal: principal.into(),
            ..Default::default()
        },
        Arc::new(NoModel),
        None,
        actions,
        Redactor::default(),
    )
    .await
    .unwrap()
}
#[tokio::test]
async fn accepted_outbox_reopens_with_same_effect_key_and_completion_once() {
    let dir = tempfile::tempdir().unwrap();
    let db = Database::open(
        dir.path().join("main.db"),
        DatabaseKind::Main,
        Default::default(),
    )
    .unwrap();
    let actions = Arc::new(Actions::default());
    actions.lose_result.store(true, Ordering::SeqCst);
    let master = open(db.clone(), actions.clone(), "owner").await;
    master
        .receive(incoming("command", "/cancel worker", None))
        .await
        .unwrap();
    let id = master.questions("origin").await.unwrap()[0].id.clone();
    let key = format!("cancel-confirmation:{id}");
    assert!(
        !master
            .authorizes_cancel("origin", "general", "worker", &key)
            .await
            .unwrap()
    );
    master
        .receive(incoming("yes", "yes", Some(&id)))
        .await
        .unwrap();
    assert_eq!(actions.effects.lock().unwrap().len(), 1);
    assert_eq!(actions.calls.load(Ordering::SeqCst), 1);
    assert!(
        master
            .authorizes_cancel("origin", "general", "worker", &key)
            .await
            .unwrap()
    );
    assert!(
        !master
            .authorizes_cancel("origin", "project:other", "worker", &key)
            .await
            .unwrap()
    );
    assert!(
        !master
            .authorizes_cancel("origin", "general", "other", &key)
            .await
            .unwrap()
    );
    assert!(
        master
            .authorizes_cancel("other-conversation", "general", "worker", &key)
            .await
            .is_err()
    );
    assert_eq!(
        master
            .events(0, 100)
            .await
            .unwrap()
            .iter()
            .filter(|e| e.kind == "cancellation_completed")
            .count(),
        0
    );
    master.shutdown().await;
    drop(master);
    // Another principal cannot drain or answer the original authorization.
    let stranger = open(db.clone(), actions.clone(), "stranger").await;
    stranger.retry_pending().await.unwrap();
    assert_eq!(actions.calls.load(Ordering::SeqCst), 1);
    assert!(
        stranger
            .receive(incoming("intruder", "yes", Some(&id)))
            .await
            .is_err()
    );
    stranger.shutdown().await;
    actions.lose_result.store(false, Ordering::SeqCst);
    let master = open(db.clone(), actions.clone(), "owner").await;
    master.retry_pending().await.unwrap();
    master.retry_pending().await.unwrap();
    master
        .receive(incoming("yes", "yes", Some(&id)))
        .await
        .unwrap();
    assert_eq!(actions.effects.lock().unwrap().len(), 1);
    assert_eq!(actions.calls.load(Ordering::SeqCst), 2);
    assert_eq!(
        master
            .events(0, 100)
            .await
            .unwrap()
            .iter()
            .filter(|e| e.kind == "cancellation_completed")
            .count(),
        1
    );
    master.shutdown().await;
}
#[tokio::test]
async fn ambiguity_scope_mismatch_and_generated_answers_do_not_authorize() {
    let dir = tempfile::tempdir().unwrap();
    let db = Database::open(
        dir.path().join("main.db"),
        DatabaseKind::Main,
        Default::default(),
    )
    .unwrap();
    let actions = Arc::new(Actions::default());
    let master = open(db.clone(), actions.clone(), "owner").await;
    master
        .receive(incoming("first", "/cancel first", None))
        .await
        .unwrap();
    master
        .receive(incoming("second", "/cancel second", None))
        .await
        .unwrap();
    let questions = master.questions("origin").await.unwrap();
    assert_eq!(questions.len(), 2);
    master
        .receive(incoming("ambiguous", "yes", None))
        .await
        .unwrap();
    assert_eq!(actions.calls.load(Ordering::SeqCst), 0);
    let id = questions[0].id.clone();
    let generated = id.clone();
    db.write(Priority::Interactive,move|c| {
        c.execute("INSERT INTO messages(id,conversation_id,role,text,scope_id,principal_id) VALUES('generated','origin','user','yes','general','owner')",[])?;
        c.execute("INSERT INTO master_inbox(message_id,conversation_id,state,preanalysis,created_seq) VALUES('generated','origin','routing',?1,99999)",[json!({"reply_to":generated}).to_string()])?;Ok(())
    }).await.unwrap();
    master.retry_pending().await.unwrap();
    assert_eq!(actions.calls.load(Ordering::SeqCst), 0);
    assert!(master.answer_question("origin", &id, "yes").await.is_err());
    master.set_scope("origin", "project:other").await.unwrap();
    master
        .receive(incoming("scope", "yes", Some(&id)))
        .await
        .unwrap();
    assert_eq!(actions.calls.load(Ordering::SeqCst), 0);
    master.set_scope("origin", "general").await.unwrap();
    let normal = master
        .ask("origin", "Choose a label", true, None, 0)
        .await
        .unwrap();
    master
        .receive(incoming("normal", "label", Some(&normal)))
        .await
        .unwrap();
    assert_eq!(
        master
            .questions("origin")
            .await
            .unwrap()
            .into_iter()
            .find(|q| q.id == normal)
            .unwrap()
            .answer
            .as_deref(),
        Some("label")
    );
    master
        .receive(incoming("explicit", "yes", Some(&id)))
        .await
        .unwrap();
    assert_eq!(actions.calls.load(Ordering::SeqCst), 1);
    let n: u64 = db
        .read(ReadLane::Interactive, |c| {
            Ok(c.query_row(
                "SELECT count(*) FROM master_cancel_confirmations WHERE state='pending'",
                [],
                |r| r.get(0),
            )?)
        })
        .await
        .unwrap();
    assert_eq!(n, 1);
    master.shutdown().await;
}

#[tokio::test]
async fn independent_text_remains_available_while_cancellation_waits() {
    let dir = tempfile::tempdir().unwrap();
    let db = Database::open(
        dir.path().join("main.db"),
        DatabaseKind::Main,
        Default::default(),
    )
    .unwrap();
    let actions = Arc::new(Actions::default());
    let master = open(db.clone(), actions.clone(), "owner").await;
    master
        .receive(incoming("command", "/cancel worker", None))
        .await
        .unwrap();
    let receipt = master
        .receive(incoming("independent", "Review file Y independently", None))
        .await
        .unwrap();
    let id = receipt.message_id;
    let state: String = db
        .read(ReadLane::Interactive, move |c| {
            Ok(c.query_row(
                "SELECT state FROM master_inbox WHERE message_id=?1",
                [id],
                |r| r.get(0),
            )?)
        })
        .await
        .unwrap();
    assert_ne!(state, "consumed");
    assert!(
        master.questions("origin").await.unwrap()[0]
            .answer
            .is_none()
    );
    assert_eq!(actions.calls.load(Ordering::SeqCst), 0);
    master.shutdown().await;
}

#[tokio::test]
#[ignore = "subprocess fixture"]
async fn intent_interruption_child() {
    let path = std::env::var("EXCALIBUR_CANCEL_INTENT_DB").unwrap();
    let db = Database::open(path, DatabaseKind::Main, Default::default()).unwrap();
    let master = open(db, Arc::new(Actions::default()), "owner").await;
    master
        .receive(incoming("command", "/cancel worker", None))
        .await
        .unwrap();
    panic!("expected process interruption before request resolution returned");
}

#[tokio::test]
async fn interrupted_pure_cancel_resolution_reopens_its_question() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("main.db");
    let status = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--ignored", "--exact", "intent_interruption_child"])
        .env("EXCALIBUR_CANCEL_INTENT_DB", &path)
        .env("EXCALIBUR_CANCEL_INTENT_CUT", "1")
        .status()
        .unwrap();
    assert_eq!(status.code(), Some(77));
    let db = Database::open(path, DatabaseKind::Main, Default::default()).unwrap();
    let actions = Arc::new(Actions::default());
    let master = open(db, actions.clone(), "owner").await;
    master.retry_pending().await.unwrap();
    assert_eq!(master.questions("origin").await.unwrap().len(), 1);
    master
        .receive(incoming("affirm", "yes", None))
        .await
        .unwrap();
    assert_eq!(actions.effects.lock().unwrap().len(), 1);
    master.shutdown().await;
}
