mod support;
use excalibur_orchestrator::*;
use excalibur_types::runtime::BoxFuture;
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, Mutex},
};
use support::*;
struct Authority {
    events: Mutex<Vec<String>>,
    live: Mutex<BTreeSet<String>>,
    capacity: Mutex<AdmissionCapacity>,
}
impl Authority {
    fn new() -> Self {
        Self {
            events: Mutex::new(vec![]),
            live: Mutex::new(BTreeSet::new()),
            capacity: Mutex::new(AdmissionCapacity::default()),
        }
    }
}
impl RunAuthority for Authority {
    fn bind<'a>(&'a self, _: &'a Agent) -> BoxFuture<'a, std::result::Result<(), String>> {
        Box::pin(async { Ok(()) })
    }
    fn admission_capacity(&self) -> AdmissionCapacity {
        *self.capacity.lock().unwrap()
    }
    fn unload_idle(&self) -> BoxFuture<'_, std::result::Result<(), String>> {
        Box::pin(async {
            self.events.lock().unwrap().push("unload".into());
            Ok(())
        })
    }
    fn kill_largest_tool<'a>(
        &'a self,
        run: &'a str,
    ) -> BoxFuture<'a, std::result::Result<bool, String>> {
        Box::pin(async move {
            self.events.lock().unwrap().push(format!("kill:{run}"));
            Ok(self.live.lock().unwrap().remove(run))
        })
    }
}
#[tokio::test]
async fn sustained_memory_pressure_unloads_then_pauses_one_low_priority_run_and_finds_live_victim()
{
    let f = fixture().await;
    let authority = Arc::new(Authority::new());
    let host = f.host.clone().with_authority(authority.clone());
    let mut agents = vec![];
    for priority in 1..=3 {
        let mut d = delegation(&format!("p{priority}"), "general", 300);
        d.profile = "coordinator".into();
        d.priority = priority;
        let a = host.spawn(&f.master.id, d).await.unwrap();
        host.engine()
            .journal()
            .set_state_if(
                a.run_id.as_ref().unwrap(),
                &["ready"],
                "running",
                None,
                &format!("fixture:{priority}"),
            )
            .await
            .unwrap();
        agents.push(a);
    }
    let threshold = host.limits().ram_bytes;
    for (sample, expected) in [2, 1, 0].into_iter().enumerate() {
        let action = host
            .resource_sample(threshold * 2, sample as i64)
            .await
            .unwrap();
        assert_eq!(action.pause, vec![agents[expected].id.clone()]);
        assert!(action.last_resort.is_none());
        assert!(!action.admit);
    }
    authority
        .live
        .lock()
        .unwrap()
        .insert(agents[1].run_id.clone().unwrap());
    let action = host.resource_sample(threshold * 2, 3).await.unwrap();
    assert_eq!(action.last_resort, Some(agents[1].id.clone()));
    let trace = authority.events.lock().unwrap().clone();
    assert_eq!(trace[0], "unload");
    assert_eq!(
        &trace[4..],
        &[
            format!("kill:{}", agents[2].run_id.as_ref().unwrap()),
            format!("kill:{}", agents[1].run_id.as_ref().unwrap())
        ]
    );
    host.control(&agents[0].id, Control::Pause).await.unwrap();
    host.resource_sample(1, 10).await.unwrap();
    assert!(host.resource_sample(1, 69).await.unwrap().resume.is_empty());
    assert_eq!(
        host.resource_sample(1, 70).await.unwrap().resume,
        vec![agents[1].id.clone()]
    );
    assert_eq!(
        host.resource_sample(1, 71).await.unwrap().resume,
        vec![agents[2].id.clone()]
    );
    assert_eq!(host.status(&agents[0].id).await.unwrap().state, "paused");
}
#[tokio::test]
async fn measured_process_and_provider_capacity_bound_admission_without_running_models() {
    let f = fixture().await;
    let authority = Arc::new(Authority::new());
    let limits = Limits {
        running: 8,
        process_limit: 3,
        estimated_processes_per_run: 2,
        provider_concurrency: 4,
        ..Default::default()
    };
    let work = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let host = Orchestrator::open(f.db.clone(), f.host.engine().clone(), limits)
        .await
        .unwrap()
        .with_authority(authority.clone())
        .with_runtime(work.handle().clone());
    let mut token = f.master.token.clone();
    token.processes = true;
    token.sandbox_profile = Some("project".into());
    let master = host
        .rebind_master_at_startup(&f.master.name, token.clone())
        .await
        .unwrap();
    for i in 0..3 {
        let mut d = delegation(&format!("job{i}"), "general", 100);
        d.profile = "coordinator".into();
        d.token = token.clone();
        host.spawn(&master.id, d).await.unwrap();
    }
    *authority.capacity.lock().unwrap() = AdmissionCapacity {
        process_limit: 3,
        processes_active: 2,
        provider_limit: 4,
        provider_active: 0,
    };
    assert!(host.tick().await.unwrap().is_empty());
    *authority.capacity.lock().unwrap() = AdmissionCapacity {
        process_limit: 3,
        processes_active: 1,
        provider_limit: 2,
        provider_active: 1,
    };
    assert_eq!(host.tick().await.unwrap().len(), 1);
    assert!(host.tick().await.unwrap().is_empty());
    work.shutdown_background();
}
#[tokio::test]
async fn weighted_project_round_robin_is_deterministic_and_fifth_child_stays_queued() {
    let f = fixture().await;
    let work = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let limits = Limits {
        running: 3,
        project_weights: BTreeMap::from([("project:a".into(), 2), ("project:b".into(), 1)]),
        ..Default::default()
    };
    let host = Orchestrator::open(f.db.clone(), f.host.engine().clone(), limits)
        .await
        .unwrap()
        .with_authority(authority())
        .with_runtime(work.handle().clone());
    let mut token = f.master.token.clone();
    token
        .data_scopes
        .extend(["project:a".into(), "project:b".into()]);
    let master = host
        .rebind_master_at_startup(&f.master.name, token.clone())
        .await
        .unwrap();
    let mut names = BTreeMap::new();
    for scope in ["project:a", "project:b"] {
        for i in 0..3 {
            let mut d = delegation(&format!("{scope}-{i}"), scope, 100);
            d.profile = "coordinator".into();
            d.token = token.clone();
            let a = host.spawn(&master.id, d).await.unwrap();
            names.insert(a.id, a.scope);
        }
    }
    let admitted = host.tick().await.unwrap();
    assert_eq!(
        admitted
            .iter()
            .map(|id| names[id].as_str())
            .collect::<Vec<_>>(),
        vec!["project:a", "project:b", "project:a"]
    );
    work.shutdown_background();
    let f = fixture().await;
    let work = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let host = Orchestrator::open(f.db.clone(), f.host.engine().clone(), Limits::default())
        .await
        .unwrap()
        .with_authority(authority())
        .with_runtime(work.handle().clone());
    let p = host
        .spawn(&f.master.id, delegation("p", "general", 300))
        .await
        .unwrap();
    let mut children = vec![];
    // Admission, not durable creation, enforces the four-child execution limit.
    for i in 0..5 {
        let mut d = delegation(&format!("child{i}"), "general", 20);
        d.spec.budget.max_steps = 10;
        d.spec.budget.max_tokens = 100;
        d.spec.budget.max_tool_calls = 1;
        children.push(host.spawn(&p.id, d).await.unwrap());
    }
    assert!(
        host.engine()
            .journal()
            .set_state_if(
                p.run_id.as_ref().unwrap(),
                &["ready"],
                "running",
                None,
                "parent-runs"
            )
            .await
            .unwrap()
    );
    host.engine()
        .wait(
            p.run_id.as_ref().unwrap(),
            excalibur_engine::WaitReason::Children,
        )
        .await
        .unwrap();
    let admitted = host.tick().await.unwrap();
    assert_eq!(admitted.len(), 4);
    let queued = children
        .iter()
        .find(|child| !admitted.contains(&child.id))
        .unwrap();
    let last = host.status(&queued.id).await.unwrap();
    assert_eq!(last.state, "ready");
    assert_eq!(last.queue_position, Some(1));
    work.shutdown_background();
}

