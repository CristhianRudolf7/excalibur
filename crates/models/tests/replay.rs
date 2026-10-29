use excalibur_context::ContextInput;
use excalibur_engine::{Budget, Criterion, Engine, RunSpec, Status};
use excalibur_journal::Journal;
use excalibur_models::*;
use excalibur_store::{Database, DatabaseKind, StorageConfig};
use excalibur_types::runtime::*;
use serde_json::{Value, json};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
struct FixtureModel(Arc<AtomicUsize>);
impl ModelGateway for FixtureModel {
    fn cost_is_known(&self) -> bool {
        true
    }
    fn identity(&self) -> String {
        "synthetic-replay-model-v1".into()
    }
    fn request_time_override(&self, step: u64) -> Option<String> {
        Some(format!("recorded-clock-{step}"))
    }
    fn call_bound(&self, _: &ModelRequest) -> Option<ModelCallBound> {
        Some(ModelCallBound {
            max_tokens: 20,
            max_cost_microusd: 10,
        })
    }
    fn complete<'a>(
        &'a self,
        request: ModelRequest,
    ) -> BoxFuture<'a, Result<ModelResponse, ModelError>> {
        Box::pin(async move {
            self.0.fetch_add(1, Ordering::SeqCst);
            let mut message = Message::text("assistant", "Synthetic done");
            if !request
                .messages
                .iter()
                .any(|message| message.role == "tool")
            {
                message.tool_calls = vec![ToolCall {
                    id: "fixture-call".into(),
                    name: "fixture".into(),
                    arguments: "{}".into(),
                }];
            }
            Ok(ModelResponse {
                message,
                usage: Usage {
                    input_tokens: 5,
                    output_tokens: 2,
                    cost_microusd: 2,
                    estimated: false,
                },
            })
        })
    }
}
struct FixtureTools;
impl ToolRuntime for FixtureTools {
    fn definitions(&self) -> Vec<ToolDefinition> {
        vec![ToolDefinition {
            name: "fixture".into(),
            description: "Synthetic read-only backend operation".into(),
            input_schema: json!({"type":"object"}),
        }]
    }
    fn prepare<'a>(
        &'a self,
        _: &'a str,
        call: ToolCall,
    ) -> BoxFuture<'a, Result<PreparedTool, String>> {
        Box::pin(async move {
            Ok(PreparedTool {
                call,
                effect: Effect::Pure,
                resources: vec![],
                exclusive: true,
                data: Value::Null,
            })
        })
    }
    fn execute<'a>(
        &'a self,
        _: &'a str,
        _: &'a str,
        _: &'a PreparedTool,
    ) -> BoxFuture<'a, ToolResult> {
        Box::pin(async { ToolResult::ok(json!({"answer":42})) })
    }
    fn reconcile<'a>(
        &'a self,
        _: &'a str,
        _: &'a str,
        _: &'a PreparedTool,
    ) -> BoxFuture<'a, Reconcile> {
        Box::pin(async { Reconcile::Retry })
    }
}
async fn record() -> (
    tempfile::TempDir,
    Database,
    ReplayTape,
    RunSpec,
    Arc<AtomicUsize>,
) {
    let temp = tempfile::tempdir().unwrap();
    let db = Database::open(
        temp.path().join("main.db"),
        DatabaseKind::Main,
        StorageConfig::default(),
    )
    .unwrap();
    let journal = Journal::new(db.clone()).await.unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let engine = Engine::new(
        journal.clone(),
        Arc::new(FixtureModel(calls.clone())),
        Arc::new(FixtureTools),
    );
    let spec = RunSpec {
        context: ContextInput::new("Synthetic instruction", "Compute synthetic result", "fixed"),
        budget: Budget::default(),
        criteria: vec![Criterion::ToolSuccess {
            name: "fixture".into(),
        }],
    };
    let run = engine.create("record", spec.clone()).await.unwrap();
    assert_eq!(engine.run(&run).await.unwrap().status, Status::Completed);
    let tape = ReplayTape::from_journal(&journal, &run).await.unwrap();
    (temp, db, tape, spec, calls)
}
#[tokio::test]
async fn recorded_real_engine_run_replays_exactly_with_zero_new_provider_calls_and_zero_charged_usage()
 {
    let (temp, db, tape, spec, calls) = record().await;
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    let path = temp.path().join("replay.json");
    tape.save(&path).unwrap();
    assert!(tape.save(&path).is_err());
    let restored = ReplayTape::load(&path).unwrap();
    let gateway = Arc::new(ReplayGateway::new(restored).unwrap());
    let journal = Journal::new(db.clone()).await.unwrap();
    let engine = Engine::new(journal.clone(), gateway.clone(), Arc::new(FixtureTools));
    let run = engine.create("replay", spec).await.unwrap();
    let result = engine.run(&run).await.unwrap();
    assert_eq!(result.status, Status::Completed);
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert!(gateway.divergence().is_none());
    let events = journal.replay(&run, 0).await.unwrap();
    for event in events.iter().filter(|event| event.kind == "model_response") {
        assert_eq!(event.payload["metrics"]["provider_calls"], 0);
        assert_eq!(event.payload["metrics"]["cost_microusd"], 0);
        assert_eq!(event.payload["metrics"]["usage"]["input_tokens"], 0);
        assert_eq!(event.payload["response"]["usage"]["input_tokens"], 5);
    }
    db.shutdown().await.unwrap();
}
#[tokio::test]
async fn first_divergence_is_safe_and_only_explicit_budget_allows_bounded_live_continuation() {
    let (_temp, db, tape, _spec, original) = record().await;
    let mut request = tape.entries[0].request.clone();
    request.messages[0].content = "synthetic-private-value-must-never-be-in-error".into();
    let replay = ReplayGateway::new(tape).unwrap();
    let error = replay.complete_observed(request.clone()).await.unwrap_err();
    assert!(matches!(error.error, ModelError::ReplayDivergence(_)));
    assert!(!error.to_string().contains("private-value"));
    let divergence = replay.divergence().unwrap();
    assert_eq!(divergence.pointer, "/messages/0/content");
    assert_eq!(original.load(Ordering::SeqCst), 2);
    let live = Arc::new(AtomicUsize::new(0));
    assert!(
        replay
            .authorize_live(
                &divergence,
                Arc::new(FixtureModel(live.clone())),
                ReplayLiveBudget {
                    approval_id: "".into(),
                    max_calls: 1,
                    max_tokens: 20,
                    max_cost_microusd: 10
                }
            )
            .is_err()
    );
    replay
        .authorize_live(
            &divergence,
            Arc::new(FixtureModel(live.clone())),
            ReplayLiveBudget {
                approval_id: "authenticated-explicit-budget".into(),
                max_calls: 1,
                max_tokens: 20,
                max_cost_microusd: 10,
            },
        )
        .unwrap();
    assert!(
        !replay
            .complete_observed(request.clone())
            .await
            .unwrap()
            .metrics
            .replayed
    );
    assert_eq!(live.load(Ordering::SeqCst), 1);
    assert!(matches!(
        replay.complete(request).await,
        Err(ModelError::ReplayBudget)
    ));
    assert_eq!(live.load(Ordering::SeqCst), 1);
    db.shutdown().await.unwrap();
}
