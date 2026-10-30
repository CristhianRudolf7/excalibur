//! Harness integration suite (arch 16.10): local synthetic fixtures only.
//!
//! This suite exercises the web harness `ToolRuntime`, the tool catalog, the
//! manifests, reconciliation, cancellation and the eval profile against an
//! in-memory webd stand-in. No real Chrome, no network, no daemon.

use excalibur_tools::deferred::{CallerRole, ToolCatalog};
use excalibur_types::runtime::{
    BoxFuture, Effect, PreparedTool, Reconcile, ToolCall, ToolDefinition, ToolResult, ToolRuntime,
};
use excalibur_web_adapter::BrowserBackend;
use excalibur_web_adapter::harness::{
    WebHarness, browser_catalog, browser_manifest, cancellation_class, is_web_tool, search_catalog,
    search_manifest, static_effect, web_catalog,
};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    future::Future,
    pin::Pin,
    sync::{Arc, Mutex},
};

struct FakeOp {
    hash: String,
    state: String,
    result: Value,
}

struct FakeState {
    ops: HashMap<String, FakeOp>,
    sends: HashMap<String, usize>,
    order: Vec<String>,
    closed: Vec<String>,
    tabs: HashMap<String, Vec<Value>>,
    sessions: Vec<Value>,
    listeners_relaunched: Vec<String>,
    freeze_writes: bool,
}

impl FakeState {
    fn new() -> Self {
        Self {
            ops: HashMap::new(),
            sends: HashMap::new(),
            order: Vec::new(),
            closed: Vec::new(),
            tabs: HashMap::new(),
            sessions: Vec::new(),
            listeners_relaunched: Vec::new(),
            freeze_writes: false,
        }
    }
}

const WRITES: &[&str] = &[
    "session.open",
    "session.close",
    "session.suspend",
    "tab.new",
    "tab.switch",
    "tab.close",
    "nav.go",
    "nav.reload",
    "nav.back",
    "nav.forward",
    "act.perform",
    "act.stage_upload",
    "download",
    "emulate.viewport",
];

struct FakeBackend {
    state: Mutex<FakeState>,
}

impl FakeBackend {
    fn new() -> Self {
        Self {
            state: Mutex::new(FakeState::new()),
        }
    }

    fn seed_tab(&self, session: &str, tab: &str, url: &str) {
        let mut state = self.state.lock().unwrap();
        state
            .tabs
            .entry(session.into())
            .or_default()
            .push(json!({"id": tab, "url": url}));
        if !state.sessions.iter().any(|s| s["session_id"] == session) {
            state
                .sessions
                .push(json!({"session_id": session, "state": "ready"}));
        }
    }

    fn sends(&self, method: &str, key: &str) -> usize {
        let state = self.state.lock().unwrap();
        *state.sends.get(&format!("{method}#{key}")).unwrap_or(&0)
    }

    fn closed(&self) -> Vec<String> {
        self.state.lock().unwrap().closed.clone()
    }

    fn relaunched(&self) -> Vec<String> {
        self.state.lock().unwrap().listeners_relaunched.clone()
    }

    fn restart(&self) {
        let mut state = self.state.lock().unwrap();
        let listeners: Vec<String> = state
            .sessions
            .iter()
            .filter(|s| s.get("listen").and_then(Value::as_bool).unwrap_or(false))
            .filter_map(|s| {
                s.get("session_id")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
            })
            .collect();
        state.listeners_relaunched.extend(listeners);
    }
}

fn op_hash(method: &str, params: &Value) -> String {
    let mut canonical = params.clone();
    if let Value::Object(ref mut map) = canonical {
        map.remove("deadline_ms");
    }
    format!("{:x}", {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};
        let mut hasher = DefaultHasher::new();
        format!("{method}{canonical}").hash(&mut hasher);
        hasher.finish()
    })
}