#[tokio::test]
async fn four_admitted_waiting_children_keep_slots_until_terminal_while_fifth_is_durable_ready() {
    struct WaitOnBind(excalibur_journal::Journal);
    impl RunAuthority for WaitOnBind {
        fn bind<'a>(&'a self, a: &'a Agent) -> BoxFuture<'a, std::result::Result<(), String>> {
            Box::pin(async move {
                let run = a.run_id.as_ref().unwrap();
                self.0
                    .set_state_if(run, &["ready"], "running", None, "bind-running")
                    .await
                    .map_err(|e| e.to_string())?;
                self.0
                    .set_state_if(
                        run,
                        &["running"],
                        "waiting",
                        Some("user".into()),
                        "bind-waiting",
                    )
                    .await
                    .map_err(|e| e.to_string())?;
                Ok(())
            })
        }
    }
    let f = fixture().await;
    let host = Orchestrator::open(f.db.clone(), f.host.engine().clone(), Limits::default())
        .await
        .unwrap()
        .with_authority(Arc::new(WaitOnBind(f.host.engine().journal().clone())));
    let p = host
        .spawn(&f.master.id, delegation("p", "general", 300))
        .await
        .unwrap();
    let mut children = vec![];
    for i in 0..5 {
        let mut d = delegation(&format!("wait-child{i}"), "general", 20);
        d.spec.budget.max_steps = 10;
        d.spec.budget.max_tokens = 100;
        d.spec.budget.max_tool_calls = 1;
        children.push(host.spawn(&p.id, d).await.unwrap());
    }
    host.engine()
        .pause(p.run_id.as_ref().unwrap())
        .await
        .unwrap();
    let admitted = host.tick().await.unwrap();
    assert_eq!(admitted.len(), 4);
    let fifth = children.iter().find(|a| !admitted.contains(&a.id)).unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        let mut reaped = 0;
        while reaped < 4 {
            reaped += host.reap().await.unwrap();
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(host.tick().await.unwrap().is_empty());
    assert_eq!(host.status(&fifth.id).await.unwrap().state, "ready");
    host.control(&admitted[0], Control::Cancel { confirmed: true })
        .await
        .unwrap();
    assert_eq!(host.tick().await.unwrap(), vec![fifth.id.clone()]);
    for c in children {
        assert!(
            !host
                .engine()
                .journal()
                .replay(c.run_id.as_ref().unwrap(), 0)
                .await
                .unwrap()
                .iter()
                .any(|event| event.kind == "model_request")
        );
    }
}
