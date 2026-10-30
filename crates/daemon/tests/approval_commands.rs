//! Authenticated slash-command approvals use the same durable decision operation as the API.
use excalibur_daemon::{DaemonHost, HostOptions, RuntimeSet};
use excalibur_engine::{Budget, Criterion, RunSpec, Status, WaitReason};
use excalibur_master::Incoming;
use excalibur_models::{ModelError, ModelGateway};
use excalibur_orchestrator::Delegation;
use excalibur_security::{Autonomy, CapabilityToken, RunSecurity, Security};
use excalibur_store::{Database, DatabaseKind, ReadLane};
use excalibur_types::runtime::*;
use excalibur_vault::Redactor;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
struct NoModels(AtomicUsize);
impl ModelGateway for NoModels {
    fn complete(&self, _: ModelRequest) -> BoxFuture<'_, Result<ModelResponse, ModelError>> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Box::pin(async {
            Err(ModelError::Configuration(
                "models must not be used for explicit approval commands".into(),
            ))
        })
    }
}
fn incoming(conversation: &str, id: &str, text: &str) -> Incoming {
    Incoming {
        conversation: conversation.into(),
        client_id: "approval-fixture".into(),
        client_msg_id: id.into(),
        text: text.into(),
        scope: Some("project:default".into()),
        timezone: "UTC".into(),
        channel: "local-test".into(),
        reply_to: None,
        attachments: vec![],
    }
}
async fn status(db: &Database, id: &str) -> String {
    let id = id.to_owned();
    db.read(ReadLane::Interactive, move |connection| {
        Ok(connection.query_row(
            "SELECT status FROM security_approvals WHERE id=?1",
            [id],
            |row| row.get(0),
        )?)
    })
    .await
    .unwrap()
}
async fn decisions(db: &Database) -> u64 {
    db.read(ReadLane::Interactive,|connection|Ok(connection.query_row("SELECT count(*) FROM audit WHERE json_extract(redacted_event,'$.kind')='approval_answered'",[],|row|row.get(0))?)).await.unwrap()
}
#[test]
fn approve_and_deny_are_model_free_keyed_conversation_bound_and_preserve_user_pause() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("work");
    std::fs::create_dir(&root).unwrap();
    let db = Database::open(
        temp.path().join("data/main.db"),
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
            "synthetic-token-for-approval-command-fixture",
            Redactor::default(),
            runtimes.work(),
        ))
        .unwrap();
    runtimes.block_on(async {
        host.master
            .receive(incoming("origin-a", "initial-a", "/status"))
            .await
            .unwrap();
        host.master
            .receive(incoming("origin-b", "initial-b", "/status"))
            .await
            .unwrap();
        let parent = host
            .orchestrator
            .list_agents()
            .await
            .unwrap()
            .into_iter()
            .find(|agent| agent.depth == 0)
            .unwrap();
        let mut token = parent.token.clone();
        token.budget = 10;
        token.data_scopes = ["project:default".into()].into();
        let agent = host
            .orchestrator
            .spawn(
                &parent.id,
                Delegation {
                    operation: "approval-worker".into(),
                    name: "Worker".into(),
                    profile: "coordinator".into(),
                    scope: "project:default".into(),
                    task: excalibur_types::TaskId::new().to_string(),
                    origin_conversation: "origin-a".into(),
                    goal: "Wait for explicit authorization".into(),
                    deliverable: "Report the approved decision".into(),
                    inputs: vec![],
                    pinned_instructions: vec![],
                    suggested_skills: vec![],
                    fork_context: false,
                    token: token.clone(),
                    spec: RunSpec {
                        context: excalibur_context::ContextInput::new(
                            "synthetic rules",
                            "synthetic task",
                            "fixed",
                        ),
                        budget: Budget::default(),
                        criteria: vec![Criterion::FinalResponse { schema: None }],
                    },
                    priority: 1,
                },
            )
            .await
            .unwrap();
        let run = agent.run_id.as_ref().unwrap();
        let security = Security::new(db.clone(), parent.token, Redactor::default())
            .await
            .unwrap();
        let now = excalibur_store::unix_seconds();
        security
            .register_run(
                run,
                RunSecurity {
                    parent: None,
                    project: agent.scope.clone(),
                    workspace: root.canonicalize().unwrap(),
                    token: token.clone(),
                    autonomy: vec![Autonomy::Autonomous],
                    full_acknowledged: false,
                    user_requested: true,
                },
                now,
            )
            .await
            .unwrap();
        let approval = security
            .request_grant(
                run,
                &CapabilityToken {
                    budget: 20,
                    ..token.clone()
                },
                now + 1000,
                "first explicit grant",
                now,
            )
            .await
            .unwrap();
        host.orchestrator
            .engine()
            .journal()
            .set_state_if(run, &["ready"], "running", None, "fixture-admission")
            .await
            .unwrap();
        host.orchestrator
            .engine()
            .wait(run, WaitReason::Approval)
            .await
            .unwrap();
        let command = incoming("origin-a", "approve-first", "/approve");
        let receipt = host.master.receive(command.clone()).await.unwrap();
        assert_eq!(status(&db, &approval).await, "approved");
        assert_eq!(
            host.orchestrator.engine().load(run).await.unwrap().status,
            Status::Running
        );
        assert_eq!(decisions(&db).await, 1);
        let replay = host.master.receive(command).await.unwrap();
        assert!(replay.duplicate);
        assert_eq!(receipt.message_id, replay.message_id);
        assert_eq!(decisions(&db).await, 1);
        assert_eq!(security.effective_token(run, now).await.unwrap().budget, 10);
        host.orchestrator.engine().pause(run).await.unwrap();
        let second = security
            .request_grant(
                run,
                &CapabilityToken {
                    budget: 30,
                    ..token.clone()
                },
                now + 1000,
                "second explicit grant",
                now,
            )
            .await
            .unwrap();
        let third = security
            .request_grant(
                run,
                &CapabilityToken {
                    budget: 40,
                    ..token
                },
                now + 1000,
                "third explicit grant",
                now,
            )
            .await
            .unwrap();
        host.master
            .receive(incoming("origin-a", "ambiguous", "/approve"))
            .await
            .unwrap();
        assert_eq!(status(&db, &second).await, "pending");
        assert_eq!(status(&db, &third).await, "pending");
        assert_eq!(decisions(&db).await, 1);
        host.master
            .receive(incoming(
                "origin-b",
                "foreign-conversation",
                &format!("/deny {second}"),
            ))
            .await
            .unwrap();
        assert_eq!(status(&db, &second).await, "pending");
        assert_eq!(decisions(&db).await, 1);
        host.master
            .receive(incoming(
                "origin-a",
                "deny-specific",
                &format!("/deny {second}"),
            ))
            .await
            .unwrap();
        assert_eq!(status(&db, &second).await, "rejected");
        assert_eq!(status(&db, &third).await, "pending");
        assert_eq!(
            host.orchestrator.engine().load(run).await.unwrap().status,
            Status::Paused
        );
        host.master
            .receive(incoming("origin-a", "deny-unique", "/deny"))
            .await
            .unwrap();
        assert_eq!(status(&db, &third).await, "rejected");
        assert_eq!(decisions(&db).await, 3);
        assert_eq!(
            host.orchestrator.engine().load(run).await.unwrap().status,
            Status::Paused
        );
        assert_eq!(model.0.load(Ordering::SeqCst), 0);
    });
    runtimes.block_on(host.close()).unwrap();
    runtimes.shutdown(std::time::Duration::from_secs(3));
}