impl BrowserBackend for FakeBackend {
    fn request<'a>(
        &'a self,
        method: &'a str,
        params: Value,
    ) -> Pin<Box<dyn Future<Output = Result<Value, String>> + Send + 'a>> {
        Box::pin(async move {
            let mut state = self.state.lock().unwrap();
            state.order.push(method.to_owned());
            if method == "op.get" {
                let key = params["op_key"].as_str().unwrap_or("").to_owned();
                let op = state.ops.get(&key).ok_or("unknown_operation: op_key")?;
                if op.state == "dispatched" {
                    return Ok(
                        json!({"state": "uncertain", "op_key": key, "reason": "webd_restarted_after_dispatch"}),
                    );
                }
                return Ok(json!({"state": op.state, "result": op.result}));
            }
            if method == "tab.list" {
                let sid = params["session_id"].as_str().unwrap_or("").to_owned();
                let tabs = state.tabs.get(&sid).cloned().unwrap_or_default();
                return Ok(json!({"tabs": tabs}));
            }
            if method == "session.list" {
                return Ok(json!({"sessions": state.sessions.clone()}));
            }
            if method == "session.close" {
                let sid = params["session_id"].as_str().unwrap_or("").to_owned();
                state.closed.push(sid.clone());
                state.sessions.retain(|s| s["session_id"] != sid);
                return Ok(json!({"session_id": sid, "state": "closed"}));
            }
            if method == "act.prepare" {
                let approval = params["request"]["approval"].as_bool().unwrap_or(false);
                let class = params["request"]["action"]
                    .as_str()
                    .unwrap_or("commit")
                    .to_owned();
                let mut plan = json!({"plan_id": "plan-1", "action_class": class});
                if approval {
                    plan["approval_required"] = json!(true);
                }
                return Ok(plan);
            }
            if method == "read.web_read" {
                let url = params["url"].as_str().unwrap_or("").to_owned();
                if url.contains("blocked.test") {
                    return Ok(json!({"state": "blocked", "blocked": {"kind": "needs_login"}}));
                }
                return Ok(
                    json!({"url": url, "render": "http", "markdown": "# local page",
                    "sources": [{"url": url}], "untrusted": true}),
                );
            }
            if method == "observe.snapshot" {
                let sid = params["session_id"].as_str().unwrap_or("").to_owned();
                let tid = params["tab_id"].as_str().unwrap_or("").to_owned();
                return Ok(json!({"snapshot_id": "snap-1", "title": "local",
                    "sources": [{"url": "http://127.0.0.1/"}],
                    "session_id": sid, "tab_id": tid, "untrusted": true}));
            }
            if WRITES.contains(&method) {
                let key = params["op_key"].as_str().unwrap_or("").to_owned();
                let hash = op_hash(method, &params);
                if let Some(previous) = state.ops.get(&key) {
                    if previous.hash != hash {
                        return Err(
                            "idempotency_conflict: key reused with different request".into()
                        );
                    }
                    if previous.state == "completed" || previous.state == "failed" {
                        return Ok(previous.result.clone());
                    }
                    if previous.state == "dispatched" {
                        return Err("browser effect uncertain; reconciliation required".into());
                    }
                } else {
                    state.ops.insert(
                        key.clone(),
                        FakeOp {
                            hash,
                            state: "received".into(),
                            result: Value::Null,
                        },
                    );
                }
                if state.freeze_writes {
                    state.ops.get_mut(&key).unwrap().state = "dispatched".into();
                    return Err("browser effect uncertain; reconciliation required".into());
                }
                let counter = state.sends.entry(format!("{method}#{key}")).or_insert(0);
                *counter += 1;
                let result = method_result(method, &params);
                state.ops.get_mut(&key).unwrap().state = "completed".into();
                state.ops.get_mut(&key).unwrap().result = result.clone();
                if method == "session.open" {
                    let sid = result["session_id"].as_str().unwrap_or("").to_owned();
                    state
                        .sessions
                        .push(json!({"session_id": sid, "state": "ready",
                        "profile": params["profile"].as_str().unwrap_or("ephemeral")}));
                }
                return Ok(result);
            }
            Ok(json!({"ok": true}))
        })
    }
}

