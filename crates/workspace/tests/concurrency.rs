use excalibur_store::{Database, DatabaseKind, Priority, StorageConfig};
use excalibur_types::runtime::BoxFuture;
use excalibur_vault::Redactor;
use excalibur_workspace::*;
use std::{
    collections::BTreeMap,
    io::{BufRead, BufReader},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::Duration,
};
async fn fixture() -> (tempfile::TempDir, Database, Workspace) {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("project");
    std::fs::create_dir(&root).unwrap();
    let db = Database::open(
        temp.path().join("db/main.db"),
        DatabaseKind::Main,
        StorageConfig::default(),
    )
    .unwrap();
    db.write(Priority::Normal, |c| {
        c.execute(
            "INSERT INTO runs(id,state) VALUES('one','running'),('two','running')",
            [],
        )?;
        Ok(())
    })
    .await
    .unwrap();
    let workspace = Workspace::open(
        db.clone(),
        root,
        temp.path().join("data"),
        Redactor::default(),
    )
    .await
    .unwrap();
    (temp, db, workspace)
}
#[tokio::test]
async fn leases_renew_during_long_tools_release_on_human_wait_and_expire_without_heartbeat() {
    let (_temp, db, w) = fixture().await;
    let leases = w.leases();
    let files = vec![PathBuf::from("source")];
    let guard = leases
        .acquire_many_at("one", &files, "20 minute tool", 0, Duration::ZERO)
        .await
        .unwrap();
    for now in (60..=1200).step_by(60) {
        leases
            .heartbeat_at("one", "waiting_tool", None, now)
            .await
            .unwrap();
        assert_eq!(leases.owners().await.unwrap()[0].expires_at, now + 600);
    }
    drop(guard);
    let error = leases
        .acquire_many_at("two", &files, "competing", 1201, Duration::ZERO)
        .await
        .err()
        .unwrap();
    assert!(
        matches!(error,Error::Busy(LeaseOwner{holder,activity,..})if holder=="one"&&activity=="20 minute tool")
    );
    for state in [
        "paused",
        "blocked",
        "waiting_user",
        "waiting_approval",
        "completed",
    ] {
        leases.heartbeat_at("one", state, None, 1201).await.unwrap();
        let guard = leases
            .acquire_many_at("two", &files, "resumed", 1202, Duration::ZERO)
            .await
            .unwrap();
        drop(guard);
        leases.release_run("two").await.unwrap();
        let guard = leases
            .acquire_many_at("one", &files, "renewed", 1202, Duration::ZERO)
            .await
            .unwrap();
        drop(guard);
    }
    let guard = leases
        .acquire_many_at("two", &files, "expired owner", 1803, Duration::ZERO)
        .await
        .unwrap();
    drop(guard);
    db.shutdown().await.unwrap();
}
#[tokio::test]
async fn busy_wait_is_bounded_to_thirty_seconds_with_owner_metadata() {
    let (_temp, db, w) = fixture().await;
    let leases = w.leases();
    let paths = vec![PathBuf::from("same")];
    drop(
        leases
            .acquire_many_at("one", &paths, "long task", 100, Duration::ZERO)
            .await
            .unwrap(),
    );
    tokio::time::pause();
    let start = tokio::time::Instant::now();
    assert!(matches!(
        leases
            .acquire_many_at("two", &paths, "other", 100, Duration::from_secs(30))
            .await,
        Err(Error::Busy(_))
    ));
    assert!(
        start.elapsed() >= Duration::from_secs(30)
            && start.elapsed() <= Duration::from_millis(30010)
    );
    tokio::time::resume();
    db.shutdown().await.unwrap();
}
#[tokio::test]
async fn effect_guard_keeps_os_lock_after_state_release() {
    let (_temp, db, w) = fixture().await;
    let leases = w.leases();
    let paths = vec![PathBuf::from("same")];
    let guard = leases
        .acquire_many_at("one", &paths, "inflight write", 0, Duration::ZERO)
        .await
        .unwrap();
    leases.release_run("one").await.unwrap();
    assert!(matches!(
        leases
            .acquire_many_at("two", &paths, "other", 1, Duration::ZERO)
            .await,
        Err(Error::Busy(_))
    ));
    drop(guard);
    drop(
        leases
            .acquire_many_at("two", &paths, "after completion", 1, Duration::ZERO)
            .await
            .unwrap(),
    );
    db.shutdown().await.unwrap();
}
fn git(path: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .current_dir(path)
        .args([
            "-c",
            "user.name=Synthetic Fixture",
            "-c",
            "user.email=fixture@example.invalid",
        ])
        .args(args)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap()
}
fn init(root: &Path) {
    git(root, &["init", "-b", "main"]);
    std::fs::write(root.join("a.txt"), "base\n").unwrap();
    git(root, &["add", "."]);
    git(root, &["commit", "-m", "Synthetic fixture baseline"]);
}
fn identity() -> GitIdentity {
    GitIdentity {
        name: "Synthetic Fixture".into(),
        email: "fixture@example.invalid".into(),
    }
}
struct Checks(bool);
impl IntegrationChecks for Checks {
    fn verify<'a>(
        &'a self,
        tree: &'a Worktree,
        _changed: &'a [String],
    ) -> BoxFuture<'a, Result<TestReport>> {
        Box::pin(async move {
            let checked = Command::new("git")
                .current_dir(&tree.path)
                .args(["diff", "--check"])
                .status()?;
            Ok(TestReport {
                passed: self.0 && checked.success(),
                checks_run: 1,
                not_applicable_reason: None,
                report: "Actual git diff --check fixture".into(),
            })
        })
    }
}
#[tokio::test]
async fn three_worktrees_rebase_test_and_fast_forward_only_integration_with_conflict_report() {
    let (_temp, db, w) = fixture().await;
    init(w.root());
    let original = git(w.root(), &["rev-parse", "HEAD"]);
    let g = w.git();
    let a = g.create("task", "a").await.unwrap();
    let b = g.create("task", "b").await.unwrap();
    let c = g.create("task", "c").await.unwrap();
    for (tree, file, content) in [
        (&a, "a.txt", "first\n"),
        (&b, "a.txt", "conflict\n"),
        (&c, "c.txt", "independent\n"),
    ] {
        std::fs::write(tree.path.join(file), content).unwrap();
        checkpoint(&g, "task", &tree.owner, "Synthetic child change").await;
        g.enqueue("task", &tree.owner, "Synthetic intention")
            .await
            .unwrap();
    }
    assert_eq!(
        g.process_next("task", &identity(), &Checks(true))
            .await
            .unwrap()
            .unwrap()
            .state,
        "integrated"
    );
    let conflict = g
        .process_next("task", &identity(), &Checks(true))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(conflict.state, "conflicted");
    assert!(!conflict.child_diff.is_empty());
    assert!(!conflict.integration_diff.is_empty());
    assert_eq!(
        g.process_next("task", &identity(), &Checks(true))
            .await
            .unwrap()
            .unwrap()
            .state,
        "integrated"
    );
    assert_eq!(git(w.root(), &["rev-parse", "HEAD"]), original);
    assert_eq!(git(w.root(), &["branch", "--show-current"]).trim(), "main");
    assert_eq!(
        std::fs::read_to_string(w.root().join("a.txt")).unwrap(),
        "base\n"
    );
    assert!(a.path.starts_with(w.root().parent().unwrap().join("data")));
    assert_eq!(g.list_worktrees("task").await.unwrap().len(), 3);
    std::fs::write(c.path.join("dirty"), "preserve").unwrap();
    assert!(g.remove("task", "c").await.is_err());
    std::fs::remove_file(c.path.join("dirty")).unwrap();
    g.remove("task", "c").await.unwrap();
    assert_eq!(g.list_worktrees("task").await.unwrap().len(), 2);
    db.shutdown().await.unwrap();
}
#[tokio::test]
async fn failed_or_interrupted_tests_never_advance_and_submodules_select_shared_mode() {
    let (_temp, db, w) = fixture().await;
    init(w.root());
    let g = w.git();
    let a = g.create("test", "a").await.unwrap();
    std::fs::write(a.path.join("new"), "change").unwrap();
    checkpoint(&g, "test", "a", "Synthetic change").await;
    let id = g.enqueue("test", "a", "intent").await.unwrap();
    let before = git(&a.integration_path, &["rev-parse", "HEAD"]);
    assert_eq!(
        g.process_next("test", &identity(), &Checks(false))
            .await
            .unwrap()
            .unwrap()
            .state,
        "returned"
    );
    assert_eq!(git(&a.integration_path, &["rev-parse", "HEAD"]), before);
    let key = id.clone();
    db.write(Priority::Normal, move |c| {
        c.execute(
            "UPDATE workspace_integrations SET state='testing' WHERE id=?1",
            [key],
        )?;
        Ok(())
    })
    .await
    .unwrap();
    assert_eq!(
        g.process_next("test", &identity(), &Checks(true))
            .await
            .unwrap()
            .unwrap()
            .state,
        "returned"
    );
    assert_eq!(git(&a.integration_path, &["rev-parse", "HEAD"]), before);
    std::fs::write(
        w.root().join(".gitmodules"),
        "[submodule \"fixture\"]\npath=dependency\nurl=./unused\n",
    )
    .unwrap();
    assert_eq!(g.mode().await.unwrap(), ProjectMode::Shared);
    assert!(g.create("other", "x").await.is_err());
    db.shutdown().await.unwrap();
}
#[tokio::test]
async fn malicious_git_filters_are_rejected_without_executing_and_shadow_does_not_touch_user_index()
{
    let (_temp, db, w) = fixture().await;
    init(w.root());
    git(
        w.root(),
        &["config", "filter.unsafe.clean", "touch SHOULD_NOT_EXIST"],
    );
    assert!(w.git().create("unsafe", "child").await.is_err());
    assert!(!w.root().join("SHOULD_NOT_EXIST").exists());
    let before = std::fs::read(w.root().join(".git/index")).unwrap();
    let snapshot = w
        .shadow()
        .snapshot_files(
            "one",
            0,
            "before",
            BTreeMap::from([(PathBuf::from("a.txt"), b"base\n".to_vec())]),
        )
        .await
        .unwrap();
    assert!(!snapshot.tree.is_empty());
    assert_eq!(std::fs::read(w.root().join(".git/index")).unwrap(), before);
    assert_eq!(git(w.root(), &["log", "--format=%s"]).lines().count(), 1);
    db.shutdown().await.unwrap();
}
#[tokio::test]
async fn shadow_restore_merges_later_nonoverlapping_edits_and_reports_conflicts_without_writes() {
    let (_temp, db, w) = fixture().await;
    let shadow = w.shadow();
    let before = shadow
        .snapshot_files(
            "one",
            1,
            "before",
            BTreeMap::from([(PathBuf::from("a"), b"one\ntwo\nthree\n".to_vec())]),
        )
        .await
        .unwrap();
    let after = shadow
        .snapshot_files(
            "one",
            1,
            "after",
            BTreeMap::from([(PathBuf::from("a"), b"changed\ntwo\nthree\n".to_vec())]),
        )
        .await
        .unwrap();
    let plan = shadow
        .plan_restore(
            &before.id,
            BTreeMap::from([(
                PathBuf::from("a"),
                Some(b"changed\ntwo\nlater-user\n".to_vec()),
            )]),
        )
        .await
        .unwrap();
    assert!(plan.conflicts.is_empty());
    assert_eq!(
        plan.files[0].post.as_deref(),
        Some(b"one\ntwo\nlater-user\n".as_slice())
    );
    assert!(!w.root().join("a").exists());
    let conflict = shadow
        .plan_restore(
            &before.id,
            BTreeMap::from([(
                PathBuf::from("a"),
                Some(b"user same line\ntwo\nthree\n".to_vec()),
            )]),
        )
        .await
        .unwrap();
    assert_eq!(conflict.conflicts.len(), 1);
    assert!(conflict.files.is_empty());
    assert!(
        shadow
            .diff(&before.id, &after.id)
            .await
            .unwrap()
            .contains("changed")
    );
    assert_eq!(shadow.list_snapshots("one").await.unwrap().len(), 2);
    assert!(
        shadow
            .snapshot_files(
                "one",
                2,
                "before",
                BTreeMap::from([(PathBuf::from("../escape"), vec![])])
            )
            .await
            .is_err()
    );
    db.shutdown().await.unwrap();
}
#[test]
#[ignore = "subprocess fixture"]
fn daemon_fixture() {
    let Some(root) = std::env::var_os("EXCALIBUR_WORKSPACE_FIXTURE") else {
        return;
    };
    let root = PathBuf::from(root);
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let cut = std::env::var_os("EXCALIBUR_WORKSPACE_CUT").is_some();
        let db = Database::open(
            root.join(if cut { "db/main.db" } else { "daemon/main.db" }),
            DatabaseKind::Main,
            StorageConfig::default(),
        )
        .unwrap();
        let w = Workspace::open(
            db.clone(),
            root.join("project"),
            root.join("data"),
            Redactor::default(),
        )
        .await
        .unwrap();
        if cut {
            let g = w.git();
            let tree = g.create("cut-task", "child").await.unwrap();
            std::fs::write(tree.path.join("cut.txt"), "synthetic change").unwrap();
            checkpoint(&g, "cut-task", "child", "Synthetic crash fixture").await;
            g.enqueue("cut-task", "child", "synthetic intention")
                .await
                .unwrap();
            let _ = g
                .process_next("cut-task", &identity(), &HangingChecks)
                .await;
            return;
        }
        let result = w
            .leases()
            .acquire_many_at(
                "daemon",
                &[PathBuf::from("same")],
                "second daemon",
                0,
                Duration::ZERO,
            )
            .await;
        assert!(matches!(result,Err(Error::Busy(LeaseOwner{holder,..}))if holder=="one"));
        println!("DAEMON_COORDINATED");
        db.shutdown().await.unwrap();
    });
}
#[tokio::test]
async fn two_daemons_with_independent_databases_coordinate_by_os_lock() {
    let (temp, db, w) = fixture().await;
    let guard = w
        .leases()
        .acquire_many_at(
            "one",
            &[PathBuf::from("same")],
            "first daemon",
            0,
            Duration::ZERO,
        )
        .await
        .unwrap();
    let out = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "daemon_fixture", "--ignored", "--nocapture"])
        .env("EXCALIBUR_WORKSPACE_FIXTURE", temp.path())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap()
        .wait_with_output()
        .unwrap();
    assert!(out.status.success());
    assert!(String::from_utf8_lossy(&out.stdout).contains("DAEMON_COORDINATED"));
    drop(guard);
    db.shutdown().await.unwrap();
}

