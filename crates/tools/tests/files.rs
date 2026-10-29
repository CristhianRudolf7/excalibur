use excalibur_store::{Database, DatabaseKind, Priority, ReadLane, StorageConfig};
use excalibur_tools::{Access, LocalTools, PostEditHooks, ToolPolicy};
use excalibur_types::runtime::{BoxFuture, Reconcile, ToolCall, ToolResult, ToolRuntime};
use serde_json::{Value, json};
use std::{path::Path, sync::Arc};
async fn setup(policy: ToolPolicy) -> (tempfile::TempDir, Database, Arc<LocalTools>) {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("project");
    std::fs::create_dir(&root).unwrap();
    let db = Database::open(
        temp.path().join("data/excalibur.db"),
        DatabaseKind::Main,
        StorageConfig::default(),
    )
    .unwrap();
    db.write(Priority::Normal, |conn| {
        conn.execute("INSERT INTO runs(id) VALUES('run1'),('run2')", [])?;
        Ok(())
    })
    .await
    .unwrap();
    if matches!(policy.exec, Access::Allow) {
        excalibur_tools::terminal::prepare_project_boundary(&root).unwrap();
    }
    let tools = Arc::new(LocalTools::new(db.clone(), root, policy).await.unwrap());
    (temp, db, tools)
}
fn allow() -> ToolPolicy {
    ToolPolicy {
        read: Access::Allow,
        write: Access::Allow,
        exec: Access::Deny,
        ..Default::default()
    }
}
fn call(id: &str, name: &str, args: Value) -> ToolCall {
    ToolCall {
        id: id.into(),
        name: name.into(),
        arguments: args.to_string(),
    }
}
async fn run(tools: &LocalTools, id: &str, name: &str, args: Value) -> ToolResult {
    let prepared = tools.prepare("run1", call(id, name, args)).await.unwrap();
    tools.execute("run1", id, &prepared).await
}

