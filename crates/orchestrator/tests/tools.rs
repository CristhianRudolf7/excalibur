mod support;
use excalibur_engine::{Budget, CompletionGate, Engine, GateInput};
use excalibur_orchestrator::*;
use excalibur_types::runtime::*;
use serde_json::json;
use std::sync::Arc;
use support::*;
#[tokio::test]
async fn real_model_callable_spawn_reconciles_and_workers_cannot_delegate() {
    let f = fixture().await;
    let adapter = Arc::new(OrchestratorTools::new(no_tools()));
    let gate = Arc::new(OrchestrationGate::new());
    let engine = Arc::new(
        Engine::new(f.host.engine().journal().clone(), model(), adapter.clone())
            .with_completion_gate(gate.clone()),
    );
    let host = Orchestrator::open(f.db.clone(), engine, Limits::default())
        .await
        .unwrap();
    adapter.attach(&host).unwrap();
    gate.attach(&host).unwrap();
    let p = host
        .spawn(&f.master.id, delegation("p", "general", 300))
        .await
        .unwrap();
    let run = p.run_id.as_ref().unwrap();
    let call = ToolCall {
        id: "spawncall".into(),
        name: "subagent".into(),
        arguments: json!({"action":"spawn","delegation":delegation("c","general",60)}).to_string(),
    };
    let prepared = adapter.prepare(run, call).await.unwrap();
    let result = adapter.execute(run, "stable-spawn", &prepared).await;
    assert!(result.success, "{:?}", result.output);
    let child: Agent = serde_json::from_value(result.output.clone()).unwrap();
    let repeated = adapter.reconcile(run, "stable-spawn", &prepared).await;
    assert!(matches!(repeated,Reconcile::Done(r) if r.output==result.output));
    let catalog = adapter
        .definitions_for_run(child.run_id.as_ref().unwrap())
        .await
        .unwrap();
    assert!(!catalog.iter().any(|d| d.name == "subagent"));
    let forged = ToolCall {
        id: "forged".into(),
        name: "subagent".into(),
        arguments: json!({"action":"spawn","delegation":delegation("grandchild","general",10)})
            .to_string(),
    };
    assert!(
        adapter
            .prepare(child.run_id.as_ref().unwrap(), forged)
            .await
            .is_err()
    );
    let input = GateInput {
        step: 0,
        instruction_revision: 0,
        final_text: "Premature final".into(),
        local_pending: vec![],
        budget_remaining: Budget::default(),
    };
    let guard = gate.inspect(run, &input).await.unwrap();
    assert_eq!(guard.report().issues.len(), 1);
    drop(guard);
    assert_eq!(host.list_agents().await.unwrap().len(), 3);
}

