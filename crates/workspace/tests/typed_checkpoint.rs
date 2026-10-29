#![cfg(unix)]
use excalibur_store::{Database, DatabaseKind, StorageConfig};
use excalibur_types::runtime::BoxFuture;
use excalibur_vault::Redactor;
use excalibur_workspace::*;
use std::{
    collections::BTreeMap,
    os::unix::{
        ffi::OsStrExt,
        fs::{PermissionsExt, symlink},
    },
    path::{Path, PathBuf},
    process::Command,
    sync::atomic::{AtomicUsize, Ordering},
};

fn git(root: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .current_dir(root)
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
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}
fn identity() -> GitIdentity {
    GitIdentity {
        name: "Synthetic Fixture".into(),
        email: "fixture@example.invalid".into(),
    }
}
async fn fixture() -> (tempfile::TempDir, Database, Workspace, Worktree) {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("project");
    std::fs::create_dir(&root).unwrap();
    git(&root, &["init", "-b", "main"]);
    for (path, bytes) in [
        ("base.txt", "base\n"),
        ("private.txt", "private baseline\n"),
        ("delete.txt", "delete\n"),
        ("mode.sh", "#!/bin/sh\nexit 0\n"),
    ] {
        std::fs::write(root.join(path), bytes).unwrap();
    }
    std::fs::set_permissions(root.join("mode.sh"), std::fs::Permissions::from_mode(0o755)).unwrap();
    git(&root, &["add", "."]);
    git(&root, &["commit", "-m", "Synthetic baseline"]);
    let db = Database::open(
        temp.path().join("db/main.db"),
        DatabaseKind::Main,
        StorageConfig::default(),
    )
    .unwrap();
    let workspace = Workspace::open(
        db.clone(),
        root,
        temp.path().join("data"),
        Redactor::default(),
    )
    .await
    .unwrap();
    let tree = workspace.git().create("typed-task", "child").await.unwrap();
    (temp, db, workspace, tree)
}
fn read_entry(path: &Path) -> Result<Option<SnapshotEntry>> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(value) => value,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    if metadata.is_symlink() {
        return Ok(Some(SnapshotEntry::symlink(
            std::fs::read_link(path)?.as_os_str().as_bytes().to_vec(),
        )));
    }
    if !metadata.is_file() {
        return Err(Error::Invalid("fixture requires file or symlink"));
    }
    Ok(Some(SnapshotEntry::file(
        std::fs::read(path)?,
        metadata.permissions().mode() & 0o111 != 0,
    )))
}
#[derive(Default)]
struct Guard {
    calls: AtomicUsize,
    mutate_at: Option<usize>,
    move_ref_at: Option<usize>,
}
impl CheckpointEntriesGuard for Guard {
    fn verify<'a>(
        &'a self,
        tree: &'a Worktree,
        expected: &'a BTreeMap<PathBuf, Option<SnapshotEntry>>,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            let call = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
            if self.mutate_at == Some(call) {
                std::fs::set_permissions(
                    tree.path.join("mode.sh"),
                    std::fs::Permissions::from_mode(0o644),
                )?;
            }
            for (path, entry) in expected {
                if read_entry(&tree.path.join(path))?.as_ref() != entry.as_ref() {
                    return Err(Error::Invalid("typed fixture fingerprint changed"));
                }
            }
            if self.move_ref_at == Some(call) {
                let head = git(&tree.path, &["rev-parse", "HEAD"]);
                let tree_id = git(&tree.path, &["rev-parse", "HEAD^{tree}"]);
                let commit = git(
                    &tree.path,
                    &[
                        "commit-tree",
                        tree_id.trim(),
                        "-p",
                        head.trim(),
                        "-m",
                        "Concurrent synthetic host commit",
                    ],
                );
                git(
                    &tree.path,
                    &[
                        "update-ref",
                        &format!("refs/heads/{}", tree.branch),
                        commit.trim(),
                        head.trim(),
                    ],
                );
            }
            Ok(())
        })
    }
}
#[tokio::test]
async fn typed_checkpoint_stages_modes_literal_links_and_deletions_without_unlisted_content() {
    let (_temp, db, workspace, tree) = fixture().await;
    let original_parent = git(workspace.root(), &["rev-parse", "HEAD"]);
    std::fs::write(tree.path.join("new.sh"), b"#!/bin/sh\nprintf synthetic\n").unwrap();
    std::fs::set_permissions(
        tree.path.join("new.sh"),
        std::fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    std::fs::write(tree.path.join("base.txt"), b"authorized new bytes\n").unwrap();
    symlink("base.txt", tree.path.join("link")).unwrap();
    std::fs::remove_file(tree.path.join("delete.txt")).unwrap();
    std::fs::write(
        tree.path.join("private.txt"),
        b"unlisted staged synthetic private bytes\n",
    )
    .unwrap();
    git(&tree.path, &["add", "--", "private.txt"]);
    let entries = BTreeMap::from([
        (
            PathBuf::from("new.sh"),
            read_entry(&tree.path.join("new.sh")).unwrap(),
        ),
        (
            PathBuf::from("base.txt"),
            read_entry(&tree.path.join("base.txt")).unwrap(),
        ),
        (
            PathBuf::from("link"),
            read_entry(&tree.path.join("link")).unwrap(),
        ),
        (PathBuf::from("delete.txt"), None),
    ]);
    let guard = Guard::default();
    workspace
        .git()
        .checkpoint_entries(
            "typed-task",
            "child",
            "Capture authorized entry types",
            &identity(),
            entries,
            &guard,
        )
        .await
        .unwrap();
    assert_eq!(guard.calls.load(Ordering::SeqCst), 3);
    let listing = git(&tree.path, &["ls-tree", "HEAD"]);
    assert!(
        listing
            .lines()
            .any(|line| line.starts_with("100755 ") && line.ends_with("\tnew.sh"))
    );
    assert!(
        listing
            .lines()
            .any(|line| line.starts_with("100644 ") && line.ends_with("\tbase.txt"))
    );
    assert!(
        listing
            .lines()
            .any(|line| line.starts_with("120000 ") && line.ends_with("\tlink"))
    );
    assert!(!listing.lines().any(|line| line.ends_with("\tdelete.txt")));
    assert_eq!(git(&tree.path, &["show", "HEAD:link"]), "base.txt");
    assert_eq!(
        git(&tree.path, &["show", "HEAD:private.txt"]),
        "private baseline\n"
    );
    assert_eq!(
        git(&tree.path, &["show", ":private.txt"]),
        "unlisted staged synthetic private bytes\n"
    );
    assert_eq!(
        git(workspace.root(), &["rev-parse", "HEAD"]),
        original_parent
    );
    db.shutdown().await.unwrap();
}
#[tokio::test]
async fn metadata_changes_at_each_verification_boundary_prevent_ref_advancement() {
    for boundary in 1..=3 {
        let (_temp, db, workspace, tree) = fixture().await;
        let before = git(&tree.path, &["rev-parse", "HEAD"]);
        let entries = BTreeMap::from([(
            PathBuf::from("mode.sh"),
            read_entry(&tree.path.join("mode.sh")).unwrap(),
        )]);
        let guard = Guard {
            mutate_at: Some(boundary),
            ..Guard::default()
        };
        assert!(
            workspace
                .git()
                .checkpoint_entries(
                    "typed-task",
                    "child",
                    "Must preserve metadata CAS",
                    &identity(),
                    entries,
                    &guard
                )
                .await
                .is_err()
        );
        assert_eq!(guard.calls.load(Ordering::SeqCst), boundary);
        assert_eq!(git(&tree.path, &["rev-parse", "HEAD"]), before);
        db.shutdown().await.unwrap();
    }
}
#[tokio::test]
async fn typed_checkpoint_updates_existing_modes_and_file_link_kinds() {
    let (_temp, db, workspace, tree) = fixture().await;
    std::fs::set_permissions(
        tree.path.join("mode.sh"),
        std::fs::Permissions::from_mode(0o644),
    )
    .unwrap();
    std::fs::remove_file(tree.path.join("base.txt")).unwrap();
    symlink("mode.sh", tree.path.join("base.txt")).unwrap();
    let entries = ["mode.sh", "base.txt"]
        .into_iter()
        .map(|path| {
            (
                PathBuf::from(path),
                read_entry(&tree.path.join(path)).unwrap(),
            )
        })
        .collect();
    workspace
        .git()
        .checkpoint_entries(
            "typed-task",
            "child",
            "Preserve explicit metadata changes",
            &identity(),
            entries,
            &Guard::default(),
        )
        .await
        .unwrap();
    assert!(git(&tree.path, &["ls-tree", "HEAD", "--", "mode.sh"]).starts_with("100644 "));
    assert!(git(&tree.path, &["ls-tree", "HEAD", "--", "base.txt"]).starts_with("120000 "));
    assert_eq!(git(&tree.path, &["show", "HEAD:base.txt"]), "mode.sh");
    std::fs::remove_file(tree.path.join("base.txt")).unwrap();
    std::fs::write(tree.path.join("base.txt"), b"ordinary file again\n").unwrap();
    let entries = BTreeMap::from([(
        PathBuf::from("base.txt"),
        read_entry(&tree.path.join("base.txt")).unwrap(),
    )]);
    workspace
        .git()
        .checkpoint_entries(
            "typed-task",
            "child",
            "Capture literal file replacement",
            &identity(),
            entries,
            &Guard::default(),
        )
        .await
        .unwrap();
    assert!(git(&tree.path, &["ls-tree", "HEAD", "--", "base.txt"]).starts_with("100644 "));
    assert_eq!(
        git(&tree.path, &["show", "HEAD:base.txt"]),
        "ordinary file again\n"
    );
    db.shutdown().await.unwrap();
}
#[tokio::test]
async fn concurrent_branch_advance_wins_compare_and_set_without_rewriting_host_commit() {
    let (_temp, db, workspace, tree) = fixture().await;
    let entries = BTreeMap::from([(
        PathBuf::from("mode.sh"),
        read_entry(&tree.path.join("mode.sh")).unwrap(),
    )]);
    let guard = Guard {
        move_ref_at: Some(3),
        ..Guard::default()
    };
    assert!(
        workspace
            .git()
            .checkpoint_entries(
                "typed-task",
                "child",
                "Must not overwrite concurrent ref",
                &identity(),
                entries,
                &guard
            )
            .await
            .is_err()
    );
    assert_eq!(
        git(&tree.path, &["log", "-1", "--format=%s"]).trim(),
        "Concurrent synthetic host commit"
    );
    db.shutdown().await.unwrap();
}
struct Legacy;
impl CheckpointGuard for Legacy {
    fn verify<'a>(
        &'a self,
        tree: &'a Worktree,
        expected: &'a BTreeMap<PathBuf, Option<Vec<u8>>>,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            for (path, bytes) in expected {
                if std::fs::read(tree.path.join(path)).ok().as_ref() != bytes.as_ref() {
                    return Err(Error::Invalid("legacy content changed"));
                }
            }
            Ok(())
        })
    }
}
#[tokio::test]
async fn legacy_content_wrapper_preserves_head_executable_mode() {
    let (_temp, db, workspace, tree) = fixture().await;
    let content = b"#!/bin/sh\nprintf updated\n".to_vec();
    std::fs::write(tree.path.join("mode.sh"), &content).unwrap();
    std::fs::set_permissions(
        tree.path.join("mode.sh"),
        std::fs::Permissions::from_mode(0o644),
    )
    .unwrap();
    workspace
        .git()
        .checkpoint_files(
            "typed-task",
            "child",
            "Keep legacy HEAD metadata",
            &identity(),
            BTreeMap::from([(PathBuf::from("mode.sh"), Some(content))]),
            &Legacy,
        )
        .await
        .unwrap();
    assert!(git(&tree.path, &["ls-tree", "HEAD", "--", "mode.sh"]).starts_with("100755 "));
    db.shutdown().await.unwrap();
}
