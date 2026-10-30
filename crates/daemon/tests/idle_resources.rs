use excalibur_daemon::{DaemonHost, HostOptions, RuntimeSet};
use excalibur_models::{ModelError, ModelGateway};
use excalibur_store::{Database, DatabaseKind};
use excalibur_types::runtime::*;
use excalibur_vault::Redactor;
use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
struct Resident {
    bytes: Mutex<Vec<u8>>,
    busy: AtomicBool,
}
impl ModelGateway for Resident {
    fn complete(&self, _: ModelRequest) -> BoxFuture<'_, Result<ModelResponse, ModelError>> {
        Box::pin(async { Err(ModelError::Provider) })
    }
    fn unload_idle(&self) -> BoxFuture<'_, Result<usize, ModelError>> {
        Box::pin(async move {
            if self.busy.load(Ordering::SeqCst) {
                return Ok(0);
            }
            let mut bytes = self.bytes.lock().unwrap();
            let resident = usize::from(!bytes.is_empty());
            bytes.clear();
            bytes.shrink_to_fit();
            Ok(resident)
        })
    }
}
#[test]
fn host_memory_pressure_releases_only_idle_primary_and_fallback_resources() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("project");
    std::fs::create_dir(&root).unwrap();
    let db = Database::open(
        temp.path().join("data/main.db"),
        DatabaseKind::Main,
        Default::default(),
    )
    .unwrap();
    let primary = Arc::new(Resident {
        bytes: Mutex::new(vec![0; 1024 * 1024]),
        busy: AtomicBool::new(true),
    });
    let fallback = Arc::new(Resident {
        bytes: Mutex::new(vec![0; 1024 * 1024]),
        busy: AtomicBool::new(false),
    });
    let mut options = HostOptions::project(root);
    options.limits.ram_bytes = 1024;
    let rt = RuntimeSet::new(2, 1).unwrap();
    let host = rt
        .block_on(DaemonHost::open(
            db,
            options,
            primary.clone(),
            Some(fallback.clone()),
            "synthetic-idle-resource-host-token-000",
            Redactor::default(),
            rt.work(),
        ))
        .unwrap();
    rt.block_on(async {
        host.orchestrator.resource_sample(2048, 100).await.unwrap();
        assert_eq!(primary.bytes.lock().unwrap().len(), 1024 * 1024);
        assert!(fallback.bytes.lock().unwrap().is_empty());
        primary.busy.store(false, Ordering::SeqCst);
        host.orchestrator.resource_sample(2048, 101).await.unwrap();
        assert!(primary.bytes.lock().unwrap().is_empty());
    });
    rt.block_on(host.close()).unwrap();
    rt.shutdown(Duration::from_secs(3));
}
