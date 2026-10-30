mod support;
use excalibur_engine::Status;
use excalibur_orchestrator::*;
use support::*;

#[tokio::test]
async fn hierarchy_tokens_budgets_and_context_are_enforced_before_dispatch() {
    let f = fixture().await;
    let parent = f
        .host
        .spawn(&f.master.id, delegation("root", "general", 300))
        .await
        .unwrap();
    let mut child_spec = delegation("child", "general", 60);
    child_spec.pinned_instructions = vec!["Keep exact user bytes".into()];
    let child = f.host.spawn(&parent.id, child_spec).await.unwrap();
    assert_eq!(child.depth, 2);
    assert!(parent.token.contains(&child.token));
    assert!(
        f.host
            .spawn(&child.id, delegation("grandchild", "general", 10))
            .await
            .is_err()
    );
    let mut excessive = delegation("excessive", "project:other", 10);
    excessive.token.data_scopes.insert("project:other".into());
    assert!(f.host.spawn(&parent.id, excessive).await.is_err());
    let snapshot = f
        .host
        .engine()
        .load(child.run_id.as_ref().unwrap())
        .await
        .unwrap();
    assert!(snapshot.spec.context.layers[3].contains("Keep exact user bytes"));
    assert!(snapshot.spec.context.activity.is_empty());
    let repeated = f
        .host
        .spawn(&f.master.id, delegation("root", "general", 300))
        .await
        .unwrap();
    assert_eq!(parent.id, repeated.id);
    let mut changed = delegation("root", "general", 300);
    changed.goal = "Changed".into();
    assert!(f.host.spawn(&f.master.id, changed).await.is_err());
}
#[tokio::test]
async fn cascades_resume_only_owned_pauses_cancel_is_terminal_and_commands_replay() {
    let f = fixture().await;
    let p = f
        .host
        .spawn(&f.master.id, delegation("p", "general", 300))
        .await
        .unwrap();
    let c1 = f
        .host
        .spawn(&p.id, delegation("c1", "general", 60))
        .await
        .unwrap();
    let c2 = f
        .host
        .spawn(&p.id, delegation("c2", "general", 60))
        .await
        .unwrap();
    f.host
        .control_keyed("userchild", &c2.id, Control::Pause)
        .await
        .unwrap();
    let pause = f
        .host
        .control_keyed("pause-tree", &p.id, Control::Pause)
        .await
        .unwrap();
    assert_eq!(pause.affected.len(), 2);
    let replay = f
        .host
        .control_keyed("pause-tree", &p.id, Control::Pause)
        .await
        .unwrap();
    assert_eq!(pause.affected, replay.affected);
    assert!(
        f.host
            .control_keyed("pause-tree", &p.id, Control::Resume)
            .await
            .is_err()
    );
    let resume = f.host.control(&p.id, Control::Resume).await.unwrap();
    assert_eq!(resume.affected.len(), 2);
    assert_eq!(f.host.status(&c2.id).await.unwrap().state, "paused");
    assert_eq!(f.host.status(&c1.id).await.unwrap().state, "ready");
    assert!(
        f.host
            .control(&p.id, Control::Cancel { confirmed: false })
            .await
            .is_err()
    );
    let cancel = f
        .host
        .control(&p.id, Control::Cancel { confirmed: true })
        .await
        .unwrap();
    assert_eq!(cancel.affected.len(), 3);
    assert_eq!(cancel.affected.last(), Some(&p.id));
    f.host.control(&p.id, Control::Resume).await.unwrap();
    assert_eq!(f.host.status(&p.id).await.unwrap().state, "cancelled");
    assert_eq!(
        f.host
            .engine()
            .load(c1.run_id.as_ref().unwrap())
            .await
            .unwrap()
            .status,
        Status::Cancelled
    );
}
#[tokio::test]
async fn two_project_trees_waiting_parents_do_not_consume_worker_capacity() {
    let f = fixture().await;
    let p = f
        .host
        .spawn(&f.master.id, delegation("p", "general", 300))
        .await
        .unwrap();
    let q = f
        .host
        .spawn(&f.master.id, delegation("q", "general", 300))
        .await
        .unwrap();
    let c = f
        .host
        .spawn(&p.id, delegation("c", "general", 60))
        .await
        .unwrap();
    let d = f
        .host
        .spawn(&q.id, delegation("d", "general", 60))
        .await
        .unwrap();
    f.host.control(&p.id, Control::Pause).await.unwrap();
    f.host.control(&q.id, Control::Pause).await.unwrap();
    f.host.control(&c.id, Control::Resume).await.unwrap();
    f.host.control(&d.id, Control::Resume).await.unwrap();
    let started = f.host.tick().await.unwrap();
    assert_eq!(started.len(), 2);
    for _ in 0..100 {
        f.host.reap().await.unwrap();
        if f.host.status(&c.id).await.unwrap().state == "completed"
            && f.host.status(&d.id).await.unwrap().state == "completed"
        {
            break;
        }
        tokio::task::yield_now().await;
    }
    assert_eq!(f.host.status(&p.id).await.unwrap().state, "paused");
    f.host.shutdown().await.unwrap();
}
#[tokio::test]
async fn resource_hysteresis_never_resumes_user_pauses() {
    let f = fixture().await;
    let p = f
        .host
        .spawn(&f.master.id, delegation("p", "general", 300))
        .await
        .unwrap();
    f.host.control(&p.id, Control::Pause).await.unwrap();
    assert!(!f.host.resource_sample(u64::MAX, 100).await.unwrap().admit);
    assert!(!f.host.resource_sample(0, 101).await.unwrap().admit);
    assert!(f.host.resource_sample(0, 161).await.unwrap().admit);
    assert_eq!(f.host.status(&p.id).await.unwrap().state, "paused");
}

