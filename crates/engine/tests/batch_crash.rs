//! A later exclusive command is not admitted before the preceding file batch.
#![cfg(unix)]
use excalibur_context::ContextInput;
use excalibur_engine::{Boundary, Budget, Criterion, Engine, Observer, RunSpec, Status};
use excalibur_journal::Journal;
use excalibur_models::{ModelError, ModelGateway};
use excalibur_store::{Database, DatabaseKind, ReadLane, StorageConfig};
use excalibur_tools::{Access, LocalTools, ToolPolicy};
use excalibur_types::runtime::{BoxFuture, Message, ModelRequest, ModelResponse, ToolCall, Usage};
use serde_json::json;
use std::{
    fs,
    io::{Read, Write},
    os::unix::process::ExitStatusExt,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::Arc,
    time::{Duration, Instant},
};

struct BatchModel {
    root: PathBuf,
}
impl ModelGateway for BatchModel {
    fn complete<'a>(
        &'a self,
        request: ModelRequest,
    ) -> BoxFuture<'a, Result<ModelResponse, ModelError>> {
        Box::pin(async move {
            let mut calls = fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(self.root.join("model-calls"))
                .unwrap();
            calls.write_all(b"1\n").unwrap();
            calls.sync_all().unwrap();
            let completed = request
                .messages
                .iter()
                .filter_map(|message| message.tool_call_id.as_deref())
                .collect::<Vec<_>>();
            let tool_calls = if !completed.contains(&"first_file") {
                vec![
                    call(
                        "first_file",
                        "write_file",
                        json!({"path":"first.txt","content":"first-batch-complete\n"}),
                    ),
                    call(
                        "exclusive_once",
                        "exec",
                        json!({"cmd":"printf '1\\n' >> marker.log; test \"$(cat first.txt)\" = first-batch-complete","yield_ms":250}),
                    ),
                ]
            } else if !completed.contains(&"verify_file") {
                vec![call(
                    "verify_file",
                    "read_file",
                    json!({"path":"first.txt"}),
                )]
            } else {
                Vec::new()
            };
            let mut message = Message::text(
                "assistant",
                if tool_calls.is_empty() {
                    "The file and one command execution were verified."
                } else {
                    ""
                },
            );
            message.tool_calls = tool_calls;
            Ok(ModelResponse {
                message,
                usage: Usage::default(),
            })
        })
    }
}
fn call(id: &str, name: &str, args: serde_json::Value) -> ToolCall {
    ToolCall {
        id: id.into(),
        name: name.into(),
        arguments: args.to_string(),
    }
}
fn spec() -> RunSpec {
    RunSpec {
        context: ContextInput::new(
            "Use file tools and execute the command once.",
            "Create first.txt, then append one marker only after that file batch is confirmed.",
            "fixed synthetic time",
        ),
        budget: Budget::default(),
        criteria: vec![
            Criterion::ToolOutput {
                name: "exec".into(),
                pointer: "/exit_code".into(),
                expected: json!(0),
            },
            Criterion::ToolOutput {
                name: "read_file".into(),
                pointer: "/lines/0/text".into(),
                expected: json!("first-batch-complete"),
            },
        ],
    }
}
async fn setup(root: &Path) -> (Database, Engine, Arc<LocalTools>) {
    let workspace = root.join("workspace");
    fs::create_dir_all(&workspace).unwrap();
    let db = Database::open(
        root.join("data/main.db"),
        DatabaseKind::Main,
        StorageConfig::default(),
    )
    .unwrap();
    let journal = Journal::new(db.clone()).await.unwrap();
    excalibur_tools::terminal::prepare_project_boundary(&workspace).unwrap();
    let tools = Arc::new(
        LocalTools::new(
            db.clone(),
            workspace,
            ToolPolicy {
                write: Access::Allow,
                exec: Access::Allow,
                ..Default::default()
            },
        )
        .await
        .unwrap(),
    );
    let engine = Engine::new(
        journal,
        Arc::new(BatchModel { root: root.into() }),
        tools.clone(),
    );
    (db, engine, tools)
}
struct StopAfterFirst {
    root: PathBuf,
    phase: Boundary,
}
impl Observer for StopAfterFirst {
    fn reached(&self, phase: Boundary, run: &str, step: u64, key: Option<&str>) {
        if phase == self.phase && step == 0 && key.is_some_and(|key| key.ends_with(":tool:0")) {
            assert_eq!(
                fs::read(self.root.join("workspace/first.txt")).unwrap(),
                b"first-batch-complete\n"
            );
            assert!(
                !self.root.join("workspace/marker.log").exists(),
                "the exclusive next batch must not have executed"
            );
            let temporary = self.root.join("ready.tmp");
            fs::write(&temporary, run.as_bytes()).unwrap();
            fs::File::open(&temporary).unwrap().sync_all().unwrap();
            fs::rename(temporary, self.root.join("ready")).unwrap();
            loop {
                std::thread::park();
            }
        }
    }
}
#[test]
#[ignore = "subprocess worker invoked by file_batch_recovery_starts_only_the_unadmitted_exclusive_command"]
fn file_batch_process_worker() {
    let root = PathBuf::from(
        std::env::var_os("EXCALIBUR_BATCH_CRASH_ROOT").expect("parent supplies synthetic root"),
    );
    let phase = match std::env::var("EXCALIBUR_BATCH_CRASH_PHASE")
        .unwrap()
        .as_str()
    {
        "effect" => Boundary::ToolEffectReturned,
        "result" => Boundary::ToolResultConfirmed,
        _ => panic!("unknown phase"),
    };
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let (_db, engine, _tools) = setup(&root).await;
        let engine = engine.with_observer(Arc::new(StopAfterFirst {
            root: root.clone(),
            phase,
        }));
        let run = engine
            .create("ordered-file-and-command", spec())
            .await
            .unwrap();
        engine.run(&run).await.unwrap();
    });
}
struct ChildGuard(Child);
impl Drop for ChildGuard {
    fn drop(&mut self) {
        if self.0.try_wait().ok().flatten().is_none() {
            let _ = self.0.kill();
        }
        let _ = self.0.wait();
    }
}
async fn interrupt(root: &Path, phase: &str) -> String {
    let child = Command::new(std::env::current_exe().unwrap())
        .args([
            "--ignored",
            "--exact",
            "file_batch_process_worker",
            "--nocapture",
        ])
        .env("EXCALIBUR_BATCH_CRASH_ROOT", root)
        .env("EXCALIBUR_BATCH_CRASH_PHASE", phase)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut child = ChildGuard(child);
    let started = Instant::now();
    while !root.join("ready").exists() {
        if let Some(status) = child.0.try_wait().unwrap() {
            let mut error = String::new();
            if let Some(mut stderr) = child.0.stderr.take() {
                stderr.read_to_string(&mut error).unwrap();
            }
            panic!("worker exited before {phase} barrier: {status}: {error}");
        }
        assert!(
            started.elapsed() < Duration::from_secs(30),
            "worker did not reach synchronized {phase} barrier"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let run = fs::read_to_string(root.join("ready")).unwrap();
    assert!(!root.join("workspace/marker.log").exists());
    child.0.kill().unwrap();
    let status = child.0.wait().unwrap();
    assert_eq!(
        status.signal(),
        Some(9),
        "the worker must be interrupted by real SIGKILL"
    );
    run
}
fn count_models(root: &Path) -> usize {
    fs::read_to_string(root.join("model-calls"))
        .unwrap()
        .lines()
        .count()
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn file_batch_recovery_starts_only_the_unadmitted_exclusive_command() {
    for phase in ["effect", "result"] {
        let dir = tempfile::tempdir().unwrap();
        let original_run = interrupt(dir.path(), phase).await;
        assert_eq!(
            count_models(dir.path()),
            1,
            "the first complete response was produced before the barrier"
        );
        let (db, engine, tools) = setup(dir.path()).await;
        let run = engine
            .create("ordered-file-and-command", spec())
            .await
            .unwrap();
        assert_eq!(run, original_run);
        let prior = engine.journal().replay(&run, 0).await.unwrap();
        let intents = prior
            .iter()
            .filter(|event| event.kind == "tool_intent")
            .collect::<Vec<_>>();
        assert_eq!(
            intents.len(),
            1,
            "only the active file batch may have an intention at {phase}; an unstarted exec must remain unadmitted"
        );
        assert_eq!(intents[0].payload["prepared"]["call"]["id"], "first_file");
        assert_eq!(intents[0].payload["prepared"]["effect"], "idempotent");
        let first_results = prior
            .iter()
            .filter(|event| event.kind == "tool_result" && event.payload["name"] == "write_file")
            .count();
        assert_eq!(first_results, usize::from(phase == "result"));
        assert!(!dir.path().join("workspace/marker.log").exists());
        let resumed = engine.run(&run).await.unwrap();
        assert_eq!(
            resumed.status,
            Status::Completed,
            "{phase}: {:?}",
            resumed.reason
        );
        assert_eq!(
            fs::read(dir.path().join("workspace/first.txt")).unwrap(),
            b"first-batch-complete\n"
        );
        assert_eq!(
            fs::read_to_string(dir.path().join("workspace/marker.log")).unwrap(),
            "1\n",
            "the previously unstarted non-idempotent command must execute exactly once after recovery"
        );
        assert_eq!(
            count_models(dir.path()),
            3,
            "the confirmed two-call response is reused; only verification and finalization call the gateway"
        );
        let events = engine.journal().replay(&run, 0).await.unwrap();
        let first_response = events
            .iter()
            .filter(|event| {
                event.kind == "model_response"
                    && event.payload["response"]["message"]["tool_calls"][0]["id"] == "first_file"
            })
            .count();
        assert_eq!(first_response, 1);
        let exec_intents = events
            .iter()
            .filter(|event| {
                event.kind == "tool_intent"
                    && event.payload["prepared"]["call"]["id"] == "exclusive_once"
            })
            .collect::<Vec<_>>();
        assert_eq!(exec_intents.len(), 1);
        let file_result = events
            .iter()
            .find(|event| event.kind == "tool_result" && event.payload["name"] == "write_file")
            .unwrap();
        assert!(file_result.sequence < exec_intents[0].sequence);
        let (versions, quick_check) = db
            .read(ReadLane::Interactive, |c| {
                Ok((
                    c.query_row(
                        "SELECT count(*) FROM file_versions WHERE tool_call_id='first_file'",
                        [],
                        |r| r.get::<_, u64>(0),
                    )?,
                    c.query_row("PRAGMA quick_check", [], |r| r.get::<_, String>(0))?,
                ))
            })
            .await
            .unwrap();
        assert_eq!(
            versions, 1,
            "reconciliation must not create a duplicate file version"
        );
        assert_eq!(quick_check, "ok");
        assert_eq!(engine.run(&run).await.unwrap().status, Status::Completed);
        assert_eq!(count_models(dir.path()), 3);
        assert_eq!(
            fs::read_to_string(dir.path().join("workspace/marker.log")).unwrap(),
            "1\n"
        );
        println!(
            "phase={phase} active_intents_at_kill=1 later_exec_intents_at_kill=0 completed=true marker_count=1 quick_check=ok"
        );
        tools.terminals().shutdown().await;
        db.shutdown().await.unwrap();
    }
}
