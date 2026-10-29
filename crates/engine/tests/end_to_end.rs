//! Real engine + durable SQLite + OpenAI-compatible HTTP adapter + local file/process tools.
#![cfg(unix)]
use excalibur_context::ContextInput;
use excalibur_engine::{Budget, Criterion, Engine, RunSpec, Status};
use excalibur_journal::Journal;
use excalibur_models::{GatewayConfig, ModelRole, OpenAiGateway};
use excalibur_providers::{Provider, ProviderClient, Secret};
use excalibur_store::{Database, DatabaseKind, StorageConfig};
use excalibur_tools::{Access, LocalTools, ToolPolicy};
use serde_json::{Value, json};
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn simple_task_runs_from_mock_model_through_files_terminal_and_verified_completion() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let mut requests = Vec::new();
        for step in 0..3 {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut bytes = Vec::new();
            let mut chunk = [0u8; 4096];
            let (head, body_len) = loop {
                let n = socket.read(&mut chunk).await.unwrap();
                assert_ne!(n, 0);
                bytes.extend_from_slice(&chunk[..n]);
                assert!(bytes.len() < 2 * 1024 * 1024);
                if let Some(index) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
                    let headers = String::from_utf8_lossy(&bytes[..index]);
                    assert!(headers.starts_with("POST /v1/chat/completions "));
                    let len = headers
                        .lines()
                        .find_map(|line| {
                            line.to_ascii_lowercase()
                                .strip_prefix("content-length: ")
                                .and_then(|n| n.parse::<usize>().ok())
                        })
                        .unwrap();
                    break (index + 4, len);
                }
            };
            while bytes.len() < head + body_len {
                let n = socket.read(&mut chunk).await.unwrap();
                assert_ne!(n, 0);
                bytes.extend_from_slice(&chunk[..n]);
            }
            let request: Value = serde_json::from_slice(&bytes[head..head + body_len]).unwrap();
            assert_eq!(request["model"], "synthetic-model");
            requests.push(request);
            let message = match step {
                0 => {
                    json!({"role":"assistant","content":null,"tool_calls":[{"id":"create","type":"function","function":{"name":"write_file","arguments":json!({"path":"answer.txt","content":"42\n"}).to_string()}}]})
                }
                1 => {
                    json!({"role":"assistant","content":null,"tool_calls":[{"id":"inspect","type":"function","function":{"name":"read_file","arguments":json!({"path":"answer.txt"}).to_string()}},{"id":"verify","type":"function","function":{"name":"exec","arguments":json!({"cmd":"test \"$(cat answer.txt)\" = 42","yield_ms":250}).to_string()}}]})
                }
                _ => {
                    json!({"role":"assistant","content":"Created answer.txt and verified its content with a process exiting successfully."})
                }
            };
            let response=json!({"choices":[{"message":message,"finish_reason":if step<2{"tool_calls"}else{"stop"}}],"usage":{"prompt_tokens":100,"completion_tokens":20}}).to_string();
            let wire = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}",
                response.len()
            );
            socket.write_all(wire.as_bytes()).await.unwrap();
        }
        requests
    });
    let dir = tempfile::tempdir().unwrap();
    let workspace = dir.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let db = Database::open(
        dir.path().join("data/main.db"),
        DatabaseKind::Main,
        StorageConfig::default(),
    )
    .unwrap();
    let journal = Journal::new(db.clone()).await.unwrap();
    let tools = Arc::new(
        LocalTools::new(
            db.clone(),
            workspace.clone(),
            ToolPolicy {
                write: Access::Allow,
                exec: Access::Allow,
                ..Default::default()
            },
        )
        .await
        .unwrap(),
    );
    let client = ProviderClient::with_base_url(
        Provider::OpenAi,
        Secret::new("synthetic-test-token").unwrap(),
        &format!("http://{address}/v1/"),
    )
    .unwrap();
    let gateway = Arc::new(
        OpenAiGateway::new(
            client,
            GatewayConfig::new("synthetic-model", ModelRole::Worker),
        )
        .unwrap(),
    );
    let mut context = ContextInput::new(
        "Use tools and report only verified results.",
        "Create answer.txt containing 42 and verify it using terminal.",
        "2026-10-29T00:00:00Z",
    );
    context.layers[2] = tools.terminals().environment().to_string();
    let engine = Engine::new(journal.clone(), gateway, tools.clone());
    let run = engine
        .create(
            "simple-task",
            RunSpec {
                context,
                budget: Budget::default(),
                criteria: vec![
                    Criterion::ToolOutput {
                        name: "read_file".into(),
                        pointer: "/lines/0/text".into(),
                        expected: json!("42"),
                    },
                    Criterion::ToolOutput {
                        name: "exec".into(),
                        pointer: "/exit_code".into(),
                        expected: json!(0),
                    },
                ],
            },
        )
        .await
        .unwrap();
    let final_state = tokio::time::timeout(std::time::Duration::from_secs(30), engine.run(&run))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        final_state.status,
        Status::Completed,
        "{:?}",
        final_state.reason
    );
    assert_eq!(final_state.usage.input_tokens, 300);
    assert_eq!(final_state.usage.output_tokens, 60);
    assert_eq!(
        std::fs::read(workspace.join("answer.txt")).unwrap(),
        b"42\n"
    );
    let requests = server.await.unwrap();
    let final_messages = requests[2]["messages"].as_array().unwrap();
    let tool_ids: Vec<_> = final_messages
        .iter()
        .filter_map(|m| m["tool_call_id"].as_str())
        .collect();
    assert_eq!(tool_ids, vec!["create", "inspect", "verify"]);
    let events = journal.replay(&run, 0).await.unwrap();
    assert_eq!(events.iter().filter(|e| e.kind == "tool_intent").count(), 3);
    assert_eq!(events.iter().filter(|e| e.kind == "tool_result").count(), 3);
    assert!(
        !serde_json::to_string(&events)
            .unwrap()
            .contains("synthetic-test-token")
    );
    let again = engine.run(&run).await.unwrap();
    assert_eq!(again.status, Status::Completed);
    tools.terminals().shutdown().await;
    db.shutdown().await.unwrap();
}
