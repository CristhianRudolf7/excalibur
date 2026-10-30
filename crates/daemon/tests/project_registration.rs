//! Live registration cannot silently change already provisioned process boundaries.
use excalibur_daemon::{DaemonHost, HostOptions, RuntimeSet};
use excalibur_models::{ModelError, ModelGateway};
use excalibur_orchestrator::Project;
use excalibur_store::{Database, DatabaseKind, ReadLane};
use excalibur_types::runtime::*;
use excalibur_vault::Redactor;
use serde_json::{Value, json};
use std::{
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};
struct NoModels(AtomicUsize);
impl ModelGateway for NoModels {
    fn complete(&self, _: ModelRequest) -> BoxFuture<'_, Result<ModelResponse, ModelError>> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Box::pin(async {
            Err(ModelError::Configuration(
                "project registration must not call a model".into(),
            ))
        })
    }
}
fn project(id: &str, root: PathBuf) -> Project {
    Project {
        id: id.into(),
        name: id.into(),
        aliases: vec![],
        description: "Host-provisioned project".into(),
        roots: vec![root],
        status: "active".into(),
        vcs: None,
        settings: json!({}),
    }
}
const TOKEN: &str = "synthetic-token-for-fixed-project-roots";
async fn rpc(
    client: &reqwest::Client,
    address: std::net::SocketAddr,
    method: &str,
    params: Value,
) -> Value {
    let response = client
        .post(format!("http://{address}/rpc"))
        .bearer_auth(TOKEN)
        .json(&json!({"jsonrpc":"2.0","id":1,"method":method,"params":params}))
        .send()
        .await
        .unwrap();
    assert!(response.status().is_success());
    response.json().await.unwrap()
}
#[test]
fn authenticated_registration_preserves_fixed_roots_scope_authority_and_runtime_mapping() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("work");
    let a = root.join("a");
    let b = root.join("b");
    let nested = a.join("nested");
    let fresh = root.join("fresh");
    for path in [&a, &b, &nested, &fresh] {
        std::fs::create_dir_all(path).unwrap();
    }
    let db = Database::open(
        temp.path().join("data/main.db"),
        DatabaseKind::Main,
        Default::default(),
    )
    .unwrap();
    let runtimes = RuntimeSet::new(2, 1).unwrap();
    let mut options = HostOptions::project(root.clone());
    options.projects = vec![project("a", a.clone()), project("b", b.clone())];
    let model = Arc::new(NoModels(AtomicUsize::new(0)));
    let host = runtimes
        .block_on(DaemonHost::open(
            db.clone(),
            options,
            model.clone(),
            None,
            TOKEN,
            Redactor::default(),
            runtimes.work(),
        ))
        .unwrap();
    runtimes.block_on(async{
        let listener=tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();let address=listener.local_addr().unwrap();let router=host.gateway.clone().router();let server=tokio::spawn(async move{axum::serve(listener,router).await.unwrap();});let client=reqwest::Client::new();
        for (id,path) in [("fresh",fresh),("nested",nested)]{
            let denied=rpc(&client,address,"project.register",serde_json::to_value(project(id,path)).unwrap()).await;assert_eq!(denied["error"]["code"],-32602,"{denied}");assert!(host.orchestrator.project(id).await.is_err());
            let id=id.to_owned();let persisted=db.read(ReadLane::Interactive,move|connection|Ok(connection.query_row("SELECT count(*) FROM projects WHERE id=?1",[id],|row|row.get::<_,u64>(0))?)).await.unwrap();assert_eq!(persisted,0);
        }
        let remapped=rpc(&client,address,"project.register",serde_json::to_value(project("a",b.clone())).unwrap()).await;assert_eq!(remapped["error"]["code"],-32602);assert_eq!(host.orchestrator.project("a").await.unwrap().roots,vec![a.canonicalize().unwrap()]);
        let mut metadata=project("a",a.clone());metadata.name="Updated name".into();metadata.aliases=vec!["known-alias".into()];metadata.description="Updated description".into();metadata.settings=json!({"build_command":"declared metadata"});assert_eq!(rpc(&client,address,"project.register",serde_json::to_value(metadata).unwrap()).await["result"]["registered"],true);assert_eq!(host.orchestrator.project("a").await.unwrap().name,"Updated name");
        assert_eq!(rpc(&client,address,"project.register",serde_json::to_value(project("metadata-alias",a.clone())).unwrap()).await["result"]["registered"],true);
        let received=rpc(&client,address,"conversation.send",json!({"conversation":"fixed-project-conversation","client_id":"fixture","client_msg_id":"initial-status","text":"/status","scope":"project:a"})).await;assert!(received.get("result").is_some(),"{received}");
        let rejected=rpc(&client,address,"conversation.scope",json!({"conversation":"fixed-project-conversation","scope":"project:metadata-alias"})).await;assert_eq!(rejected["error"]["code"],-32602);assert_eq!(host.master.conversation_scope("fixed-project-conversation").await.unwrap(),"project:a");
        let rejected=rpc(&client,address,"conversation.send",json!({"conversation":"unprovisioned-conversation","client_id":"fixture","client_msg_id":"unprovisioned-status","text":"/status","scope":"project:metadata-alias"})).await;assert_eq!(rejected["error"]["code"],-32602);let count=db.read(ReadLane::Interactive,|connection|Ok(connection.query_row("SELECT count(*) FROM conversations WHERE id='unprovisioned-conversation'",[],|row|row.get::<_,u64>(0))?)).await.unwrap();assert_eq!(count,0);
        host.master.set_scope("fixed-project-conversation","project:metadata-alias").await.unwrap();
        let before=db.read(ReadLane::Interactive,|connection|Ok(connection.query_row("SELECT count(*) FROM messages WHERE conversation_id='fixed-project-conversation'",[],|row|row.get::<_,u64>(0))?)).await.unwrap();
        let implicit=rpc(&client,address,"conversation.send",json!({"conversation":"fixed-project-conversation","client_id":"fixture","client_msg_id":"implicit-unprovisioned","text":"Must not be persisted"})).await;assert_eq!(implicit["error"]["code"],-32602);
        let after=db.read(ReadLane::Interactive,|connection|Ok(connection.query_row("SELECT count(*) FROM messages WHERE conversation_id='fixed-project-conversation'",[],|row|row.get::<_,u64>(0))?)).await.unwrap();assert_eq!(before,after);host.master.set_scope("fixed-project-conversation","project:a").await.unwrap();
        host.orchestrator.register_project(project("trusted-metadata",a)).await.unwrap();
        for scope in ["project:metadata-alias","project:trusted-metadata"]{let result=rpc(&client,address,"task.batch",json!({"scope":scope,"operation":format!("query-{scope}"),"batch":{"ops":[{"op":"query","entity":"task"}]}})).await;assert_eq!(result["error"]["code"],-32602,"{result}");}
        let master=host.orchestrator.active_agents().await.unwrap().into_iter().find(|agent|agent.depth==0).unwrap();assert!(!master.token.data_scopes.contains("project:metadata-alias"));assert!(!master.token.data_scopes.contains("project:trusted-metadata"));assert_eq!(model.0.load(Ordering::SeqCst),0);server.abort();
    });
    runtimes.block_on(host.close()).unwrap();
    runtimes.shutdown(std::time::Duration::from_secs(3));
}