#[tokio::test]
async fn delegated_tasks_bind_real_contracts_runs_scopes_and_baseline_agents() {
    let f = fixture().await;
    let tasks = excalibur_tasks::TaskStore::open(f.db.clone())
        .await
        .unwrap();
    let host = f.host.clone().with_tasks(tasks.clone());
    let mut background = delegation("p", "general", 300);
    background.priority = 2;
    background.spec.context.layers[0].push_str("\n[excalibur-host-lane:3]\n");
    let p = host.spawn(&f.master.id, background).await.unwrap();
    let context = host
        .engine()
        .load(p.run_id.as_ref().unwrap())
        .await
        .unwrap()
        .spec
        .context;
    assert!(context.layers[0].contains("\n[excalibur-host-lane:2]\n"));
    assert!(!context.layers[0].contains("\n[excalibur-host-lane:3]\n"));
    let c = host
        .spawn(&p.id, delegation("c", "general", 60))
        .await
        .unwrap();
    let task = tasks
        .task_for_run(c.run_id.as_ref().unwrap())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(task.id, c.task);
    assert_eq!(task.scope_id, "general");
    assert_eq!(task.request_card["original"], "Synthetic independent work");
    let pid = p.id.clone();
    let cid = c.id.clone();
    let crun = c.run_id.clone().unwrap();
    let parent_run = p.run_id.clone().unwrap();
    let count =
        f.db.read(excalibur_store::ReadLane::Interactive, move |db| {
            assert_eq!(
                db.query_row("SELECT parent_id FROM agents WHERE id=?1", [cid], |r| {
                    r.get::<_, String>(0)
                })?,
                pid
            );
            assert_eq!(
                db.query_row(
                    "SELECT parent_run_id FROM task_run_links WHERE run_id=?1",
                    [crun],
                    |r| r.get::<_, String>(0)
                )?,
                parent_run
            );
            Ok(
                db.query_row("SELECT count(*) FROM task_requirements", [], |r| {
                    r.get::<_, u64>(0)
                })?,
            )
        })
        .await
        .unwrap();
    assert_eq!(count, 2);
}

