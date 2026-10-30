#![cfg(target_os = "linux")]
//! Real confined worktree routing and Git integration with synthetic identities.
use excalibur_daemon::{GitOptions, WorkerTools};
use excalibur_engine::Budget;
use excalibur_orchestrator::{Agent, Delegation};
use excalibur_security::{Autonomy, CapabilityToken, Root, RunSecurity, SecuredTools, Security};
use excalibur_store::{Database, DatabaseKind, Priority, StorageConfig};
use excalibur_tools::{
    Access, LocalTools, ToolPolicy,
    terminal::{SandboxConfig, prepare_project_boundary},
};
use excalibur_types::runtime::*;
use excalibur_vault::Redactor;
use excalibur_workspace::{GitIdentity, IntegrationChecks, TestReport, Worktree};
use serde_json::{Value, json};
use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
    process::Command,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};
fn git(root: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
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
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap().trim_end().into()
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
            git(&tree.path, &["diff", "--check", "HEAD~1", "HEAD"]);
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(TestReport {
                passed: true,
                checks_run: 1,
                not_applicable_reason: None,
                report: "Real synthetic git diff --check".into(),
            })
        })
    }
}
fn agent(
    id: &str,
    run: &str,
    parent: Option<&Agent>,
    token: CapabilityToken,
    owner: Option<&str>,
) -> Agent {
    Agent {
        id: id.into(),
        parent_id: parent.map(|p| p.id.clone()),
        run_id: Some(run.into()),
        depth: if parent.is_some() { 2 } else { 1 },
        name: id.into(),
        profile: if parent.is_some() {
            "implementer"
        } else {
            "coordinator"
        }
        .into(),
        profile_id: None,
        scope: "general".into(),
        task: format!("task:{id}"),
        origin_conversation: "synthetic-conversation".into(),
        token,
        budget: Budget::default(),
        delegation: owner.map(|owner| Delegation {
            operation: format!("operation:{id}"),
            name: id.into(),
            profile: "implementer".into(),
            scope: "general".into(),
            task: format!("task:{id}"),
            origin_conversation: "synthetic-conversation".into(),
            goal: "Synthetic worktree edit".into(),
            deliverable: "A checked local commit".into(),
            inputs: vec![format!("worktree-owner:{owner}")],
            pinned_instructions: vec![],
            suggested_skills: vec![],
            fork_context: false,
            token: CapabilityToken::default(),
            spec: excalibur_engine::RunSpec {
                context: excalibur_context::ContextInput::new(
                    "Synthetic rules",
                    "Synthetic task",
                    "fixed",
                ),
                budget: Budget::default(),
                criteria: vec![excalibur_engine::Criterion::FinalResponse { schema: None }],
            },
            priority: 1,
        }),
    }
}
async fn invoke(
    runtime: &SecuredTools,
    run: &str,
    key: &str,
    name: &str,
    args: Value,
) -> ToolResult {
    let call = ToolCall {
        id: key.into(),
        name: name.into(),
        arguments: args.to_string(),
    };
    let prepared = match runtime.prepare(run, call.clone()).await {
        Ok(prepared) => prepared,
        Err(error) => {
            let approval = error
                .strip_prefix("approval_required:")
                .and_then(|s| s.split(':').next())
                .unwrap_or_else(|| panic!("prepare {name}: {error}"));
            // This fixture represents an explicit authenticated synthetic user
            // approving this concrete action, not a relaxed production policy.
            runtime
                .security()
                .answer_approval(
                    approval,
                    true,
                    excalibur_security::ApprovalScope::Task,
                    &format!("synthetic-user:{key}"),
                    excalibur_store::unix_seconds(),
                )
                .await
                .unwrap();
            runtime
                .prepare(run, call)
                .await
                .unwrap_or_else(|e| panic!("approved {name}: {e}"))
        }
    };
    let mut result = runtime.execute(run, key, &prepared).await;
    assert!(result.success, "{name}: {:?}", result.output);
    if name == "exec" && result.output["exited"] != true {
        let session = result.output["session_id"].as_str().unwrap();
        let wait_key = format!("wait:{key}");
        let wait = runtime
            .prepare(
                run,
                ToolCall {
                    id: wait_key.clone(),
                    name: "exec_io".into(),
                    arguments: json!({"action":"wait_for","session_id":session,"timeout_ms":30000})
                        .to_string(),
                },
            )
            .await
            .unwrap();
        result = runtime.execute(run, &wait_key, &wait).await;
        assert!(
            result.success && result.output["exited"] == true && result.output["exit_code"] == 0,
            "{result:?}"
        );
    }
    result
}
struct Fixture {
    _dir: tempfile::TempDir,
    root: PathBuf,
    db: Database,
    source: Arc<LocalTools>,
    worker: Arc<WorkerTools>,
    security: Security,
    runtime: SecuredTools,
    parent: Agent,
    checks: Arc<Checks>,
    head: String,
    index: String,
}
impl Fixture {
    async fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("project");
        std::fs::create_dir(&root).unwrap();
        git(&root, &["init", "-q", "-b", "user-main"]);
        std::fs::write(root.join("base.txt"), "Synthetic baseline\n").unwrap();
        std::fs::write(root.join(".gitignore"), "build/\n").unwrap();
        git(&root, &["add", "base.txt", ".gitignore"]);
        git(
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
        std::fs::write(root.join("staged.txt"), "Synthetic user index\n").unwrap();
        git(&root, &["add", "staged.txt"]);
        std::fs::write(root.join("base.txt"), "Synthetic user work in progress\n").unwrap();
        let head = git(&root, &["rev-parse", "HEAD"]);
        let index = git(&root, &["ls-files", "--stage"]);
        prepare_project_boundary(&root).unwrap();
        let db = Database::open(
            dir.path().join("data/main.db"),
            DatabaseKind::Main,
            StorageConfig::default(),
        )
        .unwrap();
        db.write(Priority::Journal, |c| {
            for id in [
                "parent",
                "child-a",
                "child-b",
                "child-c",
                "duplicate",
                "unknown",
            ] {
                c.execute("INSERT INTO runs(id,state) VALUES(?1,'ready')", [id])?;
            }
            Ok(())
        })
        .await
        .unwrap();
        let policy = ToolPolicy {
            read: Access::Allow,
            write: Access::Allow,
            exec: Access::Allow,
            sandbox: Some(SandboxConfig::project(root.clone())),
            ..Default::default()
        };
        let source = Arc::new(
            LocalTools::new(db.clone(), root.clone(), policy.clone())
                .await
                .unwrap(),
        );
        let checks = Arc::new(Checks(AtomicUsize::new(0)));
        let worker = Arc::new(
            WorkerTools::new(
                db.clone(),
                source.clone(),
                policy,
                vec![],
                Some(GitOptions {
                    identity: GitIdentity {
                        name: "Synthetic Integration".into(),
                        email: "integration@example.invalid".into(),
                    },
                    checks: checks.clone(),
                }),
            )
            .await
            .unwrap(),
        );
        let base = worker.managed_worktree_roots()["general"].clone();
        let token = CapabilityToken {
            roots: vec![
                Root {
                    path: root.clone(),
                    write: true,
                },
                Root {
                    path: base,
                    write: true,
                },
            ],
            processes: true,
            sandbox_profile: Some("project".into()),
            data_scopes: BTreeSet::from(["general".into()]),
            external_actions: BTreeSet::from([
                "tool:worktree".into(),
                "tool:integration_queue".into(),
            ]),
            budget: 1000,
            ..Default::default()
        };
        let security = Security::new(db.clone(), token.clone(), Redactor::default())
            .await
            .unwrap();
        let parent = agent("parent-agent", "parent", None, token, None);
        let binding = worker.bind_agent(&parent, None).await.unwrap();
        security
            .register_run(
                "parent",
                RunSecurity {
                    parent: None,
                    project: "general".into(),
                    workspace: binding.workspace,
                    token: binding.token,
                    autonomy: vec![Autonomy::Autonomous],
                    full_acknowledged: false,
                    user_requested: true,
                },
                0,
            )
            .await
            .unwrap();
        let runtime = SecuredTools::new(worker.clone(), security.clone());
        Self {
            _dir: dir,
            root,
            db,
            source,
            worker,
            security,
            runtime,
            parent,
            checks,
            head,
            index,
        }
    }
    async fn child(&self, owner: &str) -> Agent {
        let run = format!("child-{owner}");
        invoke(
            &self.runtime,
            "parent",
            &format!("create-{owner}"),
            "worktree",
            json!({"action":"create","owner":owner}),
        )
        .await;
        let mut child = agent(
            &format!("agent-{owner}"),
            &run,
            Some(&self.parent),
            self.parent.token.clone(),
            Some(owner),
        );
        let binding = self
            .worker
            .bind_agent(&child, Some(&self.parent))
            .await
            .unwrap();
        child.token = binding.token.clone();
        self.security
            .register_run(
                &run,
                RunSecurity {
                    parent: Some("parent".into()),
                    project: "general".into(),
                    workspace: binding.workspace,
                    token: binding.token,
                    autonomy: vec![Autonomy::Autonomous],
                    full_acknowledged: false,
                    user_requested: true,
                },
                0,
            )
            .await
            .unwrap();
        child
    }
}
#[tokio::test]
async fn three_children_edit_execute_checkpoint_and_integrate_in_registered_worktrees() {
    let f = Fixture::new().await;
    let a = f.child("a").await;
    let b = f.child("b").await;
    let c = f.child("c").await;
    let children = [a, b, c];
    for child in &children {
        let run = child.run_id.as_deref().unwrap();
        let path = format!("{}.txt", child.name);
        invoke(
            &f.runtime,
            run,
            &format!("write:{}", child.name),
            "write_file",
            json!({"path":path,"content":format!("Synthetic {}\n",child.name)}),
        )
        .await;
        let own = f.worker.workspace_for(run).unwrap();
        assert_eq!(child.token.roots.len(), 1);
        assert_eq!(child.token.roots[0].path, own);
        assert!(!f.root.join(&path).exists());
        let owner = child.name.strip_prefix("agent-").unwrap();
        let result = invoke(
            &f.runtime,
            run,
            &format!("exec:{owner}"),
            "exec",
            json!({"cmd":"mkdir -p build/out && printf 'int main(void){return 0;}\n' >build/out/synthetic.c && cc build/out/synthetic.c -o build/out/synthetic && build/out/synthetic && printf 'confined worktree command'","yield_ms":1000,"timeout_ms":30000}),
        )
        .await;
        assert!(
            result.output["output"]
                .as_str()
                .unwrap()
                .contains("confined worktree command")
        );
        invoke(
            &f.runtime,
            run,
            &format!("checkpoint:{owner}"),
            "integration_queue",
            json!({"action":"checkpoint","owner":owner,"message":"Synthetic checked child change"}),
        )
        .await;
        invoke(
            &f.runtime,
            run,
            &format!("enqueue:{owner}"),
            "integration_queue",
            json!({"action":"enqueue","owner":owner,"intent":"Synthetic independent child edit"}),
        )
        .await;
    }
    for n in 0..3 {
        let result = invoke(
            &f.runtime,
            "parent",
            &format!("integrate:{n}"),
            "integration_queue",
            json!({"action":"process"}),
        )
        .await;
        assert_eq!(result.output["report"]["state"], "integrated");
    }
    assert_eq!(f.checks.0.load(Ordering::SeqCst), 3);
    let tree = f
        .source
        .workspace()
        .git()
        .worktree(&f.parent.task, "a")
        .await
        .unwrap();
    for child in &children {
        let path = format!("{}.txt", child.name);
        assert_eq!(
            git(&tree.integration_path, &["show", &format!("HEAD:{path}")]),
            format!("Synthetic {}", child.name)
        );
    }
    assert_eq!(git(&f.root, &["rev-parse", "HEAD"]), f.head);
    assert_eq!(git(&f.root, &["ls-files", "--stage"]), f.index);
    assert_eq!(
        std::fs::read_to_string(f.root.join("base.txt")).unwrap(),
        "Synthetic user work in progress\n"
    );
    f.worker.shutdown().await;
}
#[tokio::test]
async fn worker_owner_and_private_siblings_remain_isolated_in_files_and_real_processes() {
    let f = Fixture::new().await;
    let a = f.child("a").await;
    let b = f.child("b").await;
    let run = a.run_id.as_deref().unwrap();
    let sibling = f
        .worker
        .workspace_for(b.run_id.as_deref().unwrap())
        .unwrap()
        .join("private.txt");
    std::fs::write(&sibling, "SYNTHETIC_SIBLING_CANARY").unwrap();
    let private = f.db.path().parent().unwrap().join("private.txt");
    std::fs::write(&private, "SYNTHETIC_PRIVATE_CANARY").unwrap();
    for path in [&private, &sibling, f.db.path()] {
        let denied = f
            .runtime
            .prepare(
                run,
                ToolCall {
                    id: "denied-read".into(),
                    name: "read_file".into(),
                    arguments: json!({"path":path}).to_string(),
                },
            )
            .await;
        assert!(denied.is_err());
    }
    let denied = f
        .runtime
        .prepare(
            run,
            ToolCall {
                id: "denied-owner".into(),
                name: "integration_queue".into(),
                arguments:
                    json!({"action":"checkpoint","owner":"b","message":"Denied cross-owner"})
                        .to_string(),
            },
        )
        .await;
    assert!(denied.is_err());
    let duplicate = agent(
        "duplicate-agent",
        "duplicate",
        Some(&f.parent),
        f.parent.token.clone(),
        Some("a"),
    );
    assert!(
        f.worker
            .bind_agent(&duplicate, Some(&f.parent))
            .await
            .is_err()
    );
    let unknown = agent(
        "unknown-agent",
        "unknown",
        Some(&f.parent),
        f.parent.token.clone(),
        Some("not-created"),
    );
    assert!(
        f.worker
            .bind_agent(&unknown, Some(&f.parent))
            .await
            .is_err()
    );
    let alias = f
        .worker
        .workspace_for(run)
        .unwrap()
        .join("private-alias.txt");
    std::fs::hard_link(&private, &alias).unwrap();
    let prepared = f
        .runtime
        .prepare(
            run,
            ToolCall {
                id: "denied-alias".into(),
                name: "read_file".into(),
                arguments: json!({"path":"private-alias.txt"}).to_string(),
            },
        )
        .await
        .unwrap();
    let denied = f.runtime.execute(run, "denied-alias", &prepared).await;
    assert!(!denied.success);
    assert!(denied.output.to_string().contains("hardlink"));
    assert!(
        !denied
            .output
            .to_string()
            .contains("SYNTHETIC_PRIVATE_CANARY")
    );
    std::fs::remove_file(alias).unwrap();
    let cmd = format!(
        "mkdir -p build/out && printf ready >build/out/result.txt && test ! -e '{}' && test ! -e '{}' && test ! -e '{}' && printf 'private boundaries held'",
        private.display(),
        sibling.display(),
        f.db.path().display()
    );
    let result = invoke(
        &f.runtime,
        run,
        "private-exec",
        "exec",
        json!({"cmd":cmd,"yield_ms":1000,"timeout_ms":30000}),
    )
    .await;
    assert!(
        result.output["output"]
            .as_str()
            .unwrap()
            .contains("private boundaries held")
    );
    assert!(
        !result
            .output
            .to_string()
            .contains("SYNTHETIC_PRIVATE_CANARY")
    );
    assert!(
        !result
            .output
            .to_string()
            .contains("SYNTHETIC_SIBLING_CANARY")
    );
    let verifier = f.worker.verifier_tools(run).await.unwrap();
    assert!(
        verifier
            .prepare(
                run,
                ToolCall {
                    id: "read-only-denial".into(),
                    name: "write_file".into(),
                    arguments: json!({"path":"denied.txt","content":"denied"}).to_string()
                }
            )
            .await
            .is_err()
    );
    let explicit = LocalTools::for_registered_worktree(
        f.db.clone(),
        f.source.workspace(),
        &f.parent.task,
        "a",
        ToolPolicy {
            write: Access::Allow,
            protected_paths: vec![f.db.path().parent().unwrap().join("workspaces")],
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert!(
        explicit
            .prepare(
                run,
                ToolCall {
                    id: "explicit-denial".into(),
                    name: "write_file".into(),
                    arguments: json!({"path":"denied.txt","content":"denied"}).to_string()
                }
            )
            .await
            .is_err()
    );
    f.worker.shutdown().await;
}

#[tokio::test]
async fn general_runtime_cannot_read_registered_projects_but_scoped_runtime_can() {
    let f = Fixture::new().await;
    let a = f.root.join("a");
    let b = f.root.join("b");
    std::fs::create_dir_all(&a).unwrap();
    std::fs::create_dir_all(&b).unwrap();
    std::fs::write(a.join("data.txt"), "PROJECT_A_CANARY").unwrap();
    std::fs::write(b.join("data.txt"), "PROJECT_B_CANARY").unwrap();
    std::fs::write(f.root.join("general.txt"), "GENERAL_DOCUMENT").unwrap();
    let projects = [("a", a.clone()), ("b", b.clone())]
        .into_iter()
        .map(|(id, root)| excalibur_orchestrator::Project {
            id: id.into(),
            name: id.into(),
            aliases: vec![],
            description: String::new(),
            roots: vec![root],
            status: "active".into(),
            vcs: None,
            settings: json!({}),
        })
        .collect();
    let workers = WorkerTools::new(
        f.db.clone(),
        f.source.clone(),
        ToolPolicy {
            read: Access::Allow,
            write: Access::Allow,
            exec: Access::Allow,
            sandbox: Some(SandboxConfig::project(f.root.clone())),
            ..Default::default()
        },
        projects,
        None,
    )
    .await
    .unwrap();
    workers.bind_agent(&f.parent, None).await.unwrap();
    let call = |id: &str, path: &Path| ToolCall {
        id: id.into(),
        name: "read_file".into(),
        arguments: json!({"path":path}).to_string(),
    };
    assert!(
        workers
            .prepare("parent", call("general-denied-a", &a.join("data.txt")))
            .await
            .is_err()
    );
    let prepared = workers
        .prepare("parent", call("general-doc", &f.root.join("general.txt")))
        .await
        .unwrap();
    let read = workers.execute("parent", "general-doc", &prepared).await;
    assert!(read.success);
    assert!(read.output.to_string().contains("GENERAL_DOCUMENT"));
    let verifier = workers.verifier_tools("parent").await.unwrap();
    assert!(
        verifier
            .prepare("parent", call("verifier-denied-a", &a.join("data.txt")))
            .await
            .is_err()
    );
    f.db.write(Priority::Journal, |c| {
        c.execute("INSERT INTO runs(id,state) VALUES('scope-a','ready')", [])?;
        Ok(())
    })
    .await
    .unwrap();
    let mut scoped = agent(
        "scope-a-agent",
        "scope-a",
        None,
        f.parent.token.clone(),
        None,
    );
    scoped.scope = "project:a".into();
    scoped.token.data_scopes.insert("project:a".into());
    workers.bind_agent(&scoped, None).await.unwrap();
    let prepared = workers
        .prepare("scope-a", call("own-a", &a.join("data.txt")))
        .await
        .unwrap();
    let read = workers.execute("scope-a", "own-a", &prepared).await;
    assert!(read.success);
    assert!(read.output.to_string().contains("PROJECT_A_CANARY"));
    assert!(
        workers
            .prepare("scope-a", call("other-b", &b.join("data.txt")))
            .await
            .is_err()
    );
    workers.shutdown().await;
    f.worker.shutdown().await;
}

#[tokio::test]
async fn explicit_cross_project_roots_confine_files_search_snapshots_verifier_and_real_exec() {
    let f = Fixture::new().await;
    let roots: Vec<_> = ["a", "b", "c"]
        .into_iter()
        .map(|n| f.root.join(n))
        .collect();
    for (i, root) in roots.iter().enumerate() {
        std::fs::create_dir_all(root).unwrap();
        std::fs::write(root.join("data.txt"), format!("PROJECT_{}_CANARY\n", i)).unwrap();
    }
    let projects = roots
        .iter()
        .zip(["a", "b", "c"])
        .map(|(root, id)| excalibur_orchestrator::Project {
            id: id.into(),
            name: id.into(),
            aliases: vec![],
            description: String::new(),
            roots: vec![root.clone()],
            status: "active".into(),
            vcs: None,
            settings: json!({}),
        })
        .collect();
    let workers = Arc::new(
        WorkerTools::new(
            f.db.clone(),
            f.source.clone(),
            ToolPolicy {
                read: Access::Allow,
                write: Access::Allow,
                exec: Access::Allow,
                sandbox: Some(SandboxConfig::project(f.root.clone())),
                ..Default::default()
            },
            projects,
            None,
        )
        .await
        .unwrap(),
    );
    let mut cross = f.parent.clone();
    cross.id = "cross-agent".into();
    cross.run_id = Some("cross".into());
    f.db.write(Priority::Journal, |c| {
        c.execute("INSERT INTO runs(id,state) VALUES('cross','ready')", [])?;
        Ok(())
    })
    .await
    .unwrap();
    cross.scope = "project:a".into();
    cross.token.roots = roots[..2]
        .iter()
        .map(|p| Root {
            path: p.clone(),
            write: true,
        })
        .collect();
    cross.token.data_scopes =
        BTreeSet::from(["general".into(), "project:a".into(), "project:b".into()]);
    let (historic, _) = f
        .source
        .workspace()
        .shadow()
        .snapshot_files(
            "cross",
            999,
            "before",
            std::collections::BTreeMap::from([
                (PathBuf::from("a/data.txt"), b"old permitted".to_vec()),
                (
                    PathBuf::from("c/private.txt"),
                    b"HISTORICAL_PRIVATE".to_vec(),
                ),
            ]),
        )
        .await
        .unwrap();
    let bound = workers.bind_agent(&cross, None).await.unwrap();
    assert_eq!(bound.workspace, f.root);
    let security = Security::new(
        f.db.clone(),
        CapabilityToken {
            roots: vec![Root {
                path: f.root.clone(),
                write: true,
            }],
            data_scopes: BTreeSet::from([
                "general".into(),
                "project:a".into(),
                "project:b".into(),
                "project:c".into(),
            ]),
            ..cross.token.clone()
        },
        Redactor::default(),
    )
    .await
    .unwrap();
    security
        .register_run(
            "cross",
            RunSecurity {
                parent: None,
                project: cross.scope.clone(),
                workspace: bound.workspace,
                token: bound.token,
                autonomy: vec![Autonomy::Autonomous],
                full_acknowledged: false,
                user_requested: true,
            },
            0,
        )
        .await
        .unwrap();
    let secured = SecuredTools::new(workers.clone(), security);
    for name in ["a", "b"] {
        invoke(
            &secured,
            "cross",
            &format!("read-{name}"),
            "read_file",
            json!({"path":format!("{name}/data.txt")}),
        )
        .await;
    }
    let verifier = workers.verifier_tools("cross").await.unwrap();
    std::fs::write(f.root.join("future-general.txt"), "FUTURE_GENERAL_PRIVATE").unwrap();
    let file_call = |id: &str, path: &str| ToolCall {
        id: id.into(),
        name: "read_file".into(),
        arguments: json!({"path":path}).to_string(),
    };
    for path in ["c/data.txt", "future-general.txt"] {
        assert!(
            workers
                .prepare("cross", file_call(path, path))
                .await
                .is_err(),
            "{path}"
        );
        assert!(
            verifier
                .prepare("cross", file_call(path, path))
                .await
                .is_err(),
            "verifier {path}"
        );
    }
    let verify_call = ToolCall {
        id: "cross-readonly-probe".into(), name: "exec".into(),
        arguments: json!({"cmd":"test -r a/data.txt && test -r b/data.txt && test ! -e c/data.txt && test ! -e future-general.txt && printf verifier_confined","yield_ms":1000}).to_string(),
    };
    let prepared = verifier.prepare("cross", verify_call).await.unwrap();
    let mut checked = verifier
        .execute("cross", "cross-readonly-probe", &prepared)
        .await;
    if checked.output["exited"] != true {
        let session = checked.output["session_id"].as_str().unwrap();
        let wait = verifier
            .prepare(
                "cross",
                ToolCall {
                    id: "readonly-wait".into(),
                    name: "exec_io".into(),
                    arguments: json!({"action":"wait_for","session_id":session,"timeout_ms":30000})
                        .to_string(),
                },
            )
            .await
            .unwrap();
        checked = verifier.execute("cross", "readonly-wait", &wait).await;
    }
    assert!(
        checked.success && checked.output.to_string().contains("verifier_confined"),
        "{checked:?}"
    );
    let found = invoke(
        &secured,
        "cross",
        "search",
        "grep",
        json!({"pattern":"CANARY"}),
    )
    .await;
    assert!(
        !found.output.to_string().contains("c/data.txt"),
        "{found:?}"
    );
    let paths = invoke(
        &secured,
        "cross",
        "paths",
        "glob",
        json!({"pattern":"**/*.txt"}),
    )
    .await;
    assert!(
        !paths.output.to_string().contains("future-general"),
        "{paths:?}"
    );
    assert!(
        !paths.output.to_string().contains("c/data.txt"),
        "{paths:?}"
    );
    // Multiple authorized roots can share an inode; an alias into an excluded
    // root cannot turn an allowed pathname into authority to read private bytes.
    std::fs::hard_link(roots[0].join("data.txt"), roots[1].join("alias.txt")).unwrap();
    invoke(
        &secured,
        "cross",
        "allowed-link",
        "read_file",
        json!({"path":"b/alias.txt"}),
    )
    .await;
    std::fs::hard_link(
        roots[2].join("data.txt"),
        roots[0].join("forbidden-link.txt"),
    )
    .unwrap();
    let prepared = workers
        .prepare("cross", file_call("badlink", "a/forbidden-link.txt"))
        .await
        .unwrap();
    assert!(!workers.execute("cross", "badlink", &prepared).await.success);
    std::fs::remove_file(roots[0].join("forbidden-link.txt")).unwrap();
    workers.begin_step("cross", 0).await.unwrap();
    let command = "cat a/data.txt b/data.txt; test ! -e c/data.txt && test ! -e future-general.txt && printf 'changed A\n' > a/new.txt && printf 'changed B\n' > b/new.txt";
    let result = invoke(
        &secured,
        "cross",
        "cross-exec",
        "exec",
        json!({"cmd":command,"yield_ms":1000}),
    )
    .await;
    assert!(
        result.output.to_string().contains("PROJECT_0_CANARY"),
        "{result:?}"
    );
    assert!(
        result.output.to_string().contains("PROJECT_1_CANARY"),
        "{result:?}"
    );
    workers.end_step("cross", 0).await.unwrap();
    let snapshots = invoke(
        &secured,
        "cross",
        "snapshots",
        "workspace_snapshot",
        json!({"action":"list"}),
    )
    .await;
    let values = snapshots.output["snapshots"].as_array().unwrap();
    assert!(!values.iter().any(|snapshot| snapshot["id"] == historic.id));
    let before = values.iter().find(|v| v["phase"] == "before").unwrap()["id"]
        .as_str()
        .unwrap();
    let after = values.iter().find(|v| v["phase"] == "after").unwrap()["id"]
        .as_str()
        .unwrap();
    for snapshot in [before, after] {
        let retained = f
            .source
            .workspace()
            .shadow()
            .snapshot_paths(snapshot)
            .await
            .unwrap();
        assert!(
            retained
                .iter()
                .all(|path| path.starts_with("a") || path.starts_with("b")),
            "{retained:?}"
        );
    }
    let diff = invoke(
        &secured,
        "cross",
        "diff",
        "workspace_snapshot",
        json!({"action":"diff","before":before,"after":after}),
    )
    .await;
    assert!(
        secured
            .prepare(
                "cross",
                ToolCall {
                    id: "historical-denied".into(),
                    name: "workspace_snapshot".into(),
                    arguments: json!({"action":"diff","before":historic.id,"after":after})
                        .to_string()
                }
            )
            .await
            .is_err()
    );
    let text = diff.output.to_string();
    assert!(
        text.contains("a/new.txt") && text.contains("b/new.txt"),
        "{text}"
    );
    assert!(
        !text.contains("c/data")
            && !text.contains("future-general")
            && !text.contains("PROJECT_2_CANARY"),
        "{text}"
    );
    // Generic delegates without private positive authority retain the common
    // root requirement even when their model arguments claim narrower paths.
    let generic = SecuredTools::new(f.source.clone(), secured.security().clone());
    assert!(
        generic
            .prepare(
                "cross",
                ToolCall {
                    id: "generic-search".into(),
                    name: "glob".into(),
                    arguments: json!({"pattern":"a/**"}).to_string()
                }
            )
            .await
            .is_err()
    );
    workers.shutdown().await;
    f.worker.shutdown().await;
}

#[tokio::test]
async fn single_project_cannot_read_a_nested_ungranted_project_or_its_verifier() {
    let f = Fixture::new().await;
    let a = f.root.join("a");
    let b = a.join("nested-b");
    std::fs::create_dir_all(&b).unwrap();
    std::fs::write(a.join("own.txt"), "OWN_PROJECT").unwrap();
    std::fs::write(b.join("private.txt"), "NESTED_PROJECT_PRIVATE").unwrap();
    let projects = [("a", a.clone()), ("b", b.clone())]
        .into_iter()
        .map(|(id, root)| excalibur_orchestrator::Project {
            id: id.into(),
            name: id.into(),
            aliases: vec![],
            description: String::new(),
            roots: vec![root],
            status: "active".into(),
            vcs: None,
            settings: json!({}),
        })
        .collect();
    let workers = WorkerTools::new(
        f.db.clone(),
        f.source.clone(),
        ToolPolicy {
            read: Access::Allow,
            write: Access::Allow,
            exec: Access::Allow,
            sandbox: Some(SandboxConfig::project(f.root.clone())),
            ..Default::default()
        },
        projects,
        None,
    )
    .await
    .unwrap();
    let mut scoped = f.parent.clone();
    scoped.id = "nested-agent".into();
    scoped.run_id = Some("nested".into());
    f.db.write(Priority::Journal, |c| {
        c.execute("INSERT INTO runs(id,state) VALUES('nested','ready')", [])?;
        Ok(())
    })
    .await
    .unwrap();
    scoped.scope = "project:a".into();
    scoped.token.roots = vec![Root {
        path: a,
        write: true,
    }];
    scoped.token.data_scopes = BTreeSet::from(["general".into(), "project:a".into()]);
    workers.bind_agent(&scoped, None).await.unwrap();
    let call = |path: &str| ToolCall {
        id: path.into(),
        name: "read_file".into(),
        arguments: json!({"path":path}).to_string(),
    };
    let own = workers.prepare("nested", call("own.txt")).await.unwrap();
    assert!(workers.execute("nested", "own", &own).await.success);
    assert!(
        workers
            .prepare("nested", call("nested-b/private.txt"))
            .await
            .is_err()
    );
    let verifier = workers.verifier_tools("nested").await.unwrap();
    assert!(
        verifier
            .prepare("nested", call("nested-b/private.txt"))
            .await
            .is_err()
    );
    workers.shutdown().await;
    f.worker.shutdown().await;
}
