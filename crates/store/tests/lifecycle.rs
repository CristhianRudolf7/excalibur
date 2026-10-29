use excalibur_store::{
    Database, DatabaseKind, Priority, ReadLane, ShortCodeKind, StorageConfig, StoreEvent, schema,
};
use rusqlite::Connection;
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}
fn main_db(path: &Path) -> Database {
    Database::open(path, DatabaseKind::Main, StorageConfig::default()).unwrap()
}
fn backups(path: &Path) -> PathBuf {
    path.parent()
        .unwrap()
        .join("backups")
        .join(path.file_name().unwrap())
}
async fn ref_count(db: &Database, hash: String) -> u32 {
    db.read(ReadLane::Interactive, move |c| {
        Ok(
            c.query_row("SELECT ref_count FROM blobs WHERE hash=?1", [hash], |r| {
                r.get(0)
            })?,
        )
    })
    .await
    .unwrap()
}

#[tokio::test]
async fn blobs_deduplicate_count_references_and_retain_young_or_referenced_content() {
    let temp = tempfile::tempdir().unwrap();
    let db = main_db(&temp.path().join("excalibur.db"));
    let blob_store = db.blobs();
    let hash_a = db
        .attach_blob(
            blob_store.prepare(b"synthetic durable content").unwrap(),
            "test".into(),
            "owner-a".into(),
            "output".into(),
        )
        .await
        .unwrap();
    let hash_b = db
        .attach_blob(
            blob_store.prepare(b"synthetic durable content").unwrap(),
            "test".into(),
            "owner-b".into(),
            "output".into(),
        )
        .await
        .unwrap();
    assert_eq!(hash_a, hash_b);
    assert_eq!(
        blob_store.read(&hash_a, 1024).unwrap(),
        b"synthetic durable content"
    );
    assert_eq!(ref_count(&db, hash_a.as_str().into()).await, 2);
    let rows: u32 = db
        .read(ReadLane::Interactive, |c| {
            Ok(c.query_row("SELECT count(*) FROM blobs", [], |r| r.get(0))?)
        })
        .await
        .unwrap();
    assert_eq!(rows, 1);
    db.write(Priority::Normal, |c| {
        c.execute("DELETE FROM blob_refs WHERE owner_id='owner-a'", [])?;
        Ok(())
    })
    .await
    .unwrap();
    assert_eq!(ref_count(&db, hash_a.as_str().into()).await, 1);
    db.maintenance(now() + 2 * 86400).await.unwrap();
    assert!(
        blob_store.path(&hash_a).is_file(),
        "referenced files are never garbage collected"
    );
    assert_eq!(ref_count(&db, hash_a.as_str().into()).await, 1);
    db.shutdown().await.unwrap();
}

#[tokio::test]
async fn garbage_collection_removes_old_orphans_but_keeps_backup_references() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("excalibur.db");
    let db = main_db(&path);
    let blob_store = db.blobs();
    let protected = db
        .attach_blob(
            blob_store.prepare(b"content retained in backup").unwrap(),
            "test".into(),
            "protected".into(),
            "output".into(),
        )
        .await
        .unwrap();
    db.backup(now()).await.unwrap();
    db.write(Priority::Normal, |c| {
        c.execute("DELETE FROM blob_refs WHERE owner_id='protected'", [])?;
        Ok(())
    })
    .await
    .unwrap();
    let orphan_hash = db
        .write_with_blob(
            blob_store.prepare(b"old unreferenced content").unwrap(),
            Priority::Normal,
            |_c, hash| Ok(hash.clone()),
        )
        .await
        .unwrap();
    let young_hash = db
        .write_with_blob(
            blob_store.prepare(b"young unreferenced content").unwrap(),
            Priority::Normal,
            |_c, hash| Ok(hash.clone()),
        )
        .await
        .unwrap();
    let young = young_hash.as_str().to_string();
    let future = now() + 2 * 86400;
    db.write(Priority::Normal, move |c| {
        c.execute(
            "UPDATE blobs SET created_at=?1 WHERE hash=?2",
            rusqlite::params![future, young],
        )?;
        Ok(())
    })
    .await
    .unwrap();
    db.maintenance(future).await.unwrap();
    assert!(
        blob_store.path(&protected).is_file(),
        "retained backup protects its blob"
    );
    assert!(
        !blob_store.path(&orphan_hash).exists(),
        "old unreferenced content is collected"
    );
    assert!(
        blob_store.path(&young_hash).exists(),
        "young registered blobs stay retained"
    );
    assert_eq!(
        blob_store.read(&protected, 1024).unwrap(),
        b"content retained in backup"
    );
    db.shutdown().await.unwrap();
}

