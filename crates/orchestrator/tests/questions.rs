mod support;
use excalibur_engine::{Engine, Status};
use excalibur_models::{ModelError, ModelGateway};
use excalibur_orchestrator::*;
use excalibur_types::runtime::*;
use serde_json::json;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use support::*;
struct AskingModel {
    calls: AtomicUsize,
    blocked: bool,
    blocking: bool,
}
impl ModelGateway for AskingModel {
    fn complete<'a>(
        &'a self,
        _: ModelRequest,
    ) -> BoxFuture<'a, std::result::Result<ModelResponse, ModelError>> {
        Box::pin(async move {
            let call = self.calls.fetch_add(1, Ordering::SeqCst);
            let mut message = Message::text("assistant", "Synthetic response");
            if call == 0 {
                message.tool_calls.push(ToolCall{id:"question-call".into(),name:"agent_message".into(),arguments:if self.blocked{json!({"action":"blocked","payload":{"reason":"Synthetic required input is missing"}})}else{json!({"action":"question","payload":{"question":"Choose a synthetic destination","blocking":self.blocking,"reason_kind":if self.blocking{"required_data"}else{"optional_preference"},"default":"Synthetic destination A"}})}.to_string()});
            }
            Ok(ModelResponse {
                message,
                usage: Usage::default(),
            })
        })
    }
}
async fn host(
    f: &Fixture,
    blocked: bool,
    blocking: bool,
) -> (
    Orchestrator,
    Arc<OrchestratorTools>,
    Arc<AskingModel>,
    Agent,
    Agent,
) {
    let adapter = Arc::new(OrchestratorTools::new(no_tools()));
    let model = Arc::new(AskingModel {
        calls: AtomicUsize::new(0),
        blocked,
        blocking,
    });
    let engine = Arc::new(Engine::new(
        f.host.engine().journal().clone(),
        model.clone(),
        adapter.clone(),
    ));
    let host = Orchestrator::open(f.db.clone(), engine, Limits::default())
        .await
        .unwrap()
        .with_authority(authority());
    adapter.attach(&host).unwrap();
    let parent = host
        .spawn(&f.master.id, delegation("p", "general", 300))
        .await
        .unwrap();
    let child = host
        .spawn(&parent.id, delegation("c", "general", 60))
        .await
        .unwrap();
    (host, adapter, model, parent, child)
}
#[tokio::test]
async fn worker_question_waits_after_receipt_without_extra_calls_and_answer_wakes_it() {
    let f = fixture().await;
    let (host, _adapter, model, parent, child) = host(&f, false, true).await;
    let run = child.run_id.as_ref().unwrap();
    let state = host.engine().run(run).await.unwrap();
    assert_eq!(state.status, Status::Waiting);
    assert_eq!(state.reason.as_deref(), Some("user"));
    assert_eq!(state.step, 1);
    assert_eq!(model.calls.load(Ordering::SeqCst), 1);
    assert!(state.calls.is_empty());
    let events = host.engine().journal().replay(run, 0).await.unwrap();
    assert!(
        events
            .iter()
            .any(|e| e.kind == "tool_result" && e.payload["result"]["success"] == true)
    );
    assert_eq!(
        host.engine().run(run).await.unwrap().status,
        Status::Waiting
    );
    assert_eq!(model.calls.load(Ordering::SeqCst), 1);
    let question = host
        .mailbox(&parent.id)
        .await
        .unwrap()
        .into_iter()
        .find(|m| m.kind == "question")
        .unwrap();
    host.send_message(
        "answer",
        &parent.id,
        &child.id,
        "answer",
        json!({"question_id":question.id,"text":"Use synthetic destination A"}),
        "normal",
    )
    .await
    .unwrap();
    host.deliver_mailbox(&child.id).await.unwrap();
    assert_eq!(
        host.engine().run(run).await.unwrap().status,
        Status::Completed
    );
    assert_eq!(model.calls.load(Ordering::SeqCst), 2);
}
#[tokio::test]
async fn nonblocking_question_does_not_suspend_worker() {
    let f = fixture().await;
    let (host, _adapter, model, _parent, child) = host(&f, false, false).await;
    assert_eq!(
        host.engine()
            .run(child.run_id.as_ref().unwrap())
            .await
            .unwrap()
            .status,
        Status::Completed
    );
    assert_eq!(model.calls.load(Ordering::SeqCst), 2);
}
#[tokio::test]
async fn blocked_report_commits_receipt_before_stopping_and_redirect_resolves_it() {
    let f = fixture().await;
    let (host, _adapter, model, _parent, child) = host(&f, true, true).await;
    let run = child.run_id.as_ref().unwrap();
    let state = host.engine().run(run).await.unwrap();
    assert_eq!(state.status, Status::Blocked);
    assert_eq!(
        state.reason.as_deref(),
        Some("Synthetic required input is missing")
    );
    assert_eq!(model.calls.load(Ordering::SeqCst), 1);
    assert_eq!(state.step, 1);
    let events = host.engine().journal().replay(run, 0).await.unwrap();
    assert!(
        events
            .iter()
            .any(|e| e.kind == "tool_result" && e.payload["result"]["success"] == true)
    );
    assert!(events.iter().all(|e| e.kind != "uncertain_effect"));
    host.control(
        &child.id,
        Control::Redirect {
            text: "Synthetic missing input is now supplied".into(),
        },
    )
    .await
    .unwrap();
    assert_eq!(
        host.engine().run(run).await.unwrap().status,
        Status::Completed
    );
    assert_eq!(model.calls.load(Ordering::SeqCst), 2);
}
#[tokio::test]
async fn unconfirmed_communication_effect_is_reconciled_before_wait_admission() {
    let f = fixture().await;
    let (host, adapter, _model, _parent, child) = host(&f, false, true).await;
    let run = child.run_id.as_ref().unwrap();
    host.engine()
        .journal()
        .set_state_if(run, &["ready"], "running", None, "fixture-running")
        .await
        .unwrap();
    let prepared = adapter
        .prepare(
            run,
            ToolCall {
                id: "before-receipt".into(),
                name: "agent_message".into(),
                arguments:
                    json!({"action":"question","payload":{"question":"Synthetic receipt guard"}})
                        .to_string(),
            },
        )
        .await
        .unwrap();
    let result = adapter.execute(run, "before-receipt", &prepared).await;
    assert!(result.success);
    assert!(!host.apply_agent_control(&child.id).await.unwrap());
    assert_eq!(
        host.engine().load(run).await.unwrap().status,
        Status::Running
    );
    let replay = adapter.reconcile(run, "before-receipt", &prepared).await;
    assert!(matches!(replay,Reconcile::Done(r)if r==result));
    host.engine()
        .journal()
        .append(
            run,
            "tool_result",
            "before-receipt:result",
            json!({"index":0,"name":"agent_message","result":result}),
        )
        .await
        .unwrap();
    assert!(host.apply_agent_control(&child.id).await.unwrap());
    assert_eq!(
        host.engine().load(run).await.unwrap().status,
        Status::Waiting
    );
}

