//! The production master delegation route sizes its actual composed core catalog.
use excalibur_daemon::{DaemonHost, HostOptions, MemoryOptions, RuntimeSet};
use excalibur_master::{FastActions, Incoming};
use excalibur_models::{ModelError, ModelGateway};
use excalibur_store::{Database, DatabaseKind};
use excalibur_types::runtime::*;
use excalibur_vault::Redactor;
use serde_json::json;
use std::sync::{Arc, Mutex};
#[derive(Default)]
struct Observed {
    request: Mutex<Option<ModelRequest>>,
    ready: tokio::sync::Notify,
}
impl ModelGateway for Observed {
    fn complete(&self, request: ModelRequest) -> BoxFuture<'_, Result<ModelResponse, ModelError>> {
        Box::pin(async move {
            *self.request.lock().unwrap() = Some(request);
            self.ready.notify_one();
            std::future::pending().await
        })
    }
}
#[test]
fn native_memory_coordinator_reaches_the_model_without_relaxing_caller_caps() {
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
    let model = Arc::new(Observed::default());
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
            "synthetic-native-context-budget-token",
            Redactor::default(),
            runtimes.work(),
        ))
        .unwrap();
    runtimes.block_on(async {
        host.master.receive(Incoming{conversation:"chat".into(),client_id:"fixture".into(),client_msg_id:"source".into(),text:"/status".into(),scope:Some("project:default".into()),timezone:"UTC".into(),channel:"test".into(),reply_to:None,attachments:vec![]}).await.unwrap();
        let value=host.control_actions().invoke_scoped("chat","project:default","subagent",json!({"action":"spawn","name":"Native coordinator","goal":"Inspect the current task","deliverable":"A concise report"}),"native-budget-spawn").await.unwrap();
        let id=value["id"].as_str().unwrap();let agent=host.orchestrator.agent(id).await.unwrap();let run=agent.run_id.as_ref().unwrap();
        let snapshot=host.orchestrator.engine().load(run).await.unwrap();
        assert!(snapshot.spec.context.token_budgets[0]>4096);
        assert_eq!(&snapshot.spec.context.token_budgets[1..],&excalibur_context::ContextInput::new("","","").token_budgets[1..]);
        host.orchestrator.tick().await.unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(10),model.ready.notified()).await.unwrap();
        {
            let guard=model.request.lock().unwrap();let request=guard.as_ref().unwrap();
            let history=request.tools.iter().find(|tool|tool.name=="history_search").unwrap();
            assert!(history.input_schema["properties"].get("text").is_some());
            assert!(request.tools.iter().any(|tool|tool.name=="run_history_search"));
        }
        host.orchestrator.control(id,excalibur_orchestrator::Control::Pause).await.unwrap();
    });
    runtimes.block_on(host.close()).unwrap();
    runtimes.shutdown(std::time::Duration::from_secs(3));
}
