use excalibur_store::{Database, DatabaseKind, Priority, ReadLane, StorageConfig};
use excalibur_tools::{Access, LocalTools, ToolPolicy, monitor::*};
use excalibur_types::runtime::{BoxFuture, ToolCall, ToolRuntime};
use serde_json::json;
use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};
struct Observed(Mutex<Vec<(ProjectChange, Option<Vec<u8>>)>>);
impl ChangeObserver for Observed {
    fn changed<'a>(
        &'a self,
        change: &'a ProjectChange,
        content: Option<Vec<u8>>,
    ) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async move {
            self.0.lock().unwrap().push((change.clone(), content));
            Ok(())
        })
    }
}
#[tokio::test]
async fn external_changes_invalidate_evidence_preserve_conflict_preimages_notify_holder_and_ignore_exclusions()
 {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("project");
    std::fs::create_dir_all(root.join("node_modules")).unwrap();
    std::fs::write(root.join("source.rs"), "fn old() {}\n").unwrap();
    std::fs::write(root.join("node_modules/excluded.rs"), "fn ignored() {}\n").unwrap();
    let db = Database::open(
        temp.path().join("data/main.db"),
        DatabaseKind::Main,
        StorageConfig::default(),
    )
    .unwrap();
    let journal = excalibur_journal::Journal::new(db.clone()).await.unwrap();
    let run = journal
        .create_run("monitor-owner", json!({"synthetic":true}))
        .await
        .unwrap();
    let tasks = excalibur_tasks::TaskStore::open(db.clone()).await.unwrap();
    tasks.host_batch("general","monitor-plan",json!({"ops":[{"op":"create_task","id":"task","title":"Synthetic","data":{"metadata":{"workspace":root}}},{"op":"create_requirement","id":"req","data":{"text":"Synthetic file","state":"accepted"}},{"op":"create_criterion","id":"criterion","data":{"requirement_id":"req","statement":"File works","method":"test","impact_area":["source.rs"]}},{"op":"link_requirement","task":"task","requirement":"req"},{"op":"create_check","id":"check","data":{"criterion_id":"criterion","definition":{"kind":"file_contains","path":"source.rs","needle":"old"}}}]})).await.unwrap();
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
    let read = tools
        .prepare(
            &run,
            ToolCall {
                id: "read".into(),
                name: "read_file".into(),
                arguments: json!({"path":"source.rs"}).to_string(),
            },
        )
        .await
        .unwrap();
    assert!(tools.execute(&run, "read", &read).await.success);
    let owner = run.clone();
    let path = root.join("source.rs").to_string_lossy().into_owned();
    db.write(Priority::Journal, move |c| {
        c.execute(
            "INSERT INTO leases(path,holder_id,expires_at) VALUES(?1,?2,unixepoch()+600)",
            excalibur_store::rusqlite::params![path, owner],
        )?;
        Ok(())
    })
    .await
    .unwrap();
    let seen = Arc::new(Observed(Mutex::new(vec![])));
    let monitor = ProjectMonitor::start(
        tools.clone(),
        Some(seen.clone()),
        vec![EvidenceTarget {
            tasks: tasks.clone(),
            scope: "general".into(),
        }],
    )
    .await
    .unwrap();
    seen.0.lock().unwrap().clear();
    tasks.bind_root("task", &run).await.unwrap();
    let evidence = excalibur_tasks::CheckRunner::new(Arc::new(tools.clone()), root.clone())
        .run(&tasks, "task", "check", &run)
        .await
        .unwrap();
    assert_eq!(evidence.result, excalibur_tasks::EvidenceResult::Pass);
    std::fs::write(root.join("source.rs"), "fn changed() {}\n").unwrap();
    std::fs::write(
        root.join("node_modules/excluded.rs"),
        "fn still_ignored() {}\n",
    )
    .unwrap();
    monitor.scan_now().await.unwrap();
    let changes = seen.0.lock().unwrap().clone();
    assert_eq!(changes.len(), 1);
    assert!(changes[0].0.external);
    assert_eq!(changes[0].1.as_deref(), Some(&b"fn changed() {}\n"[..]));
    assert!(
        journal
            .entries(&run, 0, 100)
            .await
            .unwrap()
            .iter()
            .any(|e| e.kind == "directive_received"
                && e.payload["text"].as_str().unwrap().contains("source.rs"))
    );
    let conflict = tools
        .prepare(
            &run,
            ToolCall {
                id: "edit".into(),
                name: "edit_file".into(),
                arguments: json!({"path":"source.rs","old_text":"old","new_text":"updated"})
                    .to_string(),
            },
        )
        .await
        .unwrap_err();
    assert!(conflict.contains("fingerprint_conflict"));
    assert!(conflict.contains("changed"));
    let reason = db
        .read(ReadLane::Interactive, |c| {
            Ok(c.query_row(
                "SELECT reason FROM task_criterion_state WHERE criterion_id='criterion'",
                [],
                |r| r.get::<_, String>(0),
            )?)
        })
        .await
        .unwrap();
    assert!(reason.contains("changed"));
    let stale = db
        .read(ReadLane::Interactive, |c| {
            Ok(c.query_row("SELECT stale FROM evidence LIMIT 1", [], |r| {
                r.get::<_, bool>(0)
            })?)
        })
        .await
        .unwrap();
    assert!(stale);
    monitor.stop().await;
    drop(monitor);
    drop(tools);
    db.shutdown().await.unwrap();
}
struct Signalled(tokio::sync::mpsc::UnboundedSender<ProjectChange>);
impl ChangeObserver for Signalled {
    fn changed<'a>(
        &'a self,
        change: &'a ProjectChange,
        _: Option<Vec<u8>>,
    ) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async move {
            if change.external {
                self.0.send(change.clone()).map_err(|_| "observer closed")?;
            }
            Ok(())
        })
    }
}
#[tokio::test]
async fn native_file_event_updates_the_durable_queue_without_manual_polling() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("project");
    std::fs::create_dir(&root).unwrap();
    std::fs::write(root.join("observed.txt"), "before").unwrap();
    let db = Database::open(
        temp.path().join("data/main.db"),
        DatabaseKind::Main,
        StorageConfig::default(),
    )
    .unwrap();
    let tools = LocalTools::new(db.clone(), root.clone(), ToolPolicy::default())
        .await
        .unwrap();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let monitor = ProjectMonitor::start(tools, Some(Arc::new(Signalled(tx))), vec![])
        .await
        .unwrap();
    std::fs::write(root.join("observed.txt"), "after").unwrap();
    let change = tokio::time::timeout(std::time::Duration::from_secs(10), rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(change.path, std::path::PathBuf::from("observed.txt"));
    assert_ne!(change.previous_hash, change.current_hash);
    monitor.scan_now().await.unwrap();
    monitor.stop().await;
    let pending = db
        .read(ReadLane::Interactive, |c| {
            Ok(c.query_row(
                "SELECT count(*) FROM project_changes WHERE state='pending'",
                [],
                |r| r.get::<_, u32>(0),
            )?)
        })
        .await
        .unwrap();
    assert_eq!(pending, 0);
    db.shutdown().await.unwrap();
}
#[tokio::test]
async fn managed_checklist_updates_preserve_fresh_evidence_but_outside_edits_invalidate_it() {
    use excalibur_tasks::{CheckRunner, CriterionState, EvidenceResult, TaskStore};
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("project");
    std::fs::create_dir_all(root.join("tests")).unwrap();
    std::fs::write(root.join("source.rs"), "fn valid() {}\n").unwrap();
    let readme = root.join("tests/README.md");
    std::fs::write(&readme, "# User notes\n").unwrap();
    let db = Database::open(
        temp.path().join("data/main.db"),
        DatabaseKind::Main,
        StorageConfig::default(),
    )
    .unwrap();
    let journal = excalibur_journal::Journal::new(db.clone()).await.unwrap();
    let run = journal
        .create_run("checklist-owner", json!({}))
        .await
        .unwrap();
    let tasks = TaskStore::open(db.clone()).await.unwrap();
    tasks.host_batch("general","plan",json!({"ops":[{"op":"create_task","id":"task","title":"Synthetic","data":{"metadata":{"workspace":root}}},{"op":"create_requirement","id":"req","data":{"text":"Source remains valid","state":"accepted"}},{"op":"create_criterion","id":"criterion","data":{"requirement_id":"req","statement":"Source check passes","method":"test","impact_area":["**"]}},{"op":"link_requirement","task":"task","requirement":"req"},{"op":"create_check","id":"check","data":{"criterion_id":"criterion","definition":{"kind":"file_contains","path":"source.rs","needle":"valid"}}}]})).await.unwrap();
    tasks.bind_root("task", &run).await.unwrap();
    let tools = LocalTools::new(db.clone(), root.clone(), ToolPolicy::default())
        .await
        .unwrap();
    let monitor = ProjectMonitor::start(
        tools.clone(),
        None,
        vec![EvidenceTarget {
            tasks: tasks.clone(),
            scope: "general".into(),
        }],
    )
    .await
    .unwrap();
    let evidence = CheckRunner::new(Arc::new(tools), root.clone())
        .with_projection(readme.clone())
        .run(&tasks, "task", "check", &run)
        .await
        .unwrap();
    assert_eq!(evidence.result, EvidenceResult::Pass);
    let entries = vec![excalibur_tasks::checklist::ChecklistEntry {
        check_id: "check".into(),
        label: "Synthetic proof".into(),
        passed: true,
        fresh: true,
        recorded_at: Some(1),
        commit: None,
        evidence_id: Some(evidence.id.clone()),
    }];
    let rendered = excalibur_tasks::checklist::render(b"# User notes\n", &entries).unwrap();
    std::fs::write(&readme, &rendered).unwrap();
    monitor.scan_now().await.unwrap();
    let (_, criteria) = tasks.requirements_and_criteria("task").await.unwrap();
    assert_eq!(
        tasks.assess_criterion(&criteria[0], 0).await.unwrap().state,
        CriterionState::Satisfied
    );
    assert!(!tasks.evidence(&evidence.id).await.unwrap().unwrap().stale);
    let mut outside = rendered;
    outside.extend_from_slice(b"\nUser changed outside the managed block.\n");
    std::fs::write(&readme, outside).unwrap();
    monitor.scan_now().await.unwrap();
    assert!(tasks.evidence(&evidence.id).await.unwrap().unwrap().stale);
    assert_eq!(
        tasks.assess_criterion(&criteria[0], 0).await.unwrap().state,
        CriterionState::Unverified
    );
    monitor.stop().await;
    db.shutdown().await.unwrap();
}
#[tokio::test]
async fn configured_projection_path_canonicalizes_generated_checklist_sections() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("project");
    std::fs::create_dir_all(root.join("test")).unwrap();
    std::fs::write(
        root.join("test/README.md"),
        "# Notes\n<!-- excalibur:checklist -->\n- [ ] old\n<!-- /excalibur:checklist -->\n",
    )
    .unwrap();
    std::fs::write(root.join("notes.txt"), "user notes\n").unwrap();
    let db = Database::open(
        temp.path().join("data/main.db"),
        DatabaseKind::Main,
        StorageConfig::default(),
    )
    .unwrap();
    let tools = LocalTools::new(db.clone(), root.clone(), ToolPolicy::default())
        .await
        .unwrap();
    let seen = Arc::new(Observed(Mutex::new(vec![])));
    let monitor = ProjectMonitor::start_with_projections(
        tools.clone(),
        Some(seen.clone()),
        vec![],
        vec![PathBuf::from("test/README.md")],
    )
    .await
    .unwrap();
    seen.0.lock().unwrap().clear();
    // Only the generated section changes; user text is untouched.
    std::fs::write(
        root.join("test/README.md"),
        "# Notes\n<!-- excalibur:checklist -->\n- [x] new\n<!-- /excalibur:checklist -->\n",
    )
    .unwrap();
    std::fs::write(root.join("notes.txt"), "edited notes\n").unwrap();
    monitor.scan_now().await.unwrap();
    let mut changes = seen.0.lock().unwrap().clone();
    changes.sort_by(|a, b| a.0.path.cmp(&b.0.path));
    assert_eq!(changes.len(), 2);
    let readme = changes
        .iter()
        .find(|(change, _)| change.path == Path::new("test/README.md"))
        .unwrap();
    assert!(
        !readme.0.semantic_changed,
        "a configured projection must canonicalize its generated section"
    );
    let notes = changes
        .iter()
        .find(|(change, _)| change.path == Path::new("notes.txt"))
        .unwrap();
    assert!(notes.0.semantic_changed);
    monitor.stop().await;
    drop(monitor);
    drop(tools);
    db.shutdown().await.unwrap();
}
#[tokio::test]
async fn gitignore_excluded_files_never_queue_project_changes() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("project");
    std::fs::create_dir(&root).unwrap();
    std::fs::write(root.join(".gitignore"), "ignored.txt\n").unwrap();
    std::fs::write(root.join("ignored.txt"), "v1\n").unwrap();
    std::fs::write(root.join("tracked.txt"), "v1\n").unwrap();
    let db = Database::open(
        temp.path().join("data/main.db"),
        DatabaseKind::Main,
        StorageConfig::default(),
    )
    .unwrap();
    let tools = LocalTools::new(db.clone(), root.clone(), ToolPolicy::default())
        .await
        .unwrap();
    let seen = Arc::new(Observed(Mutex::new(vec![])));
    let monitor = ProjectMonitor::start(tools.clone(), Some(seen.clone()), vec![])
        .await
        .unwrap();
    seen.0.lock().unwrap().clear();
    std::fs::write(root.join("ignored.txt"), "v2\n").unwrap();
    std::fs::write(root.join("tracked.txt"), "v2\n").unwrap();
    monitor.scan_now().await.unwrap();
    let changes = seen.0.lock().unwrap().clone();
    assert_eq!(changes.len(), 1);
    assert_eq!(changes[0].0.path, PathBuf::from("tracked.txt"));
    monitor.stop().await;
    drop(monitor);
    drop(tools);
    db.shutdown().await.unwrap();
}
#[tokio::test]
async fn agent_exec_changes_are_attributed_before_marking_them_external() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("project");
    std::fs::create_dir(&root).unwrap();
    excalibur_tools::terminal::prepare_project_boundary(&root).unwrap();
    let db = Database::open(
        temp.path().join("data/main.db"),
        DatabaseKind::Main,
        StorageConfig::default(),
    )
    .unwrap();
    let journal = excalibur_journal::Journal::new(db.clone()).await.unwrap();
    let run = journal
        .create_run("exec-owner", json!({"synthetic": true}))
        .await
        .unwrap();
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
    let seen = Arc::new(Observed(Mutex::new(vec![])));
    let monitor = ProjectMonitor::start(tools.clone(), Some(seen.clone()), vec![])
        .await
        .unwrap();
    seen.0.lock().unwrap().clear();
    let prepared = tools
        .prepare(
            &run,
            ToolCall {
                id: "spawn".into(),
                name: "exec".into(),
                arguments: json!({"cmd": "sh -c 'printf agent > agent.txt'"}).to_string(),
            },
        )
        .await
        .unwrap();
    let result = tools.execute(&run, "spawn", &prepared).await;
    assert!(result.success, "{result:?}");
    tools.end_step(&run, 0).await.unwrap();
    monitor.scan_now().await.unwrap();
    let changes = seen.0.lock().unwrap().clone();
    assert_eq!(changes.len(), 1);
    assert_eq!(changes[0].0.path, PathBuf::from("agent.txt"));
    assert!(
        !changes[0].0.external,
        "the agent's own exec output is not an outside change: {changes:?}"
    );
    monitor.stop().await;
    drop(monitor);
    drop(tools);
    db.shutdown().await.unwrap();
}