#[tokio::test]
async fn mutable_worker_cannot_choose_nonblocking_but_readonly_optional_wait_can_default() {
    for mutable in [true, false] {
        let f = fixture().await;
        let adapter = Arc::new(OrchestratorTools::new(no_tools()));
        struct OptionalWait {
            calls: AtomicUsize,
        }
        impl ModelGateway for OptionalWait {
            fn complete(
                &self,
                _: ModelRequest,
            ) -> BoxFuture<'_, std::result::Result<ModelResponse, ModelError>> {
                Box::pin(async move {
                    let mut message = Message::text("assistant", "Synthetic done");
                    if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
                        message.tool_calls.push(ToolCall{id:"question".into(),name:"agent_message".into(),arguments:json!({"action":"question","payload":{"question":"Optional reversible label?","reason_kind":"optional_preference","default":"local-label","blocking":false,"host_requires_blocking":false,"wait_for_answer":true}}).to_string()});
                    }
                    Ok(ModelResponse {
                        message,
                        usage: Usage::default(),
                    })
                })
            }
        }
        let model = Arc::new(OptionalWait {
            calls: AtomicUsize::new(0),
        });
        let engine = Arc::new(Engine::new(
            f.host.engine().journal().clone(),
            model.clone(),
            adapter.clone(),
        ));
        let host = Orchestrator::open(f.db.clone(), engine, Limits::default())
            .await
            .unwrap()
            .with_authority(authority());
        adapter.attach(&host).unwrap();
        let mut token = f.master.token.clone();
        if mutable {
            token.roots.push(excalibur_security::Root {
                path: f.dir.path().canonicalize().unwrap(),
                write: true,
            });
        }
        let master = host
            .rebind_master_at_startup(&f.master.name, token.clone())
            .await
            .unwrap();
        let mut pd = delegation("p", "general", 300);
        pd.token = token.clone();
        let p = host.spawn(&master.id, pd).await.unwrap();
        let mut cd = delegation("c", "general", 30);
        cd.token = token;
        let c = host.spawn(&p.id, cd).await.unwrap();
        let run = c.run_id.as_ref().unwrap();
        assert_eq!(
            host.engine().run(run).await.unwrap().status,
            Status::Waiting
        );
        assert_eq!(model.calls.load(Ordering::SeqCst), 1);
        let question = host
            .mailbox(&p.id)
            .await
            .unwrap()
            .into_iter()
            .find(|m| m.kind == "question")
            .unwrap();
        assert_eq!(question.payload["blocking"], mutable);
        if mutable {
            assert!(
                host.service_question_deadlines(question.created + 900)
                    .await
                    .unwrap()
                    .is_empty()
            );
            assert_eq!(
                host.engine().load(run).await.unwrap().status,
                Status::Waiting
            );
        } else {
            assert_eq!(
                host.service_question_deadlines(question.created + 900)
                    .await
                    .unwrap(),
                vec![question.id]
            );
            assert_eq!(
                host.engine().run(run).await.unwrap().status,
                Status::Completed
            );
            assert_eq!(model.calls.load(Ordering::SeqCst), 2);
        }
    }
}