#[tokio::test]
async fn backup_rotation_keeps_seven_daily_and_four_weekly_snapshots() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("excalibur.db");
    let db = main_db(&path);
    let base = now().div_euclid(86400 * 7) * (86400 * 7) + 35 * 86400;
    for day in 0..36 {
        db.backup(base + day * 86400).await.unwrap();
    }
    let names: Vec<_> = fs::read_dir(backups(&path))
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(names.iter().filter(|n| n.starts_with("daily-")).count(), 7);
    assert_eq!(names.iter().filter(|n| n.starts_with("weekly-")).count(), 4);
    let snapshot = fs::read_dir(backups(&path))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let c = Connection::open(snapshot).unwrap();
    assert_eq!(
        c.query_row("PRAGMA quick_check", [], |r| r.get::<_, String>(0))
            .unwrap(),
        "ok"
    );
    db.shutdown().await.unwrap();
}

#[tokio::test]
async fn corrupted_main_restores_valid_backup_and_reconstructible_graph_reopens() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("excalibur.db");
    let db = main_db(&path);
    db.write(Priority::Normal, |c| {
        c.execute(
            "INSERT INTO memories(id,statement) VALUES('mem_restore','synthetic restored iceberg')",
            [],
        )?;
        // Restore also rebuilds stale derived indexes from authoritative content.
        c.execute(
            "INSERT INTO memories_fts(memories_fts) VALUES('delete-all')",
            [],
        )?;
        Ok(())
    })
    .await
    .unwrap();
    db.backup(now()).await.unwrap();
    db.shutdown().await.unwrap();
    drop(db);
    let flush = Connection::open(&path).unwrap();
    flush
        .execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")
        .unwrap();
    drop(flush);
    fs::OpenOptions::new()
        .write(true)
        .truncate(true)
        .open(&path)
        .unwrap()
        .write_all(b"synthetic corrupted database")
        .unwrap();
    let restored = main_db(&path);
    assert!(restored.startup_events().contains(&StoreEvent::Recovered));
    let count: u32 = restored
        .read(ReadLane::Interactive, |c| {
            Ok(c.query_row(
                "SELECT count(*) FROM memories_fts WHERE memories_fts MATCH 'iceberg'",
                [],
                |r| r.get(0),
            )?)
        })
        .await
        .unwrap();
    assert_eq!(count, 1);
    restored.shutdown().await.unwrap();
    drop(restored);
    let graph_path = temp
        .path()
        .join("projects")
        .join("synthetic")
        .join("codegraph.db");
    let graph = Database::open(
        &graph_path,
        DatabaseKind::CodeGraph,
        StorageConfig::default(),
    )
    .unwrap();
    graph.shutdown().await.unwrap();
    drop(graph);
    // Remove the legitimate WAL recovery path: this test corrupts the authoritative
    // file, rather than simulating a crash recoverable from an intact WAL.
    let flush = Connection::open(&graph_path).unwrap();
    flush
        .execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")
        .unwrap();
    drop(flush);
    fs::OpenOptions::new()
        .write(true)
        .truncate(true)
        .open(&graph_path)
        .unwrap()
        .write_all(b"synthetic corrupt graph")
        .unwrap();
    let rebuilt = Database::open(
        &graph_path,
        DatabaseKind::CodeGraph,
        StorageConfig::default(),
    )
    .unwrap();
    assert!(rebuilt.startup_events().contains(&StoreEvent::Recovered));
    assert_eq!(
        rebuilt
            .read(ReadLane::Interactive, |c| Ok(c.query_row(
                "SELECT count(*) FROM nodes",
                [],
                |r| r.get::<_, u32>(0)
            )?))
            .await
            .unwrap(),
        0
    );
    rebuilt.shutdown().await.unwrap();
}

