//! Subprocess SIGKILL at every purge boundary, then resume with the same keys.
mod common;
use common::*;
use excalibur_retention::PurgeBoundary;
use excalibur_store::ReadLane;
use std::{
    path::PathBuf,
    process::{Command, Stdio},
    sync::Arc,
    time::{Duration, Instant},
};

/// (boundary, subject index: 0 conversation, 1 document, 2 memory)
const BOUNDARIES: [(PurgeBoundary, usize); 9] = [
    (PurgeBoundary::Planned, 0),
    (PurgeBoundary::MainSweepBatch, 0),
    (PurgeBoundary::MainSwept, 1),
    (PurgeBoundary::JournalBlobsSwept, 1),
    (PurgeBoundary::TelemetrySwept, 2),
    (PurgeBoundary::BaseDeleteUncommitted, 0),
    (PurgeBoundary::BaseDeleted, 1),
    (PurgeBoundary::BlobsReleased, 2),
    (PurgeBoundary::Completed, 0),
];
fn park() -> ! {
    loop {
        std::thread::park();
    }
}

#[test]
fn purge_crash_worker() {
    let Ok(root) = std::env::var("EXCALIBUR_PURGE_CRASH_ROOT") else {
        return;
    };
    let root = PathBuf::from(root);
    let index: usize = std::env::var("EXCALIBUR_PURGE_CRASH_CASE")
        .unwrap()
        .parse()
        .unwrap();
    let (target, subject) = BOUNDARIES[index];
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async move {
        let world = World::open(&root).await;
        let planted: Planted =
            serde_json::from_slice(&std::fs::read(root.join("planted.json")).unwrap()).unwrap();
        for (i, request) in planted.requests(SCOPE).into_iter().enumerate() {
            let service = world.service().await;
            let service = if i == subject {
                let ready = root.join("ready");
                service.with_observer(Arc::new(move |boundary| {
                    if boundary == target {
                        std::fs::write(&ready, b"ready").unwrap();
                        park();
                    }
                }))
            } else {
                service
            };
            service.purge(request).await.unwrap();
        }
        std::fs::write(root.join("missed"), b"boundary never reached").unwrap();
    });
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sigkill_at_every_boundary_then_resume_converges() {
    for (case, (boundary, subject)) in BOUNDARIES.iter().enumerate() {
        let dir = tempfile::tempdir().unwrap();
        {
            let world = World::open(dir.path()).await;
            let planted = plant(&world, SCOPE, "k").await;
            std::fs::write(
                dir.path().join("planted.json"),
                serde_json::to_vec(&planted).unwrap(),
            )
            .unwrap();
            world.shutdown().await;
        }
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "purge_crash_worker", "--nocapture"])
            .env("EXCALIBUR_PURGE_CRASH_ROOT", dir.path())
            .env("EXCALIBUR_PURGE_CRASH_CASE", case.to_string())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let start = Instant::now();
        while !dir.path().join("ready").exists() {
            assert!(
                !dir.path().join("missed").exists(),
                "{boundary:?} was never reached"
            );
            if let Some(status) = child.try_wait().unwrap() {
                let mut stderr = String::new();
                use std::io::Read;
                child
                    .stderr
                    .take()
                    .unwrap()
                    .read_to_string(&mut stderr)
                    .unwrap();
                panic!("worker exited before {boundary:?}: {status}\n{stderr}");
            }
            if start.elapsed() > Duration::from_secs(60) {
                child.kill().unwrap();
                panic!("worker did not reach {boundary:?}");
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        // std::process::Child::kill sends SIGKILL on Unix.
        child.kill().unwrap();
        child.wait().unwrap();

        let world = World::open(dir.path()).await;
        let planted: Planted =
            serde_json::from_slice(&std::fs::read(dir.path().join("planted.json")).unwrap())
                .unwrap();
        let service = world.service().await;
        let resumed = service.resume_pending().await.unwrap();
        let expected_pending = usize::from(*boundary != PurgeBoundary::Completed);
        assert_eq!(resumed.len(), expected_pending, "{boundary:?}");
        let mut receipts = Vec::new();
        for request in planted.requests(SCOPE) {
            receipts.push(service.purge(request).await.unwrap());
        }
        for (request, receipt) in planted.requests(SCOPE).into_iter().zip(&receipts) {
            assert_eq!(
                &service.purge(request).await.unwrap(),
                receipt,
                "idempotent replay"
            );
        }
        let hits = scan(dir.path(), &MARKERS);
        assert!(
            hits.is_empty(),
            "{boundary:?} (subject {subject}): {hits:#?}"
        );
        assert!(fts_hits(dir.path(), &MARKERS).is_empty(), "{boundary:?}");
        assert!(!probe_present(dir.path()), "{boundary:?}");
        let state: (String, i64, i64, i64) = world
            .main
            .read(ReadLane::Interactive, |c| {
                Ok((
                    c.query_row("PRAGMA quick_check", [], |r| r.get(0))?,
                    c.query_row("SELECT count(*) FROM retention_tombstones", [], |r| {
                        r.get(0)
                    })?,
                    c.query_row(
                        "SELECT count(*) FROM retention_purges WHERE phase='completed'",
                        [],
                        |r| r.get(0),
                    )?,
                    c.query_row("SELECT count(*) FROM retention_purge_needles", [], |r| {
                        r.get::<_, i64>(0)
                    })?,
                ))
            })
            .await
            .unwrap();
        assert_eq!(state, ("ok".into(), 3, 3, 0), "{boundary:?}");
        let trigger_count: i64 = world
            .main
            .read(ReadLane::Interactive, |c| {
                Ok(c.query_row(
                    "SELECT count(*) FROM sqlite_schema WHERE type='trigger' AND name IN ('durable_journal_no_update','durable_journal_no_delete')",
                    [],
                    |r| r.get(0),
                )?)
            })
            .await
            .unwrap();
        assert_eq!(
            trigger_count, 2,
            "append-only guards restored after {boundary:?}"
        );
        world.journal.replay(&planted.run, 0).await.unwrap();
        world
            .telemetry
            .reconcile(&world.journal, &planted.run)
            .await
            .unwrap();
        println!(
            "kill -9 at {boundary:?} (subject {subject}): resumed={} converged, markers=0, tombstones=3, quick_check=ok",
            resumed.len()
        );
        world.shutdown().await;
    }
}