#[tokio::test]
async fn history_revert_is_pure_rust_and_preserves_later_user_changes() {
    let (_temp, db, w) = fixture().await;
    let mut ids = Vec::new();
    for bytes in [
        b"first\nsecond\nlast\n".as_slice(),
        b"agent\nsecond\nlast\n".as_slice(),
    ] {
        let prepared = db.blobs().prepare(bytes).unwrap();
        ids.push(
            db.write_with_blob(prepared, Priority::Normal, |_, hash| {
                Ok(hash.as_str().to_owned())
            })
            .await
            .unwrap(),
        );
    }
    let (path, pre, post) = (
        w.root().join("plain.txt").to_string_lossy().into_owned(),
        ids[0].clone(),
        ids[1].clone(),
    );
    db.write(Priority::Normal,move|c|{c.execute("INSERT INTO file_versions(id,path,preimage_blob,postimage_blob,run_id,tool_call_id) VALUES('edit',?1,?2,?3,'one','call')",excalibur_store::rusqlite::params![path,pre,post])?;Ok(())}).await.unwrap();
    assert!(!w.root().join(".git").exists());
    let history = w.history();
    assert_eq!(
        history
            .version("edit")
            .await
            .unwrap()
            .tool_call_id
            .as_deref(),
        Some("call")
    );
    assert_eq!(
        history
            .file_history(Path::new("plain.txt"), 10)
            .await
            .unwrap()
            .len(),
        1
    );
    let plan = history
        .plan_revert("edit", Some(b"agent\nsecond\nuser-last\n".to_vec()))
        .await
        .unwrap();
    assert!(plan.conflicts.is_empty());
    assert_eq!(
        plan.files[0].post.as_deref(),
        Some(b"first\nsecond\nuser-last\n".as_slice())
    );
    let conflict = history
        .plan_revert("edit", Some(b"user-conflict\nsecond\nlast\n".to_vec()))
        .await
        .unwrap();
    assert_eq!(conflict.conflicts.len(), 1);
    assert!(!w.root().join("plain.txt").exists());
    db.shutdown().await.unwrap();
}
struct HangingChecks;
impl IntegrationChecks for HangingChecks {
    fn verify<'a>(&'a self, _: &'a Worktree, _: &'a [String]) -> BoxFuture<'a, Result<TestReport>> {
        Box::pin(async {
            println!("QUEUE_TESTING_DURABLE");
            std::io::Write::flush(&mut std::io::stdout()).unwrap();
            std::future::pending().await
        })
    }
}
#[cfg(unix)]
#[tokio::test]
async fn process_cut_during_integration_tests_cannot_fast_forward_on_restart() {
    let (temp, db, w) = fixture().await;
    init(w.root());
    let original = git(w.root(), &["rev-parse", "HEAD"]);
    drop(w);
    db.shutdown().await.unwrap();
    drop(db);
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "daemon_fixture", "--ignored", "--nocapture"])
        .env("EXCALIBUR_WORKSPACE_FIXTURE", temp.path())
        .env("EXCALIBUR_WORKSPACE_CUT", "1")
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let reader = BufReader::new(child.stdout.take().unwrap());
    assert!(
        reader
            .lines()
            .any(|line| line.unwrap().contains("QUEUE_TESTING_DURABLE"))
    );
    child.kill().unwrap();
    let status = child.wait().unwrap();
    use std::os::unix::process::ExitStatusExt;
    assert_eq!(status.signal(), Some(9));
    let db = Database::open(
        temp.path().join("db/main.db"),
        DatabaseKind::Main,
        StorageConfig::default(),
    )
    .unwrap();
    let w = Workspace::open(
        db.clone(),
        temp.path().join("project"),
        temp.path().join("data"),
        Redactor::default(),
    )
    .await
    .unwrap();
    let report = w
        .git()
        .process_next("cut-task", &identity(), &Checks(true))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(report.state, "returned");
    assert!(report.detail.contains("interrupted"));
    assert_eq!(git(w.root(), &["rev-parse", "HEAD"]), original);
    let tree = w.git().worktree("cut-task", "child").await.unwrap();
    assert_eq!(
        git(&tree.integration_path, &["rev-parse", "HEAD"]),
        original
    );
    db.shutdown().await.unwrap();
}