#[tokio::test]
async fn json_schema_repairs_minor_json_and_rejects_grave_errors_and_permissions() {
    let (temp, db, tools) = setup(allow()).await;
    std::fs::write(temp.path().join("project/file"), "data\n").unwrap();
    let prepared = tools
        .prepare(
            "run1",
            ToolCall {
                id: "minor".into(),
                name: "read_file".into(),
                arguments: "{'path':'file',}".into(),
            },
        )
        .await
        .unwrap();
    assert!(tools.execute("run1", "minor", &prepared).await.success);
    for args in [
        json!({"path":"file","line_count":0}),
        json!({"path":true}),
        json!({"path":"file","unknown":1}),
    ] {
        assert!(
            tools
                .prepare("run1", call("invalid", "read_file", args))
                .await
                .is_err()
        );
    }
    let error = tools
        .prepare(
            "run1",
            ToolCall {
                id: "broken".into(),
                name: "read_file".into(),
                arguments: "{path:[".into(),
            },
        )
        .await
        .unwrap_err();
    assert!(error.contains("invalid JSON"));
    for (access, label) in [
        (Access::Ask, "permission_required"),
        (Access::Deny, "permission_denied"),
    ] {
        let policy = ToolPolicy {
            write: access,
            ..allow()
        };
        let guarded = LocalTools::new(db.clone(), temp.path().join("project"), policy)
            .await
            .unwrap();
        let error = guarded
            .prepare(
                "run1",
                call("denied", "write_file", json!({"path":"new","content":"x"})),
            )
            .await
            .unwrap_err();
        assert!(error.contains(label));
    }
    assert!(!temp.path().join("project/new").exists());
    db.shutdown().await.unwrap();
}
#[tokio::test]
async fn read_file_defaults_to_200_lines_and_requires_durable_fingerprint_for_rewrites() {
    let (temp, db, tools) = setup(allow()).await;
    let path = temp.path().join("project/file");
    std::fs::write(
        &path,
        (1..=250).map(|n| format!("line {n}\n")).collect::<String>(),
    )
    .unwrap();
    assert!(
        tools
            .prepare(
                "run1",
                call(
                    "unread",
                    "write_file",
                    json!({"path":"file","content":"replacement"})
                )
            )
            .await
            .unwrap_err()
            .contains("must be read_file")
    );
    let read = run(&tools, "read", "read_file", json!({"path":"file"})).await;
    assert!(read.success);
    assert_eq!(read.output["lines"].as_array().unwrap().len(), 200);
    assert_eq!(read.output["lines"][0]["line"], 1);
    assert_eq!(read.output["total_lines"], 250);
    let prepared = tools
        .prepare(
            "run1",
            call(
                "write",
                "write_file",
                json!({"path":"file","content":"replacement","fp":read.output["fp"]}),
            ),
        )
        .await
        .unwrap();
    let pre = prepared.data["edits"][0]["pre"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    assert!(
        db.read_blob(pre, 1024 * 1024)
            .await
            .unwrap()
            .starts_with(b"line 1")
    );
    assert!(
        std::fs::read_to_string(&path)
            .unwrap()
            .starts_with("line 1")
    );
    let result = tools.execute("run1", "write-key", &prepared).await;
    assert!(result.success, "{:?}", result);
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "replacement");
    let count=db.read(ReadLane::Interactive,|conn|Ok(conn.query_row("SELECT count(*) FROM file_versions WHERE tool_call_id='write' AND preimage_blob IS NOT NULL AND postimage_blob IS NOT NULL",[],|r|r.get::<_,i64>(0))?)).await.unwrap();
    assert_eq!(count, 1);
    let reopened = LocalTools::new(db.clone(), temp.path().join("project"), allow())
        .await
        .unwrap();
    assert!(
        reopened
            .prepare(
                "run1",
                call(
                    "again",
                    "edit_file",
                    json!({"path":"file","old_text":"replacement","new_text":"next"})
                )
            )
            .await
            .is_ok()
    );
    db.shutdown().await.unwrap();
}
#[tokio::test]
async fn conflicts_include_compact_diff_and_escalate_on_third_mismatch() {
    let (temp, db, tools) = setup(allow()).await;
    let path = temp.path().join("project/file");
    std::fs::write(&path, "first\n").unwrap();
    run(&tools, "read", "read_file", json!({"path":"file"})).await;
    std::fs::write(&path, "user changed\n").unwrap();
    for mismatch in 1..=3 {
        let error = tools
            .prepare(
                "run1",
                call(
                    "edit",
                    "edit_file",
                    json!({"path":"file","old_text":"first","new_text":"agent"}),
                ),
            )
            .await
            .unwrap_err();
        let error: Value = serde_json::from_str(&error).unwrap();
        assert_eq!(error["mismatches"], mismatch);
        assert_eq!(error["escalate_to_parent"], mismatch >= 3);
        assert!(error["diff"].as_str().unwrap().contains("user changed"));
    }
    assert_eq!(std::fs::read_to_string(path).unwrap(), "user changed\n");
    db.shutdown().await.unwrap();
}
#[tokio::test]
async fn text_matching_supports_three_levels_and_reports_ambiguous_lines() {
    let (temp, db, tools) = setup(allow()).await;
    for (id, content, old, expected) in [
        ("exact", "hello world\n", "world", "hello next\n"),
        ("typography", "say “hello”\n", "\"hello\"", "say next\n"),
        ("trailing", "hello   \nworld\n", "hello\nworld", "next\n"),
    ] {
        let path = temp.path().join("project").join(id);
        std::fs::write(path, content).unwrap();
        run(
            &tools,
            &format!("read-{id}"),
            "read_file",
            json!({"path":id}),
        )
        .await;
        let result = run(
            &tools,
            &format!("edit-{id}"),
            "edit_file",
            json!({"path":id,"old_text":old,"new_text":"next"}),
        )
        .await;
        assert!(result.success, "{result:?}");
        assert_eq!(
            std::fs::read_to_string(temp.path().join("project").join(id)).unwrap(),
            expected
        );
    }
    std::fs::write(temp.path().join("project/repeat"), "same\nsame\n").unwrap();
    run(&tools, "repeatread", "read_file", json!({"path":"repeat"})).await;
    assert!(
        tools
            .prepare(
                "run1",
                call(
                    "ambiguous",
                    "edit_file",
                    json!({"path":"repeat","old_text":"same","new_text":"x"})
                )
            )
            .await
            .unwrap_err()
            .contains("[1, 2]")
    );
    db.shutdown().await.unwrap();
}
#[tokio::test]
async fn multifile_patch_recovers_partial_effects_and_records_versions_once() {
    let (temp, db, tools) = setup(allow()).await;
    let root = temp.path().join("project");
    std::fs::write(root.join("a"), "before\n").unwrap();
    std::fs::write(root.join("delete"), "old\n").unwrap();
    for path in ["a", "delete"] {
        run(
            &tools,
            &format!("read-{path}"),
            "read_file",
            json!({"path":path}),
        )
        .await;
    }
    let patch = "*** Begin Patch\n*** Update File: a\n@@\n-before\n+after\n*** Delete File: delete\n*** Add File: nested/new\n+created\n*** End Patch";
    let prepared = tools
        .prepare("run1", call("patch", "apply_patch", json!({"patch":patch})))
        .await
        .unwrap();
    assert_eq!(std::fs::read_to_string(root.join("a")).unwrap(), "before\n");
    // Deterministic interruption point: only first postcondition reached before reconciliation.
    std::fs::write(root.join("a"), "after\n").unwrap();
    let reconciled = tools.reconcile("run1", "patch-key", &prepared).await;
    assert!(
        matches!(reconciled,Reconcile::Done(ref r) if r.success),
        "{reconciled:?}"
    );
    assert!(!root.join("delete").exists());
    assert_eq!(
        std::fs::read_to_string(root.join("nested/new")).unwrap(),
        "created\n"
    );
    assert!(
        matches!(tools.reconcile("run1","patch-key",&prepared).await,Reconcile::Done(ref r) if r.success)
    );
    let count = db
        .read(ReadLane::Interactive, |conn| {
            Ok(conn.query_row(
                "SELECT count(*) FROM file_versions WHERE tool_call_id='patch'",
                [],
                |r| r.get::<_, i64>(0),
            )?)
        })
        .await
        .unwrap();
    assert_eq!(count, 3);
    db.shutdown().await.unwrap();
}
#[tokio::test]
async fn search_respects_gitignore_and_returns_bounded_summaries() {
    let (temp, db, tools) = setup(allow()).await;
    let root = temp.path().join("project");
    std::fs::write(root.join(".gitignore"), "ignored.txt\n").unwrap();
    std::fs::write(
        root.join("a.txt"),
        "needle\nneedle two\nneedle three\nneedle four\n",
    )
    .unwrap();
    std::fs::write(root.join("ignored.txt"), "needle secret\n").unwrap();
    let result = run(
        &tools,
        "grep",
        "grep",
        json!({"pattern":"needle","glob":"*.txt"}),
    )
    .await;
    assert!(result.success);
    let values = result.output["results"].as_array().unwrap();
    assert_eq!(values.len(), 1);
    assert_eq!(values[0]["match_count"], 4);
    assert_eq!(values[0]["first_lines"].as_array().unwrap().len(), 3);
    let result = run(&tools, "glob", "glob", json!({"pattern":"*.txt"})).await;
    assert_eq!(result.output["results"], json!(["a.txt"]));
    db.shutdown().await.unwrap();
}
#[tokio::test]
async fn path_traversal_and_external_symlinks_are_rejected() {
    let (temp, db, tools) = setup(allow()).await;
    for path in [
        "../outside".to_owned(),
        temp.path().join("outside").to_string_lossy().into_owned(),
    ] {
        assert!(
            tools
                .prepare(
                    "run1",
                    call(
                        "escape",
                        "write_file",
                        json!({"path":path,"content":"escape"})
                    )
                )
                .await
                .is_err()
        );
    }
    #[cfg(unix)]
    {
        std::fs::write(temp.path().join("outside"), "secret").unwrap();
        std::os::unix::fs::symlink(
            temp.path().join("outside"),
            temp.path().join("project/link"),
        )
        .unwrap();
        assert!(
            tools
                .prepare("run1", call("symlink", "read_file", json!({"path":"link"})))
                .await
                .unwrap_err()
                .contains("outside")
        );
    }
    db.shutdown().await.unwrap();
}
#[tokio::test]
async fn concurrent_prepared_writes_conflict_and_foreign_lease_blocks_execution() {
    let (temp, db, tools) = setup(allow()).await;
    let root = temp.path().join("project");
    std::fs::write(root.join("file"), "before").unwrap();
    for run_id in ["run1", "run2"] {
        let tool = tools
            .prepare(run_id, call("read", "read_file", json!({"path":"file"})))
            .await
            .unwrap();
        assert!(tools.execute(run_id, "read", &tool).await.success);
    }
    let one = tools
        .prepare(
            "run1",
            call("one", "write_file", json!({"path":"file","content":"one"})),
        )
        .await
        .unwrap();
    let two = tools
        .prepare(
            "run2",
            call("two", "write_file", json!({"path":"file","content":"two"})),
        )
        .await
        .unwrap();
    let (one_result, two_result) = tokio::join!(
        tools.execute("run1", "one", &one),
        tools.execute("run2", "two", &two)
    );
    assert_ne!(one_result.success, two_result.success);
    assert!(
        one_result
            .output
            .to_string()
            .contains("fingerprint_conflict")
            || two_result
                .output
                .to_string()
                .contains("fingerprint_conflict")
    );
    let path = root.join("held").to_string_lossy().into_owned();
    db.write(Priority::Normal, move |conn| {
        conn.execute(
            "INSERT INTO leases(path,holder_id,expires_at) VALUES(?1,'other',?2)",
            excalibur_store::rusqlite::params![path, excalibur_store::unix_seconds() + 300],
        )?;
        Ok(())
    })
    .await
    .unwrap();
    let held = tools
        .prepare(
            "run1",
            call("held", "write_file", json!({"path":"held","content":"no"})),
        )
        .await
        .unwrap();
    let result = tools.execute("run1", "held", &held).await;
    assert!(!result.success);
    assert!(result.output.to_string().contains("lease held"));
    assert!(!root.join("held").exists());
    db.shutdown().await.unwrap();
}
struct Hooks;
impl PostEditHooks for Hooks {
    fn format<'a>(
        &'a self,
        _path: &'a Path,
        bytes: Vec<u8>,
    ) -> BoxFuture<'a, Result<Vec<u8>, String>> {
        Box::pin(async move {
            Ok(String::from_utf8(bytes)
                .unwrap()
                .to_uppercase()
                .into_bytes())
        })
    }
    fn after_edit<'a>(&'a self, path: &'a Path) -> BoxFuture<'a, Result<Value, String>> {
        Box::pin(async move {
            let text = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
            Ok(
                json!({"formatter":"uppercase_host","diagnostics":if text.contains("BROKEN"){vec!["synthetic syntax error"]}else{vec![]},"evidence_invalidated":true}),
            )
        })
    }
}
#[tokio::test]
async fn configured_hooks_run_and_large_output_is_retrievable_by_blob() {
    let policy = ToolPolicy {
        hooks: Some(Arc::new(Hooks)),
        output_budget_bytes: 256,
        ..allow()
    };
    let (temp, db, tools) = setup(policy).await;
    let result = run(
        &tools,
        "format",
        "write_file",
        json!({"path":"new","content":"broken"}),
    )
    .await;
    assert!(result.success);
    assert_eq!(
        std::fs::read_to_string(temp.path().join("project/new")).unwrap(),
        "BROKEN"
    );
    // Native report may be truncated to budget; its complete JSON is retained in the blob.
    let full = if let Some(hash) = result.full_output_blob {
        serde_json::from_slice::<Value>(
            &db.read_blob(hash.parse().unwrap(), 1024 * 1024)
                .await
                .unwrap(),
        )
        .unwrap()
    } else {
        result.output
    };
    assert_eq!(
        full["files"][0]["postprocessing"]["diagnostics"][0],
        "synthetic syntax error"
    );
    std::fs::write(temp.path().join("project/large"), "a".repeat(4000) + "TAIL").unwrap();
    let result = run(&tools, "large", "read_file", json!({"path":"large"})).await;
    assert!(result.success);
    assert_eq!(result.output["truncated"], true);
    let full = db
        .read_blob(
            result.full_output_blob.unwrap().parse().unwrap(),
            1024 * 1024,
        )
        .await
        .unwrap();
    assert!(String::from_utf8(full).unwrap().contains("TAIL"));
    db.shutdown().await.unwrap();
}

