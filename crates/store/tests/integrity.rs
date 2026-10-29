use excalibur_store::{Database, DatabaseKind, Priority, ReadLane, StorageConfig, Store};
use std::io::Read;

#[tokio::test]
async fn blob_damage_never_reverts_confirmed_messages() {
    for backup in [0, 1, 2] {
        for corrupt in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("excalibur.db");
            let db = Database::open(&path, DatabaseKind::Main, StorageConfig::default()).unwrap();
            db.write(Priority::Interactive, |c| {
                c.execute(
                    "INSERT INTO messages(id,text) VALUES('first','before backup')",
                    [],
                )?;
                Ok(())
            })
            .await
            .unwrap();
            if backup == 1 {
                db.backup(excalibur_store::unix_seconds()).await.unwrap();
            }
            let prepared = db.blobs().prepare(b"synthetic attachment").unwrap();
            let hash = db
                .attach_blob(prepared, "test".into(), "owner".into(), "payload".into())
                .await
                .unwrap();
            if backup == 2 {
                db.backup(excalibur_store::unix_seconds()).await.unwrap();
            }
            for n in 0..5 {
                db.write(Priority::Interactive, move |c| {
                    c.execute(
                        "INSERT INTO messages(id,text) VALUES(?1,'after backup')",
                        [format!("after{n}")],
                    )?;
                    Ok(())
                })
                .await
                .unwrap();
            }
            let blob = db.blobs().path(&hash);
            db.shutdown().await.unwrap();
            drop(db);
            if corrupt {
                std::fs::write(blob, b"damaged").unwrap();
            } else {
                std::fs::remove_file(blob).unwrap();
            }
            let reopened = Database::open(&path, DatabaseKind::Main, StorageConfig::default())
                .expect("blob availability must not control SQLite availability");
            let count = reopened
                .read(ReadLane::Interactive, |c| {
                    Ok(c.query_row("SELECT count(*) FROM messages", [], |r| r.get::<_, i64>(0))?)
                })
                .await
                .unwrap();
            assert_eq!(
                count, 6,
                "all confirmed messages survive; backup={backup} corrupt={corrupt}"
            );
            assert!(
                !reopened
                    .startup_events()
                    .contains(&excalibur_store::StoreEvent::Recovered)
            );
            let result = reopened.get_blob(hash.clone(), 100).await;
            assert!(if corrupt {
                matches!(result, Err(excalibur_store::Error::BlobCorrupt(_)))
            } else {
                matches!(result, Err(excalibur_store::Error::BlobMissing(_)))
            });
            let state = reopened
                .read(ReadLane::Interactive, move |c| {
                    Ok(c.query_row(
                        "SELECT state FROM blob_integrity WHERE hash=?1",
                        [hash.as_str()],
                        |r| r.get::<_, String>(0),
                    )?)
                })
                .await
                .unwrap();
            assert_eq!(state, if corrupt { "corrupt" } else { "missing" });
            reopened.shutdown().await.unwrap();
        }
    }
}

#[tokio::test]
async fn principal_databases_can_maintain_together_without_cross_deletion() {
    let dir = tempfile::tempdir().unwrap();
    let main = Database::open(
        dir.path().join("excalibur.db"),
        DatabaseKind::Main,
        StorageConfig::default(),
    )
    .unwrap();
    let telemetry = Database::open(
        dir.path().join("telemetry.db"),
        DatabaseKind::Telemetry,
        StorageConfig::default(),
    )
    .unwrap();
    let graph = Database::open(
        dir.path().join("projects/a/codegraph.db"),
        DatabaseKind::CodeGraph,
        StorageConfig::default(),
    )
    .unwrap();
    let staged = main.blobs().prepare(b"keep principal data").unwrap();
    let hash = main
        .attach_blob(staged, "test".into(), "owner".into(), "slot".into())
        .await
        .unwrap();
    let now = excalibur_store::unix_seconds() + 2 * 86400;
    let (a, b, c) = tokio::join!(
        main.maintenance(now),
        telemetry.maintenance(now),
        graph.maintenance(now)
    );
    a.unwrap();
    b.unwrap();
    c.unwrap();
    assert_eq!(
        main.get_blob(hash, 100).await.unwrap(),
        b"keep principal data"
    );
    main.shutdown().await.unwrap();
    telemetry.shutdown().await.unwrap();
    graph.shutdown().await.unwrap();
}