#[tokio::test]
async fn selected_shadow_restore_never_requires_or_returns_unselected_contents() {
    let (_temp, db, w) = fixture().await;
    let shadow = w.shadow();
    let before = shadow
        .snapshot_files(
            "one",
            8,
            "before",
            BTreeMap::from([
                (PathBuf::from("a"), b"A".to_vec()),
                (PathBuf::from("b"), b"B".to_vec()),
            ]),
        )
        .await
        .unwrap();
    shadow
        .snapshot_files(
            "one",
            8,
            "after",
            BTreeMap::from([
                (PathBuf::from("a"), b"changed A".to_vec()),
                (PathBuf::from("b"), b"changed B".to_vec()),
            ]),
        )
        .await
        .unwrap();
    let plan = shadow
        .plan_restore_selected(
            &before.id,
            BTreeMap::from([(PathBuf::from("a"), Some(b"changed A".to_vec()))]),
        )
        .await
        .unwrap();
    assert_eq!(plan.files.len(), 1);
    assert_eq!(plan.files[0].path, w.root().join("a"));
    assert_eq!(plan.files[0].post.as_deref(), Some(b"A".as_slice()));
    db.shutdown().await.unwrap();
}
#[test]
fn partition_overlap_reports_every_shared_canonical_path() {
    use std::collections::BTreeSet;
    let shared = PathBuf::from("/synthetic/src/shared.rs");
    let report = overlaps(&BTreeMap::from([
        (
            "first".into(),
            BTreeSet::from([shared.clone(), PathBuf::from("/synthetic/src/a.rs")]),
        ),
        ("second".into(), BTreeSet::from([shared.clone()])),
    ]));
    assert_eq!(report.len(), 1);
    assert_eq!(report[0].file, shared);
    assert_eq!(report[0].owners, vec!["first", "second"]);
}

