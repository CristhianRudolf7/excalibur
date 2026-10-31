use excalibur_daemon::{DaemonActions, DaemonHost, HostOptions, RuntimeSet};
use excalibur_gateway::{RpcBackend, SharedBackend};
use excalibur_master::{FastActions, Incoming};
use excalibur_models::{ModelError, ModelGateway};
use excalibur_store::{Database, DatabaseKind};
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
fn delegation_persists_literal_request_card_and_criteria_once_in_the_frozen_scope() {
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
            "synthetic-token-for-request-card-tests",
            Redactor::default(),
            runtimes.work(),
        ))
        .unwrap();
    runtimes.block_on(async{
        let literal="Keep the exact accents: exportación CSV.\nDo not replace the source text.";
        let receipt=host.master.receive(Incoming{conversation:"request-conversation".into(),client_id:"fixture".into(),client_msg_id:"literal-request".into(),text:literal.into(),scope:Some("general".into()),timezone:"UTC".into(),channel:"test".into(),reply_to:None,attachments:vec![]}).await.unwrap();
        host.master.set_scope("request-conversation","project:default").await.unwrap();
        let tasks=TaskStore::open(db.clone()).await.unwrap();
        let master=host.orchestrator.list_agents().await.unwrap().into_iter().find(|agent|agent.depth==0).unwrap();
        let actions=Arc::new(DaemonActions::new(host.orchestrator.clone(),master.id).with_tasks(tasks.clone()));actions.bind_master(host.master.clone());
        let arguments=json!({"action":"spawn","name":"Lena","goal":"Export a CSV","deliverable":"A verified CSV export"});
        let result=actions.invoke_scoped("request-conversation","general","subagent",arguments.clone(),"request-operation").await.unwrap();
        let repeated=actions.invoke_scoped("request-conversation","general","subagent",arguments,"request-operation").await.unwrap();assert_eq!(result["agent_id"],repeated["agent_id"]);assert_eq!(result["task_id"],repeated["task_id"]);
        let task=tasks.task("general",result["task_id"].as_str().unwrap()).await.unwrap();
        assert_eq!(task.request_card["original"][0]["msg_id"],receipt.message_id);assert_eq!(task.request_card["original"][0]["text"],literal);assert_eq!(task.request_card["interpretation"],"Export a CSV");assert_eq!(task.origin_conversation_id.as_deref(),Some("request-conversation"));assert_eq!(task.root_run_id.as_deref(),result["run_id"].as_str());
        assert!(tasks.task("project:default",&task.id).await.is_err());
        let (requirements,criteria)=tasks.requirements_and_criteria(&task.id).await.unwrap();assert_eq!(requirements.len(),1);assert_eq!(criteria.len(),1);assert_eq!(criteria[0].statement,"A verified CSV export");
        let worker=host.orchestrator.agent(result["agent_id"].as_str().unwrap()).await.unwrap();assert_eq!(worker.scope,"general");assert_eq!(worker.task,task.id);assert!(worker.delegation.unwrap().inputs.contains(&requirements[0].id));
        assert!(actions.invoke_scoped("request-conversation","general","subagent",json!({"action":"spawn","goal":"Changed goal"}),"request-operation").await.is_err());
        let second=host.master.receive(Incoming{conversation:"request-conversation".into(),client_id:"fixture".into(),client_msg_id:"project-request".into(),text:"Independent project work".into(),scope:Some("project:default".into()),timezone:"UTC".into(),channel:"test".into(),reply_to:None,attachments:vec![]}).await.unwrap();
        assert_ne!(receipt.message_id,second.message_id);
        let other=actions.invoke_scoped("request-conversation","project:default","subagent",json!({"action":"spawn","goal":"Independent project work"}),"independent-operation").await.unwrap();
        assert_eq!(tasks.task("project:default",other["task_id"].as_str().unwrap()).await.unwrap().project_id.as_deref(),Some("default"));
        let other_id=other["agent_id"].as_str().unwrap();
        let worker_id=result["agent_id"].as_str().unwrap();
        let correction=json!({"ops":[{"op":"update_requirement","id":requirements[0].id,"expected_version":requirements[0].version,"data":{"text":"Export CSV rather than PDF"}}]});
        let rpc=SharedBackend::new(host.master.clone(),host.orchestrator.clone()).with_task_actions(actions.clone());
        rpc.dispatch("task.batch",json!({"scope":"general","operation":"correction-operation","batch":correction.clone()})).await.unwrap();
        let notices=host.orchestrator.mailbox(worker_id).await.unwrap();
        assert!(notices.iter().any(|message|message.kind=="directive"&&message.payload.to_string().contains("Export CSV rather than PDF")));
        assert!(host.orchestrator.mailbox(other_id).await.unwrap().is_empty());
        actions.invoke_scoped("request-conversation","general","task_db",correction,"correction-operation").await.unwrap();
        assert_eq!(host.orchestrator.mailbox(worker_id).await.unwrap().len(),notices.len());
        db.write(excalibur_store::Priority::Normal,|connection|{connection.execute("UPDATE daemon_task_directives SET state='pending' WHERE operation='correction-operation'",[])?;Ok(())}).await.unwrap();
        assert_eq!(actions.recover_task_directives().await.unwrap(),1);
        assert_eq!(host.orchestrator.mailbox(worker_id).await.unwrap().len(),notices.len());
        assert_eq!(tasks.requirements_and_criteria(&task.id).await.unwrap().0[0].text,"Export CSV rather than PDF");
        let parent=host.orchestrator.agent(worker_id).await.unwrap();
        let mut child_request=parent.delegation.clone().unwrap();child_request.operation="child-control-operation".into();child_request.name="Child worker".into();child_request.profile="implementer".into();child_request.token.budget=10;child_request.spec.budget=excalibur_engine::Budget{max_steps:20,max_tokens:20_000,max_wall_seconds:60,max_tool_calls:10,tool_timeout_seconds:60,..Default::default()};
        let child=host.orchestrator.spawn(worker_id,child_request).await.unwrap();
        // The inherited natively sized L0 must fit the child's own run budget, as native sizing requires.
        let child_context=host.orchestrator.engine().load(child.run_id.as_ref().unwrap()).await.unwrap().spec.context;assert_eq!(child_context.layers[0],parent.delegation.as_ref().unwrap().spec.context.layers[0]);assert!(child_context.token_budgets[0]<=20_000);
        actions.execute("request-conversation","stop",Some("all"),"stop-all").await.unwrap();
        assert_eq!(host.orchestrator.status(&child.id).await.unwrap().state,"paused");
        actions.execute("request-conversation","resume",Some(worker_id),"resume-one-tree").await.unwrap();
        assert_eq!(host.orchestrator.status(&child.id).await.unwrap().state,"ready");
        assert_eq!(host.orchestrator.status(other_id).await.unwrap().state,"paused");
        for message in host.orchestrator.mailbox(worker_id).await.unwrap(){host.orchestrator.ack_message(worker_id,&message.id).await.unwrap();}
        let engine=host.orchestrator.engine();let run=result["run_id"].as_str().unwrap();engine.journal().set_state_if(run,&["ready"],"running",None,"fixture-question-admission").await.unwrap();engine.wait(run,excalibur_engine::WaitReason::User).await.unwrap();
        let question=host.orchestrator.send_message("worker-question",worker_id,&actions.master_agent,"question",json!({"text":"Choose CSV or PDF"}),"normal").await.unwrap();
        let answer=json!({"reply_to":question.id,"text":"CSV"});
        actions.invoke_scoped("request-conversation","general","subagent",json!({"action":"answer","target":worker_id,"payload":answer}),"answer-operation").await.unwrap();
        host.orchestrator.deliver_mailbox(worker_id).await.unwrap();assert_eq!(engine.load(run).await.unwrap().status,excalibur_engine::Status::Running);
        assert!(host.orchestrator.mailbox(worker_id).await.unwrap().iter().any(|message|message.kind=="answer"&&message.payload==answer));
        let frozen_question=actions.invoke_scoped("request-conversation","general","question",json!({"text":"Clarify the original general request","blocking":true}),"frozen-question").await.unwrap();
        let question_id=frozen_question["question_id"].as_str().unwrap().to_owned();
        let question_scope=db.read(excalibur_store::ReadLane::Interactive,move|connection|Ok(connection.query_row("SELECT scope_id FROM master_questions WHERE id=?1",[question_id],|row|row.get::<_,String>(0))?)).await.unwrap();assert_eq!(question_scope,"general");
        let long_key="k".repeat(200);
        let long_operation=actions.invoke_scoped("request-conversation","general","subagent",json!({"action":"spawn","goal":"A compact durable delegation key"}),&long_key).await.unwrap();
        assert!(long_operation["task_id"].as_str().unwrap().starts_with("tsk_"));
        actions.detach();
    });
    runtimes.block_on(host.close()).unwrap();
    runtimes.shutdown(std::time::Duration::from_secs(3));
}
