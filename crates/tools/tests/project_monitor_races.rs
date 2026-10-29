#[cfg(unix)]
use excalibur_store::ReadLane;
use excalibur_store::{Database, DatabaseKind, StorageConfig};
use excalibur_tools::{
    LocalTools, ToolPolicy,
    monitor::{ChangeObserver, ProjectChange, ProjectMonitor},
};
use excalibur_types::runtime::BoxFuture;
use std::{
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

struct MutateDuringBaseline {
    root: PathBuf,
    changed: AtomicBool,
    observed: tokio::sync::mpsc::UnboundedSender<(ProjectChange, Option<Vec<u8>>)>,
}
impl ChangeObserver for MutateDuringBaseline {
    fn changed<'a>(
        &'a self,
        change: &'a ProjectChange,
        content: Option<Vec<u8>>,
    ) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async move {
            if change.path.as_path() == Path::new("source.rs")
                && !self.changed.swap(true, Ordering::SeqCst)
            {
                // This barrier writes after the inventory read and while the initial
                // observer is awaited. No timing interval controls the ordering.
                assert_eq!(content.as_deref(), Some(&b"fn original() {}\n"[..]));
                std::fs::write(
                    self.root.join("source.rs"),
                    b"fn changed_during_start() {}\n",
                )
                .map_err(|e| e.to_string())?;
            } else if change.path.as_path() == Path::new("source.rs") {
                let _ = self.observed.send((change.clone(), content));
            }
            Ok(())
        })
    }
}
#[tokio::test]
async fn watcher_captures_a_write_inside_the_initial_observer_without_manual_rescan() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("project");
    std::fs::create_dir(&root).unwrap();
    std::fs::write(root.join("source.rs"), b"fn original() {}\n").unwrap();
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
    let observer = Arc::new(MutateDuringBaseline {
        root: root.clone(),
        changed: AtomicBool::new(false),
        observed: tx,
    });
    let monitor = ProjectMonitor::start(tools, Some(observer), vec![])
        .await
        .unwrap();
    // The timeout is only a deadlock bound: the event must originate from the
    // baseline callback, and no poll or later filesystem write can produce it.
    let (change, content) = tokio::time::timeout(Duration::from_secs(10), rx.recv())
        .await
        .expect("write inside baseline was lost by the watcher")
        .unwrap();
    assert!(change.external);
    assert_eq!(
        content.as_deref(),
        Some(&b"fn changed_during_start() {}\n"[..])
    );
    assert_ne!(change.previous_hash, change.current_hash);
    assert!(monitor.last_error().is_none());
    monitor.stop().await;
    db.shutdown().await.unwrap();
}

#[cfg(unix)]
struct IndexAndObserve {
    services: excalibur_tools::intelligence::ProjectIntelligence,
    observed: tokio::sync::mpsc::UnboundedSender<(ProjectChange, Option<Vec<u8>>)>,
}
#[cfg(unix)]
impl ChangeObserver for IndexAndObserve {
    fn changed<'a>(
        &'a self,
        change: &'a ProjectChange,
        content: Option<Vec<u8>>,
    ) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async move {
            self.services
                .index(&change.path, content.as_deref())
                .await?;
            if change.current_hash.is_none() {
                let _ = self.observed.send((change.clone(), content));
            }
            Ok(())
        })
    }
}
#[cfg(unix)]
#[tokio::test]
async fn replacing_an_indexed_file_with_an_external_symlink_removes_the_index_without_reading_target()
 {
    use excalibur_code_intelligence::CodeGraph;
    use excalibur_tools::intelligence::ProjectIntelligence;
    use std::os::unix::fs::symlink;
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("project");
    std::fs::create_dir(&root).unwrap();
    std::fs::write(root.join("source.rs"), b"fn original() {}\n").unwrap();
    let outside = temp.path().join("private-synthetic.rs");
    std::fs::write(&outside, b"fn outside_private_canary_never_indexed() {}\n").unwrap();
    let db = Database::open(
        temp.path().join("data/main.db"),
        DatabaseKind::Main,
        StorageConfig::default(),
    )
    .unwrap();
    let graph_db = Database::open(
        temp.path().join("data/codegraph.db"),
        DatabaseKind::CodeGraph,
        StorageConfig::default(),
    )
    .unwrap();
    let graph = CodeGraph::open(graph_db.clone(), "monitor-synthetic-project")
        .await
        .unwrap();
    let services = ProjectIntelligence::new(
        graph.clone(),
        "monitor-synthetic-project".into(),
        root.clone(),
    )
    .unwrap();
    let tools = LocalTools::new(db.clone(), root.clone(), ToolPolicy::default())
        .await
        .unwrap();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let monitor = ProjectMonitor::start(
        tools,
        Some(Arc::new(IndexAndObserve {
            services,
            observed: tx,
        })),
        vec![],
    )
    .await
    .unwrap();
    assert_eq!(graph.indexed_paths().await.unwrap(), vec!["source.rs"]);
    std::fs::remove_file(root.join("source.rs")).unwrap();
    symlink(&outside, root.join("source.rs")).unwrap();
    let (change, content) = tokio::time::timeout(Duration::from_secs(10), rx.recv())
        .await
        .expect("revoked path blocked the durable change queue")
        .unwrap();
    assert_eq!(change.path, PathBuf::from("source.rs"));
    assert!(change.external);
    assert!(content.is_none());
    assert!(graph.indexed_paths().await.unwrap().is_empty());
    monitor.scan_now().await.unwrap();
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
    let nodes = graph_db
        .read(ReadLane::Interactive, |c| {
            Ok(c.query_row(
                "SELECT count(*) FROM nodes WHERE name LIKE '%outside_private_canary%'",
                [],
                |r| r.get::<_, u32>(0),
            )?)
        })
        .await
        .unwrap();
    assert_eq!(nodes, 0);
    assert!(monitor.last_error().is_none());
    monitor.stop().await;
    graph_db.shutdown().await.unwrap();
    db.shutdown().await.unwrap();
}
