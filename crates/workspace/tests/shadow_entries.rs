use excalibur_store::{Database, DatabaseKind, Priority, StorageConfig};
use excalibur_vault::Redactor;
use excalibur_workspace::{EntryKind, SnapshotEntry, Workspace};
use std::{collections::BTreeMap, path::PathBuf};
#[tokio::test]
async fn shadow_captures_modes_and_link_targets_and_selected_restore_preserves_other_files() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("project");
    std::fs::create_dir(&root).unwrap();
    let db = Database::open(
        temp.path().join("db/main.db"),
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
    let workspace = Workspace::open(
        db.clone(),
        root.clone(),
        temp.path().join("data"),
        Redactor::default(),
    )
    .await
    .unwrap();
    let before = BTreeMap::from([
        (
            PathBuf::from("script"),
            SnapshotEntry::file(b"echo synthetic\n".to_vec(), true),
        ),
        (
            PathBuf::from("link"),
            SnapshotEntry::symlink(b"script".to_vec()),
        ),
        (
            PathBuf::from("other"),
            SnapshotEntry::file(b"original".to_vec(), false),
        ),
    ]);
    let after = BTreeMap::from([
        (
            PathBuf::from("script"),
            SnapshotEntry::file(b"echo synthetic\n".to_vec(), false),
        ),
        (
            PathBuf::from("link"),
            SnapshotEntry::file(b"new regular file".to_vec(), false),
        ),
        (
            PathBuf::from("other"),
            SnapshotEntry::file(b"changed by command".to_vec(), false),
        ),
    ]);
    let pre = workspace
        .shadow()
        .snapshot_entries("run", 0, "before", before.clone())
        .await
        .unwrap();
    let post = workspace
        .shadow()
        .snapshot_entries("run", 0, "after", after.clone())
        .await
        .unwrap();
    assert_eq!(
        workspace
            .shadow()
            .changed_files(&pre.id, &post.id)
            .await
            .unwrap()
            .len(),
        3
    );
    drop(workspace);
    let reopened = Workspace::open(
        db.clone(),
        root,
        temp.path().join("data"),
        Redactor::default(),
    )
    .await
    .unwrap();
    let selected = BTreeMap::from([
        (
            PathBuf::from("script"),
            after.get(&PathBuf::from("script")).cloned(),
        ),
        (
            PathBuf::from("link"),
            after.get(&PathBuf::from("link")).cloned(),
        ),
    ]);
    let plan = reopened
        .shadow()
        .plan_restore_entries(&pre.id, selected, true)
        .await
        .unwrap();
    assert!(plan.conflicts.is_empty());
    assert_eq!(plan.files.len(), 2);
    let script = plan
        .files
        .iter()
        .find(|p| p.path.ends_with("script"))
        .unwrap();
    assert!(script.executable);
    assert_eq!(script.post, Some(b"echo synthetic\n".to_vec()));
    assert_eq!(script.pre_executable, Some(false));
    let link = plan
        .files
        .iter()
        .find(|p| p.path.ends_with("link"))
        .unwrap();
    assert_eq!(link.kind, EntryKind::Symlink);
    assert_eq!(link.post, Some(b"script".to_vec()));
    assert_eq!(link.pre_kind, Some(EntryKind::File));
    assert!(plan.files.iter().all(|p| !p.path.ends_with("other")));
    let diff = reopened.shadow().diff(&pre.id, &post.id).await.unwrap();
    assert!(diff.contains("100755"));
    assert!(diff.contains("120000"));
    db.shutdown().await.unwrap();
}
#[tokio::test]
async fn outside_link_restore_conflicts_and_newer_user_link_change_is_preserved() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("project");
    std::fs::create_dir(&root).unwrap();
    let db = Database::open(
        temp.path().join("db/main.db"),
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
    let workspace = Workspace::open(
        db.clone(),
        root,
        temp.path().join("data"),
        Redactor::default(),
    )
    .await
    .unwrap();
    let pre = workspace
        .shadow()
        .snapshot_entries(
            "run",
            0,
            "before",
            BTreeMap::from([(
                PathBuf::from("link"),
                SnapshotEntry::symlink(b"../outside".to_vec()),
            )]),
        )
        .await
        .unwrap();
    workspace
        .shadow()
        .snapshot_entries(
            "run",
            0,
            "after",
            BTreeMap::from([(
                PathBuf::from("link"),
                SnapshotEntry::file(b"replacement".to_vec(), false),
            )]),
        )
        .await
        .unwrap();
    let plan = workspace
        .shadow()
        .plan_restore_entries(
            &pre.id,
            BTreeMap::from([(
                PathBuf::from("link"),
                Some(SnapshotEntry::file(b"replacement".to_vec(), false)),
            )]),
            true,
        )
        .await
        .unwrap();
    assert_eq!(plan.conflicts.len(), 1);
    assert!(plan.files.is_empty());
    let plan = workspace
        .shadow()
        .plan_restore_entries(
            &pre.id,
            BTreeMap::from([(
                PathBuf::from("link"),
                Some(SnapshotEntry::symlink(b"user-target".to_vec())),
            )]),
            true,
        )
        .await
        .unwrap();
    assert_eq!(plan.conflicts.len(), 1);
    assert!(plan.files.is_empty());
    db.shutdown().await.unwrap();
}
