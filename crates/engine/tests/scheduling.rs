use excalibur_context::ContextInput;
use excalibur_engine::{Budget, Criterion, Engine, RunSpec, Status};
use excalibur_journal::Journal;
use excalibur_models::{ModelError, ModelGateway};
use excalibur_store::{Database, DatabaseKind, StorageConfig};
use excalibur_types::runtime::*;
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};
use tokio::sync::{Barrier, Notify};
// Global exclusive resource locks are process-wide by design. Serialize fixtures,
// while each fixture drives its own intentional concurrent Engine instances.
static TEST_SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
fn call(id: &str, resource: &str, mode: &str) -> ToolCall {
    ToolCall {
        id: id.into(),
        name: "probe".into(),
        arguments: json!({"resource":resource,"mode":mode}).to_string(),
    }
}
fn response(calls: Vec<ToolCall>) -> ModelResponse {
    ModelResponse {
        message: Message {
            provider_state: None,
            role: "assistant".into(),
            content: if calls.is_empty() {
                "Verified synthetic result".into()
            } else {
                String::new()
            },
            tool_calls: calls,
            tool_call_id: None,
        },
        usage: Usage::default(),
    }
}
fn spec() -> RunSpec {
    RunSpec {
        context: ContextInput::new(
            "Synthetic deterministic fixture",
            "Run probes",
            "2026-10-29T00:00:00Z",
        ),
        budget: Budget {
            max_steps: 10,
            ..Default::default()
        },
        criteria: vec![Criterion::ToolSuccess {
            name: "probe".into(),
        }],
    }
}
async fn database() -> (tempfile::TempDir, Database, Journal) {
    let root = tempfile::tempdir().unwrap();
    let db = Database::open(
        root.path().join("excalibur.db"),
        DatabaseKind::Main,
        StorageConfig::default(),
    )
    .unwrap();
    let journal = Journal::new(db.clone()).await.unwrap();
    (root, db, journal)
}
struct ScriptModel {
    calls: Vec<ToolCall>,
    identity: String,
    count: AtomicUsize,
    requests: Mutex<Vec<ModelRequest>>,
}
impl ScriptModel {
    fn new(calls: Vec<ToolCall>, identity: &str) -> Self {
        Self {
            calls,
            identity: identity.into(),
            count: AtomicUsize::new(0),
            requests: Mutex::new(Vec::new()),
        }
    }
}
impl ModelGateway for ScriptModel {
    fn identity(&self) -> String {
        self.identity.clone()
    }
    fn complete<'a>(
        &'a self,
        request: ModelRequest,
    ) -> BoxFuture<'a, Result<ModelResponse, ModelError>> {
        Box::pin(async move {
            self.count.fetch_add(1, Ordering::SeqCst);
            let done = request.messages.iter().any(|m| m.role == "tool");
            self.requests.lock().unwrap().push(request);
            Ok(response(if done { vec![] } else { self.calls.clone() }))
        })
    }
}
#[derive(Default)]
struct Activity {
    active: usize,
    exclusive: bool,
    resource_active: BTreeMap<String, usize>,
    resource_peak: BTreeMap<String, usize>,
    violations: Vec<String>,
    order: Vec<String>,
    peak: usize,
}
struct Probes {
    state: Mutex<Activity>,
    before: Barrier,
    after: Barrier,
    held_entered: Notify,
    release_held: Notify,
    independent_entered: Notify,
    effects: AtomicUsize,
}
impl Probes {
    fn new() -> Self {
        Self {
            state: Mutex::new(Activity::default()),
            before: Barrier::new(2),
            after: Barrier::new(2),
            held_entered: Notify::new(),
            release_held: Notify::new(),
            independent_entered: Notify::new(),
            effects: AtomicUsize::new(0),
        }
    }
}
impl ToolRuntime for Probes {
    fn scheduling<'a>(
        &'a self,
        run: &'a str,
        call: &'a ToolCall,
    ) -> BoxFuture<'a, Option<ToolScheduling>> {
        Box::pin(async move {
            self.prepare(run, call.clone())
                .await
                .ok()
                .map(|tool| ToolScheduling::from_prepared(&tool))
        })
    }

    fn definitions(&self) -> Vec<ToolDefinition> {
        vec![ToolDefinition {
            name: "probe".into(),
            description: "Synthetic scheduling probe".into(),
            input_schema: json!({"type":"object"}),
        }]
    }
    fn prepare<'a>(
        &'a self,
        _run: &'a str,
        call: ToolCall,
    ) -> BoxFuture<'a, Result<PreparedTool, String>> {
        Box::pin(async move {
            let data: Value = serde_json::from_str(&call.arguments).map_err(|e| e.to_string())?;
            let exclusive = data["mode"] == "exclusive";
            Ok(PreparedTool {
                resources: vec![Resource {
                    key: data["resource"].as_str().unwrap().into(),
                    write: true,
                }],
                call,
                effect: Effect::Pure,
                exclusive,
                data,
            })
        })
    }
    fn execute<'a>(
        &'a self,
        _run: &'a str,
        _key: &'a str,
        tool: &'a PreparedTool,
    ) -> BoxFuture<'a, ToolResult> {
        Box::pin(async move {
            let resource = tool.data["resource"].as_str().unwrap().to_string();
            let mode = tool.data["mode"].as_str().unwrap();
            {
                let mut state = self.state.lock().unwrap();
                if state.exclusive || (tool.exclusive && state.active != 0) {
                    state
                        .violations
                        .push("exclusive tool overlapped another effect".into());
                }
                state.active += 1;
                state.peak = state.peak.max(state.active);
                if tool.exclusive {
                    state.exclusive = true;
                }
                let active = state.resource_active.entry(resource.clone()).or_default();
                *active += 1;
                let current = *active;
                let peak = state.resource_peak.entry(resource.clone()).or_default();
                *peak = (*peak).max(current);
                state.order.push(format!("enter:{}", tool.call.id));
            }
            match mode {
                "before" => {
                    self.before.wait().await;
                }
                "after" => {
                    self.after.wait().await;
                }
                "held" => {
                    self.held_entered.notify_one();
                    self.release_held.notified().await;
                }
                "independent" => {
                    self.independent_entered.notify_one();
                }
                _ => {
                    tokio::task::yield_now().await;
                }
            }
            self.effects.fetch_add(1, Ordering::SeqCst);
            {
                let mut state = self.state.lock().unwrap();
                state.active -= 1;
                *state.resource_active.get_mut(&resource).unwrap() -= 1;
                if tool.exclusive {
                    state.exclusive = false;
                }
                state.order.push(format!("exit:{}", tool.call.id));
            }
            ToolResult::ok(json!({"id":tool.call.id}))
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn same_resource_writes_serialize_and_exclusive_effect_runs_alone_between_parallel_batches() {
    let _serial = TEST_SERIAL.lock().await;
    let (_root, db, journal) = database().await;
    let tools = Arc::new(Probes::new());
    let model = Arc::new(ScriptModel::new(
        vec![
            call("a", "fixture:before-a", "before"),
            call("b", "fixture:before-b", "before"),
            call("write-1", "fixture:same-run-shared", "normal"),
            call("write-2", "fixture:same-run-shared", "normal"),
            call("exclusive", "fixture:exclusive", "exclusive"),
            call("c", "fixture:after-c", "after"),
            call("d", "fixture:after-d", "after"),
        ],
        "scheduling-profile",
    ));
    let engine = Engine::new(journal, model, tools.clone());
    let run = engine.create("ordered-batches", spec()).await.unwrap();
    let result = tokio::time::timeout(std::time::Duration::from_secs(20), engine.run(&run))
        .await
        .expect("barriers detect a scheduling deadlock")
        .unwrap();
    assert_eq!(result.status, Status::Completed);
    {
        let state = tools.state.lock().unwrap();
        assert!(state.violations.is_empty(), "{:?}", state.violations);
        assert!(
            state.peak >= 2,
            "barrier proves independent work actually overlaps"
        );
        assert_eq!(state.resource_peak["fixture:same-run-shared"], 1);
        let position = |item: &str| state.order.iter().position(|s| s == item).unwrap();
        assert!(position("exit:write-1") < position("enter:write-2"));
        assert!(position("exit:write-2") < position("enter:exclusive"));
        assert!(position("exit:exclusive") < position("enter:c"));
        assert!(position("exit:exclusive") < position("enter:d"));
    }
    db.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn resource_lock_is_shared_across_engine_instances_and_runs_without_blocking_independent_work()
 {
    let _serial = TEST_SERIAL.lock().await;
    let (_root, db, journal) = database().await;
    let tools = Arc::new(Probes::new());
    let first = Engine::new(
        journal.clone(),
        Arc::new(ScriptModel::new(
            vec![call("held", "fixture:cross-run-shared", "held")],
            "first-profile",
        )),
        tools.clone(),
    );
    let second = Engine::new(
        journal,
        Arc::new(ScriptModel::new(
            vec![
                call("contender", "fixture:cross-run-shared", "normal"),
                call("independent", "fixture:independent", "independent"),
            ],
            "second-profile",
        )),
        tools.clone(),
    );
    let run_a = first.create("cross-a", spec()).await.unwrap();
    let run_b = second.create("cross-b", spec()).await.unwrap();
    let first_task = tokio::spawn({
        let first = first.clone();
        async move { first.run(&run_a).await.unwrap() }
    });
    tools.held_entered.notified().await;
    let second_task = tokio::spawn({
        let second = second.clone();
        async move { second.run(&run_b).await.unwrap() }
    });
    tools.independent_entered.notified().await;
    {
        let state = tools.state.lock().unwrap();
        assert_eq!(state.resource_peak["fixture:cross-run-shared"], 1);
        assert!(
            !state.order.iter().any(|s| s == "enter:contender"),
            "contender is pending while disjoint work has reached its effect"
        );
    }
    tools.release_held.notify_one();
    assert_eq!(first_task.await.unwrap().status, Status::Completed);
    assert_eq!(second_task.await.unwrap().status, Status::Completed);
    {
        let state = tools.state.lock().unwrap();
        assert_eq!(state.resource_peak["fixture:cross-run-shared"], 1);
        let end = state.order.iter().position(|s| s == "exit:held").unwrap();
        let start = state
            .order
            .iter()
            .position(|s| s == "enter:contender")
            .unwrap();
        assert!(end < start);
    }
    db.shutdown().await.unwrap();
}

struct HeldModel {
    entered: Notify,
    release: Notify,
    calls: AtomicUsize,
}
impl ModelGateway for HeldModel {
    fn call_bound(&self, _: &ModelRequest) -> Option<excalibur_models::ModelCallBound> {
        Some(excalibur_models::ModelCallBound {
            max_tokens: 100,
            max_cost_microusd: 0,
        })
    }
    fn identity(&self) -> String {
        "held-model-profile".into()
    }
    fn complete<'a>(
        &'a self,
        request: ModelRequest,
    ) -> BoxFuture<'a, Result<ModelResponse, ModelError>> {
        Box::pin(async move {
            let n = self.calls.fetch_add(1, Ordering::SeqCst);
            if n == 0 {
                self.entered.notify_one();
                self.release.notified().await;
            }
            Ok(response(
                if request.messages.iter().any(|m| m.role == "tool") {
                    vec![]
                } else {
                    vec![call("effect", "fixture:pause-effect", "normal")]
                },
            ))
        })
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pause_cancels_unconfirmed_inference_without_effects_and_resume_reissues_only_inference() {
    let _serial = TEST_SERIAL.lock().await;
    let (_root, db, journal) = database().await;
    let model = Arc::new(HeldModel {
        entered: Notify::new(),
        release: Notify::new(),
        calls: AtomicUsize::new(0),
    });
    let tools = Arc::new(Probes::new());
    let engine = Engine::new(journal, model.clone(), tools.clone());
    let run = engine.create("paused-model", spec()).await.unwrap();
    let driver = tokio::spawn({
        let engine = engine.clone();
        let run = run.clone();
        async move { engine.run(&run).await.unwrap() }
    });
    model.entered.notified().await;
    engine.pause(&run).await.unwrap();
    let paused = tokio::time::timeout(std::time::Duration::from_secs(2), driver)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(paused.status, Status::Paused);
    assert!(paused.response.is_none());
    assert!(paused.usage.estimated);
    assert_eq!(
        paused
            .usage
            .input_tokens
            .saturating_add(paused.usage.output_tokens),
        100
    );
    assert_eq!(tools.effects.load(Ordering::SeqCst), 0);
    assert_eq!(model.calls.load(Ordering::SeqCst), 1);
    // Reconstruct the engine boundary from durable events before resuming.
    let restarted = Engine::new(engine.journal().clone(), model.clone(), tools.clone());
    assert!(restarted.load(&run).await.unwrap().response.is_none());
    restarted.resume(&run).await.unwrap();
    assert_eq!(restarted.run(&run).await.unwrap().status, Status::Completed);
    assert_eq!(tools.effects.load(Ordering::SeqCst), 1);
    assert_eq!(
        model.calls.load(Ordering::SeqCst),
        3,
        "unconfirmed inference is reissued once, then the final post-tool call"
    );
    let events = restarted.journal().replay(&run, 0).await.unwrap();
    assert_eq!(
        events
            .iter()
            .filter(|e| e.kind == "model_response" && e.key == "model:0:response")
            .count(),
        0
    );
    db.shutdown().await.unwrap();
}

#[tokio::test]
async fn received_only_directive_is_applied_once_before_model_and_changed_gateway_is_rejected() {
    let _serial = TEST_SERIAL.lock().await;
    let (_root, db, journal) = database().await;
    let model = Arc::new(ScriptModel::new(
        vec![call("effect", "fixture:directive", "normal")],
        "profile-original",
    ));
    let tools = Arc::new(Probes::new());
    let engine = Engine::new(journal.clone(), model.clone(), tools.clone());
    let run = engine.create("directive-recovery", spec()).await.unwrap();
    journal
        .append(
            &run,
            "directive_received",
            "directive:correction:received",
            json!({"id":"correction","text":"Preserve literal synthetic instruction"}),
        )
        .await
        .unwrap();
    let recovered = Engine::new(journal.clone(), model.clone(), tools.clone());
    let received = recovered.load(&run).await.unwrap();
    assert_eq!(received.pending_directives.len(), 1);
    assert_eq!(received.revision, 0);
    assert_eq!(recovered.run(&run).await.unwrap().status, Status::Completed);
    let final_state = recovered.load(&run).await.unwrap();
    assert_eq!(final_state.revision, 1);
    assert!(final_state.pending_directives.is_empty());
    assert!(
        model.requests.lock().unwrap()[0]
            .messages
            .iter()
            .any(|m| m.content == "Context update v1: Preserve literal synthetic instruction")
    );
    let events = journal.replay(&run, 0).await.unwrap();
    let applied = events
        .iter()
        .filter(|e| e.kind == "directive_applied")
        .collect::<Vec<_>>();
    assert_eq!(applied.len(), 1);
    assert!(
        applied[0].sequence
            < events
                .iter()
                .find(|e| e.kind == "model_request")
                .unwrap()
                .sequence
    );
    let pending = recovered
        .create("profile-not-started", spec())
        .await
        .unwrap();
    let changed = Arc::new(ScriptModel::new(vec![], "profile-changed"));
    let altered = Engine::new(journal.clone(), changed.clone(), tools);
    assert!(altered.run(&run).await.is_err());
    assert!(altered.run(&pending).await.is_err());
    assert!(altered.create("directive-recovery", spec()).await.is_err());
    assert_eq!(changed.count.load(Ordering::SeqCst), 0);
    assert_eq!(
        journal.replay(&pending, 0).await.unwrap().len(),
        2,
        "pending run retains its original seed and ready admission"
    );
    assert_eq!(
        journal.replay(&run, 0).await.unwrap().len(),
        events.len(),
        "profile mismatch creates no new durable effects"
    );
    db.shutdown().await.unwrap();
}