fn method_result(method: &str, params: &Value) -> Value {
    match method {
        "session.open" => json!({"session_id": "s-1", "state": "ready",
            "profile": params["profile"].as_str().unwrap_or("ephemeral")}),
        "tab.new" => json!({"id": "t-1", "url": params["url"].as_str().unwrap_or("about:blank")}),
        "download" => json!({"file_name": "local.bin", "blob": {"sha256": "abc"}}),
        "act.perform" => json!({"applied": true, "plan_id": params["plan_id"].clone()}),
        _ => json!({"ok": true}),
    }
}

struct StubRuntime {
    defs: Vec<ToolDefinition>,
}

impl StubRuntime {
    fn new() -> Self {
        Self {
            defs: vec![
                ToolDefinition {
                    name: "exec".into(),
                    description: "run".into(),
                    input_schema: json!({"type": "object"}),
                },
                ToolDefinition {
                    name: "read_file".into(),
                    description: "read".into(),
                    input_schema: json!({"type": "object"}),
                },
            ],
        }
    }
}

impl ToolRuntime for StubRuntime {
    fn definitions(&self) -> Vec<ToolDefinition> {
        self.defs.clone()
    }
    fn prepare<'a>(
        &'a self,
        _run: &'a str,
        call: ToolCall,
    ) -> BoxFuture<'a, Result<PreparedTool, String>> {
        Box::pin(async move {
            Ok(PreparedTool {
                call,
                effect: Effect::Pure,
                resources: vec![],
                exclusive: false,
                data: json!({}),
            })
        })
    }
    fn execute<'a>(
        &'a self,
        _run: &'a str,
        _key: &'a str,
        tool: &'a PreparedTool,
    ) -> BoxFuture<'a, ToolResult> {
        Box::pin(async move { ToolResult::ok(json!({"stub": tool.call.name})) })
    }
    fn reconcile<'a>(
        &'a self,
        _run: &'a str,
        _key: &'a str,
        _tool: &'a PreparedTool,
    ) -> BoxFuture<'a, Reconcile> {
        Box::pin(async move { Reconcile::Retry })
    }
}

fn harness(backend: Arc<FakeBackend>, eval: bool) -> (WebHarness, Arc<FakeBackend>) {
    let inner: Arc<dyn ToolRuntime> = Arc::new(StubRuntime::new());
    let runtime = WebHarness::new(
        inner,
        backend.clone() as Arc<dyn BrowserBackend>,
        true,
        eval,
    );
    (runtime, backend)
}

fn call(name: &str, arguments: Value) -> ToolCall {
    ToolCall {
        id: format!("op-{name}"),
        name: name.into(),
        arguments: arguments.to_string(),
    }
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}

