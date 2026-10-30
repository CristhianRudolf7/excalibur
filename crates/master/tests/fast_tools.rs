use excalibur_master::{FastActions, MasterFastTools, NoActions};
use excalibur_tools::deferred::{CatalogTool, ToolCatalog};
use excalibur_types::runtime::*;
use serde_json::{Value, json};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
struct Runtime(AtomicUsize);
impl ToolRuntime for Runtime {
    fn definitions(&self) -> Vec<ToolDefinition> {
        ["quick_lookup", "slow_research", "exec"]
            .map(|name| ToolDefinition {
                name: name.into(),
                description: format!("{name} synthetic lookup"),
                input_schema: json!({"type":"object","additionalProperties":false}),
            })
            .to_vec()
    }
    fn prepare<'a>(
        &'a self,
        _: &'a str,
        call: ToolCall,
    ) -> BoxFuture<'a, std::result::Result<PreparedTool, String>> {
        Box::pin(async move {
            Ok(PreparedTool {
                call,
                effect: Effect::Pure,
                resources: Vec::new(),
                exclusive: false,
                data: Value::Null,
            })
        })
    }
    fn execute<'a>(
        &'a self,
        _: &'a str,
        _: &'a str,
        tool: &'a PreparedTool,
    ) -> BoxFuture<'a, ToolResult> {
        Box::pin(async move {
            self.0.fetch_add(1, Ordering::SeqCst);
            ToolResult::ok(json!({"actual_tool":tool.call.name}))
        })
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
async fn the_master_discovers_only_fast_domain_tools_and_cannot_reach_the_slow_eager_core() {
    let runtime = Arc::new(Runtime(AtomicUsize::new(0)));
    let definitions = runtime.definitions();
    let mut fast = CatalogTool::from_definition(definitions[0].clone());
    fast.fast = true;
    fast.timeout_ms = 1000;
    let slow = CatalogTool::from_definition(definitions[1].clone());
    let catalog = Arc::new(ToolCatalog::new(vec![fast, slow]).unwrap());
    let adapter = MasterFastTools::new(
        Arc::new(NoActions),
        runtime.clone(),
        catalog,
        "authorized-master-run".into(),
        ["project:one".into()].into(),
    )
    .unwrap();
    assert_eq!(
        adapter
            .definitions()
            .iter()
            .map(|d| d.name.as_str())
            .collect::<Vec<_>>(),
        vec!["call_tool", "search_tools"]
    );
    let found = adapter
        .invoke_scoped(
            "conversation",
            "project:one",
            "search_tools",
            json!({"query":"lookup research"}),
            "search1",
        )
        .await
        .unwrap()
        .to_string();
    assert!(found.contains("quick_lookup"));
    assert!(!found.contains("slow_research"));
    assert!(!found.contains("exec"));
    let fast = adapter
        .invoke_scoped(
            "conversation",
            "project:one",
            "call_tool",
            json!({"name":"quick_lookup","args":{}}),
            "quick1",
        )
        .await
        .unwrap();
    assert_eq!(fast["success"], true);
    assert!(fast.to_string().contains("quick_lookup"));
    assert_eq!(runtime.0.load(Ordering::SeqCst), 1);
    for name in ["slow_research", "exec"] {
        assert!(
            adapter
                .invoke_scoped(
                    "conversation",
                    "project:one",
                    "call_tool",
                    json!({"name":name,"args":{}}),
                    "denied"
                )
                .await
                .is_err()
        );
    }
    assert!(
        adapter
            .invoke_scoped(
                "conversation",
                "project:other",
                "call_tool",
                json!({"name":"quick_lookup","args":{}}),
                "other"
            )
            .await
            .is_err()
    );
    assert!(
        adapter
            .invoke(
                "conversation",
                "call_tool",
                json!({"name":"quick_lookup","args":{}}),
                "unscoped"
            )
            .await
            .is_err()
    );
    assert_eq!(runtime.0.load(Ordering::SeqCst), 1);
}
