//! Deterministic synthetic backend tests for writer serialization and isolation.
use excalibur_store::{Database, DatabaseKind, Error, Priority, ReadLane, StorageConfig};
use std::time::{Duration, Instant};

async fn table(db: &Database) {
    db.write(Priority::Interactive, |conn| {
        conn.execute_batch(
            "CREATE TABLE test_rows(id INTEGER PRIMARY KEY, lane INTEGER NOT NULL, data BLOB)",
        )?;
        Ok(())
    })
    .await
    .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_four_priority_writers_and_parallel_readers_have_no_busy_errors() {
    let dir = tempfile::tempdir().unwrap();
    let options = StorageConfig {
        queue_capacity: 8,
        ..Default::default()
    };
    let db = Database::open(dir.path().join("main.db"), DatabaseKind::Main, options).unwrap();
    table(&db).await;
    let priorities = [
        Priority::Interactive,
        Priority::Journal,
        Priority::Normal,
        Priority::Background,
    ];
    let mut writers = Vec::new();
    for index in 0..200 {
        let db = db.clone();
        let priority = priorities[index % 4];
        writers.push(tokio::spawn(async move {
            let start = Instant::now();
            db.write(priority, move |conn| {
                conn.execute(
                    "INSERT INTO test_rows(id,lane,data) VALUES(?1,?2,?3)",
                    (index as i64, (index % 4) as i64, "synthetic"),
                )?;
                Ok(())
            })
            .await
            .map(|()| (index % 4, start.elapsed()))
        }));
    }
    let mut readers = Vec::new();
    for lane in [ReadLane::Interactive, ReadLane::Background] {
        let db = db.clone();
        readers.push(tokio::spawn(async move {
            for _ in 0..30 {
                let count = db
                    .read(lane, |conn| {
                        Ok(conn.query_row("SELECT count(*) FROM test_rows", [], |row| {
                            row.get::<_, i64>(0)
                        })?)
                    })
                    .await?;
                assert!((0..=200).contains(&count));
            }
            Ok::<_, Error>(())
        }));
    }
    let mut latencies: [Vec<Duration>; 4] = Default::default();
    for task in writers {
        let (priority, elapsed) = task
            .await
            .unwrap()
            .expect("a serialized writer must not return SQLITE_BUSY");
        latencies[priority].push(elapsed);
    }
    for task in readers {
        task.await.unwrap().unwrap();
    }
    let stats = db.stats();
    assert_eq!(stats.failed, [0; 4]);
    assert_eq!(stats.busy_retries, 0);
    assert!(stats.max_group_operations <= 200);
    for (priority, samples) in latencies.iter().enumerate() {
        assert_eq!(samples.len(), 50);
        eprintln!(
            "priority={priority} writes={} maximum_latency_us={} busy_errors=0",
            samples.len(),
            samples.iter().max().unwrap().as_micros()
        );
    }
    assert_eq!(
        db.read(ReadLane::Interactive, |conn| Ok(conn.query_row(
            "SELECT count(*) FROM test_rows",
            [],
            |row| row.get::<_, i64>(0)
        )?))
        .await
        .unwrap(),
        200
    );
    db.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn constraint_failure_rolls_back_its_own_command_only() {
    let dir = tempfile::tempdir().unwrap();
    let db = Database::open(
        dir.path().join("main.db"),
        DatabaseKind::Main,
        StorageConfig::default(),
    )
    .unwrap();
    table(&db).await;
    db.write(Priority::Normal, |conn| {
        conn.execute("INSERT INTO test_rows VALUES(1,0,'original')", [])?;
        Ok(())
    })
    .await
    .unwrap();
    let bad_db = db.clone();
    let bad = tokio::spawn(async move {
        bad_db
            .write(Priority::Normal, |conn| {
                conn.execute("INSERT INTO test_rows VALUES(2,0,'must roll back')", [])?;
                conn.execute("INSERT INTO test_rows VALUES(1,0,'duplicate')", [])?;
                Ok(())
            })
            .await
    });
    let good_db = db.clone();
    let good = tokio::spawn(async move {
        good_db
            .write(Priority::Normal, |conn| {
                conn.execute("INSERT INTO test_rows VALUES(3,0,'survives')", [])?;
                Ok(())
            })
            .await
    });
    assert!(matches!(bad.await.unwrap(), Err(Error::Sqlite(_))));
    good.await.unwrap().unwrap();
    let ids = db
        .read(ReadLane::Interactive, |conn| {
            let mut statement = conn.prepare("SELECT id FROM test_rows ORDER BY id")?;
            Ok(statement
                .query_map([], |row| row.get::<_, i64>(0))?
                .collect::<Result<Vec<_>, _>>()?)
        })
        .await
        .unwrap();
    assert_eq!(ids, vec![1, 3]);
    db.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn interactive_reader_has_a_reserved_connection() {
    let dir = tempfile::tempdir().unwrap();
    let config = StorageConfig {
        read_connections: 2,
        ..Default::default()
    };
    let db = Database::open(dir.path().join("main.db"), DatabaseKind::Main, config).unwrap();
    let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let background_db = db.clone();
    let background = tokio::spawn(async move {
        background_db
            .read(ReadLane::Background, move |conn| {
                entered_tx.send(()).unwrap();
                release_rx.recv().unwrap();
                Ok(conn.query_row("SELECT 1", [], |row| row.get::<_, i64>(0))?)
            })
            .await
    });
    entered_rx.await.unwrap();
    let result = tokio::time::timeout(
        Duration::from_secs(3),
        db.read(ReadLane::Interactive, |conn| {
            Ok(conn.query_row("SELECT 2", [], |row| row.get::<_, i64>(0))?)
        }),
    )
    .await;
    release_tx.send(()).unwrap();
    assert_eq!(result.unwrap().unwrap(), 2);
    assert_eq!(background.await.unwrap().unwrap(), 1);
    db.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn external_lock_exhausts_bounded_retries_without_partial_writes() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("main.db");
    let config = StorageConfig {
        busy_timeout_ms: 2,
        busy_retries: 2,
        ..Default::default()
    };
    let db = Database::open(&path, DatabaseKind::Main, config).unwrap();
    table(&db).await;
    use std::io::{BufRead, Write};
    let mut external = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--ignored", "--exact", "external_lock_child", "--nocapture"])
        .env("EXCALIBUR_TEST_EXTERNAL_LOCK", &path)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let mut output = std::io::BufReader::new(external.stdout.take().unwrap());
    loop {
        let mut line = String::new();
        assert!(
            output.read_line(&mut line).unwrap() > 0,
            "lock helper exited before readiness"
        );
        if line.trim() == "EXTERNAL_LOCK_READY" {
            break;
        }
    }
    let result = db
        .write(Priority::Normal, |conn| {
            conn.execute("INSERT INTO test_rows VALUES(1,0,'no partial effect')", [])?;
            Ok(())
        })
        .await;
    external
        .stdin
        .take()
        .unwrap()
        .write_all(b"release\n")
        .unwrap();
    assert!(external.wait().unwrap().success());
    assert!(matches!(result, Err(Error::Busy)));
    assert_eq!(db.stats().busy_retries, 2);
    assert_eq!(
        db.read(ReadLane::Interactive, |conn| Ok(conn.query_row(
            "SELECT count(*) FROM test_rows",
            [],
            |row| row.get::<_, i64>(0)
        )?))
        .await
        .unwrap(),
        0
    );
    db.shutdown().await.unwrap();
}

#[tokio::test]
async fn database_kinds_have_distinct_durability_and_mount_pragmas() {
    let dir = tempfile::tempdir().unwrap();
    for (index, kind, synchronous, network) in [
        (0, DatabaseKind::Main, 2, false),
        (1, DatabaseKind::Telemetry, 1, false),
        (2, DatabaseKind::CodeGraph, 1, true),
    ] {
        let config = StorageConfig {
            network_filesystem: network,
            ..Default::default()
        };
        let db = Database::open(dir.path().join(format!("{index}.db")), kind, config).unwrap();
        let (mode, mmap, cache) = db
            .read(ReadLane::Interactive, |conn| {
                Ok((
                    conn.query_row("PRAGMA journal_mode", [], |row| row.get::<_, String>(0))?,
                    conn.query_row("PRAGMA mmap_size", [], |row| row.get::<_, i64>(0))?,
                    conn.query_row("PRAGMA cache_size", [], |row| row.get::<_, i64>(0))?,
                ))
            })
            .await
            .unwrap();
        let (sync, writer_cache) = db
            .write(Priority::Interactive, |conn| {
                Ok((
                    conn.query_row("PRAGMA synchronous", [], |row| row.get::<_, i64>(0))?,
                    conn.query_row("PRAGMA cache_size", [], |row| row.get::<_, i64>(0))?,
                ))
            })
            .await
            .unwrap();
        assert_eq!(mode, if network { "delete" } else { "wal" });
        assert_eq!(sync, synchronous);
        assert_eq!(writer_cache, -8192);
        assert_eq!(mmap, 0);
        assert_eq!(cache, -2048);
        db.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn independent_files_do_not_leak_rows_or_writer_ownership() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("alice/excalibur.db");
    let alice = Database::open(&path, DatabaseKind::Main, StorageConfig::default()).unwrap();
    let bob = Database::open(
        dir.path().join("bob/excalibur.db"),
        DatabaseKind::Main,
        StorageConfig::default(),
    )
    .unwrap();
    assert!(matches!(
        Database::open(&path, DatabaseKind::Main, StorageConfig::default()),
        Err(Error::AlreadyOpen(_))
    ));
    table(&alice).await;
    table(&bob).await;
    alice
        .write(Priority::Interactive, |conn| {
            conn.execute("INSERT INTO test_rows VALUES(1,0,'alice')", [])?;
            Ok(())
        })
        .await
        .unwrap();
    assert_eq!(
        bob.read(ReadLane::Interactive, |conn| Ok(conn.query_row(
            "SELECT count(*) FROM test_rows",
            [],
            |row| row.get::<_, i64>(0)
        )?))
        .await
        .unwrap(),
        0
    );
    alice.shutdown().await.unwrap();
    bob.shutdown().await.unwrap();
}

#[tokio::test]
async fn disk_full_pauses_background_without_disabling_interactive_lane() {
    let dir = tempfile::tempdir().unwrap();
    let db = Database::open(
        dir.path().join("main.db"),
        DatabaseKind::Main,
        StorageConfig::default(),
    )
    .unwrap();
    table(&db).await;
    db.write(Priority::Interactive, |conn| {
        let current: i64 = conn.query_row("PRAGMA page_count", [], |row| row.get(0))?;
        let limit: i64 =
            conn.query_row(&format!("PRAGMA max_page_count={current}"), [], |row| {
                row.get(0)
            })?;
        assert_eq!(limit, current);
        Ok(())
    })
    .await
    .unwrap();
    let result = db
        .write(Priority::Background, |conn| {
            conn.execute("INSERT INTO test_rows VALUES(1,3,zeroblob(2097152))", [])?;
            Ok(())
        })
        .await;
    assert!(matches!(result, Err(Error::Full)), "{result:?}");
    assert!(matches!(
        db.write(Priority::Background, |_| Ok(())).await,
        Err(Error::BackgroundPaused)
    ));
    let interactive = db
        .write(Priority::Interactive, |conn| {
            Ok(conn.query_row("SELECT count(*) FROM test_rows", [], |row| {
                row.get::<_, i64>(0)
            })?)
        })
        .await
        .unwrap();
    assert_eq!(interactive, 0);
    db.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn interactive_command_overtakes_queued_background_work() {
    use std::{future::Future, task::Poll};
    let dir = tempfile::tempdir().unwrap();
    let db = Database::open(
        dir.path().join("main.db"),
        DatabaseKind::Main,
        StorageConfig::default(),
    )
    .unwrap();
    db.write(Priority::Interactive, |conn| {
        conn.execute_batch("CREATE TABLE ordered(seq INTEGER PRIMARY KEY, label TEXT)")?;
        Ok(())
    })
    .await
    .unwrap();
    let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let held_db = db.clone();
    let held = tokio::spawn(async move {
        held_db
            .write(Priority::Normal, move |conn| {
                conn.execute("INSERT INTO ordered(label) VALUES('held')", [])?;
                entered_tx.send(()).unwrap();
                release_rx.recv().unwrap();
                Ok(())
            })
            .await
    });
    entered_rx.await.unwrap();
    // A first poll enqueues each command before it blocks on its response channel.
    // The writer is held above, making priority ordering independent of timing.
    let mut queued = Vec::new();
    for index in 0..8 {
        let mut write = Box::pin(db.write(Priority::Background, move |conn| {
            conn.execute(
                "INSERT INTO ordered(label) VALUES(?1)",
                [format!("background-{index}")],
            )?;
            Ok(())
        }));
        std::future::poll_fn(|cx| {
            assert!(write.as_mut().poll(cx).is_pending());
            Poll::Ready(())
        })
        .await;
        queued.push(write);
    }
    let mut interactive = Box::pin(db.write(Priority::Interactive, |conn| {
        conn.execute("INSERT INTO ordered(label) VALUES('interactive')", [])?;
        Ok(())
    }));
    std::future::poll_fn(|cx| {
        assert!(interactive.as_mut().poll(cx).is_pending());
        Poll::Ready(())
    })
    .await;
    release_tx.send(()).unwrap();
    held.await.unwrap().unwrap();
    interactive.await.unwrap();
    for write in queued {
        write.await.unwrap();
    }
    let labels = db
        .read(ReadLane::Interactive, |conn| {
            let mut statement = conn.prepare("SELECT label FROM ordered ORDER BY seq")?;
            Ok(statement
                .query_map([], |row| row.get::<_, String>(0))?
                .collect::<Result<Vec<_>, _>>()?)
        })
        .await
        .unwrap();
    assert_eq!(&labels[..2], &["held", "interactive"]);
    db.shutdown().await.unwrap();
}

/// Test-process helper; only its parent integration test supplies the path.
#[test]
#[ignore]
fn external_lock_child() {
    use std::io::{Read, Write};
    let path = std::env::var_os("EXCALIBUR_TEST_EXTERNAL_LOCK")
        .expect("parent supplies synthetic database path");
    let conn = excalibur_store::rusqlite::Connection::open(path).unwrap();
    conn.execute_batch("BEGIN IMMEDIATE").unwrap();
    println!("EXTERNAL_LOCK_READY");
    std::io::stdout().flush().unwrap();
    let mut release = [0u8; 1];
    std::io::stdin().read_exact(&mut release).unwrap();
    conn.execute_batch("ROLLBACK").unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn long_reader_triggers_real_wal_limit_and_checkpoint_events() {
    use excalibur_store::StoreEvent;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("main.db");
    let db = Database::open(&path, DatabaseKind::Main, StorageConfig::default()).unwrap();
    table(&db).await;
    let mut events = db.subscribe_events();
    let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let reader_db = db.clone();
    let reader = tokio::spawn(async move {
        reader_db
            .read(ReadLane::Background, move |conn| {
                conn.execute_batch("BEGIN")?;
                let count: i64 =
                    conn.query_row("SELECT count(*) FROM test_rows", [], |row| row.get(0))?;
                assert_eq!(count, 0);
                entered_tx.send(()).unwrap();
                release_rx.recv().unwrap();
                conn.execute_batch("COMMIT")?;
                Ok(())
            })
            .await
    });
    entered_rx.await.unwrap();
    // 30 x 9 MiB crosses the specified default WAL alert threshold with synthetic data.
    for index in 0..30 {
        db.write(Priority::Normal, move |conn| {
            conn.execute(
                "INSERT INTO test_rows VALUES(?1,2,zeroblob(9437184))",
                [index],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    }
    db.write(Priority::Interactive, |_| Ok(())).await.unwrap();
    let wal_size = std::fs::metadata(format!("{}-wal", path.display()))
        .unwrap()
        .len();
    assert!(wal_size > 256 * 1024 * 1024);
    let mut saw_large = false;
    let mut saw_passive = false;
    let event_result = tokio::time::timeout(Duration::from_secs(10), async {
        while !(saw_large && saw_passive) {
            match events.recv().await.unwrap() {
                StoreEvent::WalLarge(size) if size > 256 * 1024 * 1024 => saw_large = true,
                StoreEvent::Checkpoint("PASSIVE") => saw_passive = true,
                _ => {}
            }
        }
    })
    .await;
    release_tx.send(()).unwrap();
    reader.await.unwrap().unwrap();
    event_result.expect("WAL warning and idle passive checkpoint must be observable");
    db.maintenance(excalibur_store::unix_seconds())
        .await
        .unwrap();
    let mut saw_truncate = false;
    while let Ok(event) = events.try_recv() {
        if matches!(event, StoreEvent::Checkpoint("TRUNCATE")) {
            saw_truncate = true;
        }
    }
    assert!(saw_truncate);
    assert_eq!(
        std::fs::metadata(format!("{}-wal", path.display()))
            .unwrap()
            .len(),
        0
    );
    eprintln!(
        "long_reader_wal_bytes={wal_size} alert_threshold={} passive=true truncate=true",
        256 * 1024 * 1024
    );
    db.shutdown().await.unwrap();
}

#[tokio::test]
async fn writer_panic_rolls_back_and_requires_host_restart() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("main.db");
    let db = Database::open(&path, DatabaseKind::Main, StorageConfig::default()).unwrap();
    table(&db).await;
    let result: Result<(), Error> = db
        .write(Priority::Normal, |conn| {
            conn.execute("INSERT INTO test_rows VALUES(1,2,'unconfirmed')", [])?;
            panic!("synthetic writer interruption");
        })
        .await;
    assert!(matches!(result, Err(Error::WriterPanicked)));
    assert!(matches!(db.shutdown().await, Err(Error::WriterPanicked)));
    assert!(db.is_failed());
    assert!(matches!(
        db.write(Priority::Interactive, |_| Ok(())).await,
        Err(Error::WriterPanicked)
    ));
    drop(db);
    let restarted = Database::open(&path, DatabaseKind::Main, StorageConfig::default()).unwrap();
    assert_eq!(
        restarted
            .read(ReadLane::Interactive, |conn| Ok(conn.query_row(
                "SELECT count(*) FROM test_rows",
                [],
                |row| row.get::<_, i64>(0)
            )?))
            .await
            .unwrap(),
        0
    );
    restarted.shutdown().await.unwrap();
}

#[tokio::test]
async fn reader_panic_does_not_poison_or_leak_a_snapshot() {
    let dir = tempfile::tempdir().unwrap();
    let db = Database::open(
        dir.path().join("main.db"),
        DatabaseKind::Main,
        StorageConfig::default(),
    )
    .unwrap();
    for lane in [ReadLane::Interactive, ReadLane::Background] {
        let result: Result<(), Error> = db
            .read(lane, |conn| {
                conn.execute_batch("BEGIN")?;
                panic!("synthetic reader interruption");
            })
            .await;
        assert!(matches!(result, Err(Error::Invalid(_))));
        assert!(
            db.read(lane, |conn| Ok(conn.is_autocommit()))
                .await
                .unwrap()
        );
    }
    db.shutdown().await.unwrap();
}
