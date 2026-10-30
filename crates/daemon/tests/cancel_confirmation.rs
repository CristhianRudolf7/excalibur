//! Real authenticated gateway -> master intake -> orchestrator, without providers.
use excalibur_daemon::{DaemonHost, HostOptions, RuntimeSet};
use excalibur_engine::{Budget, Criterion, RunSpec, Status};
use excalibur_master::FastActions;
use excalibur_models::{ModelError, ModelGateway};
use excalibur_orchestrator::Delegation;
use excalibur_store::{Database, DatabaseKind, Priority, ReadLane};
use excalibur_types::runtime::*;
use excalibur_vault::Redactor;
use serde_json::{Value, json};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
const TOKEN: &str = "synthetic-local-cancel-confirmation-token";
struct NoModels(AtomicUsize);
impl ModelGateway for NoModels {
    fn complete(&self, _: ModelRequest) -> BoxFuture<'_, Result<ModelResponse, ModelError>> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Err(ModelError::Provider) })
    }
}
async fn serve(host: &DaemonHost) -> (String, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/rpc", listener.local_addr().unwrap());
    let router = host.gateway.clone().router();
    (
        url,
        tokio::spawn(async move { axum::serve(listener, router).await.unwrap() }),
    )
}
async fn send(url: &str, id: &str, text: &str, reply: Option<&str>) -> Value {
    let response=reqwest::Client::new().post(url).bearer_auth(TOKEN).json(&json!({"jsonrpc":"2.0","id":1,"method":"conversation.send","params":{"conversation":"cancel-origin","client_id":"fixture","client_msg_id":id,"text":text,"scope":"project:default","reply_to":reply}})).send().await.unwrap();
    assert!(response.status().is_success());
    let body: Value = response.json().await.unwrap();
    assert!(body.get("error").is_none(), "{body}");
    body
}
async fn worker(host: &DaemonHost, name: &str) -> excalibur_orchestrator::Agent {
    let parent = host
        .orchestrator
        .list_agents()
        .await
        .unwrap()
        .into_iter()
        .find(|a| a.depth == 0)
        .unwrap();
    host.orchestrator
        .spawn(
            &parent.id,
            Delegation {
                operation: format!("spawn-{name}"),
                name: name.into(),
                profile: "coordinator".into(),
                scope: "project:default".into(),
                task: excalibur_types::TaskId::new().to_string(),
                origin_conversation: "cancel-origin".into(),
                goal: "Wait for cancellation".into(),
                deliverable: "Synthetic report".into(),
                inputs: vec![],
                pinned_instructions: vec![],
                suggested_skills: vec![],
                fork_context: false,
                token: parent.token,
                spec: RunSpec {
                    context: excalibur_context::ContextInput::new("rules", "task", "fixed"),
                    budget: Budget::default(),
                    criteria: vec![Criterion::FinalResponse { schema: None }],
                },
                priority: 1,
            },
        )
        .await
        .unwrap()
}
async fn controls(db: &Database) -> u64 {
    db.read(ReadLane::Interactive,|c|Ok(c.query_row("SELECT COUNT(*) FROM orchestration_controls WHERE operation LIKE 'cancel-confirmation:%'",[],|r|r.get(0))?)).await.unwrap()
}
#[test]
fn authenticated_confirmation_survives_restart_and_cancels_once_with_full_inbox() {
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
    let model = Arc::new(NoModels(AtomicUsize::new(0)));
    let host = runtimes
        .block_on(DaemonHost::open(
            db.clone(),
            HostOptions::project(root.clone()),
            model.clone(),
            None,
            TOKEN,
            Redactor::default(),
            runtimes.work(),
        ))
        .unwrap();
    let (agent,other,question)=runtimes.block_on(async {
        let (url,server)=serve(&host).await;
        // Authentication must precede intake, even on localhost.
        assert_eq!(reqwest::Client::new().post(&url).json(&json!({"jsonrpc":"2.0","id":1,"method":"conversation.send","params":{}})).send().await.unwrap().status(),reqwest::StatusCode::UNAUTHORIZED);
        send(&url,"init","/status",None).await;
        let agent=worker(&host,"First").await;let other=worker(&host,"Second").await;
        send(&url,"ambiguous","/cancel",None).await;
        assert!(host.master.questions("cancel-origin").await.unwrap().is_empty());
        let cmd=format!("/cancel {}",agent.id);
        send(&url,"cancel-first",&cmd,None).await;
        send(&url,"cancel-first-repeat",&cmd,None).await;
        let questions=host.master.questions("cancel-origin").await.unwrap();assert_eq!(questions.len(),1);let question=questions[0].id.clone();
        assert!(host.master.answer_question("cancel-origin",&question,"yes").await.is_err());
        assert!(host.control_actions().confirm_cancel("cancel-origin","project:default",&agent.id,"forged-confirmation").await.is_err());
        // Model-produced confirmed=true is never an authorization.
        assert!(host.control_actions().invoke_scoped("cancel-origin","project:default","subagent",json!({"action":"cancel","target":agent.id,"confirmed":true}),"generated-cancel").await.is_err());
        send(&url,"wrong","yes",Some("not-this-question")).await;
        assert_eq!(controls(&db).await,0);
        // Fill ordinary intake with generated unavailable work; it carries no client receipts.
        db.write(Priority::Interactive,|c| {
            for i in 0..128 {
                let id=format!("generated-{i}");
                c.execute("INSERT INTO messages(id,conversation_id,role,text,scope_id,principal_id) SELECT ?1,id,'user','yes','project:default',principal_id FROM conversations WHERE id='cancel-origin'",[&id])?;
                c.execute("INSERT INTO master_inbox(message_id,conversation_id,preanalysis,state,created_seq) VALUES(?1,'cancel-origin','{}','unavailable',0)",[id])?;
            }Ok(())
        }).await.unwrap();
        assert_eq!(controls(&db).await,0);
        assert_ne!(host.orchestrator.engine().load(agent.run_id.as_ref().unwrap()).await.unwrap().status,Status::Cancelled);
        server.abort();(agent,other,question)
    });
    runtimes.block_on(host.close()).unwrap();
    let db = Database::open(
        temp.path().join("main.db"),
        DatabaseKind::Main,
        Default::default(),
    )
    .unwrap();
    let host = runtimes
        .block_on(DaemonHost::open(
            db.clone(),
            HostOptions::project(root.clone()),
            model.clone(),
            None,
            TOKEN,
            Redactor::default(),
            runtimes.work(),
        ))
        .unwrap();
    runtimes.block_on(async {
        let (url, server) = serve(&host).await;
        let calls = model.0.load(Ordering::SeqCst);
        send(&url, "affirm", "sí", Some(&question)).await;
        assert_eq!(
            host.orchestrator
                .engine()
                .load(agent.run_id.as_ref().unwrap())
                .await
                .unwrap()
                .status,
            Status::Cancelled
        );
        assert_ne!(
            host.orchestrator
                .engine()
                .load(other.run_id.as_ref().unwrap())
                .await
                .unwrap()
                .status,
            Status::Cancelled
        );
        send(&url, "affirm", "sí", Some(&question)).await;
        send(&url, "affirm-again", "yes", Some(&question)).await;
        assert_eq!(controls(&db).await, 1);
        assert_eq!(
            host.master
                .events(0, 1000)
                .await
                .unwrap()
                .iter()
                .filter(|e| e.kind == "cancellation_completed")
                .count(),
            1
        );
        assert_eq!(model.0.load(Ordering::SeqCst), calls);
        let cmd = format!("/cancel {}", other.id);
        send(&url, "cancel-second", &cmd, None).await;
        let question = host
            .master
            .questions("cancel-origin")
            .await
            .unwrap()
            .into_iter()
            .find(|q| q.answer.is_none())
            .unwrap();
        send(&url, "reject", "no", Some(&question.id)).await;
        send(&url, "late-yes", "yes", Some(&question.id)).await;
        assert_eq!(controls(&db).await, 1);
        assert_ne!(
            host.orchestrator
                .engine()
                .load(other.run_id.as_ref().unwrap())
                .await
                .unwrap()
                .status,
            Status::Cancelled
        );
        server.abort();
    });
    runtimes.block_on(host.close()).unwrap();
    runtimes.shutdown(std::time::Duration::from_secs(3));
}