#[tokio::test]
async fn startup_does_not_decode_and_incremental_verification_is_bounded() {
    use excalibur_store::{Error, VerificationBudget};
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("excalibur.db");
    let db = Database::open(&path, DatabaseKind::Main, StorageConfig::default()).unwrap();
    let staged = db
        .blobs()
        .prepare_reader(std::io::repeat(17).take(8 * 1024 * 1024))
        .unwrap();
    let hash = db
        .attach_blob(staged, "test".into(), "bounded".into(), "data".into())
        .await
        .unwrap();
    db.shutdown().await.unwrap();
    drop(db);
    let db = Database::open(&path, DatabaseKind::Main, StorageConfig::default()).unwrap();
    assert_eq!(db.blobs().io_stats().decoded_bytes, 0);
    assert_eq!(db.blobs().io_stats().verified_reads, 0);
    let budget = VerificationBudget {
        max_blobs: 1,
        max_bytes: 64 * 1024,
    };
    let step = db.verify_blobs(budget).await.unwrap();
    assert!(step.pending);
    assert_eq!(step.completed, 0);
    assert!(step.bytes_read <= budget.max_bytes);
    let mut complete = 0;
    for _ in 0..140 {
        let step = db.verify_blobs(budget).await.unwrap();
        assert!(step.bytes_read <= budget.max_bytes);
        complete += step.completed;
        if !step.pending {
            break;
        }
    }
    assert_eq!(complete, 1);
    let original = db.blobs().path(&hash);
    let len = std::fs::metadata(&original).unwrap().len();
    db.shutdown().await.unwrap();
    drop(db);
    std::fs::write(&original, vec![0; len as usize]).unwrap();
    let db = Database::open(&path, DatabaseKind::Main, StorageConfig::default()).unwrap();
    assert_eq!(db.blobs().io_stats().decoded_bytes, 0);
    assert!(matches!(
        db.get_blob(hash.clone(), 16 * 1024 * 1024).await,
        Err(Error::BlobCorrupt(_))
    ));
    let progress = db.verify_blobs(budget).await.unwrap();
    assert!(
        progress
            .issues
            .iter()
            .any(|i| i.hash == hash.as_str() && i.state == excalibur_store::BlobState::Corrupt)
    );
    db.shutdown().await.unwrap();
}

#[tokio::test]
async fn automatic_transaction_rollback_preserves_cause_without_full_disk_pause() {
    use excalibur_store::{Error, StoreEvent};
    let dir = tempfile::tempdir().unwrap();
    let db = Database::open(
        dir.path().join("excalibur.db"),
        DatabaseKind::Main,
        StorageConfig::default(),
    )
    .unwrap();
    db.write(Priority::Normal,|c|{c.execute_batch("CREATE TABLE rollback_probe(id INTEGER PRIMARY KEY); INSERT INTO rollback_probe VALUES(1)")?;Ok(())}).await.unwrap();
    let mut events = db.subscribe_events();
    let error = db
        .write(Priority::Normal, |c| {
            c.execute("INSERT OR ROLLBACK INTO rollback_probe VALUES(1)", [])?;
            Ok(())
        })
        .await
        .unwrap_err();
    let Error::GroupAborted(cause) = error else {
        panic!("expected typed group failure: {error:?}");
    };
    let Error::TransactionControl {
        phase,
        original,
        control,
    } = cause.as_ref()
    else {
        panic!("missing control causes: {cause:?}");
    };
    assert_eq!(*phase, "rollback to savepoint");
    assert!(
        matches!(original.as_ref(),Error::Sqlite(e) if e.sqlite_error_code()==Some(excalibur_store::rusqlite::ErrorCode::ConstraintViolation))
    );
    assert!(matches!(control.as_ref(), Error::Sqlite(_)));
    db.write(Priority::Background, |c| {
        c.execute("INSERT INTO rollback_probe VALUES(2)", [])?;
        Ok(())
    })
    .await
    .unwrap();
    while let Ok(event) = events.try_recv() {
        assert_ne!(event, StoreEvent::DiskFull);
    }
    db.shutdown().await.unwrap();
}