#[test]
fn harness_walkthrough_completes_with_evidence() {
    let rt = runtime();
    rt.block_on(async {
        let backend = Arc::new(FakeBackend::new());
        let (harness, backend) = harness(backend, false);
        let run = "run-walkthrough";

        let open = harness
            .prepare(
                run,
                call("browser_open_session", json!({"profile": "ephemeral"})),
            )
            .await
            .unwrap();
        assert_eq!(open.effect, Effect::Keyed);
        let opened = harness.execute(run, "op-browser_open_session", &open).await;
        assert!(opened.success);
        assert_eq!(opened.output["session_id"], json!("s-1"));

        let nav = harness
            .prepare(
                run,
                call(
                    "browser_navigate",
                    json!({"session_id": "s-1", "tab_id": "t-1", "url": "http://127.0.0.1/"}),
                ),
            )
            .await
            .unwrap();
        assert_eq!(nav.effect, Effect::Idempotent);
        backend.seed_tab("s-1", "t-1", "http://127.0.0.1/");
        let navigated = harness.execute(run, "op-nav", &nav).await;
        assert!(navigated.success);

        let wait = harness
            .prepare(
                run,
                call(
                    "browser_wait_ready",
                    json!({"session_id": "s-1", "tab_id": "t-1"}),
                ),
            )
            .await
            .unwrap();
        let waited = harness.execute(run, "op-wait", &wait).await;
        assert!(waited.success);

        let snap = harness
            .prepare(
                run,
                call(
                    "browser_snapshot",
                    json!({"session_id": "s-1", "tab_id": "t-1"}),
                ),
            )
            .await
            .unwrap();
        let snap_result = harness.execute(run, "op-snap", &snap).await;
        assert!(snap_result.success);
        assert!(snap_result.output.get("sources").is_some());

        let act = harness
            .prepare(
                run,
                call(
                    "browser_act",
                    json!({"session_id": "s-1", "tab_id": "t-1",
            "snapshot_id": "snap-1", "request": {"action": "type", "ref": "r1", "text": "hello"}}),
                ),
            )
            .await
            .unwrap();
        assert_eq!(act.effect, Effect::Idempotent);
        let acted = harness.execute(run, "op-act", &act).await;
        assert!(acted.success);

        let read = harness
            .prepare(
                run,
                call("web_read", json!({"url": "http://127.0.0.1/page"})),
            )
            .await
            .unwrap();
        let read_result = harness.execute(run, "op-read", &read).await;
        assert!(read_result.success);
        assert_eq!(read_result.output["untrusted"], json!(true));

        let live = harness.live_status(run);
        assert!(live.iter().any(|v| v["adapter"] == json!("web")));
    });
}

#[test]
fn harness_crash_before_result_reconciles_done_without_resend() {
    let rt = runtime();
    rt.block_on(async {
        let backend = Arc::new(FakeBackend::new());
        let run = "run-crash";
        let commit = call(
            "browser_act",
            json!({"session_id": "s-1", "tab_id": "t-1", "snapshot_id": "snap-1",
            "op_key": "op-commit", "request": {"action": "click", "ref": "r2"}}),
        );
        let prepared = {
            let (harness, _) = harness(backend.clone(), false);
            let prepared = harness.prepare(run, commit).await.unwrap();
            assert_eq!(prepared.effect, Effect::NonIdempotent);
            let done = harness.execute(run, "op-commit", &prepared).await;
            assert!(done.success);
            prepared
        };
        assert_eq!(backend.sends("act.perform", "op-commit"), 1);
        let (restarted, _) = harness(backend.clone(), false);
        match restarted.reconcile(run, "op-commit", &prepared).await {
            Reconcile::Done(result) => assert!(result.success),
            other => panic!("expected Done, got {other:?}"),
        }
        assert_eq!(backend.sends("act.perform", "op-commit"), 1);
    });
}

#[test]
fn webd_restart_turns_dispatched_commit_uncertain() {
    let rt = runtime();
    rt.block_on(async {
        let backend = Arc::new(FakeBackend::new());
        backend.state.lock().unwrap().freeze_writes = true;
        let run = "run-webd-kill";
        let (harness, _) = harness(backend.clone(), false);
        let commit = harness
            .prepare(
                run,
                call(
                    "browser_act",
                    json!({"session_id": "s-1", "tab_id": "t-1",
            "snapshot_id": "snap-1", "request": {"action": "click", "ref": "r9"}}),
                ),
            )
            .await
            .unwrap();
        let failed = harness.execute(run, "op-doomed", &commit).await;
        assert!(!failed.success);
        backend.restart();
        match harness.reconcile(run, "op-doomed", &commit).await {
            Reconcile::Uncertain(_) => {}
            other => panic!("expected Uncertain, got {other:?}"),
        }
        assert_eq!(backend.sends("act.perform", "op-doomed"), 0);
    });
}

