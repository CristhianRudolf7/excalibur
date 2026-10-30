//! Real process interruption between a fast action's effect and its receipt.
#![cfg(unix)]
use excalibur_master::*;
use excalibur_models::{ModelError, ModelGateway};
use excalibur_store::{Database, DatabaseKind, StorageConfig};
use excalibur_types::runtime::*;
use excalibur_vault::Redactor;
use serde_json::{Value, json};
use std::{
    io::Write,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
struct Model;
impl ModelGateway for Model {
    fn complete<'a>(
        &'a self,
        request: ModelRequest,
    ) -> BoxFuture<'a, std::result::Result<ModelResponse, ModelError>> {
        Box::pin(async move {
            if request.messages.iter().any(|m| m.role == "tool") {
                return Ok(ModelResponse {
                    message: Message::text(
                        "assistant",
                        "An interrupted effect requires inspection; it was not repeated.",
                    ),
                    usage: Usage::default(),
                });
            }
            let mut message = Message::text("assistant", "");
            message.tool_calls.push(ToolCall {
                id: "stable-call".into(),
                name: "subagent".into(),
                arguments: "{}".into(),
            });
            Ok(ModelResponse {
                message,
                usage: Usage::default(),
            })
        })
    }
}
struct Actions {
    directory: PathBuf,
    crash: bool,
    invocations: AtomicUsize,
}
impl FastActions for Actions {
    fn definitions(&self) -> Vec<ToolDefinition> {
        vec![ToolDefinition {
            name: "subagent".into(),
            description: "synthetic durable effect".into(),
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
            self.invocations.fetch_add(1, Ordering::SeqCst);
            let mut effect = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(self.directory.join("effect"))
                .unwrap();
            effect.write_all(b"once\n").unwrap();
            effect.sync_all().unwrap();
            if self.crash {
                std::fs::write(
                    self.directory.join("ready"),
                    "effect durable, receipt absent",
                )
                .unwrap();
                std::future::pending::<()>().await;
            }
            Ok(json!({"effect":"performed"}))
        })
    }
}
fn db(path: &std::path::Path) -> Database {
    Database::open(
        path.join("main.db"),
        DatabaseKind::Main,
        StorageConfig::default(),
    )
    .unwrap()
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "subprocess fixture invoked by its parent"]
async fn master_crash_helper() {
    let directory = PathBuf::from(std::env::var_os("EXCALIBUR_MASTER_CRASH_DIR").unwrap());
    let database = db(&directory);
    let actions = Arc::new(Actions {
        directory: directory.clone(),
        crash: true,
        invocations: AtomicUsize::new(0),
    });
    let master = Master::open(
        database,
        MasterConfig::default(),
        Arc::new(Model),
        None,
        actions,
        Redactor::default(),
    )
    .await
    .unwrap();
    master
        .receive(Incoming {
            conversation: "durable-conversation".into(),
            client_id: "synthetic".into(),
            client_msg_id: "first".into(),
            text: "delegate synthetic work".into(),
            scope: None,
            timezone: "UTC".into(),
            channel: "local".into(),
            reply_to: None,
            attachments: Vec::new(),
        })
        .await
        .unwrap();
    std::future::pending::<()>().await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sigkill_after_effect_before_receipt_never_repeats_the_fast_action() {
    use std::os::unix::process::ExitStatusExt;
    let directory = tempfile::tempdir().unwrap();
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "master_crash_helper", "--ignored", "--nocapture"])
        .env("EXCALIBUR_MASTER_CRASH_DIR", directory.path())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let ready = tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            if directory.path().join("ready").exists() {
                break;
            }
            assert!(
                child.try_wait().unwrap().is_none(),
                "fixture exited before the effect"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await;
    if ready.is_err() {
        let _ = child.kill();
        let output = child.wait_with_output().unwrap();
        panic!(
            "fixture timed out: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    child.kill().unwrap();
    let status = child.wait().unwrap();
    assert_eq!(status.signal(), Some(9));
    let database = db(directory.path());
    let actions = Arc::new(Actions {
        directory: directory.path().into(),
        crash: false,
        invocations: AtomicUsize::new(0),
    });
    let master = Master::open(
        database.clone(),
        MasterConfig::default(),
        Arc::new(Model),
        None,
        actions.clone(),
        Redactor::default(),
    )
    .await
    .unwrap();
    master.retry_pending().await.unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if master
                .history("durable-conversation", None, 128)
                .await
                .unwrap()
                .iter()
                .any(|m| m.role == "assistant" && m.text.contains("not repeated"))
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(
        std::fs::read_to_string(directory.path().join("effect")).unwrap(),
        "once\n"
    );
    assert_eq!(actions.invocations.load(Ordering::SeqCst), 0);
    master.shutdown().await;
    database.shutdown().await.unwrap();
}
