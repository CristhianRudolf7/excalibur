use excalibur_engine::Status;
use excalibur_eval::{
    ExecOptions, Execution, ExecutionProfile, exit_code,
    profile::{FrozenProfile, ProfileOverrides, hash},
};
use excalibur_models::{ModelError, ModelGateway};
use excalibur_security::Autonomy;
use excalibur_types::runtime::*;
use excalibur_vault::Redactor;
use serde_json::{Value, json};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};
struct Script {
    calls: AtomicUsize,
    requests: Mutex<Vec<ModelRequest>>,
    answers: Mutex<Vec<ModelResponse>>,
}
impl Script {
    fn new(mut answers: Vec<ModelResponse>) -> Self {
        answers.reverse();
        Self {
            calls: AtomicUsize::new(0),
            requests: Mutex::new(vec![]),
            answers: Mutex::new(answers),
        }
    }
}
impl ModelGateway for Script {
    fn complete<'a>(
        &'a self,
        request: ModelRequest,
    ) -> BoxFuture<'a, Result<ModelResponse, ModelError>> {
        Box::pin(async move {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.requests.lock().unwrap().push(request);
            self.answers
                .lock()
                .unwrap()
                .pop()
                .ok_or(ModelError::Validation("script exhausted".into()))
        })
    }
}
fn answer(text: &str) -> ModelResponse {
    ModelResponse {
        message: Message::text("assistant", text),
        usage: Usage {
            input_tokens: 7,
            output_tokens: 3,
            ..Default::default()
        },
    }
}
fn options(temp: &tempfile::TempDir, name: &str) -> ExecOptions {
    let root = temp.path().join("project");
    std::fs::create_dir_all(&root).unwrap();
    ExecOptions::new("Return a checked JSON answer", root, temp.path().join(name))
}
fn read(path: impl AsRef<std::path::Path>) -> Value {
    serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap()
}
#[test]
fn every_root_state_has_the_declared_exit_code_and_profile_permissions_are_frozen() {
    for (state, code) in [
        (Status::Completed, 0),
        (Status::CompletedWithPending, 2),
        (Status::Blocked, 3),
        (Status::Paused, 4),
        (Status::Failed, 5),
        (Status::Cancelled, 130),
        (Status::Running, 1),
    ] {
        assert_eq!(exit_code(&state), code);
    }
    let profile = FrozenProfile::evaluation(64, None).unwrap();
    assert_eq!(profile.max_active_children, 2);
    assert_eq!(profile.timezone, "UTC");
    assert_eq!(profile.language, "en");
    for denied in [
        "memory_search",
        "ask_user",
        "settings",
        "secrets",
        "send_message",
        "github",
        "agenda",
        "gpu_training",
        "nodes",
    ] {
        assert!(!profile.allows_tool(denied));
    }
    for name in [
        "task_db",
        "exec",
        "read_file",
        "search_skills",
        "load_skill",
        "subagent",
    ] {
        assert!(profile.allows_tool(name));
    }
    assert_eq!(
        hash(&profile),
        hash(&FrozenProfile::evaluation(2, None).unwrap())
    );
    let changed = FrozenProfile::evaluation(
        2,
        Some(ProfileOverrides {
            language: Some("es".into()),
            ..Default::default()
        }),
    )
    .unwrap();
    assert_ne!(hash(&profile), hash(&changed));
    assert!(
        profile
            .validate_stores(Some(std::path::Path::new("/memory")), None, None)
            .is_err()
    );
    assert!(
        profile
            .validate_stores(None, Some(std::path::Path::new("/runtime/skills")), None)
            .is_err()
    );
}
#[tokio::test]
async fn noninteractive_denial_returns_to_model_without_approval_and_final_output_matches_schema() {
    let temp = tempfile::tempdir().unwrap();
    let mut opts = options(&temp, "logs");
    opts.autonomy = Autonomy::Consultative;
    opts.output_schema = Some(
        json!({"type":"object","required":["answer"],"properties":{"answer":{"type":"integer"}},"additionalProperties":false}),
    );
    let call = ToolCall {
        id: "write".into(),
        name: "write_file".into(),
        arguments: json!({"path":"denied.txt","content":"must never appear"}).to_string(),
    };
    let first = ModelResponse {
        message: Message {
            role: "assistant".into(),
            content: String::new(),
            tool_calls: vec![call],
            tool_call_id: None,
            provider_state: None,
        },
        usage: Usage::default(),
    };
    let model = Arc::new(Script::new(vec![first, answer("{\"answer\":42}")]));
    let execution = Execution::prepare(opts, model.clone(), Redactor::default())
        .await
        .unwrap();
    let telemetry = execution.telemetry.clone();
    let outcome = execution.run(None).await.unwrap();
    assert_eq!(outcome.exit_code, 0, "{outcome:?}");
    assert_eq!(outcome.final_output.as_deref(), Some("{\"answer\":42}"));
    assert!(!temp.path().join("project/denied.txt").exists());
    {
        let requests = model.requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        let result = requests[1]
            .messages
            .iter()
            .find(|m| m.role == "tool")
            .unwrap();
        assert!(
            result
                .content
                .contains("noninteractive policy denied approval")
        );
    }
    let events = std::fs::read_to_string(temp.path().join("logs/excalibur/events.jsonl")).unwrap();
    assert!(!events.contains("approval_pending"));
    let metrics = read(temp.path().join("logs/excalibur/metrics.json"));
    assert_eq!(metrics["model_calls"], 2);
    assert_eq!(metrics["exit_code"], 0);
    assert_eq!(metrics["tokens"]["input_tokens"], 7); // first response explicitly reports zero
    assert!(temp.path().join("logs/trajectory.json").exists());
    assert!(temp.path().join("logs/excalibur/replay.json").exists());
    telemetry.shutdown().await.unwrap();
}
#[tokio::test]
async fn missing_mandatory_input_blocks_before_inference_but_has_valid_partial_artifacts() {
    let temp = tempfile::tempdir().unwrap();
    let mut opts = options(&temp, "blocked");
    opts.required_inputs.insert("destination".into(), None);
    let model = Arc::new(Script::new(vec![]));
    let execution = Execution::prepare(opts, model.clone(), Redactor::default())
        .await
        .unwrap();
    let telemetry = execution.telemetry.clone();
    let outcome = execution.run(None).await.unwrap();
    assert_eq!(outcome.exit_code, 3);
    assert_eq!(model.calls.load(Ordering::SeqCst), 0);
    let metrics = read(temp.path().join("blocked/excalibur/metrics.json"));
    assert_eq!(metrics["exit_code"], 3);
    assert_eq!(metrics["model_calls"], 0);
    assert_eq!(
        read(temp.path().join("blocked/trajectory.json"))["schema_version"],
        "ATIF-v1.8"
    );
    assert_eq!(
        read(temp.path().join("blocked/excalibur/replay-export.json"))["available"],
        false
    );
    telemetry.shutdown().await.unwrap();
}
#[tokio::test]
async fn two_episodes_start_fresh_with_identical_catalogs_and_cannot_reuse_nonempty_data() {
    let temp = tempfile::tempdir().unwrap();
    let first = options(&temp, "one");
    let second = options(&temp, "two");
    for opts in [first.clone(), second] {
        let e = Execution::prepare(
            opts,
            Arc::new(Script::new(vec![answer("done")])),
            Redactor::default(),
        )
        .await
        .unwrap();
        let t = e.telemetry.clone();
        assert_eq!(e.run(None).await.unwrap().exit_code, 0);
        t.shutdown().await.unwrap();
    }
    let a = read(temp.path().join("one/excalibur/metadata.json"));
    let b = read(temp.path().join("two/excalibur/metadata.json"));
    assert_eq!(a["tool_catalog_hash"], b["tool_catalog_hash"]);
    assert_eq!(a["skill_catalog_hash"], b["skill_catalog_hash"]);
    assert!(
        Execution::prepare(first, Arc::new(Script::new(vec![])), Redactor::default())
            .await
            .is_err()
    );
}
#[tokio::test]
async fn eval_rejects_external_stores_before_creating_any_episode_database() {
    let temp = tempfile::tempdir().unwrap();
    let mut opts = options(&temp, "eval");
    opts.profile = ExecutionProfile::Eval;
    opts.memory_store = Some(temp.path().join("persistent-memory"));
    let model = Arc::new(Script::new(vec![]));
    assert!(
        Execution::prepare(opts, model.clone(), Redactor::default())
            .await
            .is_err()
    );
    assert!(!temp.path().join("eval/excalibur/main.db").exists());
    assert_eq!(model.calls.load(Ordering::SeqCst), 0);
}
struct Held {
    entered: tokio::sync::Notify,
}
impl ModelGateway for Held {
    fn complete<'a>(&'a self, _: ModelRequest) -> BoxFuture<'a, Result<ModelResponse, ModelError>> {
        Box::pin(async move {
            self.entered.notify_one();
            std::future::pending().await
        })
    }
}
#[tokio::test]
async fn cancellation_during_inference_has_exit_130_and_incremental_trajectory() {
    let temp = tempfile::tempdir().unwrap();
    let opts = options(&temp, "cancel");
    let model = Arc::new(Held {
        entered: tokio::sync::Notify::new(),
    });
    let e = Execution::prepare(opts, model.clone(), Redactor::default())
        .await
        .unwrap();
    let t = e.telemetry.clone();
    let (send, recv) = tokio::sync::watch::channel(false);
    let task = tokio::spawn(e.run(Some(recv)));
    model.entered.notified().await;
    let partial = read(temp.path().join("cancel/trajectory.json"));
    assert!(!partial["steps"].as_array().unwrap().is_empty());
    send.send(true).unwrap();
    let out = task.await.unwrap().unwrap();
    assert_eq!(out.exit_code, 130);
    assert_eq!(
        read(temp.path().join("cancel/excalibur/metrics.json"))["exit_code"],
        130
    );
    t.shutdown().await.unwrap();
}
#[tokio::test]
async fn wall_timeout_during_inference_pauses_with_exit_4() {
    let temp = tempfile::tempdir().unwrap();
    let mut opts = options(&temp, "timeout");
    opts.budget.max_wall_seconds = 1;
    let e = Execution::prepare(
        opts,
        Arc::new(Held {
            entered: tokio::sync::Notify::new(),
        }),
        Redactor::default(),
    )
    .await
    .unwrap();
    let t = e.telemetry.clone();
    let out = e.run(None).await.unwrap();
    assert_eq!(out.exit_code, 4);
    assert!(out.reason.unwrap().contains("wall time"));
    t.shutdown().await.unwrap();
}
struct Web;
impl ToolRuntime for Web {
    fn definitions(&self) -> Vec<ToolDefinition> {
        vec![ToolDefinition {
            name: "web_read".into(),
            description: "Synthetic policy-only web fixture".into(),
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
                exclusive: false,
                data: json!({}),
            })
        })
    }
    fn execute<'a>(
        &'a self,
        _: &'a str,
        _: &'a str,
        _: &'a PreparedTool,
    ) -> BoxFuture<'a, ToolResult> {
        Box::pin(async { ToolResult::ok(json!({"fixture":true})) })
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
async fn declared_web_policy_hides_network_tools_and_blocks_benchmark_sources_before_the_adapter() {
    let profile = FrozenProfile::evaluation(1, None).unwrap();
    let hidden = excalibur_eval::ExecutionTools::new(Arc::new(Web), Some(profile.clone()), false);
    assert!(!hidden.definitions().iter().any(|d| d.name == "web_read"));
    let call = |url: &str| ToolCall {
        id: "web".into(),
        name: "web_read".into(),
        arguments: json!({"url":url}).to_string(),
    };
    assert!(
        hidden
            .prepare("run", call("https://example.org/docs"))
            .await
            .is_err()
    );
    let open = excalibur_eval::ExecutionTools::new(Arc::new(Web), Some(profile), true);
    for url in [
        "https://www.tbench.ai/tasks",
        "https://hub.harborframework.com/datasets/terminal-bench/terminal-bench/4",
        "https://github.com/harbor-framework/terminal-bench",
        "https://raw.githubusercontent.com/harbor-framework/harbor/main/test.py",
    ] {
        assert!(open.prepare("run", call(url)).await.is_err(), "{url}");
    }
    assert!(
        open.prepare("run", call("https://docs.rust-lang.org/std"))
            .await
            .is_ok()
    );
}
#[tokio::test]
async fn model_reported_missing_input_blocks_without_a_followup_inference() {
    let temp = tempfile::tempdir().unwrap();
    let opts = options(&temp, "reported-missing");
    let model = Arc::new(Script::new(vec![ModelResponse {
        message: Message {
            role: "assistant".into(),
            content: String::new(),
            tool_calls: vec![ToolCall {
                id: "missing".into(),
                name: "status".into(),
                arguments: json!({"action":"block","reason":"mandatory destination is missing"})
                    .to_string(),
            }],
            tool_call_id: None,
            provider_state: None,
        },
        usage: Usage::default(),
    }]));
    let e = Execution::prepare(opts, model.clone(), Redactor::default())
        .await
        .unwrap();
    let t = e.telemetry.clone();
    let out = e.run(None).await.unwrap();
    assert_eq!(out.exit_code, 3, "{out:?}");
    assert_eq!(model.calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        out.reason.as_deref(),
        Some("mandatory destination is missing")
    );
    t.shutdown().await.unwrap();
}
#[tokio::test]
async fn concurrent_episode_reservations_allow_only_one_owner_and_one_model_call() {
    let temp = tempfile::tempdir().unwrap();
    let opts = options(&temp, "exclusive");
    let model = Arc::new(Script::new(vec![answer("done")]));
    let (a, b) = tokio::join!(
        Execution::prepare(opts.clone(), model.clone(), Redactor::default()),
        Execution::prepare(opts, model.clone(), Redactor::default())
    );
    assert_eq!(usize::from(a.is_ok()) + usize::from(b.is_ok()), 1);
    let e = a.or(b).unwrap();
    let t = e.telemetry.clone();
    assert_eq!(e.run(None).await.unwrap().exit_code, 0);
    assert_eq!(model.calls.load(Ordering::SeqCst), 1);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(temp.path().join("exclusive/excalibur"))
                .unwrap()
                .permissions()
                .mode()
                & 0o077,
            0
        );
    }
    t.shutdown().await.unwrap();
}