#[test]
fn webd_restart_relaunches_listening_sessions() {
    let backend = Arc::new(FakeBackend::new());
    backend
        .state
        .lock()
        .unwrap()
        .sessions
        .push(json!({"session_id": "s-listen", "state": "listening", "listen": true}));
    backend.restart();
    assert_eq!(backend.relaunched(), vec!["s-listen".to_owned()]);
}

#[test]
fn cancellation_closes_ephemeral_and_keeps_persistent() {
    let rt = runtime();
    rt.block_on(async {
        let backend = Arc::new(FakeBackend::new());
        let (harness, backend) = harness(backend, false);
        let run = "run-cancel";

        let open_ephemeral = harness
            .prepare(
                run,
                call("browser_open_session", json!({"profile": "ephemeral"})),
            )
            .await
            .unwrap();
        let opened = harness.execute(run, "op-open-eph", &open_ephemeral).await;
        assert!(opened.success);

        let nav = harness
            .prepare(
                run,
                call(
                    "browser_navigate",
                    json!({"session_id": "s-1", "tab_id": "t-1", "url": "http://127.0.0.1/"}),
                ),
            )
            .await
            .unwrap();
        let graceful = harness
            .interrupt_execution(run, true, Some(&nav))
            .await
            .unwrap();
        assert!(graceful);
        assert!(backend.closed().contains(&"s-1".to_owned()));

        let immediate = harness
            .prepare(
                run,
                call(
                    "browser_snapshot",
                    json!({"session_id": "s-1", "tab_id": "t-1"}),
                ),
            )
            .await
            .unwrap();
        assert!(
            harness
                .interrupt_execution(run, false, Some(&immediate))
                .await
                .unwrap()
        );

        harness.execution_state(run, "paused", None).await.unwrap();
        assert!(
            harness
                .live_status(run)
                .iter()
                .all(|v| v.get("adapter") != Some(&json!("web")))
        );
    });
    assert_eq!(
        cancellation_class("browser_wait_ready"),
        cancellation_class("web_read")
    );
    assert!(matches!(
        cancellation_class("browser_navigate"),
        excalibur_tools::deferred::CancellationClass::Graceful
    ));
    assert!(matches!(static_effect("browser_download"), Effect::Keyed));
}

#[test]
fn catalog_retrieval_returns_snapshot_and_act_in_top_five() {
    let catalog = ToolCatalog::new(web_catalog()).unwrap();
    assert_eq!(catalog.names().len(), 14);
    let snapshot = catalog
        .search(
            "take a snapshot capture of the page",
            CallerRole::Worker,
            None,
        )
        .unwrap();
    let names: Vec<&str> = snapshot.tools.iter().map(|t| t.name.as_str()).collect();
    assert!(
        names.contains(&"browser_snapshot"),
        "snapshot missing: {names:?}"
    );
    let act = catalog
        .search("click the button in the form", CallerRole::Worker, None)
        .unwrap();
    let names: Vec<&str> = act.tools.iter().map(|t| t.name.as_str()).collect();
    assert!(names.contains(&"browser_act"), "act missing: {names:?}");
    let first = catalog.hash();
    let _ = catalog
        .search(
            "take a snapshot capture of the page",
            CallerRole::Worker,
            None,
        )
        .unwrap();
    assert_eq!(catalog.hash(), first);
    assert!(is_web_tool("browser_snapshot"));
    assert!(!is_web_tool("exec"));
}

