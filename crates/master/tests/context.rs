use excalibur_context::TokenCounter;
use excalibur_master::*;
use excalibur_models::{CacheSettings, CacheStrategy, GatewayMetadata, ModelError, ModelGateway};
use excalibur_store::{Database, DatabaseKind, Priority};
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
type Call = (ModelRequest, oneshot::Sender<ModelResponse>);
struct Model(mpsc::UnboundedSender<Call>);
impl ModelGateway for Model {
    fn metadata(&self) -> GatewayMetadata {
        GatewayMetadata {
            model: Some("gpt-4o".into()),
            ..Default::default()
        }
    }
    fn cache_settings(&self) -> CacheSettings {
        CacheSettings {
            strategy: CacheStrategy::Automatic,
            namespace_key_supported: true,
            ..Default::default()
        }
    }
    fn complete<'a>(
        &'a self,
        request: ModelRequest,
    ) -> BoxFuture<'a, std::result::Result<ModelResponse, ModelError>> {
        Box::pin(async move {
            let (tx, rx) = oneshot::channel();
            self.0
                .send((request, tx))
                .map_err(|_| ModelError::Provider)?;
            rx.await.map_err(|_| ModelError::Provider)
        })
    }
}
#[derive(Default)]
struct Actions(AtomicUsize);
impl FastActions for Actions {
    fn definitions(&self) -> Vec<ToolDefinition> {
        vec![ToolDefinition {
            name: "status".into(),
            description: "Get compact status".into(),
            input_schema: json!({"type":"object","additionalProperties":false}),
        }]
    }
    fn execute<'a>(
        &'a self,
        _: &'a str,
        _: &'a str,
        _: Option<&'a str>,
        _: &'a str,
    ) -> BoxFuture<'a, std::result::Result<Value, String>> {
        Box::pin(async { Ok(json!({"status":"available"})) })
    }
    fn invoke<'a>(
        &'a self,
        _: &'a str,
        _: &'a str,
        _: Value,
        _: &'a str,
    ) -> BoxFuture<'a, std::result::Result<Value, String>> {
        Box::pin(async { Ok(json!({"status":"available"})) })
    }
    fn context<'a>(&'a self, _: &'a str) -> BoxFuture<'a, std::result::Result<Value, String>> {
        Box::pin(async move {
            Ok(
                json!({"marker":if self.0.load(Ordering::SeqCst)==0 {"VOLATILE_JOB_ONE"}else{"VOLATILE_JOB_TWO"}}),
            )
        })
    }
}
fn incoming(id: &str, text: &str) -> Incoming {
    Incoming {
        conversation: "context-conversation".into(),
        client_id: "context-fixture".into(),
        client_msg_id: id.into(),
        text: text.into(),
        scope: Some("project:a".into()),
        timezone: "UTC".into(),
        channel: "test".into(),
        reply_to: None,
        attachments: vec![],
    }
}
fn response(text: &str) -> ModelResponse {
    ModelResponse {
        message: Message::text("assistant", text),
        usage: Usage::default(),
    }
}
fn database(dir: &tempfile::TempDir) -> Database {
    Database::open(
        dir.path().join("main.db"),
        DatabaseKind::Main,
        Default::default(),
    )
    .unwrap()
}
async fn next(rx: &mut mpsc::UnboundedReceiver<Call>) -> Call {
    tokio::time::timeout(Duration::from_secs(10), rx.recv())
        .await
        .unwrap()
        .unwrap()
}
async fn event(master: &Master, kind: &str) {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if master
                .events(0, 512)
                .await
                .unwrap()
                .iter()
                .any(|e| e.kind == kind)
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
async fn compiler_layers_keep_scope_and_original_literal_stable_while_jobs_change_only_l5() {
    let dir = tempfile::tempdir().unwrap();
    let db = database(&dir);
    let (tx, mut rx) = mpsc::unbounded_channel();
    let actions = Arc::new(Actions::default());
    let config = MasterConfig {
        identity: "Fixed master identity".into(),
        scope_context: [
            ("project:a".into(), "A-only convention".into()),
            ("project:b".into(), "B-only forbidden convention".into()),
        ]
        .into(),
        ..Default::default()
    };
    let master = Master::open(
        db.clone(),
        config.clone(),
        Arc::new(Model(tx)),
        None,
        actions.clone(),
        Redactor::default(),
    )
    .await
    .unwrap();
    let literal = "Keep this exact original: exportación CSV.\nDo not erase the second line.";
    let receipt = master.receive(incoming("first", literal)).await.unwrap();
    let (first, answer) = next(&mut rx).await;
    assert!(first.messages[0].content.starts_with("[L0]"));
    assert!(first.messages[1].content.contains("Fixed master identity"));
    assert!(first.messages[2].content.contains("A-only convention"));
    assert!(first.messages[3].content.contains(literal));
    assert!(first.messages[3].content.contains(&receipt.message_id));
    assert!(
        first
            .messages
            .iter()
            .any(|m| m.role == "user" && m.content == literal)
    );
    assert!(
        !serde_json::to_string(&first)
            .unwrap()
            .contains("B-only forbidden")
    );
    assert!(
        first
            .messages
            .last()
            .unwrap()
            .content
            .contains("VOLATILE_JOB_ONE")
    );
    assert!(
        !first.messages[..4]
            .iter()
            .any(|m| m.content.contains("VOLATILE_JOB"))
    );
    actions.0.store(1, Ordering::SeqCst);
    let mut call = response("");
    call.message.tool_calls = vec![ToolCall {
        id: "status-call".into(),
        name: "status".into(),
        arguments: "{}".into(),
    }];
    answer.send(call).unwrap();
    let (second, answer) = next(&mut rx).await;
    assert_eq!(first.messages[..4], second.messages[..4]);
    assert!(
        second
            .messages
            .last()
            .unwrap()
            .content
            .contains("VOLATILE_JOB_TWO")
    );
    assert_eq!(
        first.cache.as_ref().unwrap().stable_prefix_hash,
        second.cache.as_ref().unwrap().stable_prefix_hash
    );
    assert_eq!(
        first.cache.as_ref().unwrap().namespace_key,
        second.cache.as_ref().unwrap().namespace_key
    );
    assert!(
        TokenCounter::with_margin(TokenCounter::for_model("gpt-4o").count_request(&second))
            <= config.prompt_token_budget
    );
    assert!(serde_json::to_vec(&second).unwrap().len() <= config.context_byte_budget);
    assert!(
        second
            .messages
            .iter()
            .any(|m| m.role == "tool" && m.content.contains("_provenance"))
    );
    answer.send(response("compiled response")).unwrap();
    event(&master, "assistant_message").await;
    master.shutdown().await;
    db.shutdown().await.unwrap();
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn oversized_sources_block_before_inference_remain_literal_and_resume_under_a_larger_policy()
{
    let dir = tempfile::tempdir().unwrap();
    let db = database(&dir);
    let (tx, mut rx) = mpsc::unbounded_channel();
    let config = MasterConfig {
        context_byte_budget: 4096,
        ..Default::default()
    };
    let master = Master::open(
        db.clone(),
        config,
        Arc::new(Model(tx)),
        None,
        Arc::new(Actions::default()),
        Redactor::default(),
    )
    .await
    .unwrap();
    let literal = format!(
        "Literal source retained without a summary: {}",
        "x".repeat(20_000)
    );
    let input = incoming("oversized", &literal);
    let receipt = master.receive(input.clone()).await.unwrap();
    event(&master, "context_blocked").await;
    assert!(rx.try_recv().is_err());
    assert!(
        master
            .history("context-conversation", None, 128)
            .await
            .unwrap()
            .iter()
            .any(|m| m.id == receipt.message_id && m.text == literal)
    );
    assert!(master.receive(input).await.unwrap().duplicate);
    master
        .receive(incoming("follow-up", "continue"))
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if master
                .events(0, 128)
                .await
                .unwrap()
                .iter()
                .filter(|e| e.kind == "context_blocked")
                .count()
                >= 2
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(rx.try_recv().is_err());
    master.shutdown().await;
    drop(master);
    let (tx, mut recovered) = mpsc::unbounded_channel();
    let reopened = Master::open(
        db.clone(),
        MasterConfig::default(),
        Arc::new(Model(tx)),
        None,
        Arc::new(Actions::default()),
        Redactor::default(),
    )
    .await
    .unwrap();
    reopened
        .retry_context("context-conversation")
        .await
        .unwrap();
    let (request, answer) = next(&mut recovered).await;
    assert!(request.messages[3].content.contains(&literal));
    assert!(
        request
            .messages
            .iter()
            .any(|m| m.role == "user" && m.content == "continue")
    );
    answer
        .send(response("recovered without losing the source"))
        .unwrap();
    event(&reopened, "assistant_message").await;
    reopened.shutdown().await;
    db.shutdown().await.unwrap();
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_actual_known_tokenizer_budget_can_reject_a_character_estimate_that_fits() {
    let dir = tempfile::tempdir().unwrap();
    let db = database(&dir);
    let (tx, mut rx) = mpsc::unbounded_channel();
    let config = MasterConfig {
        base_instructions: "core".into(),
        identity: "master".into(),
        prompt_token_budget: 256,
        context_byte_budget: 8192,
        token_counter: Some(TokenCounter::for_model("gpt-4o")),
        ..Default::default()
    };
    let master = Master::open(
        db.clone(),
        config,
        Arc::new(Model(tx)),
        None,
        Arc::new(Actions::default()),
        Redactor::default(),
    )
    .await
    .unwrap();
    master
        .receive(incoming("token-bound", &"🙂".repeat(300)))
        .await
        .unwrap();
    event(&master, "context_blocked").await;
    assert!(rx.try_recv().is_err());
    let blocked = master
        .events(0, 128)
        .await
        .unwrap()
        .into_iter()
        .find(|e| e.kind == "context_blocked")
        .unwrap();
    assert!(
        blocked.payload["reason"]
            .as_str()
            .unwrap()
            .contains("compiled input estimate")
    );
    master.shutdown().await;
    db.shutdown().await.unwrap();
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn confirmed_history_is_a_bounded_scoped_tail_with_durable_older_sources() {
    let dir = tempfile::tempdir().unwrap();
    let db = database(&dir);
    let (tx, mut rx) = mpsc::unbounded_channel();
    let config = MasterConfig {
        context_byte_budget: 8192,
        prompt_token_budget: 5000,
        ..Default::default()
    };
    let master = Master::open(
        db.clone(),
        config,
        Arc::new(Model(tx)),
        None,
        Arc::new(Actions::default()),
        Redactor::default(),
    )
    .await
    .unwrap();
    master
        .receive(incoming("initial", "/status"))
        .await
        .unwrap();
    db.write(Priority::Interactive,|conn|{for index in 0..20{conn.execute("INSERT INTO messages(id,conversation_id,role,text,scope_id,principal_id) VALUES(?1,'context-conversation','assistant',?2,'project:a','local')",(format!("old-{index}"),format!("CONFIRMED-{index} {}","x".repeat(1000))))?;}conn.execute("INSERT INTO messages(id,conversation_id,role,text,scope_id,principal_id) VALUES('other-project','context-conversation','user','FORBIDDEN_OTHER_PROJECT_SOURCE','project:b','local')",[])?;Ok(())}).await.unwrap();
    master
        .receive(incoming("new", "New literal task"))
        .await
        .unwrap();
    let (request, answer) = next(&mut rx).await;
    let rendered = serde_json::to_string(&request).unwrap();
    assert!(rendered.contains("CONFIRMED-19"));
    assert!(!rendered.contains("CONFIRMED-0 "));
    assert!(!rendered.contains("FORBIDDEN_OTHER_PROJECT_SOURCE"));
    assert!(rendered.contains("older source messages remain durable"));
    assert!(serde_json::to_vec(&request).unwrap().len() <= 8192);
    let history = master
        .history("context-conversation", None, 128)
        .await
        .unwrap();
    assert!(
        history
            .iter()
            .any(|m| m.id == "old-0" && m.text.starts_with("CONFIRMED-0 "))
    );
    answer.send(response("bounded view completed")).unwrap();
    event(&master, "assistant_message").await;
    master.shutdown().await;
    db.shutdown().await.unwrap();
}