#[tokio::test]
async fn exact_host_declared_commands_are_the_only_repeatable_terminal_effects() {
    use excalibur_types::runtime::Effect;
    let policy = ToolPolicy {
        exec: Access::Allow,
        repeatable_commands: ["cargo test --offline".to_owned()].into_iter().collect(),
        ..allow()
    };
    let (_temp, db, tools) = setup(policy).await;
    let safe = tools
        .prepare(
            "run1",
            call("safe", "exec", json!({"cmd":"cargo test --offline"})),
        )
        .await
        .unwrap();
    assert_eq!(safe.effect, Effect::Idempotent);
    assert!(matches!(
        tools.reconcile("run1", "safe", &safe).await,
        Reconcile::Retry
    ));
    for command in [
        "cargo test --offline && send_secret",
        "cargo test",
        "arbitrary-command",
    ] {
        let ordinary = tools
            .prepare("run1", call("ordinary", "exec", json!({"cmd":command})))
            .await
            .unwrap();
        assert_eq!(ordinary.effect, Effect::NonIdempotent);
        assert!(matches!(
            tools.reconcile("run1", "ordinary", &ordinary).await,
            Reconcile::Uncertain(_)
        ));
    }
    db.shutdown().await.unwrap();
}
struct BlockingHook {
    entered: tokio::sync::Notify,
    release: tokio::sync::Notify,
    done: tokio::sync::Notify,
}
impl PostEditHooks for BlockingHook {
    fn format<'a>(
        &'a self,
        _path: &'a Path,
        bytes: Vec<u8>,
    ) -> BoxFuture<'a, Result<Vec<u8>, String>> {
        Box::pin(async move { Ok(bytes) })
    }
    fn after_edit<'a>(&'a self, _path: &'a Path) -> BoxFuture<'a, Result<Value, String>> {
        Box::pin(async move {
            self.entered.notify_one();
            self.release.notified().await;
            self.done.notify_one();
            Ok(json!({"completed":true}))
        })
    }
}
#[tokio::test]
async fn admitted_atomic_edit_completes_when_caller_cancels_its_wait() {
    let hook = Arc::new(BlockingHook {
        entered: Default::default(),
        release: Default::default(),
        done: Default::default(),
    });
    let (temp, db, tools) = setup(ToolPolicy {
        hooks: Some(hook.clone()),
        ..allow()
    })
    .await;
    let prepared = tools
        .prepare(
            "run1",
            call(
                "cancelwait",
                "write_file",
                json!({"path":"file","content":"durable"}),
            ),
        )
        .await
        .unwrap();
    let executor = tools.clone();
    let waiter =
        tokio::spawn(async move { executor.execute("run1", "cancelwait", &prepared).await });
    hook.entered.notified().await;
    waiter.abort();
    assert_eq!(
        std::fs::read_to_string(temp.path().join("project/file")).unwrap(),
        "durable"
    );
    hook.release.notify_one();
    hook.done.notified().await;
    assert_eq!(
        db.read(ReadLane::Interactive, |conn| Ok(conn.query_row(
            "SELECT count(*) FROM file_versions WHERE tool_call_id='cancelwait'",
            [],
            |r| r.get::<_, i64>(0)
        )?))
        .await
        .unwrap(),
        1
    );
    db.shutdown().await.unwrap();
}