#[test]
fn catalog_metadata_matches_harness_contract() {
    for tool in web_catalog() {
        assert!(tool.name.len() <= 64);
        assert!(!tool.description.is_empty());
        assert!(!tool.platforms.is_empty());
        assert!((256..=1_048_576).contains(&tool.output_budget_bytes));
    }
    let browser = browser_catalog();
    assert_eq!(browser.len(), 12);
    let search = search_catalog();
    assert_eq!(search.len(), 2);
    assert!(search.iter().all(|t| t.fast));
    let status = browser.iter().find(|t| t.name == "browser_status").unwrap();
    assert!(status.fast);
    assert_eq!(status.timeout_ms, 5_000);
    let act = browser.iter().find(|t| t.name == "browser_act").unwrap();
    assert_eq!(act.timeout_ms, 20_000);
    assert!(matches!(
        act.risk,
        excalibur_tools::deferred::ToolRisk::High
    ));
    assert!(browser_manifest()["operations"].as_array().unwrap().len() == 12);
    assert!(search_manifest()["operations"].as_array().unwrap().len() == 2);
}

#[test]
fn eval_profile_denies_persistent_and_benchmark_and_fails_blocks() {
    let rt = runtime();
    rt.block_on(async {
        let backend = Arc::new(FakeBackend::new());
        let (harness, _) = harness(backend, true);
        let run = "run-eval";

        let persistent = harness.prepare(run, call("browser_open_session", json!({"profile": "persistent"}))).await;
        assert!(persistent.is_err());

        let ephemeral = harness.prepare(run, call("browser_open_session", json!({}))).await;
        assert!(ephemeral.is_ok());

        let benchmark = harness.prepare(run, call("web_read", json!({"url": "https://tbench.ai/secret-task"}))).await;
        assert!(benchmark.is_err());

        let nav_benchmark = harness.prepare(run, call("browser_navigate",
            json!({"session_id": "s-1", "tab_id": "t-1", "url": "https://harborframework.com/x"}))).await;
        assert!(nav_benchmark.is_err());

        let blocked = harness.prepare(run, call("web_read", json!({"url": "http://blocked.test/login"}))).await.unwrap();
        let result = harness.execute(run, "op-blocked", &blocked).await;
        assert!(!result.success);
    });
}

#[test]
fn idempotency_conflict_on_reused_key_with_different_request() {
    let rt = runtime();
    rt.block_on(async {
        let backend = Arc::new(FakeBackend::new());
        let (harness, _) = harness(backend, false);
        let run = "run-idem";
        let first = harness.prepare(run, call("browser_navigate",
            json!({"session_id": "s-1", "tab_id": "t-1", "url": "http://127.0.0.1/a", "op_key": "fixed-key"}))).await.unwrap();
        assert!(harness.execute(run, "fixed-key", &first).await.success);
        let conflict = harness.prepare(run, call("browser_navigate",
            json!({"session_id": "s-1", "tab_id": "t-1", "url": "http://127.0.0.1/b", "op_key": "fixed-key"}))).await.unwrap();
        let result = harness.execute(run, "fixed-key", &conflict).await;
        assert!(!result.success);
        assert!(result.output.to_string().contains("idempotency_conflict"));
        let replay = harness.prepare(run, call("browser_navigate",
            json!({"session_id": "s-1", "tab_id": "t-1", "url": "http://127.0.0.1/a", "op_key": "fixed-key"}))).await.unwrap();
        assert!(harness.execute(run, "fixed-key", &replay).await.success);
    });
}

#[test]
fn manifests_declare_browser_and_search() {
    let browser = browser_manifest();
    assert_eq!(browser["capability"], json!("browser"));
    assert_eq!(browser["implementation"], json!("excalibur-webd"));
    assert_eq!(browser["engine"]["headless"], json!("new"));
    let unsupported: Vec<&str> = browser["unsupported"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|u| u["operation"].as_str())
        .collect();
    assert!(unsupported.contains(&"browser_routine"));
    let kinds = browser["blocked_kinds"].as_array().unwrap();
    assert!(kinds.iter().any(|k| k == "human_control"));
    let search = search_manifest();
    assert_eq!(search["operations"], json!(["web_search", "web_read"]));
    assert_eq!(search["limits"]["k_max"], json!(50));
}
