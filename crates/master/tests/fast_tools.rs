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

#[tokio::test]
async fn deferred_skill_metadata_intersects_the_authorized_run_and_scope_without_becoming_eager() {
    use excalibur_master::MasterContextRequest;
    struct Scoped(Arc<Runtime>);
    impl ToolRuntime for Scoped {
        fn definitions(&self) -> Vec<ToolDefinition> {
            self.0.definitions()
        }
        fn run_definitions<'a>(
            &'a self,
            run: &'a str,
        ) -> BoxFuture<'a, Result<Option<Vec<ToolDefinition>>, String>> {
            Box::pin(async move {
                Ok(Some(if run == "authorized" {
                    self.definitions()
                } else {
                    vec![]
                }))
            })
        }
        fn prepare<'a>(
            &'a self,
            run: &'a str,
            call: ToolCall,
        ) -> BoxFuture<'a, Result<PreparedTool, String>> {
            self.0.prepare(run, call)
        }
        fn execute<'a>(
            &'a self,
            run: &'a str,
            key: &'a str,
            tool: &'a PreparedTool,
        ) -> BoxFuture<'a, ToolResult> {
            self.0.execute(run, key, tool)
        }
        fn reconcile<'a>(
            &'a self,
            run: &'a str,
            key: &'a str,
            tool: &'a PreparedTool,
        ) -> BoxFuture<'a, Reconcile> {
            self.0.reconcile(run, key, tool)
        }
    }
    let runtime = Arc::new(Scoped(Arc::new(Runtime(AtomicUsize::new(0)))));
    let defs = runtime.definitions();
    let mut fast = CatalogTool::from_definition(defs[0].clone());
    fast.fast = true;
    fast.timeout_ms = 1000;
    let catalog = Arc::new(
        ToolCatalog::new(vec![fast, CatalogTool::from_definition(defs[1].clone())]).unwrap(),
    );
    let request = || MasterContextRequest {
        principal: "local",
        conversation: "chat",
        original_scope: "project:one",
        source_message_id: "accepted-source",
    };
    for (run, count) in [("authorized", 1), ("denied-run", 0)] {
        let adapter = MasterFastTools::new(
            Arc::new(NoActions),
            runtime.clone(),
            catalog.clone(),
            run.into(),
            ["project:one".into()].into(),
        )
        .unwrap();
        let schemas = adapter.deferred_schemas(request()).await.unwrap();
        assert_eq!(schemas.len(), count);
        if count == 1 {
            assert_eq!(schemas[0], defs[0]);
        }
        assert!(
            !adapter
                .definitions()
                .iter()
                .any(|d| d.name == "quick_lookup")
        );
        let mut foreign = request();
        foreign.original_scope = "project:other";
        assert!(adapter.deferred_schemas(foreign).await.unwrap().is_empty());
        let mut unscoped = request();
        unscoped.source_message_id = "";
        assert!(adapter.deferred_schemas(unscoped).await.is_err());
    }
}
