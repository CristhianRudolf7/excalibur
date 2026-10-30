//! Explicit multi-project requests retain literal sources and separate task scopes.
use excalibur_daemon::{DaemonHost, HostOptions, RuntimeSet};
use excalibur_master::{FastActions, Incoming};
use excalibur_models::{ModelError, ModelGateway};
use excalibur_orchestrator::Project;
use excalibur_store::{Database, DatabaseKind};
use excalibur_types::runtime::*;
use excalibur_vault::Redactor;
use serde_json::json;
use std::{sync::Arc, time::Duration};
struct Offline;
impl ModelGateway for Offline {
    fn complete(&self, _: ModelRequest) -> BoxFuture<'_, Result<ModelResponse, ModelError>> {
        Box::pin(async { Err(ModelError::Provider) })
    }
}
#[test]
fn explicit_project_mentions_delegate_separate_tasks_and_record_only_authorized_cross_scope_grants()
{
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("projects");
    std::fs::create_dir(&root).unwrap();
    let mut options = HostOptions::project(root.clone());
    options.projects = vec![];
    for (id, name) in [("a", "Alpha"), ("b", "Beta"), ("c", "Gamma")] {
        let path = root.join(id);
        std::fs::create_dir(&path).unwrap();
        options.projects.push(Project {
            id: id.into(),
            name: name.into(),
            aliases: vec![],
            description: String::new(),
            roots: vec![path],
            status: "active".into(),
            vcs: None,
            settings: json!({}),
        });
    }
    let db = Database::open(
        temp.path().join("data/main.db"),
        DatabaseKind::Main,
        Default::default(),
    )
    .unwrap();
    let tasks_db = db.clone();
    let rt = RuntimeSet::new(2, 1).unwrap();
    let redactor = Redactor::default();
    let host = rt
        .block_on(DaemonHost::open(
            db,
            options,
            Arc::new(Offline),
            None,
            "synthetic-multi-project-request-token",
            redactor.clone(),
            rt.work(),
        ))
        .unwrap();
    rt.block_on(async{
 let literal="Update Alpha and Beta together. Keep both exact project names and source identifiers.";
 let receipt=host.master.receive(Incoming{conversation:"multi-project".into(),client_id:"fixture".into(),client_msg_id:"source".into(),text:literal.into(),scope:Some("general".into()),timezone:"UTC".into(),channel:"local".into(),reply_to:None,attachments:vec![]}).await.unwrap();
 host.master.stop_turn("multi-project").await.unwrap();
 let actions=host.control_actions();let tasks=excalibur_tasks::TaskStore::open(tasks_db).await.unwrap();
 let mut ids=vec![];
 for id in ["a","b"] {
  let scope=format!("project:{id}");let key=format!("delegate-{id}");let arguments=json!({"action":"spawn","scope":scope,"goal":format!("Update {id}"),"deliverable":"Verified project file"});
  let result=actions.invoke_scoped("multi-project","general","subagent",arguments.clone(),&key).await.unwrap();
  let replay=actions.invoke_scoped("multi-project","general","subagent",arguments,&key).await.unwrap();assert_eq!(result,replay);
  let agent=host.orchestrator.agent(result["agent_id"].as_str().unwrap()).await.unwrap();assert_eq!(agent.scope,scope);assert_eq!(agent.token.data_scopes,std::collections::BTreeSet::from(["general".to_owned(),scope.clone()]));
  let task=tasks.task(&scope,result["task_id"].as_str().unwrap()).await.unwrap();assert_eq!(task.request_card["original"][0]["msg_id"],receipt.message_id);assert_eq!(task.request_card["original"][0]["text"],literal);ids.push(task.id);
 }
 assert_ne!(ids[0],ids[1]);
 let denied=actions.invoke_scoped("multi-project","general","subagent",json!({"action":"spawn","scope":"project:c","goal":"Not authorized by the literal source","deliverable":"No effect"}),"unmentioned").await;assert!(denied.is_err());
 let grouped=actions.invoke_scoped("multi-project","general","task_db",json!({"ops":[{"op":"query","entity":"task","scope":"project:a","fields":["title"]},{"op":"query","entity":"task","scope":"project:b","fields":["title"]}]}),"multi-query").await.unwrap();assert_eq!(grouped["scopes"].as_array().unwrap().len(),2);assert_eq!(grouped["scopes"][0]["scope"],"project:a");assert_eq!(grouped["scopes"][1]["scope"],"project:b");
 let combined=actions.invoke_scoped("multi-project","general","subagent",json!({"action":"spawn","scope":"project:a","scopes":["project:a","project:b"],"goal":"Unify the configuration of the two explicitly mentioned projects","deliverable":"Both configurations verified"}),"cross-project").await.unwrap();
 let agent=host.orchestrator.agent(combined["agent_id"].as_str().unwrap()).await.unwrap();assert!(agent.token.data_scopes.contains("project:a"));assert!(agent.token.data_scopes.contains("project:b"));assert!(!agent.token.data_scopes.contains("project:c"));assert_eq!(agent.token.roots.len(),2);
 host.master.set_scope("multi-project","project:b").await.unwrap();
 let replay=actions.invoke_scoped("multi-project","general","subagent",json!({"action":"spawn","scope":"project:a","goal":"Update a","deliverable":"Verified project file"}),"delegate-a").await.unwrap();
 assert_eq!(tasks.task("project:a",replay["task_id"].as_str().unwrap()).await.unwrap().request_card["original"][0]["msg_id"],receipt.message_id);
 assert!(actions.invoke_scoped("multi-project","general","subagent",json!({"action":"spawn","scope":"project:b","goal":"Change the meaning of an existing operation"}),"delegate-a").await.is_err());
 let mut ambiguous=host.orchestrator.project("c").await.unwrap();ambiguous.name="Alpha".into();host.orchestrator.register_project(ambiguous).await.unwrap();
 assert!(actions.invoke_scoped("multi-project","general","subagent",json!({"action":"spawn","scope":"project:a","goal":"Ambiguous project"}),"ambiguous").await.is_err());
 host.master.receive(Incoming{conversation:"bare-id".into(),client_id:"fixture".into(),client_msg_id:"short-source".into(),text:"Create a file in a safe location".into(),scope:Some("general".into()),timezone:"UTC".into(),channel:"local".into(),reply_to:None,attachments:vec![]}).await.unwrap();
 host.master.stop_turn("bare-id").await.unwrap();
 assert!(actions.invoke_scoped("bare-id","general","subagent",json!({"action":"spawn","scope":"project:a","goal":"An article is not a project grant"}),"bare-id-denied").await.is_err());
 let path=root.join("not-provisioned");std::fs::create_dir(&path).unwrap();host.orchestrator.register_project(Project{id:"d".into(),name:"Delta".into(),aliases:vec![],description:String::new(),roots:vec![path],status:"active".into(),vcs:None,settings:json!({})}).await.unwrap();
 assert!(actions.execute("multi-project","project",Some("d"),"unprovisioned-selector").await.is_err());
 assert!(actions.invoke_scoped("multi-project","project:d","task_db",json!({"ops":[{"op":"query","entity":"task"}]}),"no-silent-scope-grant").await.is_err());
 redactor.register_secret(&excalibur_vault::SecretValue::new(b"source identifiers".to_vec()).unwrap());
 assert!(actions.invoke_scoped("multi-project","general","subagent",json!({"action":"spawn","scope":"project:b","goal":"Do not copy a newly registered secret"}),"late-secret").await.is_err());

 });
    rt.block_on(host.close()).unwrap();
    rt.shutdown(Duration::from_secs(3));
}
