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
/// A real HTTP adapter records cache controls and reported usage, then replay
/// regenerates both cached requests exactly without opening another connection.
#[tokio::test]
async fn cached_http_engine_tape_replays_namespace_boundaries_and_calibration_without_http() {
    use excalibur_providers::{Provider, ProviderClient, Secret};
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
    };
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let observed_calls = calls.clone();
    let server = tokio::spawn(async move {
        let replies = [
            json!({"choices":[{"finish_reason":"tool_calls","message":{"role":"assistant","content":"","tool_calls":[{"id":"cached-call","type":"function","function":{"name":"fixture","arguments":"{}"}}]}}],"usage":{"prompt_tokens":100,"completion_tokens":5,"prompt_tokens_details":{"cached_tokens":0}}}),
            json!({"choices":[{"finish_reason":"stop","message":{"role":"assistant","content":"Synthetic done"}}],"usage":{"prompt_tokens":110,"completion_tokens":3,"prompt_tokens_details":{"cached_tokens":80}}}),
        ];
        let mut bodies = Vec::new();
        for reply in replies {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut bytes = Vec::new();
            loop {
                let mut chunk = [0; 4096];
                let n = socket.read(&mut chunk).await.unwrap();
                assert!(n > 0);
                bytes.extend_from_slice(&chunk[..n]);
                if let Some(end) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
                    let head = String::from_utf8_lossy(&bytes[..end]);
                    let length: usize = head
                        .lines()
                        .find_map(|line| {
                            line.to_ascii_lowercase()
                                .strip_prefix("content-length:")
                                .map(|v| v.trim().parse().unwrap())
                        })
                        .unwrap_or(0);
                    if bytes.len() >= end + 4 + length {
                        bodies.push(
                            serde_json::from_slice::<Value>(&bytes[end + 4..end + 4 + length])
                                .unwrap(),
                        );
                        break;
                    }
                }
            }
            observed_calls.fetch_add(1, Ordering::SeqCst);
            let body = reply.to_string();
            socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len()).as_bytes()).await.unwrap();
        }
        bodies
    });
    let temp = tempfile::tempdir().unwrap();
    let db = Database::open(
        temp.path().join("main.db"),
        DatabaseKind::Main,
        StorageConfig::default(),
    )
    .unwrap();
    let journal = Journal::new(db.clone()).await.unwrap();
    let client = ProviderClient::with_base_url(
        Provider::OpenAi,
        Secret::new("synthetic").unwrap(),
        &format!("http://{address}/v1/"),
    )
    .unwrap();
    let gateway = Arc::new(
        OpenAiGateway::new(
            client,
            GatewayConfig::new("synthetic-cache-model", ModelRole::Worker),
        )
        .unwrap(),
    );
    let custom_policy = excalibur_context::SessionPolicy {
        model_window: 65_536,
        output_reserve: 4096,
        compaction_cap: 25_000,
        cache_ttl_ms: 3_600_000,
        conversation: true,
        counter: excalibur_context::TokenCounter::for_model("synthetic-custom-counter"),
        ..Default::default()
    };
    let expected_policy = serde_json::to_value(&custom_policy).unwrap();
    let engine = Engine::new(journal.clone(), gateway, Arc::new(FixtureTools))
        .with_context_policy(custom_policy);
    let spec = RunSpec {
        context: ContextInput::new(
            "Synthetic cache instructions",
            "Compute synthetic cache result",
            "fixed",
        ),
        budget: Budget::default(),
        criteria: vec![Criterion::ToolSuccess {
            name: "fixture".into(),
        }],
    };
    let run = engine
        .create("record-cached-http", spec.clone())
        .await
        .unwrap();
    assert_eq!(engine.run(&run).await.unwrap().status, Status::Completed);
    let sent = server.await.unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert_eq!(sent[0]["prompt_cache_key"], sent[1]["prompt_cache_key"]);
    let tape = ReplayTape::from_journal(&journal, &run).await.unwrap();
    assert_eq!(tape.entries.len(), 2);
    assert!(tape.entries.iter().all(|e| e.request.cache.is_some()));
    let expected = tape
        .entries
        .iter()
        .map(|e| e.request.clone())
        .collect::<Vec<_>>();
    let replay = Arc::new(ReplayGateway::new(tape).unwrap());
    assert_eq!(
        replay.context_policy_override(),
        Some(expected_policy.clone())
    );
    assert_eq!(replay.request_cache_override(0), expected[0].cache);
    assert_eq!(replay.request_cache_override(1), expected[1].cache);
    assert!(replay.request_cache_override(2).is_none());
    let engine = Engine::new(journal.clone(), replay.clone(), Arc::new(FixtureTools));
    let replay_run = engine.create("replay-cached-http", spec).await.unwrap();
    let result = engine.run(&replay_run).await.unwrap();
    assert_eq!(
        result.status,
        Status::Completed,
        "{:?}",
        replay.divergence()
    );
    assert!(replay.divergence().is_none());
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    let events = journal.replay(&replay_run, 0).await.unwrap();
    let created = events.iter().find(|e| e.kind == "run_started").unwrap();
    assert_eq!(created.payload["context_policy"], expected_policy);
    let requests = events
        .iter()
        .filter(|e| e.kind == "model_request")
        .map(|e| serde_json::from_value::<ModelRequest>(e.payload["request"].clone()).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(requests, expected);
    for event in events.iter().filter(|e| e.kind == "model_response") {
        assert_eq!(event.payload["metrics"]["provider_calls"], 0);
        assert_eq!(event.payload["metrics"]["usage"]["input_tokens"], 0);
        assert!(
            event.payload["metrics"]["recorded_usage"]["input_tokens"]
                .as_u64()
                .unwrap()
                > 0
        );
    }
    db.shutdown().await.unwrap();
}
struct NativeDeferredFixtureTools(Arc<AtomicUsize>);
impl ToolRuntime for NativeDeferredFixtureTools {
    fn definitions(&self) -> Vec<ToolDefinition> {
        vec![]
    }
    fn deferred_definitions<'a>(
        &'a self,
        _: &'a str,
    ) -> BoxFuture<'a, Result<Vec<ToolDefinition>, String>> {
        Box::pin(async move {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(FixtureTools.definitions())
        })
    }
    fn prepare<'a>(
        &'a self,
        run: &'a str,
        call: ToolCall,
    ) -> BoxFuture<'a, Result<PreparedTool, String>> {
        FixtureTools.prepare(run, call)
    }
    fn execute<'a>(
        &'a self,
        run: &'a str,
        key: &'a str,
        tool: &'a PreparedTool,
    ) -> BoxFuture<'a, ToolResult> {
        FixtureTools.execute(run, key, tool)
    }
    fn reconcile<'a>(
        &'a self,
        run: &'a str,
        key: &'a str,
        tool: &'a PreparedTool,
    ) -> BoxFuture<'a, Reconcile> {
        FixtureTools.reconcile(run, key, tool)
    }
}
#[tokio::test]
async fn native_deferred_engine_pins_catalog_and_replays_references_without_rediscovery() {
    use excalibur_providers::{Provider, ProviderClient, Secret};
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
    };
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let http_calls = Arc::new(AtomicUsize::new(0));
    let observed = http_calls.clone();
    let server = tokio::spawn(async move {
        let replies = [
            json!({"content":[{"type":"server_tool_use","id":"srvtoolu_native","name":"tool_search_tool_bm25","input":{"query":"synthetic result","limit":5}},{"type":"tool_search_tool_result","tool_use_id":"srvtoolu_native","content":{"type":"tool_search_tool_search_result","tool_references":[{"type":"tool_reference","tool_name":"fixture"}]}},{"type":"tool_use","id":"native-call","name":"fixture","input":{}}],"stop_reason":"tool_use","usage":{"input_tokens":100,"cache_read_input_tokens":0,"cache_creation_input_tokens":0,"output_tokens":5}}),
            json!({"content":[{"type":"text","text":"Synthetic done"}],"stop_reason":"end_turn","usage":{"input_tokens":30,"cache_read_input_tokens":80,"cache_creation_input_tokens":0,"output_tokens":3}}),
        ];
        let mut requests = vec![];
        for reply in replies {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buffer = Vec::new();
            loop {
                let mut bytes = [0; 4096];
                let n = socket.read(&mut bytes).await.unwrap();
                assert!(n > 0);
                buffer.extend_from_slice(&bytes[..n]);
                if let Some(end) = buffer.windows(4).position(|w| w == b"\r\n\r\n") {
                    let head = String::from_utf8_lossy(&buffer[..end]);
                    let length: usize = head
                        .lines()
                        .find_map(|l| {
                            l.to_ascii_lowercase()
                                .strip_prefix("content-length:")
                                .map(|v| v.trim().parse().unwrap())
                        })
                        .unwrap_or(0);
                    if buffer.len() >= end + 4 + length {
                        requests.push(
                            serde_json::from_slice::<Value>(&buffer[end + 4..end + 4 + length])
                                .unwrap(),
                        );
                        break;
                    }
                }
            }
            observed.fetch_add(1, Ordering::SeqCst);
            let body = reply.to_string();
            socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len()).as_bytes()).await.unwrap();
        }
        requests
    });
    let temp = tempfile::tempdir().unwrap();
    let db = Database::open(
        temp.path().join("main.db"),
        DatabaseKind::Main,
        StorageConfig::default(),
    )
    .unwrap();
    let journal = Journal::new(db.clone()).await.unwrap();
    let discoveries = Arc::new(AtomicUsize::new(0));
    let tools = Arc::new(NativeDeferredFixtureTools(discoveries.clone()));
    let gateway = Arc::new(
        OpenAiGateway::new(
            ProviderClient::with_base_url(
                Provider::Anthropic,
                Secret::new("synthetic").unwrap(),
                &format!("http://{address}/v1/"),
            )
            .unwrap(),
            GatewayConfig::new("claude-sonnet-4-6", ModelRole::Worker),
        )
        .unwrap(),
    );
    let engine = Engine::new(journal.clone(), gateway, tools.clone());
    let spec = RunSpec {
        context: ContextInput::new("Synthetic core", "Compute synthetic result", "fixed"),
        budget: Budget::default(),
        criteria: vec![Criterion::ToolSuccess {
            name: "fixture".into(),
        }],
    };
    let run = engine
        .create("native-deferred-record", spec.clone())
        .await
        .unwrap();
    assert_eq!(engine.run(&run).await.unwrap().status, Status::Completed);
    assert_eq!(discoveries.load(Ordering::SeqCst), 1);
    let sent = server.await.unwrap();
    assert_eq!(sent.len(), 2);
    assert!(!sent[0]["messages"].as_array().unwrap().is_empty());
    assert!(
        sent[0]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .any(|t| t["name"] == "fixture" && t["defer_loading"] == true)
    );
    let tape = ReplayTape::from_journal(&journal, &run).await.unwrap();
    assert!(
        tape.entries[0]
            .request
            .tools
            .iter()
            .all(|t| t.name != "fixture")
    );
    assert_eq!(
        tape.entries[0].request.deferred_tools,
        FixtureTools.definitions()
    );
    assert_eq!(
        tape.entries[0].request.deferred_tools,
        tape.entries[1].request.deferred_tools
    );
    let expected = tape
        .entries
        .iter()
        .map(|e| e.request.clone())
        .collect::<Vec<_>>();
    let replay = Arc::new(ReplayGateway::new(tape).unwrap());
    assert!(replay.native_deferred_tools());
    assert_eq!(
        replay.deferred_catalog_override(),
        Some(FixtureTools.definitions())
    );
    let engine = Engine::new(journal.clone(), replay.clone(), tools);
    let replay_run = engine.create("native-deferred-replay", spec).await.unwrap();
    assert_eq!(
        engine.run(&replay_run).await.unwrap().status,
        Status::Completed,
        "{:?}",
        replay.divergence()
    );
    assert!(replay.divergence().is_none());
    assert_eq!(discoveries.load(Ordering::SeqCst), 1);
    assert_eq!(http_calls.load(Ordering::SeqCst), 2);
    let events = journal.replay(&replay_run, 0).await.unwrap();
    let requests = events
        .iter()
        .filter(|e| e.kind == "model_request")
        .map(|e| serde_json::from_value::<ModelRequest>(e.payload["request"].clone()).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(requests, expected);
    assert!(
        events
            .iter()
            .filter(|e| e.kind == "model_response")
            .all(|e| e.payload["metrics"]["provider_calls"] == 0)
    );
    db.shutdown().await.unwrap();
}
