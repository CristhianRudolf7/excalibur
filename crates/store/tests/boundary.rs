use excalibur_store::{
    BlobOwner, Database, DatabaseKind, MessageRecord, ReadLane, StorageConfig, Store,
};
use excalibur_types::MessageId;

#[tokio::test]
async fn sql_neutral_store_roundtrip_search_and_blob_access() {
    async fn exercise(store: impl Store) {
        let message = MessageRecord {
            id: MessageId::new(),
            conversation_id: None,
            text: "synthetic spacecraft log".into(),
            scope_id: "general".into(),
            role: "user".into(),
            language: "en".into(),
            user_timezone: "UTC".into(),
        };
        store.append_message(message.clone()).await.unwrap();
        assert_eq!(
            store.message(message.id).await.unwrap(),
            Some(message.clone())
        );
        assert_eq!(
            store
                .search_messages("spacecraft".into(), vec!["general".into()], 10)
                .await
                .unwrap(),
            vec![message]
        );
        let hash = store
            .put_blob(
                b"synthetic payload".to_vec(),
                BlobOwner {
                    kind: "test".into(),
                    id: "owner".into(),
                    slot: "output".into(),
                },
            )
            .await
            .unwrap();
        assert_eq!(
            store.get_blob(hash, 1024).await.unwrap(),
            b"synthetic payload"
        );
    }
    let temp = tempfile::tempdir().unwrap();
    let db = Database::open(
        temp.path().join("excalibur.db"),
        DatabaseKind::Main,
        StorageConfig::default(),
    )
    .unwrap();
    exercise(db.clone()).await;
    assert!(
        db.read(ReadLane::Interactive, |c| Ok(c.execute(
            "INSERT INTO messages(id,text) VALUES('bad','write via reader')",
            []
        )?))
        .await
        .is_err()
    );
    db.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shutdown_drains_cancelled_blocking_readers_and_closes_admissions() {
    let temp = tempfile::tempdir().unwrap();
    let db = Database::open(
        temp.path().join("excalibur.db"),
        DatabaseKind::Main,
        StorageConfig::default(),
    )
    .unwrap();
    let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let reader_db = db.clone();
    let read = tokio::spawn(async move {
        reader_db
            .read(ReadLane::Background, move |c| {
                c.execute_batch("BEGIN; SELECT count(*) FROM messages;")?;
                entered_tx.send(()).unwrap();
                release_rx.recv().unwrap();
                Ok(())
            })
            .await
    });
    entered_rx.await.unwrap();
    read.abort();
    let mut shutdown = Box::pin(db.shutdown());
    assert!(
        std::future::poll_fn(|cx| std::task::Poll::Ready(shutdown.as_mut().poll(cx)))
            .await
            .is_pending()
    );
    release_tx.send(()).unwrap();
    shutdown.await.unwrap();
    assert!(matches!(
        db.read(ReadLane::Interactive, |_| Ok(())).await,
        Err(excalibur_store::Error::Closed)
    ));
}

#[tokio::test]
async fn attachments_track_references_and_cannot_commit_missing_physical_files() {
    use excalibur_store::{Error, Priority};
    let temp = tempfile::tempdir().unwrap();
    let db = Database::open(
        temp.path().join("excalibur.db"),
        DatabaseKind::Main,
        StorageConfig::default(),
    )
    .unwrap();
    let prepared = db.blobs().prepare(b"synthetic attachment").unwrap();
    let hash = db
        .write_with_blob(prepared, Priority::Normal, |c, hash| {
            c.execute(
                "INSERT INTO messages(id,attachments) VALUES('attachment-message',json_array(?1))",
                [hash.as_str()],
            )?;
            Ok(hash.clone())
        })
        .await
        .unwrap();
    db.read(ReadLane::Interactive, move |c| {
        assert_eq!(
            c.query_row(
                "SELECT ref_count FROM blobs WHERE hash=?1",
                [hash.as_str()],
                |r| r.get::<_, i64>(0)
            )?,
            1
        );
        Ok(())
    })
    .await
    .unwrap();
    let result = db
        .write(Priority::Normal, |c| {
            let hash = "a".repeat(64);
            c.execute(
                "INSERT INTO blobs(hash,size,compressed_size) VALUES(?1,1,1)",
                [&hash],
            )?;
            c.execute(
                "INSERT INTO messages(id,attachments) VALUES('invalid',json_array(?1))",
                [&hash],
            )?;
            Ok(())
        })
        .await;
    assert!(matches!(result, Err(Error::Blob(_))));
    db.read(ReadLane::Interactive, |c| {
        assert_eq!(
            c.query_row(
                "SELECT count(*) FROM messages WHERE id='invalid'",
                [],
                |r| r.get::<_, i64>(0)
            )?,
            0
        );
        Ok(())
    })
    .await
    .unwrap();
    db.shutdown().await.unwrap();
}

#[tokio::test]
async fn offline_restore_is_validated_and_requires_exclusive_ownership() {
    use excalibur_store::{Error, Priority, restore_backup, unix_seconds};
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("excalibur.db");
    let db = Database::open(&path, DatabaseKind::Main, StorageConfig::default()).unwrap();
    db.backup(unix_seconds()).await.unwrap();
    let snapshot = std::fs::read_dir(temp.path().join("backups/excalibur.db"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    assert!(matches!(
        restore_backup(&path, &snapshot, DatabaseKind::Main),
        Err(Error::AlreadyOpen(_))
    ));
    db.write(Priority::Normal, |c| {
        c.execute(
            "INSERT INTO messages(id,text) VALUES('after-backup','later')",
            [],
        )?;
        Ok(())
    })
    .await
    .unwrap();
    db.shutdown().await.unwrap();
    drop(db);
    restore_backup(&path, &snapshot, DatabaseKind::Main).unwrap();
    let db = Database::open(&path, DatabaseKind::Main, StorageConfig::default()).unwrap();
    let count = db
        .read(ReadLane::Interactive, |c| {
            Ok(c.query_row(
                "SELECT count(*) FROM messages WHERE id='after-backup'",
                [],
                |r| r.get::<_, i64>(0),
            )?)
        })
        .await
        .unwrap();
    assert_eq!(count, 0);
    db.shutdown().await.unwrap();
}

#[tokio::test]
async fn bundled_sqlite_exposes_fts5_and_static_vec() {
    let temp = tempfile::tempdir().unwrap();
    let db = Database::open(
        temp.path().join("excalibur.db"),
        DatabaseKind::Main,
        StorageConfig::default(),
    )
    .unwrap();
    db.read(ReadLane::Interactive, |c| {
        let fts: i64 = c.query_row("SELECT sqlite_compileoption_used('ENABLE_FTS5')", [], |r| {
            r.get(0)
        })?;
        let vector: String = c.query_row("SELECT vec_version()", [], |r| r.get(0))?;
        assert_eq!(fts, 1);
        assert!(!vector.is_empty());
        println!(
            "SQLite={} FTS5={} sqlite-vec={vector}",
            excalibur_store::rusqlite::version(),
            fts
        );
        Ok(())
    })
    .await
    .unwrap();
    db.shutdown().await.unwrap();
}
