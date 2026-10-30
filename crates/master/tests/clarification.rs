//! Receipt-first clarification routing while another conversation holds the sole
//! model slot. Routing itself performs no model call.
use excalibur_master::{Incoming, Master, MasterConfig, NoActions, Preanalysis};
use excalibur_models::{ModelError, ModelGateway};
use excalibur_store::{Database, DatabaseKind, Priority, ReadLane};
use excalibur_types::{MessageId, runtime::*};
use excalibur_vault::Redactor;
use serde_json::json;
use sha2::{Digest, Sha256};
use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::sync::mpsc;
struct HeldModel {
    calls: AtomicUsize,
    requests: mpsc::UnboundedSender<ModelRequest>,
}
impl ModelGateway for HeldModel {
    fn complete<'a>(
        &'a self,
        request: ModelRequest,
    ) -> BoxFuture<'a, Result<ModelResponse, ModelError>> {
        Box::pin(async move {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.requests.send(request).unwrap();
            std::future::pending().await
        })
    }
}
fn input(conversation: &str, id: &str, text: &str, reply: Option<String>) -> Incoming {
    Incoming {
        conversation: conversation.into(),
        client_id: "clarification-client".into(),
        client_msg_id: id.into(),
        text: text.into(),
        scope: None,
        timezone: "UTC".into(),
        channel: "test".into(),
        reply_to: reply,
        attachments: vec![],
    }
}
async fn fixture() -> (
    tempfile::TempDir,
    Database,
    Master,
    Arc<HeldModel>,
    mpsc::UnboundedReceiver<ModelRequest>,
) {
    let dir = tempfile::tempdir().unwrap();
    let db = Database::open(
        dir.path().join("main.db"),
        DatabaseKind::Main,
        Default::default(),
    )
    .unwrap();
    let (tx, mut rx) = mpsc::unbounded_channel();
    let model = Arc::new(HeldModel {
        calls: AtomicUsize::new(0),
        requests: tx,
    });
    let master = Master::open(
        db.clone(),
        MasterConfig {
            max_active_turns: 1,
            ..Default::default()
        },
        model.clone(),
        None,
        Arc::new(NoActions),
        Redactor::default(),
    )
    .await
    .unwrap();
    for (conversation, scope) in [("a", "project:a"), ("b", "project:b")] {
        let mut initial = input(
            conversation,
            &format!("initial-{conversation}"),
            "/status",
            None,
        );
        initial.scope = Some(scope.into());
        master.receive(initial).await.unwrap();
    }
    master
        .receive(input("held", "held", "Hold the only model slot", None))
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(10), rx.recv())
        .await
        .unwrap()
        .unwrap();
    (dir, db, master, model, rx)
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn explicit_question_replies_are_atomic_scoped_and_idempotent_with_literal_source_links() {
    let (_dir, db, master, model, mut rx) = fixture().await;
    let question = master
        .ask("a", "Which export format?", true, None, 1000)
        .await
        .unwrap();
    master.set_scope("a", "project:b").await.unwrap();
    let incoming = input(
        "a",
        "answer",
        "CSV — keep these exact bytes.",
        Some(question.clone()),
    );
    let receipt = master.receive(incoming.clone()).await.unwrap();
    assert_eq!(model.calls.load(Ordering::SeqCst), 1);
    assert!(rx.try_recv().is_err());
    assert_eq!(
        master
            .questions("a")
            .await
            .unwrap()
            .iter()
            .find(|q| q.id == question)
            .unwrap()
            .answer
            .as_deref(),
        Some("CSV — keep these exact bytes.")
    );
    assert_eq!(master.conversation_scope("a").await.unwrap(), "project:b");
    let source = receipt.message_id.clone();
    let q = question.clone();
    let rows=db.read(ReadLane::Interactive,move|conn|{let source:(String,String,String)=conn.query_row("SELECT m.text,m.scope_id,i.state FROM messages m JOIN master_inbox i ON i.message_id=m.id WHERE m.id=?1",[&source],|row|Ok((row.get(0)?,row.get(1)?,row.get(2)?)))?;let payload:String=conn.query_row("SELECT payload FROM master_events WHERE kind='question_answered' AND json_extract(payload,'$.id')=?1",[&q],|row|row.get(0))?;Ok((source,serde_json::from_str::<serde_json::Value>(&payload).unwrap()))}).await.unwrap();
    assert_eq!(
        rows.0,
        (
            "CSV — keep these exact bytes.".into(),
            "project:a".into(),
            "consumed".into()
        )
    );
    assert_eq!(rows.1["source_message_id"], receipt.message_id);
    let answer_id = rows.1["message_id"].as_str().unwrap().to_string();
    let answer = db
        .read(ReadLane::Interactive, move |conn| {
            Ok(conn.query_row(
                "SELECT scope_id,reply_to FROM messages WHERE id=?1",
                [&answer_id],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
            )?)
        })
        .await
        .unwrap();
    assert_eq!(answer, ("project:a".into(), receipt.message_id.clone()));
    assert!(master.receive(incoming.clone()).await.unwrap().duplicate);
    assert_eq!(
        master
            .events(0, 128)
            .await
            .unwrap()
            .iter()
            .filter(|event| event.kind == "question_answered")
            .count(),
        1
    );
    master.shutdown().await;
    drop(master);
    let reopened = Master::open(
        db.clone(),
        MasterConfig {
            max_active_turns: 1,
            ..Default::default()
        },
        model.clone(),
        None,
        Arc::new(NoActions),
        Redactor::default(),
    )
    .await
    .unwrap();
    let duplicate = reopened.receive(incoming).await.unwrap();
    assert!(duplicate.duplicate);
    assert_eq!(duplicate.message_id, receipt.message_id);
    assert_eq!(
        reopened
            .events(0, 128)
            .await
            .unwrap()
            .iter()
            .filter(|event| event.kind == "question_answered")
            .count(),
        1
    );
    assert_eq!(model.calls.load(Ordering::SeqCst), 1);
    reopened.shutdown().await;
    db.shutdown().await.unwrap();
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ambiguous_unknown_and_cross_conversation_references_never_guess_an_answer() {
    let (_dir, db, master, model, mut rx) = fixture().await;
    let first = master
        .ask("a", "Which format?", true, None, 1000)
        .await
        .unwrap();
    let second = master
        .ask("a", "Which destination?", true, None, 1001)
        .await
        .unwrap();
    let foreign = master
        .ask("b", "Private other-conversation question", true, None, 1000)
        .await
        .unwrap();
    let ambiguous = master
        .receive(input("a", "ambiguous", "yes", None))
        .await
        .unwrap();
    let unknown = master
        .receive(input(
            "a",
            "unknown",
            "answer",
            Some("msg_unknown-reference".into()),
        ))
        .await
        .unwrap();
    let cross = master
        .receive(input("a", "cross", "answer", Some(foreign.clone())))
        .await
        .unwrap();
    assert!(
        master
            .questions("a")
            .await
            .unwrap()
            .iter()
            .all(|question| question.answer.is_none())
    );
    assert!(
        master
            .questions("b")
            .await
            .unwrap()
            .iter()
            .all(|question| question.answer.is_none())
    );
    assert_eq!(model.calls.load(Ordering::SeqCst), 1);
    assert!(rx.try_recv().is_err());
    let events = master.events(0, 128).await.unwrap();
    let selection = events
        .iter()
        .find(|event| {
            event.kind == "clarification_selection_required"
                && event.payload["source_message_id"] == ambiguous.message_id
        })
        .unwrap();
    let candidates = selection.payload["question_ids"].as_array().unwrap();
    assert_eq!(candidates.len(), 2);
    assert!(candidates.contains(&json!(first)));
    assert!(candidates.contains(&json!(second)));
    assert!(!candidates.contains(&json!(foreign)));
    for source in [unknown.message_id, cross.message_id] {
        assert!(
            events
                .iter()
                .any(|event| event.kind == "clarification_not_matched"
                    && event.payload["source_message_id"] == source)
        );
    }
    assert!(!events.iter().any(|event| event.kind == "question_answered"));
    master.shutdown().await;
    db.shutdown().await.unwrap();
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn one_mandatory_question_in_the_waiting_scope_routes_once_and_consumed_input_is_not_delivered_twice()
 {
    let (_dir, db, master, model, mut rx) = fixture().await;
    let question = master
        .ask("a", "Which format?", true, None, 1000)
        .await
        .unwrap();
    let receipt = master
        .receive(input("a", "implicit", "CSV", None))
        .await
        .unwrap();
    assert_eq!(model.calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        master
            .questions("a")
            .await
            .unwrap()
            .iter()
            .find(|q| q.id == question)
            .unwrap()
            .answer
            .as_deref(),
        Some("CSV")
    );
    master.stop_turn("held").await.unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            master.retry_pending().await.unwrap();
            if let Ok(request) = rx.try_recv() {
                assert!(
                    !request
                        .messages
                        .iter()
                        .any(|message| message.role == "user" && message.content == "CSV")
                );
                assert_eq!(
                    request
                        .messages
                        .iter()
                        .filter(|message| message.role == "user"
                            && message.content.contains("User answer: CSV"))
                        .count(),
                    1,
                    "captured messages: {:?}",
                    request.messages
                );
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let id = receipt.message_id;
    let state = db
        .read(ReadLane::Interactive, move |conn| {
            Ok(conn.query_row(
                "SELECT state FROM master_inbox WHERE message_id=?1",
                [&id],
                |row| row.get::<_, String>(0),
            )?)
        })
        .await
        .unwrap();
    assert_eq!(state, "consumed");
    master.shutdown().await;
    db.shutdown().await.unwrap();
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn receipt_recovery_routes_the_durable_reply_before_any_model_consumes_it() {
    let (_dir, db, master, model, mut rx) = fixture().await;
    let question = master
        .ask("a", "Which format after recovery?", true, None, 1000)
        .await
        .unwrap();
    let source = MessageId::new().to_string();
    let original = source.clone();
    let reply = input(
        "a",
        "receipt-gap",
        "Recovered literal CSV",
        Some(question.clone()),
    );
    let fingerprint = format!("{:x}", Sha256::digest(serde_json::to_vec(&reply).unwrap()));
    let analysis = serde_json::to_string(&Preanalysis {
        reply_to: Some(question.clone()),
        ..Default::default()
    })
    .unwrap();
    // Valid persisted receipt boundary: no question mutation has occurred yet.
    db.write(Priority::Interactive,move|conn|{conn.execute("INSERT INTO messages(id,conversation_id,role,text,scope_id,principal_id) VALUES(?1,'a','user','Recovered literal CSV','project:a','local')",[&source])?;conn.execute("INSERT INTO master_events(conversation_id,kind,payload) VALUES('a','received',?1)",[json!({"message_id":source,"scope":"project:a"}).to_string()])?;let cursor=conn.last_insert_rowid();conn.execute("INSERT INTO master_inbox(message_id,conversation_id,state,preanalysis,created_seq) VALUES(?1,'a','routing',?2,?3)",(&source,&analysis,cursor))?;conn.execute("INSERT INTO master_client_receipts(client_id,client_msg_id,message_id,fingerprint,cursor) VALUES('clarification-client','receipt-gap',?1,?2,?3)",(&source,&fingerprint,cursor))?;Ok(())}).await.unwrap();
    master.stop_turn("held").await.unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            master.retry_pending().await.unwrap();
            if let Ok(request) = rx.try_recv() {
                assert!(
                    request
                        .messages
                        .iter()
                        .any(|message| message.content.contains("Recovered literal CSV")),
                    "captured messages: {:?}",
                    request.messages
                );
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(
        master
            .questions("a")
            .await
            .unwrap()
            .iter()
            .find(|q| q.id == question)
            .unwrap()
            .answer
            .as_deref(),
        Some("Recovered literal CSV")
    );
    let receipt = master.receive(reply).await.unwrap();
    assert!(receipt.duplicate);
    assert_eq!(receipt.message_id, original);
    assert_eq!(
        master
            .events(0, 128)
            .await
            .unwrap()
            .iter()
            .filter(|event| event.kind == "question_answered")
            .count(),
        1
    );
    assert_eq!(model.calls.load(Ordering::SeqCst), 2);
    master.shutdown().await;
    db.shutdown().await.unwrap();
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn interrupted_fast_command_receipts_replay_outside_model_admission_and_uncertain_intents_never_repeat()
 {
    struct Controls(AtomicUsize);
    impl excalibur_master::FastActions for Controls {
        fn execute<'a>(
            &'a self,
            _: &'a str,
            command: &'a str,
            target: Option<&'a str>,
            _: &'a str,
        ) -> BoxFuture<'a, Result<serde_json::Value, String>> {
            Box::pin(async move {
                self.0.fetch_add(1, Ordering::SeqCst);
                Ok(json!({"command":command,"target":target}))
            })
        }
    }
    let dir = tempfile::tempdir().unwrap();
    let db = Database::open(
        dir.path().join("main.db"),
        DatabaseKind::Main,
        Default::default(),
    )
    .unwrap();
    let (tx, mut rx) = mpsc::unbounded_channel();
    let model = Arc::new(HeldModel {
        calls: AtomicUsize::new(0),
        requests: tx,
    });
    let controls = Arc::new(Controls(AtomicUsize::new(0)));
    let master = Master::open(
        db.clone(),
        MasterConfig {
            max_active_turns: 1,
            ..Default::default()
        },
        model.clone(),
        None,
        controls.clone(),
        Redactor::default(),
    )
    .await
    .unwrap();
    master
        .receive(input("a", "initialize", "/status", None))
        .await
        .unwrap();
    master
        .receive(input("held", "hold", "Block the only model slot", None))
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(10), rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(controls.0.load(Ordering::SeqCst), 1);
    async fn receipt_boundary(
        db: &Database,
        client_id: &str,
        uncertain: bool,
    ) -> (Incoming, String) {
        let incoming = input(
            "a",
            client_id,
            if client_id.starts_with("frozen") {
                "Pause Lena"
            } else {
                "/pause fixture-run"
            },
            None,
        );
        let message_id = MessageId::new().to_string();
        let source = message_id.clone();
        let fingerprint = format!(
            "{:x}",
            Sha256::digest(serde_json::to_vec(&incoming).unwrap())
        );
        let command_text = incoming.text.clone();
        let client_id = client_id.to_string();
        let analysis = serde_json::to_string(&Preanalysis {
            command: Some(excalibur_master::ExplicitCommand {
                name: "pause".into(),
                target: Some("fixture-run".into()),
            }),
            ..Default::default()
        })
        .unwrap();
        db.write(Priority::Interactive,move|conn|{conn.execute("INSERT INTO messages(id,conversation_id,role,text,scope_id,principal_id) VALUES(?1,'a','user',?2,'general','local')",(&source,&command_text))?;conn.execute("INSERT INTO master_events(conversation_id,kind,payload) VALUES('a','received',?1)",[json!({"message_id":source,"scope":"general"}).to_string()])?;let cursor=conn.last_insert_rowid();conn.execute("INSERT INTO master_inbox(message_id,conversation_id,state,preanalysis,created_seq) VALUES(?1,'a','routing',?2,?3)",(&source,&analysis,cursor))?;conn.execute("INSERT INTO master_client_receipts(client_id,client_msg_id,message_id,fingerprint,cursor) VALUES('clarification-client',?1,?2,?3,?4)",(&client_id,&source,&fingerprint,cursor))?;if uncertain {conn.execute("INSERT INTO master_action_receipts(operation_key,conversation_id,tool_name,arguments,state) VALUES(?1,'a','command:pause',?2,'intent')",(format!("message:{source}"),json!({"target":"fixture-run"}).to_string()))?;}Ok(())}).await.unwrap();
        (incoming, message_id)
    }
    let (incoming, source) = receipt_boundary(&db, "before-dispatch", false).await;
    master.retry_pending().await.unwrap();
    assert_eq!(controls.0.load(Ordering::SeqCst), 2);
    assert_eq!(model.calls.load(Ordering::SeqCst), 1);
    assert!(rx.try_recv().is_err());
    let receipt = master.receive(incoming).await.unwrap();
    assert!(receipt.duplicate);
    assert_eq!(receipt.message_id, source);
    master.retry_pending().await.unwrap();
    assert_eq!(controls.0.load(Ordering::SeqCst), 2);
    let (_, uncertain_source) = receipt_boundary(&db, "after-intent", true).await;
    master.retry_pending().await.unwrap();
    assert_eq!(controls.0.load(Ordering::SeqCst), 2);
    assert_eq!(model.calls.load(Ordering::SeqCst), 1);
    let history = master.history("a", None, 128).await.unwrap();
    assert!(history.iter().any(|message| message.role == "assistant"
        && message.text.contains("uncertain interrupted master action")));
    let states = db
        .read(ReadLane::Interactive, move |conn| {
            let state: String = conn.query_row(
                "SELECT state FROM master_inbox WHERE message_id=?1",
                [&source],
                |row| row.get(0),
            )?;
            let uncertain: String = conn.query_row(
                "SELECT state FROM master_action_receipts WHERE operation_key=?1",
                [format!("message:{uncertain_source}")],
                |row| row.get(0),
            )?;
            Ok((state, uncertain))
        })
        .await
        .unwrap();
    assert_eq!(states, ("done".into(), "intent".into()));
    struct NeverReclassify(AtomicUsize);
    impl excalibur_master::IntakeDecider for NeverReclassify {
        fn classify<'a>(
            &'a self,
            _: &'a str,
            _: &'a Preanalysis,
        ) -> BoxFuture<'a, Result<excalibur_master::Classification, String>> {
            Box::pin(async move {
                self.0.fetch_add(1, Ordering::SeqCst);
                Err("durable receipt must retain its classification".into())
            })
        }
    }
    let decider = Arc::new(NeverReclassify(AtomicUsize::new(0)));
    master.set_decider(Some(decider.clone()));
    let (frozen, _) = receipt_boundary(&db, "frozen-routing", false).await;
    assert!(master.receive(frozen).await.unwrap().duplicate);
    assert_eq!(controls.0.load(Ordering::SeqCst), 3);
    assert_eq!(decider.0.load(Ordering::SeqCst), 0);
    let (stopped, stopped_source) = receipt_boundary(&db, "frozen-stopped", false).await;
    master.stop_turn("a").await.unwrap();
    assert!(master.receive(stopped).await.unwrap().duplicate);
    assert_eq!(controls.0.load(Ordering::SeqCst), 3);
    assert_eq!(decider.0.load(Ordering::SeqCst), 0);
    let stopped = db
        .read(ReadLane::Interactive, move |conn| {
            Ok(conn.query_row(
                "SELECT state,preanalysis FROM master_inbox WHERE message_id=?1",
                [&stopped_source],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
            )?)
        })
        .await
        .unwrap();
    assert_eq!(stopped.0, "stopped");
    assert_eq!(
        serde_json::from_str::<Preanalysis>(&stopped.1)
            .unwrap()
            .command
            .unwrap()
            .target
            .as_deref(),
        Some("fixture-run")
    );
    assert_eq!(model.calls.load(Ordering::SeqCst), 1);
    master.shutdown().await;
    db.shutdown().await.unwrap();
}
