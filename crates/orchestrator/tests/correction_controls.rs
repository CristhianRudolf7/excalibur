mod support;
use excalibur_engine::{Status, WaitReason};
use excalibur_orchestrator::*;
use support::*;
#[tokio::test]
async fn repeated_pause_and_interrupt_preserve_cascade_and_independent_child_ownership() {
    for (first, second) in [
        (Control::Pause, Control::Pause),
        (Control::Pause, Control::Interrupt),
        (Control::Interrupt, Control::Pause),
    ] {
        let f = fixture().await;
        let p = f
            .host
            .spawn(&f.master.id, delegation("p", "general", 300))
            .await
            .unwrap();
        let c = f
            .host
            .spawn(&p.id, delegation("c", "general", 30))
            .await
            .unwrap();
        let independent = f
            .host
            .spawn(&p.id, delegation("independent", "general", 30))
            .await
            .unwrap();
        f.host
            .control(&independent.id, Control::Pause)
            .await
            .unwrap();
        f.host
            .control_keyed("first-pause", &p.id, first)
            .await
            .unwrap();
        f.host
            .control_keyed("second-pause", &p.id, second)
            .await
            .unwrap();
        let resumed = f.host.control(&p.id, Control::Resume).await.unwrap();
        assert!(resumed.affected.contains(&p.id) && resumed.affected.contains(&c.id));
        assert!(!resumed.affected.contains(&independent.id));
        assert_eq!(
            f.host.status(&independent.id).await.unwrap().state,
            "paused"
        );
    }
}
#[tokio::test]
async fn explicit_resume_does_not_bypass_resource_hysteresis_for_parent_or_children() {
    let f = fixture().await;
    let p = f
        .host
        .spawn(&f.master.id, delegation("p", "general", 300))
        .await
        .unwrap();
    let c = f
        .host
        .spawn(&p.id, delegation("c", "general", 30))
        .await
        .unwrap();
    for a in [&p, &c] {
        let run = a.run_id.as_ref().unwrap();
        f.host
            .engine()
            .journal()
            .set_state_if(
                run,
                &["ready"],
                "running",
                None,
                &format!("running:{}", a.id),
            )
            .await
            .unwrap();
    }
    let threshold = f.host.limits().ram_bytes;
    f.host.resource_sample(threshold + 1, 10).await.unwrap();
    f.host.resource_sample(threshold + 1, 11).await.unwrap();
    f.host.control(&p.id, Control::Resume).await.unwrap();
    assert_eq!(f.host.status(&p.id).await.unwrap().state, "paused");
    assert_eq!(f.host.status(&c.id).await.unwrap().state, "paused");
    f.host.resource_sample(1, 20).await.unwrap();
    assert!(
        f.host
            .resource_sample(1, 79)
            .await
            .unwrap()
            .resume
            .is_empty()
    );
    assert_eq!(f.host.resource_sample(1, 80).await.unwrap().resume.len(), 1);
}
#[tokio::test]
async fn cancelling_queued_waiting_or_paused_child_sends_completion_and_settles_once() {
    for mode in ["ready", "waiting", "paused"] {
        let f = fixture().await;
        let p = f
            .host
            .spawn(&f.master.id, delegation("p", "general", 300))
            .await
            .unwrap();
        let c = f
            .host
            .spawn(&p.id, delegation("c", "general", 30))
            .await
            .unwrap();
        let parent_run = p.run_id.as_ref().unwrap();
        let run = c.run_id.as_ref().unwrap();
        f.host
            .engine()
            .journal()
            .set_state_if(parent_run, &["ready"], "running", None, "parent-running")
            .await
            .unwrap();
        f.host
            .engine()
            .wait(parent_run, WaitReason::Children)
            .await
            .unwrap();
        if mode == "waiting" {
            f.host
                .engine()
                .journal()
                .set_state_if(run, &["ready"], "running", None, "child-running")
                .await
                .unwrap();
            f.host.engine().wait(run, WaitReason::User).await.unwrap();
        } else if mode == "paused" {
            f.host.engine().pause(run).await.unwrap();
        }
        f.host
            .control_keyed("cancel-child", &c.id, Control::Cancel { confirmed: true })
            .await
            .unwrap();
        assert_eq!(
            f.host.engine().load(run).await.unwrap().status,
            Status::Cancelled
        );
        let parent = f.host.engine().load(parent_run).await.unwrap();
        assert!(parent.child_allocations[&c.id].settled.is_some());
        let mailbox = f.host.mailbox(&p.id).await.unwrap();
        assert_eq!(
            mailbox
                .iter()
                .filter(|m| m.kind == "completion" && m.sender == c.id)
                .count(),
            1
        );
        f.host.deliver_mailbox(&p.id).await.unwrap();
        assert_eq!(
            f.host.engine().load(parent_run).await.unwrap().status,
            Status::Running
        );
        f.host
            .control_keyed("cancel-child", &c.id, Control::Cancel { confirmed: true })
            .await
            .unwrap();
        f.host
            .control(&c.id, Control::Cancel { confirmed: true })
            .await
            .unwrap();
        assert_eq!(
            f.host
                .engine()
                .journal()
                .replay(parent_run, 0)
                .await
                .unwrap()
                .iter()
                .filter(|e| e.kind == "child_budget_settled")
                .count(),
            1
        );
        assert_eq!(
            f.host
                .notifications("conversation:test")
                .await
                .unwrap()
                .iter()
                .filter(|n| n.agent_id == c.id)
                .count(),
            1
        );
    }
}

#[tokio::test]
async fn writer_failure_still_aborts_and_joins_in_memory_workers() {
    use excalibur_models::{ModelError, ModelGateway};
    use excalibur_types::runtime::*;
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };
    struct PendingModel {
        entered: tokio::sync::Notify,
        dropped: Arc<AtomicBool>,
    }
    struct DropSignal(Arc<AtomicBool>);
    impl Drop for DropSignal {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }
    impl ModelGateway for PendingModel {
        fn complete(
            &self,
            _: ModelRequest,
        ) -> BoxFuture<'_, std::result::Result<ModelResponse, ModelError>> {
            Box::pin(async move {
                let _drop = DropSignal(self.dropped.clone());
                self.entered.notify_one();
                std::future::pending().await
            })
        }
    }
    let f = fixture().await;
    let dropped = Arc::new(AtomicBool::new(false));
    let model = Arc::new(PendingModel {
        entered: tokio::sync::Notify::new(),
        dropped: dropped.clone(),
    });
    let engine = Arc::new(excalibur_engine::Engine::new(
        f.host.engine().journal().clone(),
        model.clone(),
        no_tools(),
    ));
    let host = Orchestrator::open(f.db.clone(), engine, Limits::default())
        .await
        .unwrap()
        .with_authority(authority());
    let p = host
        .spawn(&f.master.id, delegation("p", "general", 300))
        .await
        .unwrap();
    assert_eq!(host.tick().await.unwrap(), vec![p.id]);
    tokio::time::timeout(std::time::Duration::from_secs(10), model.entered.notified())
        .await
        .unwrap();
    f.db.shutdown().await.unwrap();
    assert!(
        tokio::time::timeout(std::time::Duration::from_secs(5), host.shutdown())
            .await
            .unwrap()
            .is_err()
    );
    assert!(dropped.load(Ordering::SeqCst));
}