#[tokio::test]
async fn resource_pause_has_its_reason_and_explicit_user_pause_takes_ownership() {
    let f = fixture().await;
    let p = f
        .host
        .spawn(&f.master.id, delegation("p", "general", 300))
        .await
        .unwrap();
    let run = p.run_id.as_ref().unwrap();
    f.host
        .engine()
        .journal()
        .set_state_if(run, &["ready"], "running", None, "test-running")
        .await
        .unwrap();
    let pressure = f.host.resource_sample(u64::MAX, 100).await.unwrap();
    assert!(pressure.pause.contains(&p.id));
    assert_eq!(
        f.host.status(&p.id).await.unwrap().reason.as_deref(),
        Some("resource")
    );
    f.host
        .control_keyed("user-pause", &p.id, Control::Pause)
        .await
        .unwrap();
    assert_eq!(
        f.host.status(&p.id).await.unwrap().reason.as_deref(),
        Some("user")
    );
    f.host.resource_sample(0, 101).await.unwrap();
    assert!(
        f.host
            .resource_sample(0, 161)
            .await
            .unwrap()
            .resume
            .is_empty()
    );
    assert_eq!(f.host.status(&p.id).await.unwrap().state, "paused");
}

#[tokio::test]
async fn automatic_resume_waiting_for_effect_boundary_preserves_new_user_pause() {
    let f = fixture().await;
    let p = f
        .host
        .spawn(&f.master.id, delegation("p", "general", 300))
        .await
        .unwrap();
    let run = p.run_id.clone().unwrap();
    assert!(
        f.host
            .engine()
            .pause_if_active(&run, "directive")
            .await
            .unwrap()
    );
    let lock = f.host.engine().journal().lock_run(&run).await;
    let engine = f.host.engine().clone();
    let auto_run = run.clone();
    let resume = tokio::spawn(async move {
        engine
            .resume_if_reason(&auto_run, "directive")
            .await
            .unwrap()
    });
    f.host
        .control_keyed("user-during-effect", &p.id, Control::Pause)
        .await
        .unwrap();
    assert!(!resume.is_finished());
    drop(lock);
    assert!(!resume.await.unwrap());
    assert_eq!(
        f.host.status(&p.id).await.unwrap().reason.as_deref(),
        Some("user")
    );
}

#[tokio::test]
async fn status_counts_micro_plan_items_linked_through_their_owner_task() {
    let f = fixture().await;
    let tasks = excalibur_tasks::TaskStore::open(f.db.clone())
        .await
        .unwrap();
    let host = f.host.clone().with_tasks(tasks.clone());
    let p = host
        .spawn(&f.master.id, delegation("p", "general", 300))
        .await
        .unwrap();
    tasks.host_batch("general","micro-status",serde_json::json!({"ops":[{"op":"create_plan","id":"micro","data":{"kind":"micro","title":"Micro","owner_task_id":p.task}},{"op":"create_plan_item","id":"one","data":{"plan_id":"micro","title":"Done","state":"done"}},{"op":"create_plan_item","id":"two","data":{"plan_id":"micro","title":"Pending"}}]})).await.unwrap();
    let status = host.status(&p.id).await.unwrap();
    assert_eq!(status.plan["done"], 1);
    assert_eq!(status.plan["total"], 2);
}

#[tokio::test]
async fn active_projection_does_not_replay_or_decode_terminal_history() {
    let f = fixture().await;
    let mut closed_spec = delegation("closed-history", "general", 300);
    closed_spec.profile = "coordinator".into();
    let mut active_spec = delegation("active-history", "general", 300);
    active_spec.profile = "coordinator".into();
    let closed = f.host.spawn(&f.master.id, closed_spec).await.unwrap();
    let active = f.host.spawn(&f.master.id, active_spec).await.unwrap();
    f.host
        .control(&closed.id, Control::Cancel { confirmed: true })
        .await
        .unwrap();
    let id = closed.id.clone();
    f.host
        .database()
        .write(excalibur_store::Priority::Normal, move |c| {
            c.execute(
                "UPDATE orchestration_agents SET body='unreadable historical record' WHERE id=?1",
                [id],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    let agents = f.host.active_agents().await.unwrap();
    assert!(agents.iter().any(|a| a.id == active.id));
    assert!(!agents.iter().any(|a| a.id == closed.id));
    assert!(f.host.status(&active.id).await.is_ok());
}
