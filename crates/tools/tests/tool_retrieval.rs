use excalibur_tools::deferred::*;
use excalibur_types::runtime::*;
use excalibur_vault::Redactor;
use serde_json::{Value, json};
use std::sync::{
    Arc, Mutex, RwLock,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};
fn definition(name: &str, description: &str) -> ToolDefinition {
    ToolDefinition {
        name: name.into(),
        description: description.into(),
        input_schema: json!({"$schema":"https://json-schema.org/draft/2020-12/schema","type":"object","properties":{"value":{"type":"integer","minimum":0}},"required":["value"],"unevaluatedProperties":false}),
    }
}
struct Backend {
    definitions: RwLock<Vec<ToolDefinition>>,
    prepared: Mutex<Vec<ToolCall>>,
    executions: AtomicUsize,
    deny: AtomicBool,
    bad_output: AtomicBool,
    redactor: Redactor,
    events: Mutex<Vec<String>>,
}
impl Backend {
    fn new(definitions: Vec<ToolDefinition>) -> Arc<Self> {
        Arc::new(Self {
            definitions: RwLock::new(definitions),
            prepared: Mutex::new(vec![]),
            executions: AtomicUsize::new(0),
            deny: AtomicBool::new(false),
            bad_output: AtomicBool::new(false),
            redactor: Redactor::default(),
            events: Mutex::new(vec![]),
        })
    }
}
impl ToolRuntime for Backend {
    fn definitions(&self) -> Vec<ToolDefinition> {
        self.definitions.read().unwrap().clone()
    }
    fn sanitize_durable(&self, v: &Value) -> Result<Value, String> {
        Ok(self.redactor.redact_json(v))
    }
    fn scheduling<'a>(
        &'a self,
        _: &'a str,
        call: &'a ToolCall,
    ) -> BoxFuture<'a, Option<ToolScheduling>> {
        Box::pin(async move {
            Some(ToolScheduling {
                resources: vec![Resource {
                    key: format!("domain:{}", call.name),
                    write: false,
                }],
                exclusive: false,
            })
        })
    }
    fn prepare<'a>(
        &'a self,
        _: &'a str,
        call: ToolCall,
    ) -> BoxFuture<'a, Result<PreparedTool, String>> {
        Box::pin(async move {
            if self.deny.load(Ordering::SeqCst) {
                return Err("permission_denied: exact target disabled".into());
            }
            self.prepared.lock().unwrap().push(call.clone());
            Ok(PreparedTool {
                call: call.clone(),
                effect: Effect::Pure,
                resources: vec![Resource {
                    key: format!("domain:{}", call.name),
                    write: false,
                }],
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
            self.executions.fetch_add(1, Ordering::SeqCst);
            let args: Value = serde_json::from_str(&tool.call.arguments).unwrap();
            ToolResult::ok(if self.bad_output.load(Ordering::SeqCst) {
                json!({"value":"wrong"})
            } else {
                json!({"value":args["value"]})
            })
        })
    }
    fn reconcile<'a>(
        &'a self,
        run: &'a str,
        key: &'a str,
        tool: &'a PreparedTool,
    ) -> BoxFuture<'a, Reconcile> {
        Box::pin(async move { Reconcile::Done(self.execute(run, key, tool).await) })
    }
    fn audit_builtin<'a>(
        &'a self,
        _: &'a str,
        _: &'a ToolCall,
        result: Option<&'a ToolResult>,
    ) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async move {
            self.events.lock().unwrap().push(
                if result.is_some() {
                    "search_result"
                } else {
                    "search_call"
                }
                .into(),
            );
            Ok(())
        })
    }
    fn begin_step<'a>(&'a self, _: &'a str, step: u64) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async move {
            self.events.lock().unwrap().push(format!("begin:{step}"));
            Ok(())
        })
    }
    fn end_step<'a>(&'a self, _: &'a str, step: u64) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async move {
            self.events.lock().unwrap().push(format!("end:{step}"));
            Ok(())
        })
    }
    fn execution_state<'a>(
        &'a self,
        _: &'a str,
        state: &'a str,
        _: Option<&'a str>,
    ) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async move {
            self.events.lock().unwrap().push(state.into());
            Ok(())
        })
    }
    fn live_status(&self, run: &str) -> Vec<Value> {
        vec![json!({"run":run,"tail":"live"})]
    }
}
fn entry(name: &str, description: &str) -> CatalogTool {
    let mut tool = CatalogTool::from_definition(definition(name, description));
    tool.output_schema = json!({"type":"object","properties":{"value":{"type":"integer"}},"required":["value"],"additionalProperties":false});
    tool
}
fn setup(
    entries: Vec<CatalogTool>,
    role: CallerRole,
) -> (Arc<Backend>, Arc<ToolCatalog>, DeferredTools) {
    let mut definitions = vec![definition("core_status", "Read core status")];
    definitions.extend(entries.iter().map(CatalogTool::definition));
    let backend = Backend::new(definitions);
    let catalog = Arc::new(ToolCatalog::new(entries).unwrap());
    let tools = DeferredTools::new(backend.clone(), catalog.clone(), role).unwrap();
    (backend, catalog, tools)
}
fn call(name: &str, args: Value) -> ToolCall {
    ToolCall {
        id: "call".into(),
        name: name.into(),
        arguments: args.to_string(),
    }
}
async fn invoke(tools: &DeferredTools, name: &str, args: Value) -> ToolResult {
    let p = tools.prepare("run", call(name, args)).await.unwrap();
    tools.execute("run", "key", &p).await
}
#[test]
fn bm25_gold_queries_measure_precision_and_limit_to_five() {
    let fixtures = [
        (
            "calendar_lookup",
            "Look up calendar meeting appointment events",
            "calendar appointment",
        ),
        (
            "invoice_export",
            "Export invoice finance receipts ledger",
            "invoice receipt ledger",
        ),
        (
            "browser_navigate",
            "Navigate browser pages web URL",
            "browser navigate URL",
        ),
        (
            "worktree",
            "Manage git branch workspace worktree",
            "git branch worktree",
        ),
        (
            "userdb_query",
            "Query database SQL table rows",
            "SQL database rows",
        ),
        (
            "audio_transcribe",
            "Transcribe audio speech recording",
            "audio speech transcribe",
        ),
        (
            "image_generate",
            "Generate render picture image pixels",
            "render picture image",
        ),
        (
            "model3d_ops",
            "Edit geometry 3d mesh model",
            "geometry mesh 3d",
        ),
    ];
    let catalog = ToolCatalog::new(fixtures.iter().map(|(n, d, _)| entry(n, d)).collect()).unwrap();
    let mut correct = 0;
    for (name, _, query) in fixtures {
        let results = catalog.search(query, CallerRole::Worker, None).unwrap();
        assert!(results.tools.len() <= 5);
        if results.tools.first().is_some_and(|t| t.name == name) {
            correct += 1;
        }
        assert_eq!(results.tools[0].name, name);
        assert!(results.scores[0].bm25 > 0.);
    }
    assert_eq!(correct, 8, "synthetic top-1 precision: {correct}/8");
    let results = catalog
        .search(
            "calendar invoice browser worktree database audio image model",
            CallerRole::Worker,
            None,
        )
        .unwrap();
    assert_eq!(results.tools.len(), 5);
}
#[test]
fn schema_names_mcp_identity_and_skill_references_are_validated() {
    for name in [
        "",
        "HasUpper",
        "has.dot",
        "has-dash",
        "9start",
        &"a".repeat(65),
    ] {
        assert!(ToolCatalog::new(vec![entry(name, "Read data")]).is_err());
    }
    let long = mcp_name(&"server".repeat(15), &"tool".repeat(15)).unwrap();
    assert!(long.len() <= 64);
    validate_name(&long).unwrap();
    assert_eq!(
        long,
        mcp_name(&"server".repeat(15), &"tool".repeat(15)).unwrap()
    );
    assert_ne!(
        mcp_name("a__b", "c").unwrap(),
        mcp_name("a", "b__c").unwrap()
    );
    // A separator-adjacent underscore would merge without a hash.
    assert_ne!(mcp_name("a_", "b").unwrap(), mcp_name("a", "_b").unwrap());
    assert_ne!(
        mcp_name("server-a", "tool").unwrap(),
        mcp_name("server_a", "tool").unwrap()
    );
    let name = mcp_name("account", "lookup").unwrap();
    let catalog = ToolCatalog::new(vec![entry(&name, "Lookup account")]).unwrap();
    let found = catalog
        .resolve_references(&[ToolReference::Mcp {
            server: "account".into(),
            tool: "lookup".into(),
        }])
        .unwrap();
    assert_eq!(found[0].name, name);
    assert!(catalog.resolve_skill_tools(&["missing".into()]).is_err());
    let mut invalid = entry("bad_ref", "Read data");
    invalid.input_schema = json!({"$ref":"https://unavailable.invalid/schema"});
    assert!(ToolCatalog::new(vec![invalid]).is_err());
}
#[tokio::test]
async fn search_returns_full_metadata_without_changing_eager_prefix_and_updates_atomically() {
    let (backend, catalog, tools) = setup(
        vec![
            entry("invoice_export", "Export invoice ledger"),
            entry("calendar_lookup", "Lookup calendar events"),
        ],
        CallerRole::Worker,
    );
    let initial = serde_json::to_vec(&tools.definitions()).unwrap();
    assert!(
        !tools
            .definitions()
            .iter()
            .any(|d| d.name == "invoice_export")
    );
    let result = invoke(&tools, "search_tools", json!({"query":"invoice ledger"})).await;
    assert!(result.success);
    assert_eq!(result.output["tools"][0]["name"], "invoice_export");
    for field in [
        "input_schema",
        "output_schema",
        "effect",
        "idempotency",
        "resources_description",
        "risk",
        "timeout_ms",
        "output_budget_bytes",
        "platforms",
        "inverse",
        "probe",
        "dry_run",
        "fast",
        "cancel",
    ] {
        assert!(result.output["tools"][0].get(field).is_some(), "{field}");
    }
    assert_eq!(initial, serde_json::to_vec(&tools.definitions()).unwrap());
    let next = entry("gpu_metrics", "Read GPU usage metrics");
    backend.definitions.write().unwrap().push(next.definition());
    let mut entries = vec![
        entry("invoice_export", "Export invoice ledger"),
        entry("calendar_lookup", "Lookup calendar events"),
    ];
    entries.push(next);
    catalog.replace(entries).unwrap();
    assert_eq!(catalog.revision(), 2);
    let result = invoke(&tools, "search_tools", json!({"query":"GPU usage metrics"})).await;
    assert_eq!(result.output["tools"][0]["name"], "gpu_metrics");
    assert_eq!(initial, serde_json::to_vec(&tools.definitions()).unwrap());
    assert_eq!(
        backend.events.lock().unwrap().as_slice(),
        [
            "search_call",
            "search_result",
            "search_call",
            "search_result"
        ]
    );
}
#[tokio::test]
async fn call_tool_checks_schema_resources_permissions_and_prepared_owner() {
    let (backend, _, tools) = setup(
        vec![entry("invoice_export", "Export invoices")],
        CallerRole::Worker,
    );
    let args = json!({"name":"invoice_export","args":{"value":3}});
    let p = tools
        .prepare("run", call("call_tool", args.clone()))
        .await
        .unwrap();
    assert_eq!(p.resources[0].key, "domain:invoice_export");
    assert_eq!(backend.prepared.lock().unwrap()[0].name, "invoice_export");
    assert_eq!(
        tools
            .scheduling("run", &call("call_tool", args))
            .await
            .unwrap()
            .resources,
        p.resources
    );
    assert!(tools.execute("run", "key", &p).await.success);
    for args in [
        json!({"name":"invoice_export","args":{"value":"wrong"}}),
        json!({"name":"invoice_export","args":{"value":1,"unknown":1}}),
        json!({"name":"missing","args":{"value":1}}),
        json!({"name":"invoice_export","args":{},"arguments":{}}),
    ] {
        assert!(tools.prepare("run", call("call_tool", args)).await.is_err());
    }
    let mut tampered = p.clone();
    tampered.resources.clear();
    assert!(!tools.execute("run", "key", &tampered).await.success);
    assert!(!tools.execute("other", "key", &p).await.success);
    backend.deny.store(true, Ordering::SeqCst);
    assert!(
        tools
            .prepare(
                "run",
                call(
                    "call_tool",
                    json!({"name":"invoice_export","args":{"value":1}})
                )
            )
            .await
            .unwrap_err()
            .contains("permission_denied")
    );
    assert_eq!(backend.executions.load(Ordering::SeqCst), 1);
}
#[tokio::test]
async fn master_fast_filter_applies_to_search_skills_direct_and_wrapped_calls() {
    let mut fast = entry("calendar_lookup", "Lookup calendar appointments");
    fast.fast = true;
    let (_, _, tools) = setup(
        vec![fast, entry("invoice_export", "Export invoices")],
        CallerRole::Master,
    );
    let found = invoke(&tools, "search_tools", json!({"query":"invoice calendar"})).await;
    assert_eq!(found.output["tools"].as_array().unwrap().len(), 1);
    assert_eq!(found.output["tools"][0]["name"], "calendar_lookup");
    assert!(
        tools
            .resolve_skill_tools(&["invoice_export".into()])
            .is_err()
    );
    for call in [
        call("invoice_export", json!({"value":1})),
        call(
            "call_tool",
            json!({"name":"invoice_export","arguments":{"value":1}}),
        ),
    ] {
        assert!(tools.prepare("run", call).await.is_err());
    }
    assert!(
        invoke(
            &tools,
            "call_tool",
            json!({"name":"calendar_lookup","arguments":{"value":1}})
        )
        .await
        .success
    );
}
#[tokio::test]
async fn catalog_change_revokes_preparation_but_pins_native_schema_per_run() {
    let (_, catalog, tools) = setup(
        vec![entry("invoice_export", "Original export")],
        CallerRole::Worker,
    );
    let pinned = tools.deferred_definitions("run").await.unwrap();
    let p = tools
        .prepare(
            "run",
            call(
                "call_tool",
                json!({"name":"invoice_export","args":{"value":2}}),
            ),
        )
        .await
        .unwrap();
    catalog
        .replace(vec![entry("invoice_export", "Updated export")])
        .unwrap();
    assert_eq!(tools.deferred_definitions("run").await.unwrap(), pinned);
    assert_ne!(tools.deferred_definitions("new-run").await.unwrap(), pinned);
    assert!(!tools.execute("run", "key", &p).await.success);
    assert!(matches!(
        tools.reconcile("run", "key", &p).await,
        Reconcile::Uncertain(_)
    ));
    tools.begin_step("run", 2).await.unwrap();
    tools.end_step("run", 2).await.unwrap();
    tools.execution_state("run", "paused", None).await.unwrap();
    assert_eq!(tools.live_status("run")[0]["tail"], "live");
}
struct VectorHost {
    calls: AtomicUsize,
    fail: bool,
}
impl ToolEmbedder for VectorHost {
    fn model(&self) -> &str {
        "synthetic-vectors"
    }
    fn dimensions(&self) -> usize {
        2
    }
    fn embed<'a>(&'a self, _: &'a str) -> BoxFuture<'a, Result<Vec<f32>, String>> {
        Box::pin(async move {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if self.fail {
                Err("unpublished synthetic detail".into())
            } else {
                Ok(vec![1., 0.])
            }
        })
    }
}
#[tokio::test]
async fn host_vectors_retrieve_synonyms_and_failure_is_explicit_lexical_fallback() {
    let mut flight = entry("flight_search", "Find air transportation tickets");
    flight.embedding = Some(ToolEmbedding {
        model: "synthetic-vectors".into(),
        values: vec![1., 0.],
    });
    let mut calendar = entry("calendar_lookup", "Lookup calendar events");
    calendar.embedding = Some(ToolEmbedding {
        model: "synthetic-vectors".into(),
        values: vec![0., 1.],
    });
    let (_, _, tools) = setup(vec![flight, calendar], CallerRole::Worker);
    let host = Arc::new(VectorHost {
        calls: AtomicUsize::new(0),
        fail: false,
    });
    let tools = tools.with_embedder(host.clone()).unwrap();
    let found = invoke(&tools, "search_tools", json!({"query":"book voyage"})).await;
    assert_eq!(found.output["tools"][0]["name"], "flight_search");
    assert_eq!(found.output["vector_status"], "host_vectors");
    assert_eq!(host.calls.load(Ordering::SeqCst), 1);
    assert!(found.output["tools"][0].get("embedding").is_none());
    let tools = tools
        .with_embedder(Arc::new(VectorHost {
            calls: AtomicUsize::new(0),
            fail: true,
        }))
        .unwrap();
    let found = invoke(&tools, "search_tools", json!({"query":"calendar events"})).await;
    assert_eq!(found.output["vector_status"], "unavailable");
    assert_eq!(found.output["tools"][0]["name"], "calendar_lookup");
    assert!(!found.output.to_string().contains("unpublished"));
}
#[tokio::test]
async fn output_schema_failures_are_typed_and_sensitive_metadata_is_rejected() {
    let (backend, _, tools) = setup(
        vec![entry("invoice_export", "Export invoices")],
        CallerRole::Worker,
    );
    backend.bad_output.store(true, Ordering::SeqCst);
    let result = invoke(
        &tools,
        "call_tool",
        json!({"name":"invoice_export","args":{"value":1}}),
    )
    .await;
    assert!(!result.success);
    assert_eq!(result.output["code"], "output_schema_validation_failed");
    backend.redactor.register_secret(
        &excalibur_vault::SecretValue::new(b"synthetic-catalog-secret".to_vec()).unwrap(),
    );
    let entry = entry("invoice_export", "Export synthetic-catalog-secret invoice");
    let catalog = Arc::new(ToolCatalog::new(vec![entry]).unwrap());
    assert!(DeferredTools::new(backend, catalog, CallerRole::Worker).is_err());
}
struct PendingVectors;
impl ToolEmbedder for PendingVectors {
    fn model(&self) -> &str {
        "synthetic-vectors"
    }
    fn dimensions(&self) -> usize {
        2
    }
    fn embed<'a>(&'a self, _: &'a str) -> BoxFuture<'a, Result<Vec<f32>, String>> {
        Box::pin(std::future::pending())
    }
}
#[tokio::test]
async fn pending_host_vectors_have_a_deadline_and_keep_lexical_retrieval_available() {
    let mut calendar = entry("calendar_lookup", "Lookup calendar events");
    calendar.embedding = Some(ToolEmbedding {
        model: "synthetic-vectors".into(),
        values: vec![1., 0.],
    });
    let (_, _, tools) = setup(vec![calendar], CallerRole::Worker);
    let tools = tools
        .with_embedder(Arc::new(PendingVectors))
        .unwrap()
        .with_embedding_timeout_ms(1)
        .unwrap();
    let result = invoke(&tools, "search_tools", json!({"query":"calendar events"})).await;
    assert!(result.success);
    assert_eq!(result.output["vector_status"], "unavailable");
    assert_eq!(result.output["tools"][0]["name"], "calendar_lookup");
}
#[tokio::test]
async fn call_tool_enters_real_local_read_policy_before_content_or_readset_access() {
    use excalibur_store::{Database, DatabaseKind, Priority, ReadLane, StorageConfig};
    use excalibur_tools::{Access, LocalTools, ToolPolicy};
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("project");
    std::fs::create_dir(&root).unwrap();
    std::fs::write(root.join("private.txt"), "synthetic confidential content").unwrap();
    let db = Database::open(
        temp.path().join("data/main.db"),
        DatabaseKind::Main,
        StorageConfig::default(),
    )
    .unwrap();
    db.write(Priority::Normal, |c| {
        c.execute("INSERT INTO runs(id) VALUES('run')", [])?;
        Ok(())
    })
    .await
    .unwrap();
    let local = Arc::new(
        LocalTools::new(
            db.clone(),
            root,
            ToolPolicy {
                read: Access::Deny,
                ..Default::default()
            },
        )
        .await
        .unwrap(),
    );
    let definition = local
        .definitions()
        .into_iter()
        .find(|d| d.name == "read_file")
        .unwrap();
    let catalog =
        Arc::new(ToolCatalog::new(vec![CatalogTool::from_definition(definition)]).unwrap());
    let tools = DeferredTools::new(local, catalog, CallerRole::Worker).unwrap();
    let error = tools
        .prepare(
            "run",
            call(
                "call_tool",
                json!({"name":"read_file","args":{"path":"private.txt"}}),
            ),
        )
        .await
        .unwrap_err();
    assert!(error.contains("permission_denied"));
    let readset = db
        .read(ReadLane::Interactive, |c| {
            Ok(c.query_row("SELECT count(*) FROM tool_readset", [], |r| {
                r.get::<_, i64>(0)
            })?)
        })
        .await
        .unwrap();
    assert_eq!(readset, 0);
    db.shutdown().await.unwrap();
}
