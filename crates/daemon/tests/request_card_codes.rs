use excalibur_daemon::{DaemonActions, DaemonHost, HostOptions, RuntimeSet};
use excalibur_master::{FastActions, Incoming};
use excalibur_models::{ModelError, ModelGateway};
use excalibur_store::{Database, DatabaseKind, ReadLane};
use excalibur_tasks::TaskStore;
use excalibur_types::runtime::*;
use excalibur_vault::Redactor;
use serde_json::json;
use std::sync::Arc;
struct Model;
impl ModelGateway for Model {
    fn complete(&self, _: ModelRequest) -> BoxFuture<'_, Result<ModelResponse, ModelError>> {
        Box::pin(async {
            Ok(ModelResponse {
                message: Message::text("assistant", "Ready."),
                usage: Usage::default(),
            })
        })
    }
}
#[test]
fn successive_project_cards_use_transaction_assigned_codes_and_preserve_literal_pins_and_replay() {
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
            db.clone(),
            HostOptions::project(root),
            Arc::new(Model),
            None,
            "synthetic-token-for-request-card-code-tests",
            Redactor::default(),
            runtimes.work(),
        ))
        .unwrap();
    runtimes.block_on(async {
   let tasks=TaskStore::open(db.clone()).await.unwrap();let master=host.orchestrator.list_agents().await.unwrap().into_iter().find(|a|a.depth==0).unwrap();let actions=DaemonActions::new(host.orchestrator.clone(),master.id).with_tasks(tasks.clone());actions.bind_master(host.master.clone());
   let mut codes=Vec::new();
   for index in 0..2 {
     let goal=format!("Create synthetic task {index}");let literal=format!("Do not use external services.\n{goal}");
     let receipt=host.master.receive(Incoming{conversation:"code-project".into(),client_id:"fixture".into(),client_msg_id:format!("source-{index}"),text:literal.clone(),scope:Some("project:default".into()),timezone:"UTC".into(),channel:"test".into(),reply_to:None,attachments:vec![]}).await.unwrap();
     let args=json!({"action":"spawn","name":format!("Worker{index}"),"goal":goal,"deliverable":"Verified synthetic output"});let key=format!("code-operation-{index}");
     let result=actions.invoke_source_scoped("code-project","project:default",&receipt.message_id,"subagent",args.clone(),&key).await.unwrap();
     let repeated=actions.invoke_source_scoped("code-project","project:default",&receipt.message_id,"subagent",args,&key).await.unwrap();assert_eq!(result,repeated);
     let id=result["task_id"].as_str().unwrap();let task=tasks.task("project:default",id).await.unwrap();
     let (requirements,criteria)=tasks.requirements_and_criteria(id).await.unwrap();assert_eq!(requirements.len(),1);assert_eq!(criteria.len(),1);
     let requirement=requirements[0].id.clone();let criterion=criteria[0].id.clone();
     let (r,c)=db.read(ReadLane::Interactive,move|conn|Ok((conn.query_row("SELECT code FROM requirements WHERE id=?1",[requirement],|r|r.get::<_,String>(0))?,conn.query_row("SELECT code FROM criteria WHERE id=?1",[criterion],|r|r.get::<_,String>(0))?))).await.unwrap();
     assert_eq!(task.request_card["requirements"][0]["code"],r);assert_eq!(task.request_card["criteria"][0]["code"],c);assert_eq!(task.request_card["criteria"][0]["requirement"],r);
     assert_eq!(task.request_card["original"][0]["text"],literal);assert!(task.request_card["pinned_instructions"].as_array().unwrap().iter().any(|v|v.as_str().is_some_and(|s|s.contains("Do not use external services"))));
     codes.push((r,c));
   }
   assert_ne!(codes[0],codes[1]);assert_eq!(codes[0],("R1".into(),"C1.1".into()));assert_eq!(codes[1],("R2".into(),"C2.1".into()));
   actions.detach();
 });
    runtimes.block_on(host.close()).unwrap();
    runtimes.shutdown(std::time::Duration::from_secs(3));
}
