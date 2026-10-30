use excalibur_daemon::{DaemonHost, HostOptions, RuntimeSet};
use excalibur_master::Incoming;
use excalibur_models::{ModelError, ModelGateway};
use excalibur_store::{Database, DatabaseKind};
use excalibur_types::runtime::*;
use excalibur_vault::Redactor;
use serde_json::json;
use std::{sync::Arc, time::Duration};
struct Fixture;
impl ModelGateway for Fixture {
    fn complete<'a>(
        &'a self,
        request: ModelRequest,
    ) -> BoxFuture<'a, Result<ModelResponse, ModelError>> {
        Box::pin(async move {
            let master = !request.tools.iter().any(|t| t.name == "write_file");
            let tool_done = request.messages.iter().any(|m| m.role == "tool");
            for tool in request
                .messages
                .iter()
                .filter(|message| message.role == "tool")
            {
                let value: serde_json::Value = serde_json::from_str(&tool.content).unwrap();
                assert!(
                    value.get("error").is_none() && value["success"] != false,
                    "synthetic model received a failing tool result: {value}"
                );
            }

            let message = if master && !tool_done {
                Message{role:"assistant".into(),content:String::new(),tool_calls:vec![ToolCall{id:"delegate".into(),name:"subagent".into(),arguments:json!({"action":"spawn","name":"Lena","goal":"Create answer.txt containing 42","deliverable":"Verified file"}).to_string()}],tool_call_id:None,provider_state:None}
            } else if !master && !tool_done {
                Message {
                    role: "assistant".into(),
                    content: String::new(),
                    tool_calls: vec![ToolCall {
                        id: "write".into(),
                        name: "write_file".into(),
                        arguments: json!({"path":"answer.txt","content":"42\n"}).to_string(),
                    }],
                    tool_call_id: None,
                    provider_state: None,
                }
            } else {
                Message::text(
                    "assistant",
                    if master {
                        "Delegated to Lena."
                    } else {
                        "Created answer.txt."
                    },
                )
            };
            Ok(ModelResponse {
                message,
                usage: Usage {
                    input_tokens: 10,
                    output_tokens: 10,
                    ..Default::default()
                },
            })
        })
    }
}
#[test]
fn authenticated_daemon_delegates_to_real_shared_engine_and_local_file_tools() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("project");
    std::fs::create_dir(&root).unwrap();
    let db = Database::open(
        temp.path().join("data/main.db"),
        DatabaseKind::Main,
        Default::default(),
    )
    .unwrap();
    let runtimes = RuntimeSet::new(2, 1).unwrap();
    let host = runtimes
        .block_on(DaemonHost::open(
            db,
            HostOptions::project(root.clone()),
            Arc::new(Fixture),
            None,
            "synthetic-token-for-local-tests-000000",
            Redactor::default(),
            runtimes.work(),
        ))
        .unwrap();
    runtimes.block_on(async {
        let receipt = host
            .master
            .receive(Incoming {
                conversation: "conversation-e2e".into(),
                client_id: "test-client".into(),
                client_msg_id: "message-1".into(),
                text: "Create the answer file".into(),
                scope: Some("project:default".into()),
                timezone: "UTC".into(),
                channel: "test".into(),
                reply_to: None,
                attachments: vec![],
            })
            .await
            .unwrap();
        let duplicate = host
            .master
            .receive(Incoming {
                conversation: "conversation-e2e".into(),
                client_id: "test-client".into(),
                client_msg_id: "message-1".into(),
                text: "Create the answer file".into(),
                scope: Some("project:default".into()),
                timezone: "UTC".into(),
                channel: "test".into(),
                reply_to: None,
                attachments: vec![],
            })
            .await
            .unwrap();
        assert_eq!(receipt.message_id, duplicate.message_id);
        assert!(duplicate.duplicate);
        let completed = tokio::time::timeout(Duration::from_secs(45), async {
            loop {
                host.orchestrator.tick().await.unwrap();
                let agents = host.orchestrator.list_agents().await.unwrap();
                if let Some(worker) = agents.iter().find(|a| a.depth == 1) {
                    let status = host.orchestrator.status(&worker.id).await.unwrap();
                    if status.state == "completed" {
                        break;
                    }
                    assert!(
                        !matches!(status.state.as_str(), "blocked" | "failed"),
                        "unexpected status: {status:?}"
                    );
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await;
        if completed.is_err() {
            for agent in host.orchestrator.list_agents().await.unwrap() {
                eprintln!(
                    "diagnostic agent {} status {:?}",
                    agent.id,
                    host.orchestrator.status(&agent.id).await.unwrap()
                );
            }
            let receipts = host
                .master
                .database()
                .read(excalibur_store::ReadLane::Interactive, |c| {
                    let mut q = c.prepare("SELECT result FROM master_action_receipts")?;
                    Ok(q.query_map([], |r| r.get::<_, Option<String>>(0))?
                        .collect::<Result<Vec<_>, _>>()?)
                })
                .await
                .unwrap();
            eprintln!("diagnostic receipts {receipts:?}");
            eprintln!(
                "diagnostic master events {:?}",
                host.master.events(0, 100).await.unwrap()
            );
        }
        completed.unwrap();
        assert_eq!(
            std::fs::read_to_string(root.join("answer.txt")).unwrap(),
            "42\n"
        );
        let workers = host
            .orchestrator
            .list_agents()
            .await
            .unwrap()
            .into_iter()
            .filter(|a| a.depth > 0)
            .collect::<Vec<_>>();
        assert_eq!(workers.len(), 1);
        let events = host.master.events(0, 100).await.unwrap();
        let progress = events
            .iter()
            .filter(|e| e.kind == "run_progress")
            .collect::<Vec<_>>();
        assert!(!progress.is_empty());
        let mut seen = std::collections::BTreeSet::new();
        for event in progress {
            assert!(seen.insert((
                event.payload["run_id"].to_string(),
                event.payload["sequence"].to_string()
            )));
            assert!(event.payload.get("prompt").is_none());
        }
        let run = workers[0].run_id.as_deref().unwrap();
        let metrics = host.telemetry.metrics(run).await.unwrap();
        assert_eq!(metrics.run_id, run);
        assert_eq!(metrics.final_state.as_deref(), Some("completed"));
        assert!(metrics.model_calls >= 2);
        assert!(metrics.tool_calls >= 1);
        let manifest = host.telemetry.export_run(run).await.unwrap();
        assert!(manifest.files.contains_key("trajectory.json"));
        assert!(manifest.files.contains_key("metrics.json"));
        let trajectory: serde_json::Value = serde_json::from_slice(
            &std::fs::read(
                temp.path()
                    .join("data/daemon-logs/runs")
                    .join(run)
                    .join("trajectory.json"),
            )
            .unwrap(),
        )
        .unwrap();
        assert!(trajectory.is_object());
        assert!(events.iter().any(|e| e.kind == "message_received"
            || e.kind == "received"
            || e.kind == "user_message"));
    });
    runtimes.block_on(host.close()).unwrap();
    let embedded_root = temp.path().join("embedded-project");
    std::fs::create_dir(&embedded_root).unwrap();
    let mut options = excalibur_eval::ExecOptions::new(
        "Create answer.txt containing 42",
        embedded_root.clone(),
        temp.path().join("embedded-logs"),
    );
    options.autonomy = excalibur_security::Autonomy::Autonomous;
    let result = runtimes.block_on(async {
        excalibur_eval::Execution::prepare(options, Arc::new(Fixture), Redactor::default())
            .await
            .unwrap()
            .run(None)
            .await
            .unwrap()
    });
    assert_eq!(result.status, "completed");
    assert_eq!(
        std::fs::read(embedded_root.join("answer.txt")).unwrap(),
        std::fs::read(root.join("answer.txt")).unwrap()
    );

    runtimes.shutdown(Duration::from_secs(3));
}