#[tokio::test]
async fn duplicate_publication_has_no_decoding_and_remains_safe_with_gc() {
    use std::io::Read;
    let dir = tempfile::tempdir().unwrap();
    let db = Database::open(
        dir.path().join("excalibur.db"),
        DatabaseKind::Main,
        StorageConfig::default(),
    )
    .unwrap();
    let staged = db
        .blobs()
        .prepare_reader(std::io::repeat(41).take(16 * 1024 * 1024))
        .unwrap();
    let hash = db
        .attach_blob(staged, "test".into(), "first".into(), "slot".into())
        .await
        .unwrap();
    struct Gated {
        entered: Option<tokio::sync::oneshot::Sender<()>>,
        release: std::sync::mpsc::Receiver<()>,
        left: usize,
    }
    impl Read for Gated {
        fn read(&mut self, b: &mut [u8]) -> std::io::Result<usize> {
            if let Some(tx) = self.entered.take() {
                tx.send(()).unwrap();
                self.release.recv().unwrap();
            }
            let n = b.len().min(self.left);
            b[..n].fill(41);
            self.left -= n;
            Ok(n)
        }
    }
    let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let blobs = db.blobs();
    let preparing = tokio::task::spawn_blocking(move || {
        blobs.prepare_reader(Gated {
            entered: Some(entered_tx),
            release: release_rx,
            left: 16 * 1024 * 1024,
        })
    });
    entered_rx.await.unwrap();
    db.write(Priority::Interactive,|c|{c.execute("INSERT INTO messages(id,text) VALUES('during-prepare','interactive remains available')",[])?;Ok(())}).await.unwrap();
    release_tx.send(()).unwrap();
    let duplicate = preparing.await.unwrap().unwrap();
    let before = db.blobs().io_stats();
    let (published, collected) = tokio::join!(
        db.attach_blob(duplicate, "test".into(), "duplicate".into(), "slot".into()),
        db.maintenance(excalibur_store::unix_seconds())
    );
    assert_eq!(published.unwrap(), hash);
    collected.unwrap();
    assert_eq!(db.blobs().io_stats().decoded_bytes, before.decoded_bytes);
    assert_eq!(db.blobs().io_stats().verified_reads, before.verified_reads);
    assert_eq!(
        db.get_blob(hash, 20 * 1024 * 1024).await.unwrap().len(),
        16 * 1024 * 1024
    );
    db.shutdown().await.unwrap();
}

#[tokio::test]
async fn blob_namespace_has_exclusive_principal_ownership_and_rejects_cross_owner_staging() {
    use excalibur_store::Error;
    let dir = tempfile::tempdir().unwrap();
    let a = Database::open(
        dir.path().join("alice/excalibur.db"),
        DatabaseKind::Main,
        StorageConfig::default(),
    )
    .unwrap();
    assert!(matches!(
        Database::open(
            dir.path().join("alice/alias.db"),
            DatabaseKind::Main,
            StorageConfig::default()
        ),
        Err(Error::AlreadyOpen(_))
    ));
    let b = Database::open(
        dir.path().join("bob/excalibur.db"),
        DatabaseKind::Main,
        StorageConfig::default(),
    )
    .unwrap();
    let staged = a.blobs().prepare(b"exclusive owner data").unwrap();
    assert!(matches!(
        b.attach_blob(staged, "test".into(), "bob".into(), "out".into())
            .await,
        Err(Error::BlobOwnerMismatch)
    ));
    let telemetry = Database::open(
        dir.path().join("alice/telemetry.db"),
        DatabaseKind::Telemetry,
        StorageConfig::default(),
    )
    .unwrap();
    let staged = telemetry.blobs().prepare(b"unused namespace").unwrap();
    assert!(matches!(
        telemetry
            .attach_blob(staged, "test".into(), "bad".into(), "out".into())
            .await,
        Err(Error::PrincipalOnly)
    ));
    a.shutdown().await.unwrap();
    b.shutdown().await.unwrap();
    telemetry.shutdown().await.unwrap();
}

