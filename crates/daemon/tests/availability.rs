use excalibur_daemon::{DaemonHost, HostOptions, RuntimeSet};
use excalibur_master::Incoming;
use excalibur_models::{ModelError, ModelGateway};
use excalibur_store::{Database, DatabaseKind};
use excalibur_types::runtime::*;
use excalibur_vault::Redactor;
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};
struct Unavailable;
impl ModelGateway for Unavailable {
    fn complete<'a>(&'a self, _: ModelRequest) -> BoxFuture<'a, Result<ModelResponse, ModelError>> {
        Box::pin(async { Err(ModelError::Provider) })
    }
}
#[test]
fn durable_receipts_and_fast_status_remain_available_with_zero_four_and_eight_busy_workers() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("project");
    std::fs::create_dir(&root).unwrap();
    let db = Database::open(
        temp.path().join("data/main.db"),
        DatabaseKind::Main,
        Default::default(),
    )
    .unwrap();
    let runtimes = RuntimeSet::new(8, 1).unwrap();
    let host = runtimes
        .block_on(DaemonHost::open(
            db,
            HostOptions::project(root),
            Arc::new(Unavailable),
            None,
            "synthetic-availability-test-token-000000",
            Redactor::default(),
            runtimes.work(),
        ))
        .unwrap();
    for workers in [0, 4, 8] {
        let stop = Arc::new(AtomicBool::new(false));
        let entered = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut handles = Vec::new();
        for _ in 0..workers {
            let stop = stop.clone();
            let entered = entered.clone();
            handles.push(runtimes.spawn_work(async move {
                entered.fetch_add(1, Ordering::SeqCst);
                while !stop.load(Ordering::Relaxed) {
                    std::hint::spin_loop();
                }
            }));
        }
        let deadline = Instant::now() + Duration::from_secs(5);
        while entered.load(Ordering::SeqCst) < workers {
            assert!(Instant::now() < deadline);
            std::thread::yield_now();
        }
        let mut measurements = Vec::new();
        for i in 0..20 {
            let start = Instant::now();
            runtimes
                .block_on(host.master.receive(Incoming {
                    conversation: "availability".into(),
                    client_id: "local".into(),
                    client_msg_id: format!("{workers}:{i}"),
                    text: "/status".into(),
                    scope: None,
                    timezone: "UTC".into(),
                    channel: "test".into(),
                    reply_to: None,
                    attachments: vec![],
                }))
                .unwrap();
            measurements.push(start.elapsed().as_micros());
        }
        stop.store(true, Ordering::Relaxed);
        for handle in handles {
            runtimes.block_on(handle).unwrap();
        }
        measurements.sort();
        let p50 = measurements[9];
        let p95 = measurements[18];
        eprintln!("avail workers={workers} receipt+fast-status p50_us={p50} p95_us={p95}");
        assert!(p95 < 2_000_000, "fast path starved under {workers} workers");
    }
    runtimes.block_on(host.close()).unwrap();
    runtimes.shutdown(Duration::from_secs(3));
}
