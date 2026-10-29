//! Shared engine context lifecycle: real SQLite/files, scripted model gateways.
use excalibur_context::{ContextInput, SessionPolicy};
use excalibur_engine::{Budget, Criterion, Engine, RunSpec, Status};
use excalibur_journal::{
    Journal,
    instructions::{InstructionOrigin, InstructionOriginKind, InstructionScope, InstructionStore},
};
use excalibur_models::{GatewayMetadata, ModelCallBound, ModelError, ModelGateway, ModelRole};
use excalibur_store::{Database, DatabaseKind, StorageConfig};
use excalibur_tools::{Access, LocalTools, ToolPolicy};
use excalibur_types::runtime::*;
use serde_json::{Value, json};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};
struct Script {
    requests: Arc<Mutex<Vec<ModelRequest>>>,
    retrieve: bool,
    expected: String,
}
impl ModelGateway for Script {
    fn request_time_override(&self, _: u64) -> Option<String> {
        Some("1700000000".into())
    }
    fn complete<'a>(
        &'a self,
        request: ModelRequest,
    ) -> BoxFuture<'a, Result<ModelResponse, ModelError>> {
        Box::pin(async move {
            let mut requests = self.requests.lock().unwrap();
            let first = requests.is_empty();
            requests.push(request.clone());
            let message = if self.retrieve && first {
                let marker = request
                    .messages
                    .iter()
                    .filter(|m| m.role == "tool")
                    .find_map(|m| {
                        serde_json::from_str::<Value>(&m.content)
                            .ok()
                            .filter(|v| v.get("masked_observation").is_some())
                    })
                    .expect("mask batch must be in the actual request");
                let mut m = Message::text("assistant", "");
                m.tool_calls.push(ToolCall {
                    id: "retrieve".into(),
                    name: "get_tool_output".into(),
                    arguments: marker["retrieval"]["arguments"].to_string(),
                });
                m
            } else {
                if self.retrieve {
                    let output = request
                        .messages
                        .iter()
                        .find(|m| m.tool_call_id.as_deref() == Some("retrieve"))
                        .unwrap();
                    let output: Value = serde_json::from_str(&output.content).unwrap();
                    assert_eq!(output["output"]["text"], self.expected);
                }
                Message::text("assistant", "done")
            };
            Ok(ModelResponse {
                message,
                usage: Usage {
                    input_tokens: 1,
                    output_tokens: 1,
                    estimated: true,
                    ..Default::default()
                },
            })
        })
    }
}
struct InvalidExtractor(Arc<AtomicUsize>);
impl ModelGateway for InvalidExtractor {
    fn metadata(&self) -> GatewayMetadata {
        GatewayMetadata {
            role: Some(ModelRole::Extractor),
            ..Default::default()
        }
    }
    fn call_bound(&self, _: &ModelRequest) -> Option<ModelCallBound> {
        Some(ModelCallBound {
            max_tokens: 1000,
            max_cost_microusd: 0,
        })
    }
    fn complete<'a>(&'a self, _: ModelRequest) -> BoxFuture<'a, Result<ModelResponse, ModelError>> {
        Box::pin(async move {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(ModelResponse {
                message: Message::text("assistant", "invalid JSON"),
                usage: Usage {
                    input_tokens: 10,
                    output_tokens: 2,
                    ..Default::default()
                },
            })
        })
    }
}
async fn fixture() -> (tempfile::TempDir, Database, Journal, Arc<LocalTools>) {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("workspace");
    std::fs::create_dir(&root).unwrap();
    let db = Database::open(
        temp.path().join("main.db"),
        DatabaseKind::Main,
        StorageConfig::default(),
    )
    .unwrap();
    let journal = Journal::new(db.clone()).await.unwrap();
    let tools = Arc::new(
        LocalTools::new(
            db.clone(),
            root,
            ToolPolicy {
                read: Access::Allow,
                write: Access::Allow,
                exec: Access::Deny,
                ..Default::default()
            },
        )
        .await
        .unwrap(),
    );
    (temp, db, journal, tools)
}
fn spec() -> RunSpec {
    RunSpec {
        context: ContextInput::new(
            "Synthetic core",
            "Original literal request: retain the exact artifact trace",
            "1700000000",
        ),
        budget: Budget {
            max_tokens: 1_000_000,
            ..Default::default()
        },
        criteria: vec![Criterion::FinalResponse { schema: None }],
    }
}
#[tokio::test]
async fn compaction_fallback_uses_real_artifact_history_and_literal_instructions_after_reopen() {
    let (temp, db, journal, tools) = fixture().await;
    let instructions = InstructionStore::open(db.clone(), excalibur_vault::Redactor::default())
        .await
        .unwrap();
    let literal = "Preserve this literal constraint: never alter protected.txt — línea exacta.";
    let saved_literal = literal.to_owned();
    db.write(excalibur_store::Priority::Normal,move|c|{c.execute("INSERT INTO conversations(id) VALUES('source-conversation')",[])?;c.execute("INSERT INTO messages(id,conversation_id,role,text) VALUES('source-message','source-conversation','user',?1)",[saved_literal])?;Ok(())}).await.unwrap();
    instructions
        .register(
            "literal-source",
            None,
            InstructionScope::General,
            InstructionOrigin {
                kind: InstructionOriginKind::UserMessage,
                reference: "source-message".into(),
            },
            literal,
        )
        .await
        .unwrap();
    let requests = Arc::new(Mutex::new(vec![]));
    let calls = Arc::new(AtomicUsize::new(0));
    let policy = SessionPolicy {
        compaction_cap: 10_000,
        recent_activity_tokens: 1000,
        ..Default::default()
    };
    let engine = Engine::new(
        journal.clone(),
        Arc::new(Script {
            requests: requests.clone(),
            retrieve: false,
            expected: String::new(),
        }),
        tools.clone(),
    )
    .with_context_policy(policy)
    .with_context_extractor(Arc::new(InvalidExtractor(calls.clone())))
    .unwrap();
    let mut s = spec();
    for n in 0..100 {
        s.context.activity.push(Message::text(
            "user",
            format!("Earlier context {n}: {}", "synthetic history ".repeat(100)),
        ));
    }
    for (id, exit) in [("old-failure", 1), ("corrected-check", 0)] {
        let mut call = Message::text("assistant", "Check the synthetic artifact");
        call.tool_calls.push(ToolCall {
            id: id.into(),
            name: "exec".into(),
            arguments: json!({"cmd":"synthetic-check"}).to_string(),
        });
        s.context.activity.push(call);
        let mut result = Message::text(
            "tool",
            serde_json::to_string(&ToolResult {
                success: exit == 0,
                output: json!({"exit_code":exit,"error":if exit==0{""}else{"exact prior failure"}}),
                full_output_blob: None,
            })
            .unwrap(),
        );
        result.tool_call_id = Some(id.into());
        s.context.activity.push(result);
    }
    let run = engine.create("compaction-file", s).await.unwrap();
    let prepared = tools
        .prepare(
            &run,
            ToolCall {
                id: "actual-file-edit".into(),
                name: "write_file".into(),
                arguments: json!({"path":"artifact.txt","content":"exact synthetic artifact\n"})
                    .to_string(),
            },
        )
        .await
        .unwrap();
    assert!(
        tools
            .execute(&run, "actual-file-edit", &prepared)
            .await
            .success
    );
    let state = engine.run(&run).await.unwrap();
    assert_eq!(state.status, Status::Completed);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    let events = journal.replay(&run, 0).await.unwrap();
    let receipt = events
        .iter()
        .find(|e| e.kind == "context_prepared")
        .unwrap();
    assert!(receipt.payload["receipt"]["compaction"].is_object());
    let actual = requests.lock().unwrap()[0].clone();
    assert!(actual.messages.iter().any(|m| m.content.contains(literal)));
    let block = actual
        .messages
        .iter()
        .find_map(|m| {
            serde_json::from_str::<Value>(&m.content)
                .ok()
                .filter(|v| v.get("context_compaction").is_some())
        })
        .unwrap();
    assert!(
        block["context_compaction"]["structured_state"]["open_errors"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        block["context_compaction"]["structured_state"]["commands"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    assert_eq!(
        block["context_compaction"]["structured_state"]["commands"][1]["exit_code"],
        0
    );
    let artifacts = block["context_compaction"]["structured_state"]["artifacts"]
        .as_array()
        .unwrap();
    assert_eq!(artifacts.len(), 1);
    assert_eq!(
        artifacts[0]["path"],
        temp.path().join("workspace/artifact.txt").to_str().unwrap()
    );
    assert_eq!(artifacts[0]["operation"], "create");
    let hash = artifacts[0]["hash"].as_str().unwrap().parse().unwrap();
    assert_eq!(
        db.read_blob(hash, 1024).await.unwrap(),
        b"exact synthetic artifact\n"
    );
    assert_eq!(state.model_calls, 2);
    db.shutdown().await.unwrap();
    drop(engine);
    drop(tools);
    drop(journal);
    drop(instructions);
    drop(db);
    let db = Database::open(
        temp.path().join("main.db"),
        DatabaseKind::Main,
        StorageConfig::default(),
    )
    .unwrap();
    let reopened = Journal::new(db.clone()).await.unwrap();
    let tools = Arc::new(
        LocalTools::new(
            db.clone(),
            temp.path().join("workspace"),
            ToolPolicy {
                read: Access::Allow,
                write: Access::Allow,
                exec: Access::Deny,
                ..Default::default()
            },
        )
        .await
        .unwrap(),
    );
    let engine = Engine::new(
        reopened,
        Arc::new(Script {
            requests: requests.clone(),
            retrieve: false,
            expected: String::new(),
        }),
        tools,
    );
    let restored = engine.load(&run).await.unwrap();
    assert_eq!(restored.context_session, state.context_session);
    assert_eq!(restored.usage.input_tokens, state.usage.input_tokens);
    assert!(
        receipt.payload["receipt"]["compaction"]["discarded"]
            .to_string()
            .contains(&run)
    );
    db.shutdown().await.unwrap();
}
#[tokio::test]
async fn masking_archives_small_results_and_get_tool_output_returns_exact_observation() {
    let (_temp, db, journal, tools) = fixture().await;
    let requests = Arc::new(Mutex::new(vec![]));
    let mut s = spec();
    let mut expected = String::new();
    for n in 0..31 {
        let mut call = Message::text("assistant", format!("Read source {n}"));
        call.tool_calls.push(ToolCall {
            id: format!("old-{n}"),
            name: "read_file".into(),
            arguments: json!({"path":format!("source{n}.txt")}).to_string(),
        });
        s.context.activity.push(call);
        let content = serde_json::to_string(&ToolResult::ok(
            json!({"data":"synthetic ".repeat(340),"index":n}),
        ))
        .unwrap();
        if n == 0 {
            expected = content.clone();
        }
        let mut result = Message::text("tool", content);
        result.tool_call_id = Some(format!("old-{n}"));
        s.context.activity.push(result);
    }
    let engine = Engine::new(
        journal.clone(),
        Arc::new(Script {
            requests: requests.clone(),
            retrieve: true,
            expected: expected.clone(),
        }),
        tools,
    );
    let run = engine.create("masked-archive", s).await.unwrap();
    let state = engine.run(&run).await.unwrap();
    assert_eq!(state.status, Status::Completed);
    let session = state.context_session.unwrap();
    assert_eq!(
        session.history_lookup("initial:1").unwrap().message.content,
        expected
    );
    assert_eq!(requests.lock().unwrap().len(), 2);
    let receipt = journal
        .replay(&run, 0)
        .await
        .unwrap()
        .into_iter()
        .find(|e| e.kind == "context_prepared")
        .unwrap();
    assert!(receipt.payload["receipt"]["mask"].is_object());
    db.shutdown().await.unwrap();
}

#[cfg(unix)]
struct CutAfterContext(std::path::PathBuf);
#[cfg(unix)]
impl excalibur_engine::Observer for CutAfterContext {
    fn reached(&self, phase: excalibur_engine::Boundary, _: &str, _: u64, _: Option<&str>) {
        if phase == excalibur_engine::Boundary::ContextPreparedConfirmed {
            use std::io::Write;
            let mut marker = std::fs::File::create(&self.0).unwrap();
            marker.write_all(b"confirmed").unwrap();
            marker.sync_all().unwrap();
            loop {
                std::thread::park();
            }
        }
    }
}
#[cfg(unix)]
#[tokio::test]
#[ignore = "isolated subprocess killed by its parent after context preparation"]
async fn context_prepared_child() {
    let Some(root) = std::env::var_os("EXCALIBUR_CONTEXT_CHILD_ROOT") else {
        return;
    };
    let root = std::path::PathBuf::from(root);
    let run = std::env::var("EXCALIBUR_CONTEXT_CHILD_RUN").unwrap();
    let db = Database::open(
        root.join("main.db"),
        DatabaseKind::Main,
        StorageConfig::default(),
    )
    .unwrap();
    let journal = Journal::new(db).await.unwrap();
    let tools = Arc::new(
        LocalTools::new(
            journal.database().clone(),
            root.join("workspace"),
            ToolPolicy {
                read: Access::Allow,
                write: Access::Allow,
                exec: Access::Deny,
                ..Default::default()
            },
        )
        .await
        .unwrap(),
    );
    let engine = Engine::new(
        journal,
        Arc::new(Script {
            requests: Arc::new(Mutex::new(vec![])),
            retrieve: false,
            expected: String::new(),
        }),
        tools,
    )
    .with_observer(Arc::new(CutAfterContext(root.join("context-confirmed"))));
    engine.run(&run).await.unwrap();
    panic!("child must be killed at preparation boundary");
}
#[cfg(unix)]
#[tokio::test]
async fn sigkill_after_compaction_receipt_resumes_without_repeating_model_or_losing_history() {
    let (temp, db, journal, tools) = fixture().await;
    let requests = Arc::new(Mutex::new(vec![]));
    let policy = SessionPolicy {
        compaction_cap: 10_000,
        recent_activity_tokens: 1000,
        ..Default::default()
    };
    let engine = Engine::new(
        journal.clone(),
        Arc::new(Script {
            requests: requests.clone(),
            retrieve: false,
            expected: String::new(),
        }),
        tools.clone(),
    )
    .with_context_policy(policy);
    let mut s = spec();
    for n in 0..100 {
        s.context.activity.push(Message::text(
            "user",
            format!(
                "Immutable history {n}: {}",
                "synthetic segment ".repeat(100)
            ),
        ));
    }
    let run = engine.create("kill-context", s).await.unwrap();
    db.shutdown().await.unwrap();
    drop(engine);
    drop(tools);
    drop(journal);
    drop(db);
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "context_prepared_child",
            "--ignored",
            "--nocapture",
        ])
        .env("EXCALIBUR_CONTEXT_CHILD_ROOT", temp.path())
        .env("EXCALIBUR_CONTEXT_CHILD_RUN", &run)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(30);
    while !temp.path().join("context-confirmed").exists() {
        assert!(
            child.try_wait().unwrap().is_none(),
            "child exited before confirmed compaction"
        );
        assert!(
            tokio::time::Instant::now() < deadline,
            "context child deadline"
        );
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    child.kill().unwrap();
    let status = child.wait().unwrap();
    use std::os::unix::process::ExitStatusExt;
    assert_eq!(status.signal(), Some(9));
    let db = Database::open(
        temp.path().join("main.db"),
        DatabaseKind::Main,
        StorageConfig::default(),
    )
    .unwrap();
    let journal = Journal::new(db.clone()).await.unwrap();
    let events = journal.replay(&run, 0).await.unwrap();
    assert!(events.iter().any(|e|e.kind=="context_prepared"&&e.payload["receipt"]["compaction"].is_object()));
    assert!(!events.iter().any(|e| e.kind == "model_request"));
    let tools = Arc::new(
        LocalTools::new(
            db.clone(),
            temp.path().join("workspace"),
            ToolPolicy {
                read: Access::Allow,
                write: Access::Allow,
                exec: Access::Deny,
                ..Default::default()
            },
        )
        .await
        .unwrap(),
    );
    let engine = Engine::new(
        journal,
        Arc::new(Script {
            requests: requests.clone(),
            retrieve: false,
            expected: String::new(),
        }),
        tools,
    );
    let state = engine.run(&run).await.unwrap();
    assert_eq!(state.status, Status::Completed);
    assert_eq!(requests.lock().unwrap().len(), 1);
    assert_eq!(
        state
            .context_session
            .unwrap()
            .history_search("Immutable history 0:", 0, 20)
            .len(),
        1
    );
    db.shutdown().await.unwrap();
}
