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
        .unwrap()
        .0;
    let post = workspace
        .shadow()
        .snapshot_entries("run", 0, "after", after.clone())
        .await
        .unwrap()
        .0;
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
        .unwrap()
        .0;
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
#[tokio::test]
async fn snapshot_names_with_newlines_quotes_and_fake_entries_round_trip_literally() {
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
    let newline = PathBuf::from("a\nb.txt");
    let quoted = PathBuf::from("\"q\".txt");
    let forged = PathBuf::from("x\n100644 0000000000000000000000000000000000000000\tinjected.txt");
    let files = BTreeMap::from([
        (
            PathBuf::from("ok.txt"),
            SnapshotEntry::file(b"control".to_vec(), false),
        ),
        (
            newline.clone(),
            SnapshotEntry::file(b"newline".to_vec(), false),
        ),
        (
            quoted.clone(),
            SnapshotEntry::file(b"quoted".to_vec(), false),
        ),
        (
            forged.clone(),
            SnapshotEntry::file(b"forged".to_vec(), false),
        ),
    ]);
    // A newline in a name must not fail the snapshot, quotes must not be
    // interpreted, and an embedded fake entry must not create extra paths.
    let filled = workspace
        .shadow()
        .snapshot_entries("run", 0, "before", files)
        .await
        .unwrap()
        .0;
    let empty = workspace
        .shadow()
        .snapshot_entries("run", 0, "after", BTreeMap::new())
        .await
        .unwrap()
        .0;
    let mut changed = workspace
        .shadow()
        .changed_files(&empty.id, &filled.id)
        .await
        .unwrap();
    changed.sort();
    let mut expected = vec![PathBuf::from("ok.txt"), forged, newline, quoted];
    expected.sort();
    assert_eq!(changed, expected);
    db.shutdown().await.unwrap();
}
#[tokio::test]
async fn oversized_paths_degrade_to_warnings_instead_of_failing_the_snapshot() {
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
    let big = vec![0u8; 64 * 1024 * 1024 + 1];
    let (snapshot, warnings) = workspace
        .shadow()
        .snapshot_entries(
            "run",
            0,
            "before",
            BTreeMap::from([
                (PathBuf::from("big.bin"), SnapshotEntry::file(big, false)),
                (
                    PathBuf::from("small.txt"),
                    SnapshotEntry::file(b"kept".to_vec(), false),
                ),
            ]),
        )
        .await
        .unwrap();
    assert_eq!(warnings.len(), 1);
    assert!(warnings[0].contains("big.bin"));
    let empty = workspace
        .shadow()
        .snapshot_entries("run", 0, "after", BTreeMap::new())
        .await
        .unwrap()
        .0;
    let changed = workspace
        .shadow()
        .changed_files(&empty.id, &snapshot.id)
        .await
        .unwrap();
    assert_eq!(changed, vec![PathBuf::from("small.txt")]);
    db.shutdown().await.unwrap();
}
#[tokio::test]
async fn active_run_history_survives_object_collection_without_a_snapshot_count_cap() {
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
        c.execute("INSERT INTO runs(id) VALUES('prune')", [])?;
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
    for step in 0..35u64 {
        let (snapshot, warnings) = workspace
            .shadow()
            .snapshot_files(
                "prune",
                step,
                "before",
                BTreeMap::from([(
                    PathBuf::from("file.txt"),
                    format!("revision {step}\n").into_bytes(),
                )]),
            )
            .await
            .unwrap();
        assert!(warnings.is_empty());
        assert!(!snapshot.tree.is_empty());
    }
    let kept = workspace.shadow().list_snapshots("prune").await.unwrap();
    assert_eq!(kept.len(), 35);
    assert!(kept.iter().any(|s| s.step == 0));
    assert!(kept.iter().any(|s| s.step == 34));
    let repository = std::fs::read_dir(temp.path().join("data/shadow"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path()
        .join("objects.git");
    // Simulate an old unpinned receipt from before this correction. Force the
    // production threshold using unique objects, after aging the legacy files.
    let remove = std::process::Command::new("git")
        .arg("--git-dir")
        .arg(&repository)
        .args([
            "update-ref",
            "-d",
            &format!("refs/excalibur/trees/{}", kept[0].tree),
        ])
        .status()
        .unwrap();
    assert!(remove.success());
    for directory in std::fs::read_dir(repository.join("objects")).unwrap() {
        let directory = directory.unwrap();
        if directory.file_name().len() != 2 || !directory.file_type().unwrap().is_dir() {
            continue;
        }
        for object in std::fs::read_dir(directory.path()).unwrap() {
            let file = std::fs::File::open(object.unwrap().path()).unwrap();
            file.set_modified(std::time::SystemTime::now() - std::time::Duration::from_secs(7200))
                .unwrap();
        }
    }
    let bulk = (0..1005)
        .map(|i| {
            (
                PathBuf::from(format!("bulk-{i}")),
                format!("object-{i}").into_bytes(),
            )
        })
        .collect();
    workspace
        .shadow()
        .snapshot_files("run", 1, "before", bulk)
        .await
        .unwrap();
    // A paused or other run's oldest receipt must survive the real GC branch.
    assert!(repository.parent().unwrap().join("last-shadow-gc").exists());
    let left = workspace.shadow();
    let right = workspace.shadow();
    let (left, right) = tokio::join!(
        left.snapshot_files(
            "run",
            2,
            "before",
            BTreeMap::from([(PathBuf::from("race.txt"), b"left".to_vec())])
        ),
        right.snapshot_files(
            "run",
            2,
            "before",
            BTreeMap::from([(PathBuf::from("race.txt"), b"right".to_vec())])
        )
    );
    let left = left.unwrap().0;
    let right = right.unwrap().0;
    assert_eq!(left.id, right.id);
    assert_eq!(left.tree, right.tree);
    let output = std::process::Command::new("git")
        .arg("--git-dir")
        .arg(&repository)
        .args(["gc", "--prune=now"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let restored = workspace
        .shadow()
        .plan_between_entries(
            &kept[0].id,
            &kept[34].id,
            BTreeMap::from([(
                PathBuf::from("file.txt"),
                Some(SnapshotEntry::file(b"revision 34\n".to_vec(), false)),
            )]),
            true,
        )
        .await
        .unwrap();
    assert_eq!(restored.files[0].post, Some(b"revision 0\n".to_vec()));
    assert!(
        !workspace
            .shadow()
            .diff(&kept[0].id, &kept[34].id)
            .await
            .unwrap()
            .is_empty()
    );

    let winner = workspace
        .shadow()
        .plan_between_entries(&left.id, &kept[34].id, BTreeMap::new(), false)
        .await
        .unwrap();
    let content = winner
        .files
        .iter()
        .find(|f| f.path.ends_with("race.txt"))
        .unwrap()
        .post
        .as_ref()
        .unwrap();
    assert!(content == b"left" || content == b"right");
    db.shutdown().await.unwrap();
}