#[tokio::test]
async fn stale_production_running_heartbeat_cannot_renew_a_durably_paused_run() {
    let (_temp, db, w) = fixture().await;
    let leases = w.leases();
    drop(
        leases
            .acquire_many_at(
                "one",
                &[PathBuf::from("source")],
                "active stage",
                0,
                Duration::ZERO,
            )
            .await
            .unwrap(),
    );
    db.write(Priority::Journal,|c|{c.execute("INSERT INTO journal(id,run_id,sequence,kind,payload,state,idempotency_key) VALUES('pause-source','one',1,'state_change','{\"state\":\"paused\",\"reason\":\"user\"}','confirmed','paused')",[])?;c.execute("UPDATE runs SET state='running' WHERE id='one'",[])?;Ok(())}).await.unwrap();
    leases.heartbeat("one", "running", None).await.unwrap();
    assert!(leases.owners().await.unwrap().is_empty());
    assert!(
        leases
            .acquire_many("one", &[PathBuf::from("source")], "stale effect")
            .await
            .is_err()
    );
    db.shutdown().await.unwrap();
}
#[tokio::test]
async fn lease_refresher_does_not_retain_database_ownership_after_runtime_drop() {
    let (temp, db, w) = fixture().await;
    let leases = w.leases();
    drop(
        leases
            .acquire_many("one", &[PathBuf::from("source")], "production refresher")
            .await
            .unwrap(),
    );
    leases.release_run("one").await.unwrap();
    drop(leases);
    drop(w);
    db.shutdown().await.unwrap();
    drop(db);
    let reopened = Database::open(
        temp.path().join("db/main.db"),
        DatabaseKind::Main,
        StorageConfig::default(),
    )
    .unwrap();
    reopened.shutdown().await.unwrap();
}
#[tokio::test]
async fn git_metadata_and_queue_ids_cannot_cross_project_roots_in_one_database() {
    let (temp, db, w) = fixture().await;
    init(w.root());
    let tree = w.git().create("first-task", "child").await.unwrap();
    std::fs::write(tree.path.join("change"), "first project").unwrap();
    checkpoint(&w.git(), "first-task", "child", "Synthetic first project").await;
    let queue = w
        .git()
        .enqueue("first-task", "child", "intent")
        .await
        .unwrap();
    let second = temp.path().join("second-project");
    std::fs::create_dir(&second).unwrap();
    init(&second);
    let other = Workspace::open(
        db.clone(),
        second,
        temp.path().join("data"),
        Redactor::default(),
    )
    .await
    .unwrap();
    assert!(other.git().list("first-task").await.unwrap().is_empty());
    assert!(
        other
            .git()
            .list_worktrees("first-task")
            .await
            .unwrap()
            .is_empty()
    );
    assert!(other.git().worktree("first-task", "child").await.is_err());
    assert!(other.git().retry(&queue, &identity()).await.is_err());
    db.shutdown().await.unwrap();
}

