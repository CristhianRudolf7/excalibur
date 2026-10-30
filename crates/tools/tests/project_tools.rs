use excalibur_store::{Database, DatabaseKind, Priority, StorageConfig};
use excalibur_tools::{Access, LocalTools, ToolPolicy};
use excalibur_types::runtime::*;
use serde_json::{Value, json};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
};
fn call(id: &str, name: &str, args: Value) -> ToolCall {
    ToolCall {
        id: id.into(),
        name: name.into(),
        arguments: args.to_string(),
    }
}
async fn fixture() -> (tempfile::TempDir, Database, LocalTools) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("project");
    std::fs::create_dir(&root).unwrap();
    let db = Database::open(
        dir.path().join("data/main.db"),
        DatabaseKind::Main,
        StorageConfig::default(),
    )
    .unwrap();
    db.write(Priority::Journal, |c| {
        c.execute("INSERT INTO runs(id) VALUES('run')", [])?;
        Ok(())
    })
    .await
    .unwrap();
    let tools = LocalTools::new(
        db.clone(),
        root,
        ToolPolicy {
            write: Access::Allow,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    (dir, db, tools)
}
async fn invoke(tools: &LocalTools, id: &str, name: &str, args: Value) -> ToolResult {
    let prepared = tools.prepare("run", call(id, name, args)).await.unwrap();
    tools.execute("run", id, &prepared).await
}
#[tokio::test]
async fn revert_preserves_later_changes_reopens_and_reconciles_without_duplicate_history() {
    let (dir, db, tools) = fixture().await;
    let root = dir.path().join("project");
    std::fs::write(root.join("file.txt"), "alpha\nmiddle\nomega\n").unwrap();
    invoke(&tools, "read", "read_file", json!({"path":"file.txt"})).await;
    assert!(
        invoke(
            &tools,
            "edit",
            "edit_file",
            json!({"path":"file.txt","old_text":"middle","new_text":"CHANGED"})
        )
        .await
        .success
    );
    let history = invoke(
        &tools,
        "history",
        "file_history",
        json!({"path":"file.txt"}),
    )
    .await;
    let id = history.output["versions"][0]["id"].as_str().unwrap();
    std::fs::write(
        root.join("file.txt"),
        "alpha\nCHANGED\nomega\nUSER APPEND\n",
    )
    .unwrap();
    let revert = tools
        .prepare(
            "run",
            call(
                "undo",
                "revert_edit",
                json!({"path":"file.txt","edit_id":id}),
            ),
        )
        .await
        .unwrap();
    let reopened = LocalTools::new(
        db.clone(),
        root.clone(),
        ToolPolicy {
            write: Access::Allow,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert!(reopened.execute("run", "undo", &revert).await.success);
    assert_eq!(
        std::fs::read_to_string(root.join("file.txt")).unwrap(),
        "alpha\nmiddle\nomega\nUSER APPEND\n"
    );
    assert!(matches!(
        reopened.reconcile("run", "undo", &revert).await,
        Reconcile::Done(ToolResult { success: true, .. })
    ));
    let history = invoke(
        &reopened,
        "history2",
        "file_history",
        json!({"path":"file.txt"}),
    )
    .await;
    assert_eq!(history.output["versions"].as_array().unwrap().len(), 2);
    db.shutdown().await.unwrap();
}
#[tokio::test]
async fn conflicting_revert_and_wrong_authorized_path_never_write() {
    let (dir, db, tools) = fixture().await;
    let root = dir.path().join("project");
    std::fs::write(root.join("file.txt"), "original\n").unwrap();
    invoke(&tools, "read", "read_file", json!({"path":"file.txt"})).await;
    invoke(
        &tools,
        "edit",
        "edit_file",
        json!({"path":"file.txt","old_text":"original","new_text":"AGENT"}),
    )
    .await;
    let history = invoke(
        &tools,
        "history",
        "file_history",
        json!({"path":"file.txt"}),
    )
    .await;
    let id = history.output["versions"][0]["id"].as_str().unwrap();
    std::fs::write(root.join("file.txt"), "USER\n").unwrap();
    let error = tools
        .prepare(
            "run",
            call(
                "undo",
                "revert_edit",
                json!({"path":"file.txt","edit_id":id}),
            ),
        )
        .await
        .unwrap_err();
    assert!(error.contains("merge_conflict"));
    assert!(
        tools
            .prepare(
                "run",
                call(
                    "other",
                    "revert_edit",
                    json!({"path":"other.txt","edit_id":id})
                )
            )
            .await
            .is_err()
    );
    assert_eq!(
        std::fs::read_to_string(root.join("file.txt")).unwrap(),
        "USER\n"
    );
    db.shutdown().await.unwrap();
}
#[test]
fn lsp_utf16_edits_are_exact_and_reject_split_surrogates_or_overlaps() {
    use excalibur_tools::intelligence::apply_text_edits;
    let edit = json!({"range":{"start":{"line":0,"character":2},"end":{"line":0,"character":3}},"newText":"Y"});
    assert_eq!(
        apply_text_edits("🦀X\r\n", std::slice::from_ref(&edit)).unwrap(),
        "🦀Y\r\n"
    );
    assert!(apply_text_edits("🦀X",&[json!({"range":{"start":{"line":0,"character":1},"end":{"line":0,"character":2}},"newText":"Y"})]).is_err());
    assert!(apply_text_edits("🦀X", &[edit.clone(), edit]).is_err());
}
#[cfg(target_os = "linux")]
#[tokio::test]
async fn real_readonly_lsp_diagnostics_and_rename_reenter_durable_file_pipeline() {
    use excalibur_code_intelligence::{CodeGraph, LspManager, ReadOnlySpawner, ServerConfig};
    use excalibur_tools::intelligence::ProjectIntelligence;
    let (dir, db, tools) = fixture().await;
    let root = dir.path().join("project");
    std::fs::write(root.join("file.rs"), "fn old() {}\n// OLD_ERROR\n").unwrap();
    let graph_db = Database::open(
        dir.path().join("data/codegraph.db"),
        DatabaseKind::CodeGraph,
        StorageConfig::default(),
    )
    .unwrap();
    let graph = CodeGraph::open(graph_db.clone(), "project").await.unwrap();
    let script = dir.path().join("server.py");
    std::fs::write(
        &script,
        include_str!("../../code-intelligence/tests/fixtures/lsp_server.py"),
    )
    .unwrap();
    let lsp = Arc::new(LspManager::new(Arc::new(ReadOnlySpawner::default())));
    let mut config = ServerConfig::new(
        root.clone(),
        PathBuf::from("/usr/bin/python3"),
        vec![script.to_string_lossy().into_owned(), "pull".into()],
    );
    config.read_roots.push(dir.path().to_path_buf());
    lsp.register("project", "rust", config).await.unwrap();
    let services = Arc::new(
        ProjectIntelligence::new(graph, "project".into(), root.clone())
            .unwrap()
            .with_lsp(lsp.clone()),
    );
    let tools = tools.with_intelligence(services).unwrap();
    invoke(&tools, "read", "read_file", json!({"path":"file.rs"})).await;
    let edited = invoke(
        &tools,
        "broken",
        "edit_file",
        json!({"path":"file.rs","old_text":"fn old() {}","new_text":"fn old( {}\n// NEW_ERROR"}),
    )
    .await;
    assert!(edited.success, "{edited:?}");
    let report = &edited.output["files"][0]["intelligence"];
    assert!(
        !report["diagnostics"]["syntax"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert_eq!(report["diagnostics"]["lsp"].as_array().unwrap().len(), 1);
    assert_eq!(report["diagnostics"]["lsp"][0]["message"], "new error");
    let renamed = invoke(
        &tools,
        "rename",
        "lsp",
        json!({"action":"rename","path":"file.rs","new_name":"new","line":0,"character":4}),
    )
    .await;
    assert!(renamed.success, "{renamed:?}");
    assert!(
        std::fs::read_to_string(root.join("file.rs"))
            .unwrap()
            .starts_with("fn new(")
    );
    assert!(!root.join("forbidden-server-write").exists());
    let history = invoke(&tools, "history", "file_history", json!({"path":"file.rs"})).await;
    assert_eq!(history.output["versions"].as_array().unwrap().len(), 2);
    let graph = invoke(
        &tools,
        "graph",
        "code_graph",
        json!({"operation":"file","path":"file.rs"}),
    )
    .await;
    assert!(graph.success);
    let _ = Path::new("file.rs");
    lsp.shutdown_all().await;
    graph_db.shutdown().await.unwrap();
    db.shutdown().await.unwrap();
}
#[cfg(target_os = "linux")]
#[tokio::test]
async fn command_shadow_restores_selected_files_and_preserves_user_git_index_and_head() {
    fn git(root: &Path, args: &[&str]) -> Vec<u8> {
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(root)
            .args(args)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        output.stdout
    }
    let (dir, db, _tools) = fixture().await;
    let root = dir.path().join("project");
    git(&root, &["init", "-q"]);
    std::fs::write(root.join("one.txt"), "base one\n").unwrap();
    std::fs::write(root.join("two.txt"), "base two\n").unwrap();
    git(&root, &["add", "."]);
    git(
        &root,
        &[
            "-c",
            "user.name=Synthetic User",
            "-c",
            "user.email=synthetic@example.invalid",
            "commit",
            "-qm",
            "Synthetic baseline",
        ],
    );
    let before_head = git(&root, &["rev-parse", "HEAD"]);
    let before_index = git(&root, &["ls-files", "--stage"]);
    excalibur_tools::terminal::prepare_project_boundary(&root).unwrap();
    let tools = LocalTools::new(
        db.clone(),
        root.clone(),
        ToolPolicy {
            write: Access::Allow,
            exec: Access::Allow,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    tools.begin_step("run", 0).await.unwrap();
    let executed=invoke(&tools,"command","exec",json!({"cmd":"printf 'command one\n' > one.txt; printf 'command two\n' > two.txt","yield_ms":1000})).await;
    assert!(executed.success, "{executed:?}");
    tools.end_step("run", 0).await.unwrap();
    let snapshots = invoke(
        &tools,
        "snapshots",
        "workspace_snapshot",
        json!({"action":"list"}),
    )
    .await;
    let before = snapshots.output["snapshots"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["phase"] == "before")
        .unwrap()["id"]
        .as_str()
        .unwrap();
    let restore = invoke(
        &tools,
        "restore",
        "workspace_snapshot",
        json!({"action":"restore","before":before,"paths":["one.txt"]}),
    )
    .await;
    assert!(restore.success, "{restore:?}");
    assert_eq!(
        std::fs::read_to_string(root.join("one.txt")).unwrap(),
        "base one\n"
    );
    assert_eq!(
        std::fs::read_to_string(root.join("two.txt")).unwrap(),
        "command two\n"
    );
    assert_eq!(git(&root, &["rev-parse", "HEAD"]), before_head);
    assert_eq!(git(&root, &["ls-files", "--stage"]), before_index);
    let other = excalibur_journal::Journal::new(db.clone())
        .await
        .unwrap()
        .create_run("other", json!({}))
        .await
        .unwrap();
    assert!(
        tools
            .prepare(
                &other,
                call(
                    "cross",
                    "workspace_snapshot",
                    json!({"action":"restore","before":before,"paths":["one.txt"]})
                )
            )
            .await
            .unwrap_err()
            .contains("not owned")
    );
    tools.terminals().shutdown().await;
    db.shutdown().await.unwrap();
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn shadow_snapshots_cover_exec_io_only_steps_and_cancelled_transitions() {
    let (dir, db, _tools) = fixture().await;
    let root = dir.path().join("project");
    let git = |args: &[&str]| {
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(&root)
            .args(args)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    };
    git(&["init", "-q"]);
    std::fs::write(root.join("base.txt"), "base\n").unwrap();
    excalibur_tools::terminal::prepare_project_boundary(&root).unwrap();
    let tools = LocalTools::new(
        db.clone(),
        root.clone(),
        ToolPolicy {
            write: Access::Allow,
            exec: Access::Allow,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    // A run that never touches exec records no filesystem snapshots.
    tools.begin_step("quiet", 0).await.unwrap();
    tools.end_step("quiet", 0).await.unwrap();
    let listed = invoke(
        &tools,
        "snapshots",
        "workspace_snapshot",
        json!({"action":"list"}),
    )
    .await;
    assert!(listed.success, "{listed:?}");
    assert!(
        listed.output["snapshots"]
            .as_array()
            .unwrap()
            .iter()
            .all(|s| s["run_id"] != "quiet"),
        "{listed:?}"
    );
    // Step 0 starts a session that only writes once it receives stdin.
    tools.begin_step("run", 0).await.unwrap();
    let started = invoke(
        &tools,
        "spawn",
        "exec",
        json!({"cmd":"sh -c 'read line; printf \"%s\" \"$line\" > late.txt; sleep 30'","yield_ms":1000}),
    )
    .await;
    assert!(started.success, "{started:?}");
    let session = started.output["session_id"].as_str().unwrap().to_owned();
    tools.end_step("run", 0).await.unwrap();
    // Step 1 only drives the live session through exec_io; the background
    // write lands while the step is open.
    tools.begin_step("run", 1).await.unwrap();
    let written = invoke(
        &tools,
        "write",
        "exec_io",
        json!({"action":"write","session_id":session,"text":"late\n"}),
    )
    .await;
    assert!(written.success, "{written:?}");
    for _ in 0..100 {
        if root.join("late.txt").exists() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    assert!(
        root.join("late.txt").exists(),
        "background write never landed"
    );
    tools.end_step("run", 1).await.unwrap();
    // A step that ends through cancellation (no end_step) still closes its
    // filesystem state on the lifecycle transition.
    tools.execution_state("run", "running", None).await.unwrap();
    tools.begin_step("run", 2).await.unwrap();
    tools
        .execution_state("run", "cancelled", Some("synthetic"))
        .await
        .unwrap();
    let listed = invoke(
        &tools,
        "snapshots",
        "workspace_snapshot",
        json!({"action":"list"}),
    )
    .await;
    assert!(listed.success, "{listed:?}");
    let snapshots = listed.output["snapshots"].as_array().unwrap();
    let phase_id = |step: u64, phase: &str| {
        snapshots
            .iter()
            .find(|s| s["run_id"] == "run" && s["step"] == step && s["phase"] == phase)
            .unwrap_or_else(|| panic!("missing snapshot step={step} phase={phase}: {listed:?}"))
            ["id"]
            .as_str()
            .unwrap()
            .to_owned()
    };
    let before_one = phase_id(1, "before");
    let after_one = phase_id(1, "after");
    phase_id(0, "before");
    phase_id(0, "after");
    phase_id(2, "after");
    // The exec_io-only step observed the background write: before lacks the
    // file while after records it.
    let diff = invoke(
        &tools,
        "diff",
        "workspace_snapshot",
        json!({"action":"diff","before":before_one,"after":after_one}),
    )
    .await;
    assert!(diff.success, "{diff:?}");
    assert!(
        diff.output["diff"].as_str().unwrap().contains("late.txt"),
        "{diff:?}"
    );
    tools.terminals().shutdown().await;
    db.shutdown().await.unwrap();
}