#[tokio::test]
async fn tool_catalog_is_ordered_and_contains_runtime_metadata() {
    let (_temp, db, tools) = setup(allow()).await;
    let catalog = tools.catalog();
    let names = catalog
        .iter()
        .map(|item| item["name"].as_str().unwrap())
        .collect::<Vec<_>>();
    let mut sorted = names.clone();
    sorted.sort();
    assert_eq!(names, sorted);
    for item in catalog {
        for key in [
            "input_schema",
            "output_schema",
            "effect",
            "idempotency",
            "resources",
            "risk",
            "timeout_ms",
            "output_budget_bytes",
            "platforms",
            "inverse",
            "probe",
            "dry_run",
            "fast",
            "cancel",
        ] {
            assert!(item.get(key).is_some(), "{key}");
        }
        let name = item["name"].as_str().unwrap();
        assert!(
            name.len() <= 64
                && name
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
        );
    }
    db.shutdown().await.unwrap();
}

struct ErrorHook;
impl PostEditHooks for ErrorHook {
    fn format<'a>(
        &'a self,
        _path: &'a Path,
        bytes: Vec<u8>,
    ) -> BoxFuture<'a, Result<Vec<u8>, String>> {
        Box::pin(async move { Ok(bytes) })
    }
    fn after_edit<'a>(&'a self, _path: &'a Path) -> BoxFuture<'a, Result<Value, String>> {
        Box::pin(async move { Err("literal diagnostic ".repeat(100) + "END") })
    }
}
#[tokio::test]
async fn small_results_have_full_blobs_and_long_error_text_remains_literal() {
    let (temp, db, tools) = setup(ToolPolicy {
        output_budget_bytes: 256,
        hooks: Some(Arc::new(ErrorHook)),
        ..allow()
    })
    .await;
    std::fs::write(temp.path().join("project/small"), "tiny").unwrap();
    let read = run(&tools, "small", "read_file", json!({"path":"small"})).await;
    assert!(read.full_output_blob.is_some());
    let result = run(
        &tools,
        "hook-error",
        "write_file",
        json!({"path":"error-file","content":"created"}),
    )
    .await;
    assert_eq!(
        result.output["errors"][0],
        "literal diagnostic ".repeat(100) + "END"
    );
    assert!(result.full_output_blob.is_some());
    db.shutdown().await.unwrap();
}
#[cfg(unix)]
#[tokio::test]
async fn injected_terminal_redacts_and_persists_full_output_before_spool_acknowledgement() {
    use excalibur_tools::terminal::{TerminalConfig, TerminalManager};
    let (temp, db, tools) = setup(ToolPolicy {
        exec: Access::Allow,
        ..allow()
    })
    .await;
    let manager = Arc::new(
        TerminalManager::new(TerminalConfig {
            spool_dir: temp.path().join("custom-spool"),
            redacted_secrets: vec!["SYNTHETIC_SECRET".into()],
            sandbox: Some(excalibur_tools::terminal::SandboxConfig::project(
                temp.path().join("project"),
            )),
            ..Default::default()
        })
        .unwrap(),
    );
    let tools = Arc::try_unwrap(tools)
        .ok()
        .unwrap()
        .with_terminal(manager.clone());
    let result = run(
        &tools,
        "terminal-redaction",
        "exec",
        json!({"cmd":"printf '%s' SYNTHETIC_SECRET","yield_ms":1000}),
    )
    .await;
    assert!(result.success, "{result:?}");
    assert_eq!(result.output["exited"], true);
    assert_eq!(result.output["spool_path"], Value::Null);
    let output = db
        .read_blob(
            result.full_output_blob.unwrap().parse().unwrap(),
            1024 * 1024,
        )
        .await
        .unwrap();
    let output = String::from_utf8(output).unwrap();
    assert!(!output.contains("SYNTHETIC_SECRET"));
    assert!(output.contains("REDACTED"));
    manager.shutdown().await;
    db.shutdown().await.unwrap();
}

