#![allow(dead_code)]
use excalibur_capabilities::{
    CapabilityTasks, ManualClock, PollPolicy, Submission,
    simulated::{self, CancelBehavior, Script, SimStep, SimulatedCapability},
};
use excalibur_store::{Database, DatabaseKind};
use serde_json::{Value, json};
use std::{path::Path, sync::Arc};

pub const SCOPE: &str = "principal:alice/project:a";
pub const T0: i64 = 1_000_000;

pub fn policy() -> PollPolicy {
    PollPolicy {
        default_interval_ms: 1_000,
        min_interval_ms: 100,
        max_interval_ms: 60_000,
        backoff_base_ms: 1_000,
        backoff_max_ms: 16_000,
        max_transient_failures: 4,
        in_flight_lease_ms: 30_000,
    }
}
pub struct Harness {
    pub db: Database,
    pub tasks: CapabilityTasks,
    pub sim: SimulatedCapability,
}
pub async fn open(root: &Path, clock: Arc<ManualClock>) -> Harness {
    open_with(root, clock, SCOPE, policy()).await
}
pub async fn open_with(
    root: &Path,
    clock: Arc<ManualClock>,
    scope: &str,
    policy: PollPolicy,
) -> Harness {
    let db = Database::open(
        root.join("excalibur.db"),
        DatabaseKind::Main,
        Default::default(),
    )
    .unwrap();
    let tasks = CapabilityTasks::open(db.clone(), scope.into(), policy, clock)
        .await
        .unwrap();
    tasks.register(simulated::manifest(4)).await.unwrap();
    let sim = SimulatedCapability::open(root.join("remote.json")).unwrap();
    Harness { db, tasks, sim }
}
impl Harness {
    /// Simulates a daemon restart: the harness store closes and reopens; the
    /// simulated remote keeps its own state file.
    pub async fn restart(self, root: &Path, clock: Arc<ManualClock>) -> Harness {
        let Harness { db, tasks, sim } = self;
        drop(tasks);
        drop(sim);
        db.shutdown().await.unwrap();
        drop(db);
        open(root, clock).await
    }
}
pub fn script(steps: Vec<SimStep>, cancel: CancelBehavior) -> Script {
    Script {
        steps,
        cancel,
        poll_interval_ms: Some(1_000),
    }
}
pub fn working(n: usize) -> Vec<SimStep> {
    vec![SimStep::Working; n]
}
pub fn completed(value: Value) -> SimStep {
    SimStep::Completed { result: value }
}
pub fn submission(key: &str, operation: &str) -> Submission {
    Submission {
        operation_key: key.into(),
        capability: "sim.capability".into(),
        capability_version: "1.0.0".into(),
        operation: operation.into(),
        input: json!({"query": key}),
        timeout_ms: None,
    }
}