struct FixtureGuard;
impl CheckpointGuard for FixtureGuard {
    fn verify<'a>(
        &'a self,
        tree: &'a Worktree,
        expected: &'a BTreeMap<PathBuf, Option<Vec<u8>>>,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            for (path, wanted) in expected {
                let current = match std::fs::read(tree.path.join(path)) {
                    Ok(bytes) => Some(bytes),
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
                    Err(error) => return Err(error.into()),
                };
                if &current != wanted {
                    return Err(Error::Invalid("fixture fingerprint changed"));
                }
            }
            Ok(())
        })
    }
}
async fn checkpoint(g: &GitWorkspace, task: &str, owner: &str, message: &str) -> String {
    let tree = g.worktree(task, owner).await.unwrap();
    let mut files = BTreeMap::new();
    for path in g.list_tracked_paths(task, owner).await.unwrap() {
        files.insert(path.clone(), std::fs::read(tree.path.join(path)).ok());
    }
    for entry in ignore::WalkBuilder::new(&tree.path)
        .hidden(false)
        .filter_entry(|entry| entry.file_name() != ".git")
        .build()
    {
        let entry = entry.unwrap();
        if entry.file_type().is_some_and(|t| t.is_file()) {
            files.insert(
                entry.path().strip_prefix(&tree.path).unwrap().to_path_buf(),
                Some(std::fs::read(entry.path()).unwrap()),
            );
        }
    }
    g.checkpoint_files(task, owner, message, &identity(), files, &FixtureGuard)
        .await
        .unwrap()
}
#[tokio::test]
async fn guarded_checkpoint_preserves_unlisted_tracked_files_and_refuses_stale_content() {
    let (_temp, db, w) = fixture().await;
    init(w.root());
    std::fs::write(w.root().join("private.txt"), "protected baseline").unwrap();
    git(w.root(), &["add", "."]);
    git(w.root(), &["commit", "-m", "Synthetic protected baseline"]);
    let g = w.git();
    let tree = g.create("guarded", "child").await.unwrap();
    std::fs::write(tree.path.join("a.txt"), "authorized change").unwrap();
    std::fs::write(tree.path.join("private.txt"), "unlisted user change").unwrap();
    let expected = BTreeMap::from([(PathBuf::from("a.txt"), Some(b"authorized change".to_vec()))]);
    g.checkpoint_files(
        "guarded",
        "child",
        "Authorized partial checkpoint",
        &identity(),
        expected.clone(),
        &FixtureGuard,
    )
    .await
    .unwrap();
    assert_eq!(
        git(&tree.path, &["show", "HEAD:private.txt"]),
        "protected baseline"
    );
    assert_eq!(
        git(&tree.path, &["show", "HEAD:a.txt"]),
        "authorized change"
    );
    assert_eq!(
        std::fs::read_to_string(tree.path.join("private.txt")).unwrap(),
        "unlisted user change"
    );
    assert!(g.enqueue("guarded", "child", "intent").await.is_err());
    let original = git(&tree.path, &["rev-parse", "HEAD"]);
    std::fs::write(tree.path.join("a.txt"), "later user edit").unwrap();
    assert!(
        g.checkpoint_files(
            "guarded",
            "child",
            "Must not commit stale content",
            &identity(),
            expected,
            &FixtureGuard
        )
        .await
        .is_err()
    );
    assert_eq!(git(&tree.path, &["rev-parse", "HEAD"]), original);
    db.shutdown().await.unwrap();
}

