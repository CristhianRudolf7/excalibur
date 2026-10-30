use excalibur_daemon::{DaemonHost, HostOptions, RuntimeSet};
use excalibur_models::{ModelError, ModelGateway};
use excalibur_store::{Database, DatabaseKind};
use excalibur_tools::terminal::{SandboxConfig, TerminalConfig, TerminalManager};
use excalibur_types::runtime::*;
use excalibur_vault::Redactor;
use serde_json::json;
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};
// Give this fixture a bounded real CPU allocation; all newly created runtime
// and tool threads inherit it. Eight compilers saturate that allocation without
// launching hundreds of jobs on a large shared test host.
#[cfg(target_os = "linux")]
struct CpuAllocation(libc::cpu_set_t);
#[cfg(target_os = "linux")]
impl CpuAllocation {
    fn new() -> Self {
        // SAFETY: fixed-sized kernel CPU masks; pid zero targets this test thread.
        unsafe {
            let mut original: libc::cpu_set_t = std::mem::zeroed();
            assert_eq!(
                libc::sched_getaffinity(0, std::mem::size_of_val(&original), &mut original),
                0
            );
            let mut selected: libc::cpu_set_t = std::mem::zeroed();
            let mut count = 0;
            for cpu in 0..libc::CPU_SETSIZE as usize {
                if libc::CPU_ISSET(cpu, &original) {
                    libc::CPU_SET(cpu, &mut selected);
                    count += 1;
                    if count == 4 {
                        break;
                    }
                }
            }
            assert!(count > 0);
            assert_eq!(
                libc::sched_setaffinity(0, std::mem::size_of_val(&selected), &selected),
                0
            );
            eprintln!("avail fixture allocated_cpu_count={count}");
            Self(original)
        }
    }
}
#[cfg(target_os = "linux")]
impl Drop for CpuAllocation {
    fn drop(&mut self) {
        unsafe {
            libc::sched_setaffinity(0, std::mem::size_of_val(&self.0), &self.0);
        }
    }
}
struct LoadStop {
    stop: Arc<AtomicBool>,
    path: std::path::PathBuf,
}
impl Drop for LoadStop {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        let _ = std::fs::write(&self.path, "done");
    }
}
struct Unavailable(AtomicUsize);
impl ModelGateway for Unavailable {
    fn complete<'a>(&'a self, _: ModelRequest) -> BoxFuture<'a, Result<ModelResponse, ModelError>> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Err(ModelError::Provider) })
    }
}
#[test]
fn durable_receipts_and_fast_status_remain_available_with_zero_four_and_eight_busy_workers() {
    #[cfg(target_os = "linux")]
    let _allocation = CpuAllocation::new();
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
    let model = Arc::new(Unavailable(AtomicUsize::new(0)));
    let host = runtimes
        .block_on(DaemonHost::open(
            db,
            HostOptions::project(root.clone()),
            model.clone(),
            None,
            "synthetic-availability-test-token-000000",
            Redactor::default(),
            runtimes.work(),
        ))
        .unwrap();
    let (address, server) = runtimes.block_on(async {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let router = host.gateway.clone().router();
        let server = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        (address, server)
    });
    let client = reqwest::Client::new();
    let inventory:serde_json::Value=runtimes.block_on(async {client.post(format!("http://{address}/rpc")).bearer_auth("synthetic-availability-test-token-000000").json(&json!({"jsonrpc":"2.0","id":0,"method":"files.list","params":{"project":"default"}})).send().await.unwrap().json().await.unwrap()});
    assert!(
        inventory["result"]["entries"]
            .as_array()
            .unwrap()
            .iter()
            .all(|entry| !matches!(
                entry["name"].as_str(),
                Some(".config" | ".agents" | ".codex" | ".github" | ".gitlab" | ".circleci")
            )),
        "host protective mountpoints leaked into project inventory: {inventory}"
    );
    // Real native compilation runs inside the same mandatory sandbox adapter used
    // by tools, while all work executor threads are also occupied.
    let source = (0..1500)
        .map(|i| format!("long f{i}(long x) {{ return (x*{i}+17) ^ (x>>3); }}\n"))
        .collect::<String>();
    std::fs::write(root.join("load.c"), source).unwrap();
    for workers in [0, 4, 8] {
        let manager = TerminalManager::new(TerminalConfig {
            spool_dir: temp.path().join(format!("spool-{workers}")),
            sandbox: Some(SandboxConfig::project(&root)),
            ..Default::default()
        })
        .unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let _cleanup = LoadStop {
            stop: stop.clone(),
            path: root.join(format!("stop{workers}")),
        };
        for index in 0..workers {
            let command = format!(
                "cc -c load.c -o load{index}.o && printf 'compiled\\n'; while [ ! -f stop{workers} ]; do cc -c load.c -o load{index}.o || exit 1; done"
            );
            let started = runtimes.block_on(manager.exec(
                &format!("compiler-{index}"),
                json!({"cmd":command,"cwd":root,"yield_ms":250}),
            ));
            assert!(started.success, "{started:?}");
            let ready=runtimes.block_on(manager.io(&format!("compiler-{index}"),json!({"action":"wait_for","session_id":started.output["session_id"],"pattern":"compiled","timeout_ms":10000})));
            assert_eq!(ready.output["matched"], true, "{ready:?}");
        }
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
            let response:serde_json::Value=runtimes.block_on(async {
                client.post(format!("http://{address}/rpc"))
                    .bearer_auth("synthetic-availability-test-token-000000")
                    .json(&json!({"jsonrpc":"2.0","id":i,"method":"conversation.send","params":{"conversation":"availability","client_id":"local","client_msg_id":format!("{workers}:{i}"),"text":"/status"}}))
                    .send().await.unwrap().error_for_status().unwrap().json().await.unwrap()
            });
            assert!(response.get("result").is_some(), "{response}");
            measurements.push(start.elapsed().as_micros());
        }
        std::fs::write(root.join(format!("stop{workers}")), "done").unwrap();
        runtimes.block_on(manager.shutdown());
        stop.store(true, Ordering::Relaxed);
        for handle in handles {
            runtimes.block_on(handle).unwrap();
        }
        measurements.sort();
        let p50 = measurements[9];
        let p95 = measurements[18];
        eprintln!(
            "avail real_http workers={workers} native_compilers={workers} receipt+fast-status p50_us={p50} p95_us={p95}"
        );
        assert!(p95 < 2_000_000, "fast path starved under {workers} workers");
    }
    assert_eq!(
        model.0.load(Ordering::SeqCst),
        0,
        "status must never consume inference capacity"
    );
    let replies = runtimes
        .block_on(host.master.history("availability", None, 512))
        .unwrap();
    assert_eq!(
        replies
            .iter()
            .filter(|message| message.role == "assistant")
            .count(),
        60
    );
    server.abort();
    runtimes.block_on(host.close()).unwrap();
    runtimes.shutdown(Duration::from_secs(3));
}