#[tokio::test]
async fn prepared_staging_is_not_collected_while_held() {
    let dir = tempfile::tempdir().unwrap();
    let db = Database::open(
        dir.path().join("excalibur.db"),
        DatabaseKind::Main,
        StorageConfig::default(),
    )
    .unwrap();
    let staged = db.blobs().prepare(b"in-flight staging").unwrap();
    db.maintenance(excalibur_store::unix_seconds() + 2 * 86400)
        .await
        .unwrap();
    let hash = db
        .attach_blob(staged, "test".into(), "active".into(), "out".into())
        .await
        .unwrap();
    assert_eq!(db.get_blob(hash, 100).await.unwrap(), b"in-flight staging");
    db.shutdown().await.unwrap();
}

#[tokio::test]
async fn savepoint_creation_and_release_errors_keep_control_causes() {
    use excalibur_store::Error;
    use excalibur_store::rusqlite::hooks::{
        AuthAction, AuthContext, Authorization, TransactionOperation,
    };
    for release in [true, false] {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(
            dir.path().join("excalibur.db"),
            DatabaseKind::Main,
            StorageConfig::default(),
        )
        .unwrap();
        if !release {
            db.write(Priority::Normal, |c| {
                c.authorizer(Some(|ctx: AuthContext<'_>| {
                    if matches!(
                        ctx.action,
                        AuthAction::Savepoint {
                            operation: TransactionOperation::Begin,
                            ..
                        }
                    ) {
                        Authorization::Deny
                    } else {
                        Authorization::Allow
                    }
                }));
                Ok(())
            })
            .await
            .unwrap();
        }
        let error = db
            .write(Priority::Normal, move |c| {
                if release {
                    c.execute(
                        "INSERT INTO messages(id,text) VALUES('rolled-back','unconfirmed')",
                        [],
                    )?;
                    c.execute_batch("RELEASE command")?;
                }
                Ok(())
            })
            .await
            .unwrap_err();
        let Error::GroupAborted(cause) = error else {
            panic!("unexpected: {error:?}");
        };
        assert!(
            matches!(cause.as_ref(),Error::TransactionControl{phase,..} if *phase==if release{"release"}else{"savepoint"})
        );
        if release {
            db.write(Priority::Background, |c| {
                assert_eq!(
                    c.query_row(
                        "SELECT count(*) FROM messages WHERE id='rolled-back'",
                        [],
                        |r| r.get::<_, i64>(0)
                    )?,
                    0
                );
                Ok(())
            })
            .await
            .unwrap();
        } else {
            assert!(!matches!(
                db.write(Priority::Background, |_| Ok(())).await,
                Err(Error::BackgroundPaused)
            ));
        }
        db.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn structural_sqlite_recovery_reports_window_and_salvages_readable_messages() {
    use excalibur_store::rusqlite::Connection;
    use std::io::{Seek, SeekFrom, Write};
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("excalibur.db");
    let db = Database::open(&path, DatabaseKind::Main, StorageConfig::default()).unwrap();
    let root=db.write(Priority::Normal,|c|{
        c.execute_batch("CREATE INDEX recovery_probe_index ON messages(text); INSERT INTO messages(id,text) VALUES('snapshot','known snapshot');")?;
        Ok(c.query_row("SELECT rootpage FROM sqlite_master WHERE name='recovery_probe_index'",[],|r|r.get::<_,u64>(0))?)
    }).await.unwrap();
    db.backup(excalibur_store::unix_seconds()).await.unwrap();
    for n in 0..5 {
        db.write(Priority::Interactive, move |c| {
            c.execute(
                "INSERT INTO messages(id,text) VALUES(?1,'readable newer data')",
                [format!("salvage{n}")],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    }
    db.shutdown().await.unwrap();
    drop(db);
    let c = Connection::open(&path).unwrap();
    c.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)").unwrap();
    let size: u64 = c.query_row("PRAGMA page_size", [], |r| r.get(0)).unwrap();
    drop(c);
    let mut file = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
    file.seek(SeekFrom::Start((root - 1) * size)).unwrap();
    file.write_all(&vec![0; size as usize]).unwrap();
    file.sync_all().unwrap();
    drop(file);
    let db = Database::open(&path, DatabaseKind::Main, StorageConfig::default()).unwrap();
    let report = db
        .recovery_report()
        .expect("consumer must receive explicit recovery information");
    assert!(report.data_loss_possible);
    assert!(report.original_path.exists());
    assert!(report.snapshot_path.as_ref().unwrap().exists());
    assert!(report.snapshot_at.unwrap() <= report.recovered_at);
    let salvage = Connection::open(report.salvage_path.as_ref().unwrap()).unwrap();
    let rows:i64=salvage.query_row("SELECT count(*) FROM salvaged_rows WHERE table_name='messages' AND row_json LIKE '%salvage%'",[],|r|r.get(0)).unwrap();
    assert_eq!(
        rows, 5,
        "all readable post-snapshot messages remain available in salvage"
    );
    db.shutdown().await.unwrap();
}

#[tokio::test]
async fn failed_group_rollback_retains_both_causes_and_stops_writer() {
    use excalibur_store::Error;
    use excalibur_store::rusqlite::hooks::{
        AuthAction, AuthContext, Authorization, TransactionOperation,
    };
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("excalibur.db");
    let db = Database::open(&path, DatabaseKind::Main, StorageConfig::default()).unwrap();
    let error = db
        .write(Priority::Normal, |c| {
            c.execute(
                "INSERT INTO messages(id,text) VALUES('not-committed','rollback')",
                [],
            )?;
            c.execute_batch("RELEASE command")?;
            c.authorizer(Some(|ctx: AuthContext<'_>| {
                if matches!(
                    ctx.action,
                    AuthAction::Transaction {
                        operation: TransactionOperation::Rollback
                    }
                ) {
                    Authorization::Deny
                } else {
                    Authorization::Allow
                }
            }));
            Ok(())
        })
        .await
        .unwrap_err();
    let Error::GroupAborted(cause) = error else {
        panic!("{error:?}");
    };
    assert!(
        matches!(cause.as_ref(),Error::TransactionControl{phase:"group rollback",original,control} if matches!(original.as_ref(),Error::TransactionControl{phase:"release",..}) && matches!(control.as_ref(),Error::Sqlite(_)))
    );
    assert!(matches!(db.shutdown().await, Err(Error::WriterPanicked)));
    drop(db);
    let db = Database::open(&path, DatabaseKind::Main, StorageConfig::default()).unwrap();
    assert_eq!(
        db.read(ReadLane::Interactive, |c| Ok(c.query_row(
            "SELECT count(*) FROM messages WHERE id='not-committed'",
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
async fn natural_language_search_requires_explicit_scope_and_isolates_identical_terms() {
    use excalibur_store::MessageRecord;
    use excalibur_types::MessageId;
    let dir = tempfile::tempdir().unwrap();
    let db = Database::open(
        dir.path().join("excalibur.db"),
        DatabaseKind::Main,
        StorageConfig::default(),
    )
    .unwrap();
    for scope in ["project:alpha", "project:beta"] {
        db.append_message(MessageRecord {
            id: MessageId::new(),
            conversation_id: None,
            text: "Árboles compartidos".into(),
            scope_id: scope.into(),
            role: "user".into(),
            language: "es".into(),
            user_timezone: "UTC".into(),
        })
        .await
        .unwrap();
    }
    assert!(
        db.search_messages("arboles".into(), vec![], 10)
            .await
            .is_err()
    );
    let results = db
        .search_messages("arboles".into(), vec!["project:alpha".into()], 10)
        .await
        .unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].scope_id, "project:alpha");
    db.shutdown().await.unwrap();
}

#[tokio::test]
async fn stale_integrity_pass_cannot_override_newly_published_verified_content() {
    use excalibur_store::{BlobState, VerificationBudget};
    let dir = tempfile::tempdir().unwrap();
    let db = Database::open(
        dir.path().join("excalibur.db"),
        DatabaseKind::Main,
        StorageConfig::default(),
    )
    .unwrap();
    let payload = vec![24; 2 * 1024 * 1024];
    let hash = db
        .attach_blob(
            db.blobs().prepare(&payload).unwrap(),
            "test".into(),
            "owner".into(),
            "out".into(),
        )
        .await
        .unwrap();
    let wrong =
        zstd::stream::encode_all(std::io::repeat(98).take(payload.len() as u64), 3).unwrap();
    std::fs::write(db.blobs().path(&hash), wrong).unwrap();
    let first = db
        .verify_blobs(VerificationBudget {
            max_blobs: 1,
            max_bytes: 1024,
        })
        .await
        .unwrap();
    assert_eq!(first.completed, 0);
    db.attach_blob(
        db.blobs().prepare(&payload).unwrap(),
        "test".into(),
        "owner".into(),
        "out".into(),
    )
    .await
    .unwrap();
    let mut outcomes = Vec::new();
    for _ in 0..6 {
        let pass = db
            .verify_blobs(VerificationBudget {
                max_blobs: 1,
                max_bytes: 1024 * 1024,
            })
            .await
            .unwrap();
        outcomes.extend(pass.issues);
        if !pass.pending {
            break;
        }
    }
    assert!(!outcomes.iter().any(|i| i.state == BlobState::Corrupt));
    let state = db
        .read(ReadLane::Interactive, move |c| {
            Ok(c.query_row(
                "SELECT state FROM blob_integrity WHERE hash=?1",
                [hash.as_str()],
                |r| r.get::<_, String>(0),
            )?)
        })
        .await
        .unwrap();
    assert_eq!(state, "ok");
    db.shutdown().await.unwrap();
}

#[tokio::test]
async fn sqlite_snapshot_remains_usable_when_its_blob_is_unavailable() {
    use excalibur_store::Error;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("excalibur.db");
    let db = Database::open(&path, DatabaseKind::Main, StorageConfig::default()).unwrap();
    let hash = db
        .attach_blob(
            db.blobs().prepare(b"snapshot attachment").unwrap(),
            "test".into(),
            "owner".into(),
            "out".into(),
        )
        .await
        .unwrap();
    db.write(Priority::Interactive, |c| {
        c.execute(
            "INSERT INTO messages(id,text) VALUES('recoverable','snapshot record')",
            [],
        )?;
        Ok(())
    })
    .await
    .unwrap();
    db.backup(excalibur_store::unix_seconds()).await.unwrap();
    let file = db.blobs().path(&hash);
    db.shutdown().await.unwrap();
    drop(db);
    let c = excalibur_store::rusqlite::Connection::open(&path).unwrap();
    c.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)").unwrap();
    drop(c);
    std::fs::remove_file(file).unwrap();
    std::fs::write(&path, b"invalid SQLite header").unwrap();
    let db = Database::open(&path, DatabaseKind::Main, StorageConfig::default()).unwrap();
    assert!(db.recovery_report().is_some());
    assert_eq!(
        db.read(ReadLane::Interactive, |c| Ok(c.query_row(
            "SELECT text FROM messages WHERE id='recoverable'",
            [],
            |r| r.get::<_, String>(0)
        )?))
        .await
        .unwrap(),
        "snapshot record"
    );
    assert!(matches!(
        db.get_blob(hash, 100).await,
        Err(Error::BlobMissing(_))
    ));
    db.shutdown().await.unwrap();
}
