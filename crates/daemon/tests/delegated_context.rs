//! A memory-enabled native coordinator delegates through the model-callable
//! `subagent` tool. The child's L0 core must be sized by the same native path
//! as its parent, within its own run budget, so the spawn reaches the model.
use excalibur_daemon::{DaemonHost, HostOptions, MemoryOptions, RuntimeSet};
use excalibur_master::{FastActions, Incoming};
use excalibur_models::{ModelError, ModelGateway};
use excalibur_store::{Database, DatabaseKind};
use excalibur_types::runtime::*;
use excalibur_vault::Redactor;
use serde_json::json;
use std::sync::{Arc, Mutex};

const PARENT_GOAL: &str = "Inspect the current task";
const CHILD_GOAL: &str = "Read the synthetic inputs for the coordinator";

#[derive(Default)]
struct Scripted {
    spawn_result: Mutex<Option<String>>,
    child_request: Mutex<Option<ModelRequest>>,
    spawned: tokio::sync::Notify,
    child_ready: tokio::sync::Notify,
}
fn task_layer(request: &ModelRequest) -> &str {
    request
        .messages
        .iter()
        .find(|message| message.content.starts_with("[L3]\n"))
        .map(|message| message.content.as_str())
        .unwrap_or("")
}
impl ModelGateway for Scripted {
    fn complete(&self, request: ModelRequest) -> BoxFuture<'_, Result<ModelResponse, ModelError>> {
        Box::pin(async move {
            if task_layer(&request).contains(CHILD_GOAL) {
                *self.child_request.lock().unwrap() = Some(request);
                self.child_ready.notify_one();
                return std::future::pending().await;
            }
            if let Some(result) = request
                .messages
                .iter()
                .find(|message| message.role == "tool")
            {
                *self.spawn_result.lock().unwrap() = Some(result.content.clone());
                self.spawned.notify_one();
                return std::future::pending().await;
            }
            Ok(ModelResponse {
                message: Message {
                    role: "assistant".into(),
                    content: String::new(),
                    tool_calls: vec![ToolCall {
                        id: "delegate-child".into(),
                        name: "subagent".into(),
                        arguments: json!({"action":"spawn","delegation":{"name":"Reader","profile":"explorer","goal":CHILD_GOAL,"deliverable":"A bounded summary"}}).to_string(),
                    }],
                    tool_call_id: None,
                    provider_state: None,
                },
                usage: Usage::default(),
            })
        })
    }
}

#[test]
fn native_coordinator_spawns_a_tool_child_with_a_natively_sized_core() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("project");
    std::fs::create_dir(&root).unwrap();
    let db = Database::open(
        temp.path().join("main.db"),
        DatabaseKind::Main,
        Default::default(),
    )
    .unwrap();
    let runtimes = RuntimeSet::new(2, 1).unwrap();
    let model = Arc::new(Scripted::default());
    let mut options = HostOptions::project(root);
    options.memory = Some(MemoryOptions {
        maintenance_interval: None,
        ..Default::default()
    });
    let host = runtimes
        .block_on(DaemonHost::open(
            db,
            options,
            model.clone(),
            None,
            "synthetic-delegated-context-token",
            Redactor::default(),
            runtimes.work(),
        ))
        .unwrap();
    runtimes.block_on(async {
        host.master.receive(Incoming{conversation:"chat".into(),client_id:"fixture".into(),client_msg_id:"source".into(),text:"/status".into(),scope:Some("project:default".into()),timezone:"UTC".into(),channel:"test".into(),reply_to:None,attachments:vec![]}).await.unwrap();
        let value = host
            .control_actions()
            .invoke_scoped(
                "chat",
                "project:default",
                "subagent",
                json!({"action":"spawn","name":"Native coordinator","goal":PARENT_GOAL,"deliverable":"A concise report"}),
                "delegated-context-spawn",
            )
            .await
            .unwrap();
        let parent_id = value["agent_id"].as_str().unwrap().to_owned();
        let parent = host.orchestrator.agent(&parent_id).await.unwrap();
        let parent_context = host
            .orchestrator
            .engine()
            .load(parent.run_id.as_ref().unwrap())
            .await
            .unwrap()
            .spec
            .context;
        // Precondition of the finding: the native coordinator core exceeds the
        // generic 4096-token default that ContextInput::new assigns.
        assert!(parent_context.token_budgets[0] > 4096);

        host.orchestrator.tick().await.unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(20), model.spawned.notified())
            .await
            .expect("coordinator never received the subagent tool result");
        let result = model.spawn_result.lock().unwrap().clone().unwrap();
        let output: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(output["success"], true, "subagent spawn failed: {result}");
        let child_id = output["output"]["id"]
            .as_str()
            .unwrap_or_else(|| panic!("subagent spawn failed: {result}"))
            .to_owned();
        let child = host.orchestrator.agent(&child_id).await.unwrap();
        assert_eq!(child.parent_id.as_deref(), Some(parent_id.as_str()));
        let child_context = host
            .orchestrator
            .engine()
            .load(child.run_id.as_ref().unwrap())
            .await
            .unwrap()
            .spec
            .context;
        // Same core text and same native sizing as the parent, never the default.
        assert_eq!(child_context.layers[0], parent_context.layers[0]);
        assert_eq!(child_context.token_budgets[0], parent_context.token_budgets[0]);
        assert!(child_context.token_budgets[0] as u64 <= child.budget.max_tokens);
        assert_eq!(
            &child_context.token_budgets[1..],
            &excalibur_context::ContextInput::new("", "", "").token_budgets[1..]
        );

        host.orchestrator.tick().await.unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(20), model.child_ready.notified())
            .await
            .expect("tool-created child never reached the model");
        assert!(model.child_request.lock().unwrap().is_some());
        host.orchestrator
            .control(&child_id, excalibur_orchestrator::Control::Pause)
            .await
            .unwrap();
        host.orchestrator
            .control(&parent_id, excalibur_orchestrator::Control::Pause)
            .await
            .unwrap();
    });
    runtimes.block_on(host.close()).unwrap();
    runtimes.shutdown(std::time::Duration::from_secs(3));
}