#[tokio::test]
async fn waiting_parent_spends_no_model_calls_and_child_completion_wakes_it() {
    let f = fixture().await;
    let adapter = Arc::new(OrchestratorTools::new(no_tools()));
    let gate = Arc::new(OrchestrationGate::new());
    let engine = Arc::new(
        Engine::new(f.host.engine().journal().clone(), model(), adapter.clone())
            .with_completion_gate(gate.clone()),
    );
    let host = Orchestrator::open(f.db.clone(), engine, Limits::default())
        .await
        .unwrap()
        .with_authority(authority());
    adapter.attach(&host).unwrap();
    gate.attach(&host).unwrap();
    let p = host
        .spawn(&f.master.id, delegation("p", "general", 300))
        .await
        .unwrap();
    let c = host
        .spawn(&p.id, delegation("c", "general", 60))
        .await
        .unwrap();
    host.request_wait_for_children(&p.id).await.unwrap();
    let waiting = host.engine().run(p.run_id.as_ref().unwrap()).await.unwrap();
    assert_eq!(waiting.status, excalibur_engine::Status::Waiting);
    assert_eq!(waiting.model_calls, 0);
    let started = host.tick().await.unwrap();
    assert_eq!(started, vec![c.id.clone()]);
    for _ in 0..200 {
        host.reap().await.unwrap();
        if host.status(&c.id).await.unwrap().state == "completed" {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            let started = host.tick().await.unwrap();
            if started.contains(&p.id) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    for _ in 0..200 {
        host.reap().await.unwrap();
        if host.status(&p.id).await.unwrap().state == "completed" {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    assert_eq!(host.status(&p.id).await.unwrap().state, "completed");
    assert_eq!(
        host.engine()
            .load(p.run_id.as_ref().unwrap())
            .await
            .unwrap()
            .model_calls,
        1
    );
    host.shutdown().await.unwrap();
}

#[tokio::test]
async fn compact_delegation_schema_derives_bounded_authority_and_replays_frozen_budget() {
    let f = fixture().await;
    let adapter = OrchestratorTools::new(no_tools());
    adapter.attach(&f.host).unwrap();
    let p = f
        .host
        .spawn(&f.master.id, delegation("p", "general", 300))
        .await
        .unwrap();
    let run = p.run_id.as_ref().unwrap();
    let call=ToolCall{id:"compact".into(),name:"subagent".into(),arguments:json!({"action":"spawn","delegation":{"name":"Reader","profile":"explorer","goal":"Inspect synthetic inputs","deliverable":"A bounded summary"}}).to_string()};
    let prepared = adapter.prepare(run, call).await.unwrap();
    let result = adapter.execute(run, "compact-key", &prepared).await;
    assert!(result.success, "{:?}", result.output);
    let child: Agent = serde_json::from_value(result.output.clone()).unwrap();
    assert!(child.budget.max_steps <= 60);
    assert!(p.token.contains(&child.token));
    assert!(!child.token.processes);
    let repeated = adapter.reconcile(run, "compact-key", &prepared).await;
    assert!(matches!(repeated,Reconcile::Done(r) if r.output==result.output));
    let mut changed = prepared.clone();
    changed.call.arguments=json!({"action":"spawn","delegation":{"name":"Reader","profile":"explorer","goal":"Changed goal","deliverable":"A bounded summary"}}).to_string();
    assert!(!adapter.execute(run, "compact-key", &changed).await.success);
}

#[tokio::test]
async fn custom_identity_keeps_base_role_and_cannot_acquire_writer_tools() {
    let f = fixture().await;
    f.host
        .register_profile(CustomProfile {
            id: "precise-reader".into(),
            name: "Precise reader".into(),
            base_profile: "explorer".into(),
            identity: "Be concise. Preserve quoted evidence literally.".into(),
            icon: Some("book".into()),
        })
        .await
        .unwrap();
    let p = f
        .host
        .spawn(&f.master.id, delegation("p", "general", 300))
        .await
        .unwrap();
    let adapter = OrchestratorTools::new(no_tools());
    adapter.attach(&f.host).unwrap();
    let run = p.run_id.as_ref().unwrap();
    let call=ToolCall{id:"custom".into(),name:"subagent".into(),arguments:json!({"action":"spawn","delegation":{"name":"Reader","profile":"precise-reader","goal":"Inspect synthetic inputs","deliverable":"A concise evidence report"}}).to_string()};
    let prepared = adapter.prepare(run, call).await.unwrap();
    let result = adapter.execute(run, "custom-key", &prepared).await;
    assert!(result.success, "{:?}", result.output);
    let child: Agent = serde_json::from_value(result.output).unwrap();
    assert_eq!(child.profile, "explorer");
    assert_eq!(child.profile_id.as_deref(), Some("precise-reader"));
    let snapshot = f
        .host
        .engine()
        .load(child.run_id.as_ref().unwrap())
        .await
        .unwrap();
    assert!(snapshot.spec.context.layers[1].contains("Preserve quoted evidence literally."));
    let write = ToolCall {
        id: "write".into(),
        name: "write_file".into(),
        arguments: json!({"path":"synthetic.txt","content":"Denied"}).to_string(),
    };
    assert!(
        adapter
            .prepare(child.run_id.as_ref().unwrap(), write)
            .await
            .unwrap_err()
            .contains("role profile")
    );
    let mut changed = f.host.custom_profile("precise-reader").await.unwrap();
    changed.base_profile = "implementer".into();
    assert!(f.host.register_profile(changed).await.is_err());
    assert!(serde_json::from_value::<CustomProfile>(json!({"id":"amplify","name":"Amplify","base_profile":"explorer","identity":"style","icon":null,"permissions":{"write":true}})).is_err());
}