#[tokio::test]
async fn readsets_are_keyed_by_canonical_path_across_workspace_adapters() {
    let (temp, db, tools) = setup(allow()).await;
    let other = temp.path().join("other-project");
    std::fs::create_dir(&other).unwrap();
    std::fs::write(temp.path().join("project/file"), "first").unwrap();
    std::fs::write(other.join("file"), "second").unwrap();
    let other_tools = LocalTools::new(db.clone(), other, allow()).await.unwrap();
    run(&tools, "first-read", "read_file", json!({"path":"file"})).await;
    run(
        &other_tools,
        "other-read",
        "read_file",
        json!({"path":"file"}),
    )
    .await;
    let result = run(
        &tools,
        "first-edit",
        "edit_file",
        json!({"path":"file","old_text":"first","new_text":"changed"}),
    )
    .await;
    assert!(result.success, "{result:?}");
    db.shutdown().await.unwrap();
}

#[tokio::test]
async fn codex_move_and_end_of_file_patch_preserves_missing_final_newline() {
    let (temp, db, tools) = setup(allow()).await;
    let root = temp.path().join("project");
    std::fs::write(root.join("old"), "line-one\nline-two").unwrap();
    run(&tools, "move-read", "read_file", json!({"path":"old"})).await;
    let patch = "*** Begin Patch\n*** Update File: old\n*** Move to: moved/new\n@@\n line-one\n-line-two\n+updated\n*** End of File\n*** End Patch";
    let result = run(&tools, "move-patch", "apply_patch", json!({"patch":patch})).await;
    assert!(result.success, "{result:?}");
    assert!(!root.join("old").exists());
    assert_eq!(
        std::fs::read_to_string(root.join("moved/new")).unwrap(),
        "line-one\nupdated"
    );
    db.shutdown().await.unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn live_terminal_result_archives_the_exact_observed_prefix_before_process_exit() {
    let mut policy = allow();
    policy.exec = Access::Allow;
    let (_temp, db, tools) = setup(policy).await;
    let result = run(
        &tools,
        "live-prefix",
        "exec",
        json!({"cmd":"printf READY; read -r value; printf DONE","yield_ms":250,"timeout_ms":5000}),
    )
    .await;
    assert!(result.success);
    assert_eq!(result.output["exited"], false);
    assert!(result.output.get("spool_path").is_none());
    let first = db
        .read_blob(result.full_output_blob.unwrap().parse().unwrap(), 1024)
        .await
        .unwrap();
    assert_eq!(first, b"READY");
    assert_eq!(result.output["archived_through_cursor"], 5);
    let session = result.output["session_id"].as_str().unwrap();
    let _ = run(
        &tools,
        "live-write",
        "exec_io",
        json!({"action":"write","session_id":session,"text":"continue\n"}),
    )
    .await;
    let last = run(
        &tools,
        "live-wait",
        "exec_io",
        json!({"action":"wait_for","session_id":session,"pattern":"DONE","timeout_ms":3000}),
    )
    .await;
    assert!(last.success);
    let complete = db
        .read_blob(last.full_output_blob.unwrap().parse().unwrap(), 1024)
        .await
        .unwrap();
    assert_eq!(complete, b"READYDONE");
    tools.terminals().shutdown().await;
    db.shutdown().await.unwrap();
}

#[tokio::test]
async fn lost_terminal_reads_are_retryable_but_input_effects_stay_uncertain() {
    let mut policy = allow();
    policy.exec = Access::Allow;
    let (_temp, db, tools) = setup(policy).await;
    let read = tools
        .prepare(
            "run1",
            call(
                "lost-read",
                "exec_io",
                json!({"action":"read","session_id":"lost-session"}),
            ),
        )
        .await
        .unwrap();
    assert_eq!(read.effect, excalibur_types::runtime::Effect::Pure);
    assert!(!read.exclusive);
    assert!(matches!(
        tools.reconcile("run1", "key", &read).await,
        Reconcile::Retry
    ));
    let result = tools.execute("run1", "key", &read).await;
    assert!(!result.success);
    assert!(result.output["error"].as_str().unwrap().contains("lost"));
    let write=tools.prepare("run1",call("lost-write","exec_io",json!({"action":"write","session_id":"lost-session","text":"a non-repeatable input"}))).await.unwrap();
    assert_eq!(
        write.effect,
        excalibur_types::runtime::Effect::NonIdempotent
    );
    assert!(matches!(
        tools.reconcile("run1", "key", &write).await,
        Reconcile::Uncertain(_)
    ));
    db.shutdown().await.unwrap();
}

#[tokio::test]
async fn million_match_search_reports_exact_count_with_only_three_preview_lines() {
    use std::io::Write;
    let (temp, db, tools) = setup(allow()).await;
    let mut file = std::fs::File::create(temp.path().join("project/matches.txt")).unwrap();
    let block = b"needle\n".repeat(1000);
    for _ in 0..1000 {
        file.write_all(&block).unwrap();
    }
    drop(file);
    let result = run(
        &tools,
        "many-matches",
        "grep",
        json!({"pattern":"needle","limit":1}),
    )
    .await;
    assert!(result.success);
    assert_eq!(result.output["results"][0]["match_count"], 1_000_000);
    assert_eq!(
        result.output["results"][0]["first_lines"]
            .as_array()
            .unwrap()
            .len(),
        3
    );
    assert!(result.output.to_string().len() < 2048);
    db.shutdown().await.unwrap();
}

#[tokio::test]
async fn persisted_preparation_cannot_be_replayed_in_a_different_workspace() {
    let (temp, db, a) = setup(allow()).await;
    let other = temp.path().join("other");
    std::fs::create_dir(&other).unwrap();
    let b = LocalTools::new(db.clone(), other.clone(), allow())
        .await
        .unwrap();
    let prepared = a
        .prepare(
            "run1",
            call(
                "bound-root",
                "write_file",
                json!({"path":"target.txt","content":"authorized workspace only"}),
            ),
        )
        .await
        .unwrap();
    let result = b.execute("run1", "key", &prepared).await;
    assert!(!result.success);
    assert!(!other.join("target.txt").exists());
    assert!(matches!(
        b.reconcile("run1", "key", &prepared).await,
        Reconcile::Uncertain(_)
    ));
    assert!(a.execute("run1", "key", &prepared).await.success);
    assert_eq!(
        std::fs::read(temp.path().join("project/target.txt")).unwrap(),
        b"authorized workspace only"
    );
    db.shutdown().await.unwrap();
}
