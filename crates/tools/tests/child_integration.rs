use excalibur_store::{Database, DatabaseKind, Priority, StorageConfig};
use excalibur_tools::{
    Access, LocalTools, ToolPolicy,
    project::{ChildAuthority, ParentAuthority},
};
use excalibur_types::runtime::*;
use excalibur_workspace::{GitIdentity, IntegrationChecks, TestReport, Worktree};
use serde_json::{Value, json};
use std::{
    path::{Path, PathBuf},
    process::{Command, Output},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

fn git(root: &Path, args: &[&str]) -> Output {
    Command::new("git")
        .arg("-C")
        .arg(root)
        .args([
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args)
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env(
            "GIT_CONFIG_GLOBAL",
            if cfg!(windows) { "NUL" } else { "/dev/null" },
        )
        .output()
        .unwrap()
}
fn git_text(root: &Path, args: &[&str]) -> String {
    let output = git(root, args);
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim_end().into()
}
fn identity(owner: &str) -> GitIdentity {
    GitIdentity {
        name: format!("Synthetic {owner}"),
        email: format!("{owner}@example.invalid"),
    }
}
struct Checks(AtomicUsize);
impl IntegrationChecks for Checks {
    fn verify<'a>(
        &'a self,
        tree: &'a Worktree,
        changed: &'a [String],
    ) -> BoxFuture<'a, excalibur_workspace::Result<TestReport>> {
        Box::pin(async move {
            assert!(!changed.is_empty());
            assert_ne!(
                git_text(&tree.path, &["rev-parse", "HEAD"]),
                git_text(&tree.integration_path, &["rev-parse", "HEAD"]),
                "the test callback must precede advancing integration"
            );
            let output = git(&tree.path, &["diff", "--check", "HEAD~1", "HEAD"]);
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(TestReport {
                passed: output.status.success(),
                checks_run: 1,
                not_applicable_reason: None,
                report: "Actual synthetic git diff --check".into(),
            })
        })
    }
}
struct Fixture {
    _dir: tempfile::TempDir,
    root: PathBuf,
    db: Database,
    tools: LocalTools,
    checks: Arc<Checks>,
    head: String,
    index: String,
}
impl Fixture {
    async fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("project");
        std::fs::create_dir(&root).unwrap();
        git_text(&root, &["init", "-q", "-b", "user-main"]);
        std::fs::write(root.join("base.txt"), b"user baseline\n").unwrap();
        git_text(&root, &["add", "base.txt"]);
        git_text(
            &root,
            &[
                "-c",
                "user.name=Synthetic User",
                "-c",
                "user.email=user@example.invalid",
                "commit",
                "-qm",
                "Synthetic baseline",
            ],
        );
        let head = git_text(&root, &["rev-parse", "HEAD"]);
        // Preserve both pre-existing user index and working-tree changes.
        std::fs::write(root.join("staged.txt"), b"staged user work\n").unwrap();
        git_text(&root, &["add", "staged.txt"]);
        std::fs::write(root.join("base.txt"), b"uncommitted user work\n").unwrap();
        let index = git_text(&root, &["ls-files", "--stage"]);
        let db = Database::open(
            dir.path().join("data/main.db"),
            DatabaseKind::Main,
            StorageConfig::default(),
        )
        .unwrap();
        db.write(Priority::Journal, |c| {
            for id in ["parent", "run-a", "run-b", "run-c", "unbound"] {
                c.execute("INSERT INTO runs(id) VALUES(?1)", [id])?;
            }
            Ok(())
        })
        .await
        .unwrap();
        let tools = LocalTools::new(
            db.clone(),
            root.clone(),
            ToolPolicy {
                write: Access::Allow,
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let checks = Arc::new(Checks(AtomicUsize::new(0)));
        tools
            .authorize_parent(
                "parent",
                ParentAuthority {
                    task: "task".into(),
                    identity: identity("parent"),
                    checks: checks.clone(),
                },
            )
            .unwrap();
        Self {
            _dir: dir,
            root,
            db,
            tools,
            checks,
            head,
            index,
        }
    }
    async fn create_child(&self, owner: &str) -> Worktree {
        let result = invoke(
            &self.tools,
            "parent",
            &format!("create-{owner}"),
            "worktree",
            json!({"action":"create","owner":owner}),
        )
        .await;
        assert!(result.success, "{result:?}");
        let tree: Worktree = serde_json::from_value(result.output["worktree"].clone()).unwrap();
        self.tools
            .authorize_child(
                &format!("run-{owner}"),
                ChildAuthority {
                    task: "task".into(),
                    owner: owner.into(),
                    identity: identity(owner),
                },
            )
            .unwrap();
        tree
    }
    fn assert_user_branch_untouched(&self) {
        assert_eq!(git_text(&self.root, &["rev-parse", "HEAD"]), self.head);
        assert_eq!(
            git_text(&self.root, &["branch", "--show-current"]),
            "user-main"
        );
        assert_eq!(git_text(&self.root, &["ls-files", "--stage"]), self.index);
        assert_eq!(
            std::fs::read(self.root.join("base.txt")).unwrap(),
            b"uncommitted user work\n"
        );
        assert_eq!(
            std::fs::read(self.root.join("staged.txt")).unwrap(),
            b"staged user work\n"
        );
        assert!(git_text(&self.root, &["remote"]).is_empty());
    }
}
async fn queue_id(tools: &LocalTools, child: &str) -> String {
    let listed = invoke(
        tools,
        "parent",
        "parent-list",
        "integration_queue",
        json!({"action":"list"}),
    )
    .await;
    assert!(listed.success, "{listed:?}");
    listed.output["queue"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["child"] == child)
        .unwrap_or_else(|| panic!("no queue entry for {child}"))["id"]
        .as_str()
        .unwrap()
        .to_owned()
}
fn call(id: &str, name: &str, args: Value) -> ToolCall {
    ToolCall {
        id: id.into(),
        name: name.into(),
        arguments: args.to_string(),
    }
}
async fn invoke(tools: &LocalTools, run: &str, key: &str, name: &str, args: Value) -> ToolResult {
    let prepared = tools.prepare(run, call(key, name, args)).await.unwrap();
    tools.execute(run, key, &prepared).await
}

#[tokio::test]
async fn children_checkpoint_submit_and_read_only_their_status_parent_tests_before_each_advance() {
    let f = Fixture::new().await;
    let mut trees = Vec::new();
    for owner in ["a", "b", "c"] {
        let tree = f.create_child(owner).await;
        std::fs::write(
            tree.path.join(format!("{owner}.txt")),
            format!("synthetic child {owner}\n"),
        )
        .unwrap();
        let run = format!("run-{owner}");
        let checkpoint = f
            .tools
            .prepare(
                &run,
                call(
                    "checkpoint",
                    "integration_queue",
                    json!({"action":"checkpoint","message":format!("Synthetic change {owner}")}),
                ),
            )
            .await
            .unwrap();
        let committed = f.tools.execute(&run, "checkpoint", &checkpoint).await;
        assert!(committed.success, "{committed:?}");
        assert_eq!(
            git_text(&tree.path, &["show", "-s", "--format=%an <%ae>", "HEAD"]),
            format!("Synthetic {owner} <{owner}@example.invalid>")
        );
        let before = git_text(&tree.path, &["rev-parse", "HEAD"]);
        assert!(matches!(
            f.tools.reconcile(&run, "checkpoint", &checkpoint).await,
            Reconcile::Done(ToolResult { success: true, .. })
        ));
        assert!(
            f.tools
                .execute(&run, "checkpoint", &checkpoint)
                .await
                .success
        );
        assert_eq!(
            git_text(&tree.path, &["rev-parse", "HEAD"]),
            before,
            "receipt replay must not create a second commit"
        );
        let queued = invoke(
            &f.tools,
            &run,
            "enqueue",
            "integration_queue",
            json!({"action":"enqueue","intent":format!("Synthetic intent {owner}")}),
        )
        .await;
        assert!(queued.success, "{queued:?}");
        trees.push(tree);
    }
    for owner in ["a", "b", "c"] {
        let result = invoke(
            &f.tools,
            &format!("run-{owner}"),
            "list",
            "integration_queue",
            json!({"action":"list"}),
        )
        .await;
        assert!(result.success);
        let queue = result.output["queue"].as_array().unwrap();
        assert_eq!(queue.len(), 1);
        assert_eq!(queue[0]["child"], owner);
        assert_eq!(queue[0]["intent"], format!("Synthetic intent {owner}"));
    }
    let queue = invoke(
        &f.tools,
        "parent",
        "parent-list",
        "integration_queue",
        json!({"action":"list"}),
    )
    .await;
    assert_eq!(queue.output["queue"].as_array().unwrap().len(), 3);
    for index in 0..3 {
        let result = invoke(
            &f.tools,
            "parent",
            &format!("process-{index}"),
            "integration_queue",
            json!({"action":"process"}),
        )
        .await;
        assert!(result.success, "{result:?}");
        assert_eq!(result.output["report"]["state"], "integrated");
        assert_eq!(f.checks.0.load(Ordering::SeqCst), index + 1);
    }
    for owner in ["a", "b", "c"] {
        assert!(
            trees[0]
                .integration_path
                .join(format!("{owner}.txt"))
                .is_file()
        );
    }
    f.assert_user_branch_untouched();
    f.db.shutdown().await.unwrap();
}

#[tokio::test]
async fn child_binding_rejects_foreign_work_queue_mutation_identity_injection_and_parent_actions() {
    let f = Fixture::new().await;
    let a = f.create_child("a").await;
    let b = f.create_child("b").await;
    std::fs::write(a.path.join("a.txt"), "synthetic a\n").unwrap();
    std::fs::write(b.path.join("b.txt"), "synthetic b\n").unwrap();
    let b_head = git_text(&b.path, &["rev-parse", "HEAD"]);
    let b_index = git_text(&b.path, &["ls-files", "--stage"]);
    for args in [
        json!({"action":"checkpoint","owner":"b","message":"Cannot checkpoint b"}),
        json!({"action":"enqueue","owner":"b","intent":"Cannot submit b"}),
        json!({"action":"list","owner":"b"}),
        json!({"action":"process"}),
        json!({"action":"retry","id":"foreign-known-queue-id"}),
        json!({"action":"push"}),
        json!({"action":"enqueue","intent":"No task injection","task":"foreign-task"}),
        json!({"action":"checkpoint","message":"No identity injection","identity":{"name":"Forged","email":"forged@example.invalid"}}),
    ] {
        assert!(
            f.tools
                .prepare("run-a", call("denied", "integration_queue", args.clone()))
                .await
                .is_err(),
            "accepted {args}"
        );
    }
    for action in ["create", "remove", "assign", "checkpoint", "list"] {
        assert!(
            f.tools
                .prepare(
                    "run-a",
                    call(
                        "parent-only",
                        "worktree",
                        json!({"action":action,"owner":"b","message":"Denied","paths":["base.txt"]})
                    )
                )
                .await
                .is_err()
        );
    }
    let prepared = f
        .tools
        .prepare(
            "run-a",
            call(
                "own-checkpoint",
                "integration_queue",
                json!({"action":"checkpoint","message":"Own change"}),
            ),
        )
        .await
        .unwrap();
    let mut forged = prepared.clone();
    forged.data["args"]["owner"] = json!("b");
    let denied = f.tools.execute("run-a", "forged-owner", &forged).await;
    assert!(!denied.success, "{denied:?}");
    for action in ["process", "retry"] {
        let mut forged = prepared.clone();
        forged.data["args"]["action"] = json!(action);
        forged.data["args"]["id"] = json!("foreign-known-queue-id");
        assert!(
            !f.tools
                .execute("run-a", &format!("forged-{action}"), &forged)
                .await
                .success
        );
    }
    assert!(
        f.tools
            .prepare(
                "unbound",
                call("unbound", "integration_queue", json!({"action":"list"}))
            )
            .await
            .is_err()
    );
    assert!(
        f.tools
            .authorize_child(
                "run-a",
                ChildAuthority {
                    task: "task".into(),
                    owner: "b".into(),
                    identity: identity("a")
                }
            )
            .is_err()
    );
    assert!(
        f.tools
            .authorize_child(
                "run-a",
                ChildAuthority {
                    task: "foreign-task".into(),
                    owner: "a".into(),
                    identity: identity("a")
                }
            )
            .is_err()
    );
    assert!(
        f.tools
            .authorize_child(
                "run-a",
                ChildAuthority {
                    task: "task".into(),
                    owner: "a".into(),
                    identity: identity("forged")
                }
            )
            .is_err()
    );
    assert!(
        f.tools
            .authorize_parent(
                "run-a",
                ParentAuthority {
                    task: "task".into(),
                    identity: identity("parent"),
                    checks: f.checks.clone()
                }
            )
            .is_err()
    );
    assert!(
        f.tools
            .authorize_child(
                "parent",
                ChildAuthority {
                    task: "task".into(),
                    owner: "a".into(),
                    identity: identity("a")
                }
            )
            .is_err()
    );
    // An exact host retransmission is harmless, but it cannot widen the binding.
    f.tools
        .authorize_child(
            "run-a",
            ChildAuthority {
                task: "task".into(),
                owner: "a".into(),
                identity: identity("a"),
            },
        )
        .unwrap();
    assert_eq!(git_text(&b.path, &["rev-parse", "HEAD"]), b_head);
    assert_eq!(git_text(&b.path, &["ls-files", "--stage"]), b_index);
    assert_eq!(
        std::fs::read_to_string(b.path.join("b.txt")).unwrap(),
        "synthetic b\n"
    );
    assert!(
        f.tools
            .workspace()
            .git()
            .list("task")
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(f.checks.0.load(Ordering::SeqCst), 0);
    f.assert_user_branch_untouched();
    f.db.shutdown().await.unwrap();
}

#[tokio::test]
async fn conflict_resolution_retry_rebases_checks_and_integrates() {
    let f = Fixture::new().await;
    let a = f.create_child("a").await;
    let b = f.create_child("b").await;
    std::fs::write(a.path.join("base.txt"), "child a line\n").unwrap();
    std::fs::write(b.path.join("base.txt"), "child b line\n").unwrap();
    for owner in ["a", "b"] {
        let run = format!("run-{owner}");
        let checkpoint = f
            .tools
            .prepare(
                &run,
                call(
                    "checkpoint",
                    "integration_queue",
                    json!({"action":"checkpoint","message":format!("Synthetic change {owner}")}),
                ),
            )
            .await
            .unwrap();
        let committed = f.tools.execute(&run, "checkpoint", &checkpoint).await;
        assert!(committed.success, "{committed:?}");
        let queued = invoke(
            &f.tools,
            &run,
            "enqueue",
            "integration_queue",
            json!({"action":"enqueue","intent":format!("Synthetic intent {owner}")}),
        )
        .await;
        assert!(queued.success, "{queued:?}");
    }
    // The first child advances the integration branch.
    let first = invoke(
        &f.tools,
        "parent",
        "process-0",
        "integration_queue",
        json!({"action":"process"}),
    )
    .await;
    assert!(first.success, "{first:?}");
    assert_eq!(first.output["report"]["state"], "integrated");
    // The second child now conflicts when rebased onto the advanced tip.
    let conflicted = invoke(
        &f.tools,
        "parent",
        "process-1",
        "integration_queue",
        json!({"action":"process"}),
    )
    .await;
    assert!(conflicted.success, "{conflicted:?}");
    assert_eq!(conflicted.output["report"]["state"], "conflicted");
    // The parent resolves the conflict inside the child worktree and stages it.
    std::fs::write(b.path.join("base.txt"), "resolved line\n").unwrap();
    git_text(&b.path, &["add", "base.txt"]);
    let id = queue_id(&f.tools, "b").await;
    let retry = invoke(
        &f.tools,
        "parent",
        "retry-b",
        "integration_queue",
        json!({"action":"retry","id":id.clone()}),
    )
    .await;
    assert!(retry.success, "{retry:?}");
    // Without new state the same entry cannot retry twice.
    let again = invoke(
        &f.tools,
        "parent",
        "retry-b-again",
        "integration_queue",
        json!({"action":"retry","id":id}),
    )
    .await;
    assert!(!again.success, "{again:?}");
    // The re-queued checkpoint rebases onto the advanced tip, passes the
    // impacted checks, and advances the integration branch.
    let done = invoke(
        &f.tools,
        "parent",
        "process-2",
        "integration_queue",
        json!({"action":"process"}),
    )
    .await;
    assert!(done.success, "{done:?}");
    assert_eq!(done.output["report"]["state"], "integrated");
    assert_eq!(
        std::fs::read_to_string(b.integration_path.join("base.txt")).unwrap(),
        "resolved line\n"
    );
    assert_eq!(f.checks.0.load(Ordering::SeqCst), 2);
    f.assert_user_branch_untouched();
    f.db.shutdown().await.unwrap();
}

#[tokio::test]
async fn failed_checks_retry_with_new_checkpoint_integrates() {
    let f = Fixture::new().await;
    let c = f.create_child("c").await;
    // Trailing whitespace fails the `git diff --check` impacted check.
    std::fs::write(c.path.join("c.txt"), "synthetic c  \n").unwrap();
    let checkpoint = f
        .tools
        .prepare(
            "run-c",
            call(
                "checkpoint",
                "integration_queue",
                json!({"action":"checkpoint","message":"Synthetic change c"}),
            ),
        )
        .await
        .unwrap();
    assert!(
        f.tools
            .execute("run-c", "checkpoint", &checkpoint)
            .await
            .success
    );
    let queued = invoke(
        &f.tools,
        "run-c",
        "enqueue",
        "integration_queue",
        json!({"action":"enqueue","intent":"Synthetic intent c"}),
    )
    .await;
    assert!(queued.success, "{queued:?}");
    let returned = invoke(
        &f.tools,
        "parent",
        "process-0",
        "integration_queue",
        json!({"action":"process"}),
    )
    .await;
    assert!(returned.success, "{returned:?}");
    assert_eq!(returned.output["report"]["state"], "returned");
    // The child fixes the file and records a new checkpoint.
    std::fs::write(c.path.join("c.txt"), "synthetic c\n").unwrap();
    let checkpoint = f
        .tools
        .prepare(
            "run-c",
            call(
                "checkpoint-2",
                "integration_queue",
                json!({"action":"checkpoint","message":"Synthetic fix c"}),
            ),
        )
        .await
        .unwrap();
    let fixed = f.tools.execute("run-c", "checkpoint-2", &checkpoint).await;
    assert!(fixed.success, "{fixed:?}");
    // Retry adopts the new checkpoint instead of returning it as changed.
    let id = queue_id(&f.tools, "c").await;
    let retry = invoke(
        &f.tools,
        "parent",
        "retry-c",
        "integration_queue",
        json!({"action":"retry","id":id}),
    )
    .await;
    assert!(retry.success, "{retry:?}");
    let done = invoke(
        &f.tools,
        "parent",
        "process-1",
        "integration_queue",
        json!({"action":"process"}),
    )
    .await;
    assert!(done.success, "{done:?}");
    assert_eq!(done.output["report"]["state"], "integrated");
    assert!(c.integration_path.join("c.txt").is_file());
    f.assert_user_branch_untouched();
    f.db.shutdown().await.unwrap();
}
