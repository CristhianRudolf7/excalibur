mod common;
use common::*;
use excalibur_engine::{Boundary, Engine, Observer, Status};
use excalibur_journal::Journal;
use excalibur_store::{Database, DatabaseKind, ReadLane, StorageConfig};
use excalibur_tools::{Access, LocalTools, ToolPolicy};
use excalibur_types::runtime::{Effect, ToolRuntime};
use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::Arc,
    time::{Duration, Instant},
};
struct Stop {
    root: PathBuf,
    phase: String,
}
impl Observer for Stop {
    fn reached(&self, phase: Boundary, _run: &str, _step: u64, _key: Option<&str>) {
        if format!("{phase:?}") == self.phase {
            fs::write(self.root.join("ready"), b"ready").unwrap();
            loop {
                std::thread::park();
            }
        }
    }
}
fn effect(value: &str) -> Effect {
    match value {
        "pure" => Effect::Pure,
        "idempotent" => Effect::Idempotent,
        "keyed" => Effect::Keyed,
        _ => Effect::NonIdempotent,
    }
}
async fn setup(root: &Path, class: &str) -> (Database, Engine) {
    let db = Database::open(
        root.join("main.db"),
        DatabaseKind::Main,
        StorageConfig::default(),
    )
    .unwrap();
    let journal = Journal::new(db.clone()).await.unwrap();
    let tools: Arc<dyn ToolRuntime> = if class == "file" {
        Arc::new(
            LocalTools::new(
                db.clone(),
                root.into(),
                ToolPolicy {
                    write: Access::Allow,
                    ..Default::default()
                },
            )
            .await
            .unwrap(),
        )
    } else {
        Arc::new(Effects {
            root: root.into(),
            effect: effect(class),
            journal: journal.clone(),
        })
    };
    let engine = Engine::new(journal, model(root, class == "file", 1), tools);
    (db, engine)
}
#[test]
fn crash_child() {
    let Some(root) = std::env::var_os("EXCALIBUR_ENGINE_CRASH_ROOT") else {
        return;
    };
    let root = PathBuf::from(root);
    let class = std::env::var("EXCALIBUR_ENGINE_CRASH_CLASS").unwrap();
    let phase = std::env::var("EXCALIBUR_ENGINE_CRASH_PHASE").unwrap();
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let (_db, engine) = setup(&root, &class).await;
        let engine = engine.with_observer(Arc::new(Stop { root, phase }));
        let run = engine
            .create("operation", spec(class == "file", 10))
            .await
            .unwrap();
        engine.run(&run).await.unwrap();
    });
}
async fn interrupt(root: &Path, class: &str, phase: Boundary) {
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "crash_child", "--nocapture"])
        .env("EXCALIBUR_ENGINE_CRASH_ROOT", root)
        .env("EXCALIBUR_ENGINE_CRASH_CLASS", class)
        .env("EXCALIBUR_ENGINE_CRASH_PHASE", format!("{phase:?}"))
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let started = Instant::now();
    while !root.join("ready").exists() {
        if let Some(status) = child.try_wait().unwrap() {
            panic!("child exited before barrier {class}/{phase:?}: {status}");
        }
        assert!(
            started.elapsed() < Duration::from_secs(30),
            "child failed to reach barrier"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    child.kill().unwrap();
    child.wait().unwrap();
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn kill9_at_eight_boundaries_reconciles_all_four_effect_classes_without_duplicates() {
    use Boundary::*;
    for class in ["pure", "idempotent", "keyed", "non_idempotent"] {
        for phase in [
            BeforeModelRequest,
            ModelRequestConfirmed,
            ModelReturned,
            ModelResponseConfirmed,
            BeforeToolIntent,
            ToolIntentConfirmed,
            ToolEffectReturned,
            ToolResultConfirmed,
        ] {
            let dir = tempfile::tempdir().unwrap();
            interrupt(dir.path(), class, phase).await;
            let before_models = count(&dir.path().join("models"));
            let before_executions = count(&dir.path().join("executions"));
            let (db, engine) = setup(dir.path(), class).await;
            let run = engine.create("operation", spec(false, 10)).await.unwrap();
            let result = engine.run(&run).await.unwrap();
            let uncertain = class == "non_idempotent"
                && matches!(phase, ToolIntentConfirmed | ToolEffectReturned);
            assert_eq!(
                result.status,
                if uncertain {
                    Status::Blocked
                } else {
                    Status::Completed
                },
                "{class}/{phase:?}"
            );
            let effects = count(&dir.path().join("effects"));
            assert!(effects <= 1, "duplicated effect for {class}/{phase:?}");
            if matches!(
                phase,
                ModelResponseConfirmed
                    | BeforeToolIntent
                    | ToolIntentConfirmed
                    | ToolEffectReturned
                    | ToolResultConfirmed
            ) {
                assert_eq!(
                    count(&dir.path().join("models")),
                    before_models + usize::from(!uncertain),
                    "confirmed response must be reused"
                );
            }
            if phase == ToolResultConfirmed {
                assert_eq!(count(&dir.path().join("executions")), before_executions);
            }
            if class == "pure" && phase == ToolEffectReturned {
                assert_eq!(count(&dir.path().join("executions")), 2);
            }
            if class == "idempotent" && phase == ToolEffectReturned {
                assert_eq!(count(&dir.path().join("executions")), 1);
            }
            if class == "keyed" && phase == ToolEffectReturned {
                assert_eq!(count(&dir.path().join("executions")), 2);
                assert_eq!(effects, 1);
            }
            if phase == ModelReturned {
                assert_eq!(
                    count(&dir.path().join("models")),
                    3,
                    "unconfirmed model response repeats"
                );
            }
            let check = db
                .read(ReadLane::Interactive, |c| {
                    Ok(c.query_row("PRAGMA quick_check", [], |r| r.get::<_, String>(0))?)
                })
                .await
                .unwrap();
            assert_eq!(check, "ok");
            println!(
                "class={class} boundary={phase:?} state={:?} effects={effects} quick_check=ok",
                result.status
            );
            if uncertain && phase == ToolEffectReturned {
                engine.resolve_uncertain(&run, 0,
                    excalibur_types::runtime::ToolResult::ok(serde_json::json!({"done":true})),
                    "Backend inspected the durable external simulator: exactly one confirmed effect").await.unwrap();
                assert_eq!(engine.run(&run).await.unwrap().status, Status::Completed);
                assert_eq!(count(&dir.path().join("effects")), 1);
            }
            db.shutdown().await.unwrap();
        }
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn real_file_postimage_and_versions_survive_kill_without_rewriting_or_clobbering_external_edits()
 {
    for external_change in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        interrupt(dir.path(), "file", Boundary::ToolEffectReturned).await;
        assert_eq!(fs::read(dir.path().join("answer.txt")).unwrap(), b"42\n");
        if external_change {
            fs::write(dir.path().join("answer.txt"), b"user edit\n").unwrap();
        }
        let (db, engine) = setup(dir.path(), "file").await;
        let run = engine.create("operation", spec(true, 10)).await.unwrap();
        let result = engine.run(&run).await.unwrap();
        assert_eq!(
            result.status,
            if external_change {
                Status::Blocked
            } else {
                Status::Completed
            }
        );
        assert_eq!(
            fs::read(dir.path().join("answer.txt")).unwrap(),
            if external_change {
                b"user edit\n".as_slice()
            } else {
                b"42\n".as_slice()
            }
        );
        let versions = db
            .read(ReadLane::Interactive, |c| {
                Ok(c.query_row("SELECT count(*) FROM file_versions", [], |r| {
                    r.get::<_, u64>(0)
                })?)
            })
            .await
            .unwrap();
        assert_eq!(versions, 1);
        db.shutdown().await.unwrap();
    }
}
