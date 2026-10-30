use excalibur_context::{ContextInput, SessionPolicy};
use excalibur_engine::{Budget, Criterion, Engine, RunSpec};
use excalibur_journal::Journal;
use excalibur_models::{ModelError, ModelGateway};
use excalibur_store::{Database, DatabaseKind};
use excalibur_types::runtime::*;
use serde_json::json;
use std::sync::Arc;
struct Model(Option<SessionPolicy>);
impl ModelGateway for Model {
    fn complete(&self, _: ModelRequest) -> BoxFuture<'_, Result<ModelResponse, ModelError>> {
        Box::pin(async { Err(ModelError::Provider) })
    }
    fn context_policy_override(&self) -> Option<serde_json::Value> {
        self.0
            .as_ref()
            .map(|policy| serde_json::to_value(policy).unwrap())
    }
}
struct Tools;
impl ToolRuntime for Tools {
    fn definitions(&self) -> Vec<ToolDefinition> {
        vec![ToolDefinition {
            name: "large_core".into(),
            description: "x".repeat(16000),
            input_schema: json!({"type":"object"}),
        }]
    }
    fn prepare<'a>(
        &'a self,
        _: &'a str,
        _: ToolCall,
    ) -> BoxFuture<'a, Result<PreparedTool, String>> {
        Box::pin(async { Err("not executed".into()) })
    }
    fn execute<'a>(
        &'a self,
        _: &'a str,
        _: &'a str,
        _: &'a PreparedTool,
    ) -> BoxFuture<'a, ToolResult> {
        Box::pin(async { ToolResult::error("not executed") })
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
#[tokio::test]
async fn native_builder_sizes_only_l0_without_overriding_explicit_caps_or_inventing_windows() {
    let dir = tempfile::tempdir().unwrap();
    let db = Database::open(
        dir.path().join("db"),
        DatabaseKind::Main,
        Default::default(),
    )
    .unwrap();
    let journal = Journal::new(db).await.unwrap();
    let engine = Engine::new(journal.clone(), Arc::new(Model(None)), Arc::new(Tools));
    let budget = Budget::default();
    let (context, evidence) = engine
        .native_context(
            "rules",
            "goal",
            "now",
            &budget,
            "\n[excalibur-host-lane:1]\n",
        )
        .unwrap();
    assert!(context.token_budgets[0] > 4096);
    assert_eq!(
        &context.token_budgets[1..],
        &ContextInput::new("rules", "goal", "now").token_budgets[1..]
    );
    assert_eq!(evidence.model_input_limit, None);
    let mut capped = context.clone();
    capped.token_budgets[0] = 32;
    let spec = |context| RunSpec {
        context,
        budget: budget.clone(),
        criteria: vec![Criterion::FinalResponse { schema: None }],
    };
    assert!(engine.create("explicit-small", spec(capped)).await.is_err());
    engine.create("native-sized", spec(context)).await.unwrap();
    let tiny = Budget {
        max_tokens: 64,
        ..budget.clone()
    };
    assert!(
        engine
            .native_context("rules", "goal", "now", &tiny, "")
            .is_err()
    );
    let model_policy = SessionPolicy {
        model_window: 2048,
        output_reserve: 512,
        ..Default::default()
    };
    let model_bound = Engine::new(
        journal,
        Arc::new(Model(Some(model_policy))),
        Arc::new(Tools),
    );
    assert!(
        model_bound
            .native_context("rules", "goal", "now", &budget, "")
            .is_err()
    );
}