#[tokio::test]
async fn persistent_assignments_replace_only_the_owner_and_isolate_project_partitions() {
    let (temp, db, w) = fixture().await;
    assert!(
        w.assign_files("task", "parent", "a", &[PathBuf::from("shared")])
            .await
            .unwrap()
            .is_empty()
    );
    let collision = w
        .assign_files(
            "task",
            "parent",
            "b",
            &[PathBuf::from("shared"), PathBuf::from("other")],
        )
        .await
        .unwrap();
    assert_eq!(collision[0].owners, vec!["a", "b"]);
    let second = temp.path().join("second");
    std::fs::create_dir(&second).unwrap();
    let other = Workspace::open(
        db.clone(),
        second,
        temp.path().join("data"),
        Redactor::default(),
    )
    .await
    .unwrap();
    assert!(
        other
            .assign_files("task", "other-parent", "a", &[PathBuf::from("shared")])
            .await
            .unwrap()
            .is_empty()
    );
    w.assign_files("task", "parent", "b", &[]).await.unwrap();
    let collision = w
        .assign_files("task", "parent", "c", &[PathBuf::from("shared")])
        .await
        .unwrap();
    assert_eq!(collision[0].owners, vec!["a", "c"]);
    assert!(
        w.assign_files("task", "wrong-parent", "d", &[])
            .await
            .is_err()
    );
    drop(other);
    drop(w);
    db.shutdown().await.unwrap();
    drop(db);
    let db = Database::open(
        temp.path().join("db/main.db"),
        DatabaseKind::Main,
        StorageConfig::default(),
    )
    .unwrap();
    let reopened = Workspace::open(
        db.clone(),
        temp.path().join("project"),
        temp.path().join("data"),
        Redactor::default(),
    )
    .await
    .unwrap();
    let collision = reopened
        .assign_files("task", "parent", "d", &[PathBuf::from("shared")])
        .await
        .unwrap();
    assert_eq!(collision[0].owners, vec!["a", "c", "d"]);
    db.shutdown().await.unwrap();
}
#[tokio::test]
async fn known_secrets_are_rejected_before_shadow_or_checkpoint_publication() {
    let (temp, db, w) = fixture().await;
    init(w.root());
    let secret = b"SYNTHETIC-REGISTERED-PRIVATE-VALUE";
    let redactor =
        Redactor::with_secrets([excalibur_vault::SecretValue::new(secret.to_vec()).unwrap()]);
    let secured = Workspace::open(
        db.clone(),
        w.root().to_path_buf(),
        temp.path().join("data"),
        redactor,
    )
    .await
    .unwrap();
    assert!(
        secured
            .shadow()
            .snapshot_files(
                "one",
                3,
                "before",
                BTreeMap::from([(PathBuf::from("sensitive"), secret.to_vec())])
            )
            .await
            .is_err()
    );
    let g = secured.git();
    let tree = g.create("secret", "child").await.unwrap();
    std::fs::write(tree.path.join("sensitive"), secret).unwrap();
    let before = git(&tree.path, &["rev-parse", "HEAD"]);
    assert!(
        g.checkpoint_files(
            "secret",
            "child",
            "No protected content",
            &identity(),
            BTreeMap::from([(PathBuf::from("sensitive"), Some(secret.to_vec()))]),
            &FixtureGuard
        )
        .await
        .is_err()
    );
    assert_eq!(git(&tree.path, &["rev-parse", "HEAD"]), before);
    assert!(git(&tree.path, &["status", "--porcelain"]).contains("sensitive"));
    db.shutdown().await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn automatic_renewal_adopts_reopened_runtime_without_retaining_the_old_one() {
    // SQLite uses a dedicated OS thread. Keep Tokio runnable while it replies,
    // so paused-time auto-advance cannot skip unrelated timer periods.
    let keep_awake = tokio::spawn(async {
        loop {
            tokio::task::yield_now().await;
        }
    });
    let (temp, db, first) = fixture().await;
    let first_leases = first.leases();
    drop(
        first_leases
            .acquire_many("one", &[PathBuf::from("source")], "active first host")
            .await
            .unwrap(),
    );
    let second = Workspace::open(
        db.clone(),
        first.root().to_path_buf(),
        temp.path().join("data"),
        Redactor::default(),
    )
    .await
    .unwrap();
    let second_leases = second.leases();
    second_leases
        .heartbeat("one", "running", None)
        .await
        .unwrap();
    drop(first_leases);
    drop(first);
    db.write(Priority::Journal, |c| {
        c.execute("UPDATE leases SET expires_at=0 WHERE holder_id='one'", [])?;
        Ok(())
    })
    .await
    .unwrap();
    let (tx, rx) = tokio::sync::oneshot::channel();
    let mut signal = Some(tx);
    db.write(Priority::Journal, move |c| {
        c.update_hook(Some(
            move |_: excalibur_store::rusqlite::hooks::Action, _: &str, table: &str, _: i64| {
                if table == "leases"
                    && let Some(tx) = signal.take()
                {
                    let _ = tx.send(());
                }
            },
        ));
        Ok(())
    })
    .await
    .unwrap();
    // The timer was armed by the first acquisition. Advancing its exact deadline
    // triggers a real writer UPDATE; the hook is the causal completion signal.
    tokio::time::advance(Duration::from_secs(60)).await;
    tokio::time::resume();
    tokio::time::timeout(Duration::from_secs(5), rx)
        .await
        .expect("renewal must survive original manager drop")
        .unwrap();
    // update_hook runs inside the transaction, so await a subsequent writer
    // command before a pooled reader observes the committed renewal.
    db.write(Priority::Background, |_| Ok(())).await.unwrap();
    assert!(second_leases.owners().await.unwrap()[0].expires_at > excalibur_store::unix_seconds());
    second_leases.release_run("one").await.unwrap();
    keep_awake.abort();
    db.shutdown().await.unwrap();
}

#[tokio::test]
async fn setup_writes_acquire_leases_for_created_ready_and_pending_runs() {
    use excalibur_store::rusqlite::params;
    let (_temp, db, w) = fixture().await;
    let leases = w.leases();
    for (run, state) in [
        ("setup-created", "created"),
        ("setup-ready", "ready"),
        ("setup-pending", "pending"),
    ] {
        let (run, state) = (run.to_owned(), state.to_owned());
        db.write(Priority::Journal, move |c| {
            c.execute(
                "INSERT INTO runs(id,state) VALUES(?1,?2)",
                params![run, state],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    }
    // Engine creation records a confirmed run_started seed while the run is
    // still in setup states; setup writes before the first step must succeed.
    for run in ["setup-created", "setup-ready", "setup-pending"] {
        let (id, run) = (format!("seed-{run}"), run.to_owned());
        db.write(Priority::Journal, move |c| {
            c.execute(
                "INSERT INTO journal(id,run_id,sequence,kind,payload,state,idempotency_key) VALUES(?1,?2,1,'run_started','{\"initial_state\":\"created\"}','confirmed','run_started')",
                params![id, run],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    }
    for run in ["setup-created", "setup-ready", "setup-pending"] {
        let guard = leases
            .acquire_many_at(
                run,
                &[PathBuf::from("artifact.txt")],
                "setup write",
                0,
                Duration::ZERO,
            )
            .await
            .unwrap();
        drop(guard);
        leases.release_run(run).await.unwrap();
    }
    db.shutdown().await.unwrap();
}
