#![cfg(target_os = "linux")]
use excalibur_daemon::{DaemonHost, HostOptions, RuntimeSet};
use excalibur_models::{ModelError, ModelGateway};
use excalibur_store::{Database, DatabaseKind};
use excalibur_types::runtime::*;
use excalibur_vault::Redactor;
use std::{path::PathBuf, sync::Arc, time::Duration};
struct NoModels;
impl ModelGateway for NoModels {
    fn complete(&self, _: ModelRequest) -> BoxFuture<'_, Result<ModelResponse, ModelError>> {
        Box::pin(std::future::pending())
    }
}
#[test]
fn ordinary_directories_cannot_impersonate_delegated_cgroups() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("project");
    std::fs::create_dir(&root).unwrap();
    let db = Database::open(
        temp.path().join("data/main.db"),
        DatabaseKind::Main,
        Default::default(),
    )
    .unwrap();
    let runtimes = RuntimeSet::new(1, 1).unwrap();
    let mut options = HostOptions::project(root);
    options.cgroup_root = Some(temp.path().to_owned());
    let error = runtimes
        .block_on(DaemonHost::open(
            db.clone(),
            options,
            Arc::new(NoModels),
            None,
            "synthetic-cgroup-provisioning-token",
            Redactor::default(),
            runtimes.work(),
        ))
        .err()
        .unwrap();
    assert!(error.contains("real cgroup v2"), "{error}");
    runtimes.block_on(db.shutdown()).unwrap();
    runtimes.shutdown(Duration::from_secs(2));
}
#[test]
fn an_explicit_real_cpu_delegation_provisions_distinct_groups_for_each_execution() {
    let Some(root) = std::env::var_os("EXCALIBUR_TEST_CGROUP").map(PathBuf::from) else {
        eprintln!("per-run cgroup verification unavailable: no explicit host test delegation");
        return;
    };
    let temp = tempfile::tempdir().unwrap();
    let project = temp.path().join("project");
    std::fs::create_dir(&project).unwrap();
    let db = Database::open(
        temp.path().join("data/main.db"),
        DatabaseKind::Main,
        Default::default(),
    )
    .unwrap();
    let runtimes = RuntimeSet::new(1, 1).unwrap();
    let mut options = HostOptions::project(project);
    options.cgroup_root = Some(root.clone());
    let host = runtimes
        .block_on(DaemonHost::open(
            db,
            options,
            Arc::new(NoModels),
            None,
            "synthetic-cgroup-provisioning-token",
            Redactor::default(),
            runtimes.work(),
        ))
        .unwrap();
    let before = std::fs::read_to_string("/proc/self/cgroup").unwrap();
    let weight: u32 = std::fs::read_to_string(root.join("cpu.weight"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    let groups = runtimes.block_on(async {
        let parent = host
            .orchestrator
            .list_agents()
            .await
            .unwrap()
            .into_iter()
            .find(|agent| agent.depth == 0)
            .unwrap();
        let mut groups = Vec::new();
        for index in 0..2 {
            let mut token = parent.token.clone();
            token.budget = 100;
            let agent = host
                .orchestrator
                .spawn(
                    &parent.id,
                    excalibur_orchestrator::Delegation {
                        operation: format!("cgroup-{index}"),
                        name: format!("worker-{index}"),
                        profile: "coordinator".into(),
                        scope: "project:default".into(),
                        task: format!("cgroup-task-{index}"),
                        origin_conversation: "cgroup-fixture".into(),
                        goal: "Provision only".into(),
                        deliverable: "No effect".into(),
                        inputs: vec![],
                        pinned_instructions: vec![],
                        suggested_skills: vec![],
                        fork_context: false,
                        token,
                        spec: excalibur_engine::RunSpec {
                            context: excalibur_context::ContextInput::new(
                                "Fixture",
                                "Provision only",
                                "fixed",
                            ),
                            budget: excalibur_engine::Budget {
                                max_wall_seconds: 300,
                                ..Default::default()
                            },
                            criteria: vec![excalibur_engine::Criterion::FinalResponse {
                                schema: None,
                            }],
                        },
                        priority: 1,
                    },
                )
                .await
                .unwrap();
            let run = agent.run_id.unwrap();
            let name = run
                .as_bytes()
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>();
            let group = root.join(format!("excalibur-{name}"));
            groups.push(group);
        }
        host.orchestrator.tick().await.unwrap();
        for group in &groups {
            let actual: u32 = std::fs::read_to_string(group.join("cpu.weight"))
                .unwrap()
                .trim()
                .parse()
                .unwrap();
            assert_eq!(actual, 10.min(weight - 1));
        }
        groups
    });
    assert_ne!(groups[0], groups[1]);
    assert_eq!(
        std::fs::read_to_string("/proc/self/cgroup").unwrap(),
        before
    );
    runtimes.block_on(host.close()).unwrap();
    for group in groups {
        std::fs::remove_dir(group).unwrap();
    }
    runtimes.shutdown(Duration::from_secs(2));
}
