use excalibur_master::*;
use excalibur_models::{ModelError, ModelGateway};
use excalibur_store::{Database, DatabaseKind, StorageConfig};
use excalibur_types::runtime::*;
use excalibur_vault::Redactor;
use serde_json::{Value, json};
use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::sync::{mpsc, oneshot};
type Call = (
    ModelRequest,
    oneshot::Sender<std::result::Result<ModelResponse, ModelError>>,
);
struct Controlled(mpsc::UnboundedSender<Call>);
impl ModelGateway for Controlled {
    fn complete<'a>(
        &'a self,
        request: ModelRequest,
    ) -> BoxFuture<'a, std::result::Result<ModelResponse, ModelError>> {
        Box::pin(async move {
            let (tx, rx) = oneshot::channel();
            self.0
                .send((request, tx))
                .map_err(|_| ModelError::Provider)?;
            rx.await.map_err(|_| ModelError::Provider)?
        })
    }
}
#[derive(Default)]
struct Actions(AtomicUsize);
impl FastActions for Actions {
    fn execute<'a>(
        &'a self,
        _: &'a str,
        command: &'a str,
        target: Option<&'a str>,
        _: &'a str,
    ) -> BoxFuture<'a, std::result::Result<Value, String>> {
        Box::pin(async move {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(json!({"command":command,"target":target,"status":"running"}))
        })
    }
}
fn incoming(conversation: &str, id: &str, text: &str) -> Incoming {
    Incoming {
        conversation: conversation.into(),
        client_id: "synthetic-client".into(),
        client_msg_id: id.into(),
        text: text.into(),
        scope: None,
        timezone: "UTC".into(),
        channel: "local".into(),
        reply_to: None,
        attachments: Vec::new(),
    }
}
fn response(text: &str) -> ModelResponse {
    ModelResponse {
        message: Message::text("assistant", text),
        usage: Usage::default(),
    }
}
fn db(dir: &tempfile::TempDir) -> Database {
    Database::open(
        dir.path().join("main.db"),
        DatabaseKind::Main,
        StorageConfig::default(),
    )
    .unwrap()
}
async fn next(rx: &mut mpsc::UnboundedReceiver<Call>) -> Call {
    tokio::time::timeout(Duration::from_secs(5), rx.recv())
        .await
        .unwrap()
        .unwrap()
}
async fn wait_reply(master: &Master, conversation: &str, text: &str) {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if master
                .history(conversation, None, 128)
                .await
                .unwrap()
                .iter()
                .any(|m| m.role == "assistant" && m.text == text)
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fast_control_is_independent_of_a_blocked_model_and_receipts_are_idempotent() {
    let dir = tempfile::tempdir().unwrap();
    let database = db(&dir);
    let (tx, mut rx) = mpsc::unbounded_channel();
    let actions = Arc::new(Actions::default());
    let master = Master::open(
        database.clone(),
        MasterConfig::default(),
        Arc::new(Controlled(tx)),
        None,
        actions.clone(),
        Redactor::default(),
    )
    .await
    .unwrap();
    let natural = incoming("conversation-a", "first", "Please discuss the task");
    let receipt = master.receive(natural.clone()).await.unwrap();
    let (_, blocked) = next(&mut rx).await;
    let command = incoming("conversation-a", "control", "/status Lena");
    let control = tokio::time::timeout(Duration::from_secs(2), master.receive(command.clone()))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(actions.0.load(Ordering::SeqCst), 1);
    assert!(control.cursor > receipt.cursor);
    assert!(master.receive(command).await.unwrap().duplicate);
    assert_eq!(actions.0.load(Ordering::SeqCst), 1);
    let duplicate = master.receive(natural.clone()).await.unwrap();
    assert_eq!(duplicate.message_id, receipt.message_id);
    assert!(duplicate.duplicate);
    let mut changed = natural;
    changed.text = "different".into();
    assert!(master.receive(changed).await.is_err());
    let events = master.events(0, 512).await.unwrap();
    let after = events[1].sequence;
    let replay = master.events(after, 512).await.unwrap();
    assert!(replay.iter().all(|event| event.sequence > after));
    assert_eq!(replay.len(), events.len() - 2);
    assert!(rx.try_recv().is_err());
    assert!(!blocked.is_closed());
    master.shutdown().await;
    database.shutdown().await.unwrap();
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn early_message_restarts_unshown_generation_and_conversations_run_in_parallel() {
    let dir = tempfile::tempdir().unwrap();
    let database = db(&dir);
    let (tx, mut rx) = mpsc::unbounded_channel();
    let master = Master::open(
        database.clone(),
        MasterConfig::default(),
        Arc::new(Controlled(tx)),
        None,
        Arc::new(Actions::default()),
        Redactor::default(),
    )
    .await
    .unwrap();
    master
        .receive(incoming("a", "a1", "first literal"))
        .await
        .unwrap();
    let (_, old) = next(&mut rx).await;
    master
        .receive(incoming("a", "a2", "second literal"))
        .await
        .unwrap();
    let (combined, answer_a) = next(&mut rx).await;
    assert!(old.is_closed());
    assert!(
        combined
            .messages
            .iter()
            .any(|m| m.content == "first literal")
    );
    assert!(
        combined
            .messages
            .iter()
            .any(|m| m.content == "second literal")
    );
    master
        .receive(incoming("b", "b1", "other conversation"))
        .await
        .unwrap();
    let (other, answer_b) = next(&mut rx).await;
    assert!(!other.messages.iter().any(|m| m.content == "first literal"));
    answer_b.send(Ok(response("B complete"))).unwrap();
    wait_reply(&master, "b", "B complete").await;
    assert!(!answer_a.is_closed());
    answer_a.send(Ok(response("A complete"))).unwrap();
    wait_reply(&master, "a", "A complete").await;
    assert_eq!(
        master
            .history("a", None, 128)
            .await
            .unwrap()
            .iter()
            .filter(|m| m.role == "assistant")
            .count(),
        1
    );
    master.shutdown().await;
    database.shutdown().await.unwrap();
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unavailable_provider_retains_queue_without_repeated_calls_and_restart_replays_once() {
    let dir = tempfile::tempdir().unwrap();
    let database = db(&dir);
    let (tx, mut rx) = mpsc::unbounded_channel();
    let master = Master::open(
        database.clone(),
        MasterConfig::default(),
        Arc::new(Controlled(tx)),
        None,
        Arc::new(Actions::default()),
        Redactor::default(),
    )
    .await
    .unwrap();
    let message = incoming("a", "a1", "persist me");
    master.receive(message.clone()).await.unwrap();
    next(&mut rx)
        .await
        .1
        .send(Err(ModelError::Provider))
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if master
                .events(0, 128)
                .await
                .unwrap()
                .iter()
                .any(|e| e.kind == "model_unavailable")
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    master
        .receive(incoming("a", "a2", "also persist me"))
        .await
        .unwrap();
    assert!(rx.try_recv().is_err());
    master.shutdown().await;
    drop(master);
    let (tx, mut new_rx) = mpsc::unbounded_channel();
    let restored = Master::open(
        database.clone(),
        MasterConfig::default(),
        Arc::new(Controlled(tx)),
        None,
        Arc::new(Actions::default()),
        Redactor::default(),
    )
    .await
    .unwrap();
    restored.retry_pending().await.unwrap();
    let (request, answer) = next(&mut new_rx).await;
    assert!(request.messages.iter().any(|m| m.content == "persist me"));
    assert!(
        request
            .messages
            .iter()
            .any(|m| m.content == "also persist me")
    );
    answer.send(Ok(response("Recovered all messages"))).unwrap();
    wait_reply(&restored, "a", "Recovered all messages").await;
    assert!(restored.receive(message).await.unwrap().duplicate);
    assert!(new_rx.try_recv().is_err());
    restored.shutdown().await;
    database.shutdown().await.unwrap();
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fallback_uses_the_same_request_and_master_rejects_slow_role_tools() {
    let dir = tempfile::tempdir().unwrap();
    let database = db(&dir);
    let (tx, mut primary) = mpsc::unbounded_channel();
    let (fallback_tx, mut fallback) = mpsc::unbounded_channel();
    let master = Master::open(
        database.clone(),
        MasterConfig::default(),
        Arc::new(Controlled(tx)),
        Some(Arc::new(Controlled(fallback_tx))),
        Arc::new(Actions::default()),
        Redactor::default(),
    )
    .await
    .unwrap();
    master
        .receive(incoming("a", "first", "hello"))
        .await
        .unwrap();
    let (sent, reply) = next(&mut primary).await;
    reply.send(Err(ModelError::Provider)).unwrap();
    let (backup, reply) = next(&mut fallback).await;
    assert_eq!(sent, backup);
    reply.send(Ok(response("backup reply"))).unwrap();
    wait_reply(&master, "a", "backup reply").await;
    master.shutdown().await;
    struct Slow;
    impl FastActions for Slow {
        fn definitions(&self) -> Vec<ToolDefinition> {
            vec![ToolDefinition {
                name: "exec".into(),
                description: "slow".into(),
                input_schema: json!({"type":"object"}),
            }]
        }
        fn execute<'a>(
            &'a self,
            _: &'a str,
            _: &'a str,
            _: Option<&'a str>,
            _: &'a str,
        ) -> BoxFuture<'a, std::result::Result<Value, String>> {
            Box::pin(async { Ok(json!({})) })
        }
    }
    assert!(
        Master::open(
            database.clone(),
            MasterConfig::default(),
            Arc::new(Controlled(mpsc::unbounded_channel().0)),
            None,
            Arc::new(Slow),
            Redactor::default()
        )
        .await
        .is_err()
    );
    database.shutdown().await.unwrap();
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn changed_scope_excludes_other_project_history_from_model_context() {
    let dir = tempfile::tempdir().unwrap();
    let database = db(&dir);
    let (tx, mut rx) = mpsc::unbounded_channel();
    let master = Master::open(
        database.clone(),
        MasterConfig::default(),
        Arc::new(Controlled(tx)),
        None,
        Arc::new(Actions::default()),
        Redactor::default(),
    )
    .await
    .unwrap();
    let mut first = incoming("a", "first", "private project A literal");
    first.scope = Some("project:A".into());
    master.receive(first).await.unwrap();
    next(&mut rx)
        .await
        .1
        .send(Ok(response("project A answer")))
        .unwrap();
    wait_reply(&master, "a", "project A answer").await;
    master.set_scope("a", "project:B").await.unwrap();
    master
        .receive(incoming("a", "second", "project B literal"))
        .await
        .unwrap();
    let (request, answer) = next(&mut rx).await;
    assert!(
        !request
            .messages
            .iter()
            .any(|m| m.content.contains("project A"))
    );
    assert!(
        request
            .messages
            .iter()
            .any(|m| m.content == "project B literal")
    );
    answer.send(Ok(response("project B answer"))).unwrap();
    wait_reply(&master, "a", "project B answer").await;
    master.shutdown().await;
    database.shutdown().await.unwrap();
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn later_messages_are_injected_at_the_next_boundary_without_cancelling_the_model() {
    let dir = tempfile::tempdir().unwrap();
    let database = db(&dir);
    let (tx, mut rx) = mpsc::unbounded_channel();
    let master = Master::open(
        database.clone(),
        MasterConfig::default(),
        Arc::new(Controlled(tx)),
        None,
        Arc::new(Actions::default()),
        Redactor::default(),
    )
    .await
    .unwrap();
    master
        .receive(incoming("a", "first", "original question"))
        .await
        .unwrap();
    let (_, first) = next(&mut rx).await;
    tokio::time::sleep(Duration::from_millis(2100)).await;
    master
        .receive(incoming("a", "second", "please also consider this"))
        .await
        .unwrap();
    assert!(!first.is_closed());
    first.send(Ok(response("unshown stale draft"))).unwrap();
    let (updated, last) = next(&mut rx).await;
    assert!(
        updated
            .messages
            .iter()
            .any(|m| m.content == "please also consider this")
    );
    last.send(Ok(response("answer includes the update")))
        .unwrap();
    wait_reply(&master, "a", "answer includes the update").await;
    assert!(
        !master
            .history("a", None, 128)
            .await
            .unwrap()
            .iter()
            .any(|m| m.text == "unshown stale draft")
    );
    master.shutdown().await;
    database.shutdown().await.unwrap();
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn persisted_fast_action_history_resumes_without_repeating_a_confirmed_effect() {
    struct Delegate(AtomicUsize);
    impl FastActions for Delegate {
        fn definitions(&self) -> Vec<ToolDefinition> {
            vec![ToolDefinition {
                name: "subagent".into(),
                description: "Delegate bounded work".into(),
                input_schema: json!({"type":"object","properties":{"goal":{"type":"string"}},"required":["goal"],"additionalProperties":false}),
            }]
        }
        fn execute<'a>(
            &'a self,
            _: &'a str,
            _: &'a str,
            _: Option<&'a str>,
            _: &'a str,
        ) -> BoxFuture<'a, std::result::Result<Value, String>> {
            Box::pin(async { Ok(json!({})) })
        }
        fn invoke<'a>(
            &'a self,
            _: &'a str,
            _: &'a str,
            _: Value,
            _: &'a str,
        ) -> BoxFuture<'a, std::result::Result<Value, String>> {
            Box::pin(async move {
                self.0.fetch_add(1, Ordering::SeqCst);
                Ok(json!({"run":"synthetic-run"}))
            })
        }
    }
    let dir = tempfile::tempdir().unwrap();
    let database = db(&dir);
    let actions = Arc::new(Delegate(AtomicUsize::new(0)));
    let (tx, mut rx) = mpsc::unbounded_channel();
    let master = Master::open(
        database.clone(),
        MasterConfig::default(),
        Arc::new(Controlled(tx)),
        None,
        actions.clone(),
        Redactor::default(),
    )
    .await
    .unwrap();
    master
        .receive(incoming("a", "first", "do bounded work"))
        .await
        .unwrap();
    let (_, answer) = next(&mut rx).await;
    let mut call = response("");
    call.message.tool_calls = vec![ToolCall {
        id: "delegate1".into(),
        name: "subagent".into(),
        arguments: json!({"goal":"synthetic"}).to_string(),
    }];
    answer.send(Ok(call)).unwrap();
    let (next_request, _blocked) = next(&mut rx).await;
    assert!(
        next_request
            .messages
            .iter()
            .any(|m| m.role == "tool" && m.content.contains("synthetic-run"))
    );
    assert_eq!(actions.0.load(Ordering::SeqCst), 1);
    master.shutdown().await;
    drop(master);
    let (tx, mut rx) = mpsc::unbounded_channel();
    let restored = Master::open(
        database.clone(),
        MasterConfig::default(),
        Arc::new(Controlled(tx)),
        None,
        actions.clone(),
        Redactor::default(),
    )
    .await
    .unwrap();
    restored.retry_pending().await.unwrap();
    let (request, answer) = next(&mut rx).await;
    assert!(
        request
            .messages
            .iter()
            .any(|m| m.role == "tool" && m.content.contains("synthetic-run"))
    );
    assert_eq!(actions.0.load(Ordering::SeqCst), 1);
    answer.send(Ok(response("delegation retained"))).unwrap();
    wait_reply(&restored, "a", "delegation retained").await;
    restored.shutdown().await;
    database.shutdown().await.unwrap();
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn turns_are_bounded_at_eight_model_steps_and_unknown_tools_never_dispatch() {
    let dir = tempfile::tempdir().unwrap();
    let database = db(&dir);
    let (tx, mut rx) = mpsc::unbounded_channel();
    let actions = Arc::new(Actions::default());
    let master = Master::open(
        database.clone(),
        MasterConfig::default(),
        Arc::new(Controlled(tx)),
        None,
        actions.clone(),
        Redactor::default(),
    )
    .await
    .unwrap();
    master
        .receive(incoming("a", "first", "do work"))
        .await
        .unwrap();
    for index in 0..8 {
        let (_, answer) = next(&mut rx).await;
        let mut call = response("");
        call.message.tool_calls = vec![ToolCall {
            id: format!("forbidden{index}"),
            name: "exec".into(),
            arguments: "{}".into(),
        }];
        answer.send(Ok(call)).unwrap();
    }
    wait_reply(&master,"a","The conversational turn reached its eight-step limit. The queued work and its durable state remain available.").await;
    assert!(rx.try_recv().is_err());
    assert_eq!(actions.0.load(Ordering::SeqCst), 0);
    master.shutdown().await;
    database.shutdown().await.unwrap();
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_bounded_decider_routes_resolved_natural_status_without_a_model_call() {
    struct Decider;
    impl IntakeDecider for Decider {
        fn classify<'a>(
            &'a self,
            _: &'a str,
            _: &'a Preanalysis,
        ) -> BoxFuture<'a, std::result::Result<Classification, String>> {
            Box::pin(async {
                Ok(Classification {
                    kind: IntakeKind::Status,
                    confidence: 0.99,
                    target: Some("agt_lena".into()),
                    scopes: vec!["project:gastos".into()],
                    literal_instructions: Vec::new(),
                    requires_clarification: false,
                })
            })
        }
    }
    let dir = tempfile::tempdir().unwrap();
    let database = db(&dir);
    let (tx, mut rx) = mpsc::unbounded_channel();
    let actions = Arc::new(Actions::default());
    let master = Master::open(
        database.clone(),
        MasterConfig::default(),
        Arc::new(Controlled(tx)),
        None,
        actions.clone(),
        Redactor::default(),
    )
    .await
    .unwrap();
    master
        .set_lexicon(vec![LexiconEntry {
            alias: "Lena".into(),
            id: "agt_lena".into(),
            kind: "agent".into(),
        }])
        .unwrap();
    master.set_decider(Some(Arc::new(Decider)));
    master
        .receive(incoming("a", "first", "¿Qué está haciendo Lena?"))
        .await
        .unwrap();
    assert_eq!(actions.0.load(Ordering::SeqCst), 1);
    assert!(rx.try_recv().is_err());
    let history = master.history("a", None, 128).await.unwrap();
    assert!(
        history
            .iter()
            .any(|m| m.role == "assistant" && m.text.contains("agt_lena"))
    );
    // Naming one candidate does not resolve an ambiguous alias by itself.
    master
        .set_lexicon(vec![
            LexiconEntry {
                alias: "Lena".into(),
                id: "agt_lena".into(),
                kind: "agent".into(),
            },
            LexiconEntry {
                alias: "Lena".into(),
                id: "agt_other".into(),
                kind: "agent".into(),
            },
        ])
        .unwrap();
    master
        .receive(incoming(
            "ambiguous",
            "ambiguous-message",
            "¿Qué hace Lena?",
        ))
        .await
        .unwrap();
    let (_, ambiguous_reply) = next(&mut rx).await;
    assert_eq!(actions.0.load(Ordering::SeqCst), 1);
    ambiguous_reply.send(Ok(response("Which Lena?"))).unwrap();
    wait_reply(&master, "ambiguous", "Which Lena?").await;
    // A decision cannot manufacture an unmentioned, unresolved control target.
    master
        .receive(incoming("a", "second", "Explain a new task"))
        .await
        .unwrap();
    let (_, answer) = next(&mut rx).await;
    assert_eq!(actions.0.load(Ordering::SeqCst), 1);
    answer.send(Ok(response("handled by master"))).unwrap();
    wait_reply(&master, "a", "handled by master").await;
    master.shutdown().await;
    database.shutdown().await.unwrap();
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_panicking_conversation_does_not_disable_other_master_turns() {
    struct PanicsOnce;
    impl ModelGateway for PanicsOnce {
        fn complete<'a>(
            &'a self,
            request: ModelRequest,
        ) -> BoxFuture<'a, std::result::Result<ModelResponse, ModelError>> {
            Box::pin(async move {
                if request
                    .messages
                    .iter()
                    .any(|m| m.content == "panic this turn")
                {
                    panic!("synthetic isolated turn panic");
                }
                Ok(response("other turn remains available"))
            })
        }
    }
    let dir = tempfile::tempdir().unwrap();
    let database = db(&dir);
    let master = Master::open(
        database.clone(),
        MasterConfig::default(),
        Arc::new(PanicsOnce),
        None,
        Arc::new(Actions::default()),
        Redactor::default(),
    )
    .await
    .unwrap();
    master
        .receive(incoming("a", "first", "panic this turn"))
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if master
                .events(0, 128)
                .await
                .unwrap()
                .iter()
                .any(|e| e.kind == "turn_failed")
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    master
        .receive(incoming("b", "second", "ordinary turn"))
        .await
        .unwrap();
    wait_reply(&master, "b", "other turn remains available").await;
    assert!(
        master
            .history("a", None, 128)
            .await
            .unwrap()
            .iter()
            .all(|m| m.role != "assistant")
    );
    master.shutdown().await;
    database.shutdown().await.unwrap();
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn global_master_admission_keeps_overflow_durable_without_starting_more_tasks() {
    let dir = tempfile::tempdir().unwrap();
    let database = db(&dir);
    let (tx, mut rx) = mpsc::unbounded_channel();
    let master = Master::open(
        database.clone(),
        MasterConfig {
            max_active_turns: 1,
            ..Default::default()
        },
        Arc::new(Controlled(tx)),
        None,
        Arc::new(Actions::default()),
        Redactor::default(),
    )
    .await
    .unwrap();
    master
        .receive(incoming("a", "first", "held first conversation"))
        .await
        .unwrap();
    let (_, first) = next(&mut rx).await;
    let receipt = master
        .receive(incoming(
            "b",
            "second",
            "durable queued second conversation",
        ))
        .await
        .unwrap();
    assert!(!receipt.duplicate);
    assert!(rx.try_recv().is_err());
    assert_eq!(
        master.history("b", None, 128).await.unwrap()[0].text,
        "durable queued second conversation"
    );
    first.send(Ok(response("first complete"))).unwrap();
    wait_reply(&master, "a", "first complete").await;
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            master.retry_pending().await.unwrap();
            if let Ok((request, answer)) = rx.try_recv() {
                assert!(
                    request
                        .messages
                        .iter()
                        .any(|m| m.content == "durable queued second conversation")
                );
                answer.send(Ok(response("second complete"))).unwrap();
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    wait_reply(&master, "b", "second complete").await;
    master.shutdown().await;
    database.shutdown().await.unwrap();
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn secret_tool_identifiers_are_rejected_before_any_durable_action_or_transcript() {
    let dir = tempfile::tempdir().unwrap();
    let database = db(&dir);
    let (tx, mut rx) = mpsc::unbounded_channel();
    let redactor = Redactor::default();
    let secret = "synthetic-master-canary-never-record";
    redactor
        .register_secret(&excalibur_vault::SecretValue::new(secret.as_bytes().to_vec()).unwrap());
    let master = Master::open(
        database.clone(),
        MasterConfig::default(),
        Arc::new(Controlled(tx)),
        None,
        Arc::new(Actions::default()),
        redactor,
    )
    .await
    .unwrap();
    master
        .receive(incoming("a", "first", "hello"))
        .await
        .unwrap();
    let (_, answer) = next(&mut rx).await;
    let mut unsafe_response = response("");
    unsafe_response.message.tool_calls = vec![ToolCall {
        id: secret.into(),
        name: "status".into(),
        arguments: "{}".into(),
    }];
    answer.send(Ok(unsafe_response)).unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if master
                .events(0, 128)
                .await
                .unwrap()
                .iter()
                .any(|e| e.kind == "turn_failed")
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let secret = secret.to_string();
    let records = database
        .read(excalibur_store::ReadLane::Interactive, move |conn| {
            let actions: u64 =
                conn.query_row("SELECT count(*) FROM master_action_receipts", [], |r| {
                    r.get(0)
                })?;
            let traces: u64 = conn.query_row(
                "SELECT count(*) FROM master_turn_state WHERE instr(messages,?1)>0",
                [secret],
                |r| r.get(0),
            )?;
            Ok((actions, traces))
        })
        .await
        .unwrap();
    assert_eq!(records, (0, 0));
    master.shutdown().await;
    database.shutdown().await.unwrap();
}