#[tokio::test]
async fn migration_saves_previous_version_and_short_codes_survive_deletion_and_reopen() {
    excalibur_store::initialize_sqlite().unwrap();
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("excalibur.db");
    let mut legacy = Connection::open(&path).unwrap();
    schema::migrate_to(&mut legacy, DatabaseKind::Main, 1).unwrap();
    legacy
        .execute(
            "INSERT INTO projects(id,name) VALUES('project-a','Synthetic')",
            [],
        )
        .unwrap();
    drop(legacy);
    let db = main_db(&path);
    let older = fs::read_dir(backups(&path))
        .unwrap()
        .map(|e| e.unwrap().path())
        .find(|p| {
            p.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("before-update-")
        })
        .unwrap();
    let c = Connection::open(older).unwrap();
    assert_eq!(
        c.pragma_query_value(None, "user_version", |r| r.get::<_, u32>(0))
            .unwrap(),
        1
    );
    assert_eq!(
        db.allocate_short_id("project-a".into(), ShortCodeKind::Task)
            .await
            .unwrap(),
        "T1"
    );
    db.write(Priority::Normal, |c| {
        c.execute(
            "INSERT INTO tasks(id,project_id,code) VALUES('task-a','project-a','T1')",
            [],
        )?;
        c.execute("DELETE FROM tasks WHERE id='task-a'", [])?;
        Ok(())
    })
    .await
    .unwrap();
    assert_eq!(
        db.allocate_short_id("project-a".into(), ShortCodeKind::Task)
            .await
            .unwrap(),
        "T2"
    );
    assert_eq!(
        db.allocate_short_id("project-a".into(), ShortCodeKind::Requirement)
            .await
            .unwrap(),
        "R1"
    );
    assert_eq!(
        db.allocate_short_id("project-a".into(), ShortCodeKind::Criterion(1))
            .await
            .unwrap(),
        "C1.1"
    );
    assert_eq!(
        db.allocate_short_id("project-a".into(), ShortCodeKind::Criterion(2))
            .await
            .unwrap(),
        "C2.1"
    );
    assert_eq!(
        db.allocate_short_id("project-a".into(), ShortCodeKind::Milestone)
            .await
            .unwrap(),
        "M1"
    );
    db.shutdown().await.unwrap();
    drop(db);
    let reopened = main_db(&path);
    assert_eq!(
        reopened
            .allocate_short_id("project-a".into(), ShortCodeKind::Task)
            .await
            .unwrap(),
        "T3"
    );
    reopened.shutdown().await.unwrap();
}

#[tokio::test]
async fn dangling_blob_metadata_preserves_confirmed_rows_and_reports_incidence() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("excalibur.db");
    let db = main_db(&path);
    db.write(Priority::Normal, |c| {
        c.execute(
            "INSERT INTO principals(id,name) VALUES('owner','Synthetic owner')",
            [],
        )?;
        Ok(())
    })
    .await
    .unwrap();
    db.backup(now() + 86400).await.unwrap();
    db.write(Priority::Interactive, |c| {
        c.execute(
            "INSERT INTO messages(id,text) VALUES('post-snapshot','confirmed content')",
            [],
        )?;
        Ok(())
    })
    .await
    .unwrap();
    db.shutdown().await.unwrap();
    drop(db);
    let external = Connection::open(&path).unwrap();
    external.pragma_update(None, "foreign_keys", false).unwrap();
    external.execute("INSERT INTO blob_refs(owner_type,owner_id,slot,hash) VALUES('external','invalid','output',?1)",["a".repeat(64)]).unwrap();
    external
        .execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")
        .unwrap();
    drop(external);
    let restored = main_db(&path);
    assert!(!restored.startup_events().contains(&StoreEvent::Recovered));
    assert!(restored.startup_events().iter().any(|event| matches!(event,
        StoreEvent::BlobIntegrity(issue) if issue.state == excalibur_store::BlobState::MetadataMissing
    )));
    assert_eq!(
        restored
            .read(ReadLane::Interactive, |c| {
                Ok(c.query_row(
                    "SELECT text FROM messages WHERE id='post-snapshot'",
                    [],
                    |r| r.get::<_, String>(0),
                )?)
            })
            .await
            .unwrap(),
        "confirmed content"
    );
    assert_eq!(
        restored
            .read(ReadLane::Interactive, |c| Ok(c.query_row(
                "SELECT count(*) FROM blob_refs",
                [],
                |r| r.get::<_, u32>(0)
            )?))
            .await
            .unwrap(),
        1
    );
    assert_eq!(
        restored
            .read(ReadLane::Interactive, |c| Ok(c.query_row(
                "SELECT name FROM principals WHERE id='owner'",
                [],
                |r| r.get::<_, String>(0)
            )?))
            .await
            .unwrap(),
        "Synthetic owner"
    );
    restored.shutdown().await.unwrap();
}
