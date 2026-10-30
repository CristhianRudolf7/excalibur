//! Both host-scoped message history and bounded current-run history stay callable.
use excalibur_context::ContextInput;
use excalibur_daemon::{DaemonHost, HostOptions, MemoryOptions, RuntimeSet};
use excalibur_engine::{Budget, Criterion, RunSpec};
use excalibur_models::{ModelError, ModelGateway};
use excalibur_orchestrator::Delegation;
use excalibur_store::{Database, DatabaseKind, Priority};
use excalibur_types::runtime::*;
use excalibur_vault::Redactor;
use serde_json::{Value, json};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
struct Model(AtomicUsize);
impl ModelGateway for Model {
    fn complete(&self, request: ModelRequest) -> BoxFuture<'_, Result<ModelResponse, ModelError>> {
        Box::pin(async move {
            let step = self.0.fetch_add(1, Ordering::SeqCst);
            let mut names = std::collections::BTreeSet::new();
            assert!(
                request
                    .tools
                    .iter()
                    .all(|tool| names.insert(tool.name.clone()))
            );
            let mut message = Message::text("assistant", "Both histories are available.");
            match step {
                0 => message.tool_calls.push(ToolCall {
                    id: "memory".into(),
                    name: "history_search".into(),
                    arguments: json!({"text":"MEMORY ONLY WITNESS","include":"messages"})
                        .to_string(),
                }),
                1 => {
                    let result = request
                        .messages
                        .iter()
                        .find(|m| m.tool_call_id.as_deref() == Some("memory"))
                        .unwrap();
                    let parsed: Value = serde_json::from_str(&result.content).unwrap();
                    assert_eq!(parsed["success"], true);
                    assert!(result.content.contains("MEMORY ONLY WITNESS"));
                    message.tool_calls.push(ToolCall {
                        id: "episode".into(),
                        name: "run_history_search".into(),
                        arguments: json!({"limit":16}).to_string(),
                    });
                }
                _ => {
                    let result = request
                        .messages
                        .iter()
                        .find(|m| m.tool_call_id.as_deref() == Some("episode"))
                        .unwrap();
                    let parsed: Value = serde_json::from_str(&result.content).unwrap();
                    assert_eq!(parsed["success"], true);
                    assert_eq!(parsed["output"]["scope"], "current_run");
                }
            }
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
fn memory_and_journal_history_have_distinct_schemas_and_dispatchers() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("project");
    std::fs::create_dir(&root).unwrap();
    let db = Database::open(
        dir.path().join("main.db"),
        DatabaseKind::Main,
        Default::default(),
    )
    .unwrap();
    let runtimes = RuntimeSet::new(2, 1).unwrap();
    let mut options = HostOptions::project(root);
    options.memory = Some(MemoryOptions {
        maintenance_interval: None,
        ..Default::default()
    });
    let model = Arc::new(Model(AtomicUsize::new(0)));
    let host = runtimes
        .block_on(DaemonHost::open(
            db.clone(),
            options,
            model.clone(),
            None,
            "synthetic-memory-history-token-000000",
            Redactor::default(),
            runtimes.work(),
        ))
        .unwrap();
    runtimes.block_on(async {
        db.write(Priority::Journal,|c|{
            c.execute("INSERT OR IGNORE INTO principals(id) VALUES('owner')",[])?;
            c.execute("INSERT INTO conversations(id,channel,principal_id,scope_id,active_scope) VALUES('chat','test','owner','project:default','project:default')",[])?;
            c.execute("INSERT INTO messages(id,conversation_id,role,text,principal_id,scope_id) VALUES('seed','chat','user','MEMORY ONLY WITNESS','owner','project:default')",[])?;Ok(())
        }).await.unwrap();
        let parent=host.orchestrator.list_agents().await.unwrap().into_iter().find(|a|a.depth==0).unwrap();
        let mut context=ContextInput::new("rules","Search both histories","now");context.token_budgets[0]=16000;
        let agent=host.orchestrator.spawn(&parent.id,Delegation{operation:"history-pair".into(),name:"Coordinator".into(),profile:"coordinator".into(),scope:"project:default".into(),task:excalibur_types::TaskId::new().to_string(),origin_conversation:"chat".into(),goal:"Search both histories".into(),deliverable:"report".into(),inputs:vec![],pinned_instructions:vec![],suggested_skills:vec![],fork_context:false,token:parent.token,spec:RunSpec{context,budget:Budget::default(),criteria:vec![Criterion::FinalResponse{schema:None}]},priority:1}).await.unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(30),async{loop {
            host.orchestrator.tick().await.unwrap();let status=host.orchestrator.status(&agent.id).await.unwrap();
            if status.state=="completed"{break;}assert!(!matches!(status.state.as_str(),"failed"|"blocked"),"{status:?}");
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }}).await.unwrap();assert!(model.0.load(Ordering::SeqCst)>=3);
    });
    runtimes.block_on(host.close()).unwrap();
    drop(db);
    let reopened = Database::open(
        dir.path().join("main.db"),
        DatabaseKind::Main,
        Default::default(),
    )
    .unwrap();
    runtimes.block_on(reopened.shutdown()).unwrap();
    runtimes.shutdown(std::time::Duration::from_secs(3));
}
