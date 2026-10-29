//! Real filesystem/SQLite regression tests for admitted editing conflicts and planning.
use excalibur_store::{Database, DatabaseKind, Priority, ReadLane, StorageConfig};
use excalibur_tools::{Access, LocalTools, ToolPolicy};
use excalibur_types::runtime::{ToolCall, ToolResult, ToolRuntime};
use serde_json::{Value, json};
use std::path::PathBuf;

async fn setup() -> (tempfile::TempDir, Database, LocalTools, PathBuf) {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("workspace");
    std::fs::create_dir(&root).unwrap();
    let db = Database::open(
        temp.path().join("data/excalibur.db"),
        DatabaseKind::Main,
        StorageConfig::default(),
    )
    .unwrap();
    db.write(Priority::Normal, |conn| {
        conn.execute("INSERT INTO runs(id) VALUES('run')", [])?;
        Ok(())
    })
    .await
    .unwrap();
    let tools = LocalTools::new(
        db.clone(),
        root.clone(),
        ToolPolicy {
            read: Access::Allow,
            write: Access::Allow,
            exec: Access::Allow,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    (temp, db, tools, root)
}
fn call(id: &str, name: &str, args: Value) -> ToolCall {
    ToolCall {
        id: id.into(),
        name: name.into(),
        arguments: args.to_string(),
    }
}
async fn read(tools: &LocalTools, path: &str, id: &str) {
    let prepared = tools
        .prepare("run", call(id, "read_file", json!({"path":path})))
        .await
        .unwrap();
    assert!(tools.execute("run", id, &prepared).await.success);
}
fn conflict(result: ToolResult) -> Value {
    assert!(!result.success, "{result:?}");
    serde_json::from_str(result.output["error"].as_str().unwrap()).unwrap()
}
async fn histories(db: &Database) -> i64 {
    db.read(ReadLane::Interactive, |conn| {
        Ok(conn.query_row("SELECT count(*) FROM file_versions", [], |r| r.get(0))?)
    })
    .await
    .unwrap()
}

#[tokio::test]
async fn admitted_conflicts_share_preparation_counters_and_never_overwrite_user_edits() {
    let (_temp, db, tools, root) = setup().await;
    std::fs::write(root.join("file"), "before\n").unwrap();
    read(&tools, "file", "read").await;
    let prepared = tools
        .prepare(
            "run",
            call(
                "write",
                "write_file",
                json!({"path":"file","content":"agent\n"}),
            ),
        )
        .await
        .unwrap();
    std::fs::write(root.join("file"), "external user edit\n").unwrap();
    let first = tools
        .prepare(
            "run",
            call(
                "early-conflict",
                "write_file",
                json!({"path":"file","content":"never"}),
            ),
        )
        .await
        .unwrap_err();
    let first: Value = serde_json::from_str(&first).unwrap();
    assert_eq!(first["mismatches"], 1);
    let second = conflict(tools.execute("run", "write", &prepared).await);
    assert_eq!(second["mismatches"], 2);
    assert_eq!(second["expected_fp"], prepared.data["edits"][0]["pre"]);
    assert!(second["diff"].as_str().unwrap().contains("-before"));
    assert!(
        second["diff"]
            .as_str()
            .unwrap()
            .contains("+external user edit")
    );
    assert!(
        !second["affected_region"]["line_ranges"]
            .as_array()
            .unwrap()
            .is_empty()
    );
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
    let third = conflict(reopened.execute("run", "write", &prepared).await);
    assert_eq!(third["mismatches"], 3);
    assert_eq!(third["escalate_to_parent"], true);
    assert_eq!(
        std::fs::read_to_string(root.join("file")).unwrap(),
        "external user edit\n"
    );
    assert_eq!(histories(&db).await, 0);
    let count = db
        .read(ReadLane::Interactive, |conn| {
            Ok(
                conn.query_row("SELECT count(*) FROM tool_changed_files", [], |r| {
                    r.get::<_, i64>(0)
                })?,
            )
        })
        .await
        .unwrap();
    assert_eq!(count, 0);
    read(&tools, "file", "fresh-read").await;
    let fresh = tools
        .prepare(
            "run",
            call(
                "fresh-write",
                "write_file",
                json!({"path":"file","content":"confirmed"}),
            ),
        )
        .await
        .unwrap();
    assert!(tools.execute("run", "fresh-write", &fresh).await.success);
    assert_eq!(histories(&db).await, 1);
    db.shutdown().await.unwrap();
}

#[tokio::test]
async fn admitted_conflict_diff_uses_intention_preimage_even_after_a_new_read() {
    let (_temp, db, tools, root) = setup().await;
    std::fs::write(root.join("file"), "intention preimage\n").unwrap();
    read(&tools, "file", "read-original").await;
    let prepared = tools
        .prepare(
            "run",
            call(
                "write",
                "write_file",
                json!({"path":"file","content":"agent postimage"}),
            ),
        )
        .await
        .unwrap();
    std::fs::write(root.join("file"), "new user version\n").unwrap();
    read(&tools, "file", "read-new").await;
    let report = conflict(tools.execute("run", "write", &prepared).await);
    assert!(
        report["diff"]
            .as_str()
            .unwrap()
            .contains("-intention preimage")
    );
    assert!(
        report["diff"]
            .as_str()
            .unwrap()
            .contains("+new user version")
    );
    assert_eq!(report["expected_fp"], prepared.data["edits"][0]["pre"]);
    assert_eq!(histories(&db).await, 0);
    db.shutdown().await.unwrap();
}

#[tokio::test]
async fn file_appearance_and_disappearance_are_durable_reported_conflicts() {
    let (_temp, db, tools, root) = setup().await;
    let create = tools
        .prepare(
            "run",
            call(
                "create",
                "write_file",
                json!({"path":"appeared","content":"agent"}),
            ),
        )
        .await
        .unwrap();
    std::fs::write(root.join("appeared"), "user created\n").unwrap();
    for expected_count in 1..=3 {
        let report = conflict(tools.execute("run", "create", &create).await);
        assert_eq!(report["mismatches"], expected_count);
        assert_eq!(report["expected_fp"], Value::Null);
        assert_eq!(report["expected_exists"], false);
        assert_eq!(report["current_exists"], true);
        assert_eq!(report["affected_region"]["existence_changed"], true);
        assert!(report["diff"].as_str().unwrap().contains("+user created"));
        assert_eq!(report["escalate_to_parent"], expected_count >= 3);
    }
    std::fs::write(root.join("disappeared"), "user deleted this\n").unwrap();
    read(&tools, "disappeared", "read-deleted").await;
    let replace = tools
        .prepare(
            "run",
            call(
                "replace",
                "write_file",
                json!({"path":"disappeared","content":"agent"}),
            ),
        )
        .await
        .unwrap();
    std::fs::remove_file(root.join("disappeared")).unwrap();
    let report = conflict(tools.execute("run", "replace", &replace).await);
    assert_eq!(report["expected_exists"], true);
    assert_eq!(report["current_exists"], false);
    assert_eq!(report["current_fp"], Value::Null);
    assert!(
        report["diff"]
            .as_str()
            .unwrap()
            .contains("-user deleted this")
    );
    assert!(!root.join("disappeared").exists());
    assert_eq!(
        std::fs::read_to_string(root.join("appeared")).unwrap(),
        "user created\n"
    );
    assert_eq!(histories(&db).await, 0);
    let report: Value = serde_json::from_str(
        &tools
            .prepare(
                "run",
                call(
                    "prepare-recreate",
                    "write_file",
                    json!({"path":"disappeared","content":"not permitted"}),
                ),
            )
            .await
            .unwrap_err(),
    )
    .unwrap();
    assert_eq!(report["mismatches"], 2);
    db.shutdown().await.unwrap();
}

#[tokio::test]
async fn conflicted_patch_preflight_keeps_other_files_and_history_untouched() {
    let (_temp, db, tools, root) = setup().await;
    for path in ["a", "b"] {
        std::fs::write(root.join(path), "before\n").unwrap();
        read(&tools, path, &format!("read-{path}")).await;
    }
    let patch = "*** Begin Patch\n*** Update File: a\n@@\n-before\n+agent\n*** Update File: b\n@@\n-before\n+agent\n*** End Patch";
    let prepared = tools
        .prepare("run", call("patch", "apply_patch", json!({"patch":patch})))
        .await
        .unwrap();
    std::fs::write(root.join("b"), "external\n").unwrap();
    let report = conflict(tools.execute("run", "patch", &prepared).await);
    assert_eq!(report["path"], "b");
    assert_eq!(std::fs::read_to_string(root.join("a")).unwrap(), "before\n");
    assert_eq!(
        std::fs::read_to_string(root.join("b")).unwrap(),
        "external\n"
    );
    assert_eq!(histories(&db).await, 0);
    db.shutdown().await.unwrap();
}

#[tokio::test]
async fn cheap_scheduling_resolves_aliases_and_patch_moves_without_preparing_content() {
    let (_temp, db, tools, root) = setup().await;
    std::fs::write(root.join("file"), "content need not match any edit\n").unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(root.join("file"), root.join("alias")).unwrap();
    let alias = if cfg!(unix) { "alias" } else { "file" };
    let edit = call(
        "edit",
        "edit_file",
        json!({"path":alias,"old_text":"not present","new_text":"new"}),
    );
    let planned = tools.scheduling("run", &edit).await.unwrap();
    assert!(!planned.exclusive);
    assert_eq!(planned.resources.len(), 1);
    assert_eq!(
        planned.resources[0].key,
        root.join("file").to_string_lossy()
    );
    assert!(planned.resources[0].write);
    let patch = format!(
        "*** Begin Patch\n*** Update File: {alias}\n*** Move to: nested/new\n@@\n-old\n+new\n*** End Patch"
    );
    let patch_call = call("patch", "apply_patch", json!({"patch":patch}));
    let planned = tools.scheduling("run", &patch_call).await.unwrap();
    let keys = planned
        .resources
        .iter()
        .map(|r| r.key.clone())
        .collect::<Vec<_>>();
    assert!(keys.contains(&root.join("file").to_string_lossy().into_owned()));
    assert!(keys.contains(&root.join("nested/new").to_string_lossy().into_owned()));
    assert_eq!(keys.len(), 2);
    let counts = db
        .read(ReadLane::Interactive, |conn| {
            Ok((
                conn.query_row("SELECT count(*) FROM blobs", [], |r| r.get::<_, i64>(0))?,
                conn.query_row("SELECT count(*) FROM tool_readset", [], |r| {
                    r.get::<_, i64>(0)
                })?,
                conn.query_row("SELECT count(*) FROM file_versions", [], |r| {
                    r.get::<_, i64>(0)
                })?,
            ))
        })
        .await
        .unwrap();
    assert_eq!(counts, (0, 0, 0));
    assert_eq!(
        std::fs::read_to_string(root.join("file")).unwrap(),
        "content need not match any edit\n"
    );
    assert!(!root.join("nested").exists());
    assert!(tools.prepare("run", edit).await.is_err());
    db.shutdown().await.unwrap();
}

#[tokio::test]
async fn scheduling_matches_prepared_resources_and_terminal_exclusivity() {
    let (_temp, db, tools, root) = setup().await;
    std::fs::write(root.join("file"), "before\n").unwrap();
    read(&tools, "file", "read").await;
    let patch = "*** Begin Patch\n*** Update File: file\n*** Move to: moved\n@@\n-before\n+after\n*** End Patch";
    let calls = vec![
        call("read-plan", "read_file", json!({"path":"file"})),
        call(
            "edit",
            "edit_file",
            json!({"path":"file","old_text":"before","new_text":"after"}),
        ),
        call("patch", "apply_patch", json!({"patch":patch})),
        call("search", "grep", json!({"pattern":"before"})),
        call("glob", "glob", json!({"pattern":"*"})),
        call("exec", "exec", json!({"cmd":"not executed","cwd":root})),
        call(
            "read-session",
            "exec_io",
            json!({"action":"read","session":"session-alias"}),
        ),
        call(
            "write-session",
            "exec_io",
            json!({"action":"write","session_id":"session-id","text":"not executed"}),
        ),
    ];
    for call in calls {
        let planned = tools.scheduling("run", &call).await.unwrap();
        let prepared = tools.prepare("run", call).await.unwrap();
        assert_eq!(planned.resources, prepared.resources);
        assert_eq!(planned.exclusive, prepared.exclusive);
        if prepared.call.name == "exec" {
            assert!(planned.exclusive);
        }
        if prepared.call.id == "read-session" {
            assert!(!planned.exclusive);
            assert_eq!(planned.resources[0].key, "terminal:session-alias");
            assert!(!planned.resources[0].write);
        }
        if matches!(prepared.call.name.as_str(), "glob" | "grep") {
            assert!(planned.resources.is_empty());
        }
    }
    assert_eq!(histories(&db).await, 0);
    assert!(!root.join("moved").exists());
    db.shutdown().await.unwrap();
}

#[tokio::test]
async fn scheduling_invalid_arguments_is_opaque_and_reports_errors_only_in_preparation() {
    let (_temp, db, tools, _root) = setup().await;
    for call in [
        call("bad", "read_file", json!({"path":true})),
        call(
            "escape",
            "write_file",
            json!({"path":"../outside","content":"not written"}),
        ),
        call("unknown", "not_a_tool", json!({})),
    ] {
        assert!(tools.scheduling("run", &call).await.is_none());
        assert!(tools.prepare("run", call).await.is_err());
    }
    assert_eq!(
        db.read(ReadLane::Interactive, |conn| Ok(conn.query_row(
            "SELECT count(*) FROM blobs",
            [],
            |r| r.get::<_, i64>(0)
        )?))
        .await
        .unwrap(),
        0
    );
    db.shutdown().await.unwrap();
}

#[tokio::test]
async fn additive_conflict_migration_preserves_legacy_schema_and_counts() {
    let (_temp, db, tools, root) = setup().await;
    std::fs::write(root.join("file"), "legacy\n").unwrap();
    read(&tools, "file", "read").await;
    let prepared = tools
        .prepare(
            "run",
            call(
                "write",
                "write_file",
                json!({"path":"file","content":"agent"}),
            ),
        )
        .await
        .unwrap();
    let original=db.write(Priority::Normal,|conn|{let checksum=conn.query_row("SELECT checksum FROM tool_schema WHERE version=1",[],|r|r.get::<_,String>(0))?;conn.execute_batch("DROP TABLE tool_conflicts; DELETE FROM tool_schema WHERE version=2; UPDATE tool_readset SET mismatches=2;")?;Ok(checksum)}).await.unwrap();
    let upgraded = LocalTools::new(
        db.clone(),
        root.clone(),
        ToolPolicy {
            write: Access::Allow,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    std::fs::write(root.join("file"), "external\n").unwrap();
    let report = conflict(upgraded.execute("run", "write", &prepared).await);
    assert_eq!(report["mismatches"], 3);
    assert_eq!(report["escalate_to_parent"], true);
    assert_eq!(histories(&db).await, 0);
    let (checksum, versions) = db
        .read(ReadLane::Interactive, |conn| {
            Ok((
                conn.query_row(
                    "SELECT checksum FROM tool_schema WHERE version=1",
                    [],
                    |r| r.get::<_, String>(0),
                )?,
                conn.query_row("SELECT count(*) FROM tool_schema", [], |r| {
                    r.get::<_, i64>(0)
                })?,
            ))
        })
        .await
        .unwrap();
    assert_eq!(checksum, original);
    assert_eq!(versions, 2);
    db.shutdown().await.unwrap();
}

#[tokio::test]
async fn explicit_fingerprints_remain_strict_after_a_confirmed_same_run_edit() {
    let (_temp, db, tools, root) = setup().await;
    std::fs::write(root.join("file"), "before\n").unwrap();
    read(&tools, "file", "read").await;
    let first = tools
        .prepare(
            "run",
            call(
                "first",
                "write_file",
                json!({"path":"file","content":"after first\n"}),
            ),
        )
        .await
        .unwrap();
    let old_fp = first.data["edits"][0]["pre"].clone();
    let post_fp = first.data["edits"][0]["post"].clone();
    assert!(tools.execute("run", "first", &first).await.success);
    let rejected=tools.prepare("run",call("stale-explicit","edit_file",json!({"path":"file","old_text":"after first","new_text":"after second","fp":old_fp}))).await.unwrap_err();
    let report: Value = serde_json::from_str(&rejected).unwrap();
    assert_eq!(report["mismatches"], 1);
    assert_eq!(report["supplied_fp"], old_fp);
    assert_eq!(report["expected_fp"], post_fp);
    let second=tools.prepare("run",call("fresh-explicit","edit_file",json!({"path":"file","old_text":"after first","new_text":"after second","fp":post_fp}))).await.unwrap();
    assert!(tools.execute("run", "second", &second).await.success);
    assert_eq!(
        std::fs::read_to_string(root.join("file")).unwrap(),
        "after second\n"
    );
    assert_eq!(histories(&db).await, 2);
    db.shutdown().await.unwrap();
}
