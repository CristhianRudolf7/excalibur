//! Trusted startup reprovisioning preserves identities and isolation across restart.
use excalibur_context::ContextInput;
use excalibur_daemon::{DaemonHost, HostOptions, WorkerTools};
use excalibur_engine::{Budget, RunSpec};
use excalibur_models::{ModelError, ModelGateway};
use excalibur_orchestrator::{Delegation, Project};
use excalibur_security::{CapabilityToken, Root};
use excalibur_store::{Database, DatabaseKind, Priority};
use excalibur_tools::{Access, LocalTools, ToolPolicy};
use excalibur_types::runtime::*;
use excalibur_vault::Redactor;
use serde_json::json;
use std::{
    collections::BTreeSet,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};
struct NoModels(AtomicUsize);
impl ModelGateway for NoModels {
    fn complete(&self, _: ModelRequest) -> BoxFuture<'_, Result<ModelResponse, ModelError>> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Err(ModelError::Configuration("no model call permitted".into())) })
    }
}
fn project(id: &str, root: PathBuf) -> Project {
    Project {
        id: id.into(),
        name: id.into(),
        aliases: vec![],
        description: String::new(),
        roots: vec![root],
        status: "active".into(),
        vcs: None,
        settings: json!({}),
    }
}
fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}
fn options(root: &std::path::Path, projects: Vec<Project>) -> HostOptions {
    let mut o = HostOptions::project(root.to_path_buf());
    o.projects = projects;
    o
}
fn delegation(id: &str, scope: &str, root: PathBuf) -> Delegation {
    Delegation {
        operation: id.into(),
        name: id.into(),
        profile: "coordinator".into(),
        scope: scope.into(),
        task: format!("task:{id}"),
        origin_conversation: "restore".into(),
        goal: "Synthetic scoped task".into(),
        deliverable: "Synthetic result".into(),
        inputs: vec![],
        pinned_instructions: vec![],
        suggested_skills: vec![],
        fork_context: false,
        token: CapabilityToken {
            roots: vec![Root {
                path: root,
                write: true,
            }],
            data_scopes: BTreeSet::from(["general".into(), scope.into()]),
            budget: 10,
            ..Default::default()
        },
        spec: RunSpec {
            context: ContextInput::new("Synthetic rules", "Synthetic task", "fixed"),
            budget: Budget::default(),
            criteria: vec![excalibur_engine::Criterion::FinalResponse { schema: None }],
        },
        priority: 1,
    }
}
#[test]
fn restart_reprovisions_master_preserves_children_and_retains_unprovisioned_project_privacy() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("common");
    let a = root.join("a");
    let b = a.join("nested-b");
    std::fs::create_dir_all(&b).unwrap();
    std::fs::write(a.join("own.txt"), "OWN").unwrap();
    std::fs::write(b.join("private.txt"), "B_PRIVATE").unwrap();
    let b_other = root.join("b-other");
    std::fs::create_dir_all(&b_other).unwrap();
    std::fs::write(b_other.join("private.txt"), "B_OTHER_PRIVATE").unwrap();
    let mut project_b = project("b", b.clone());
    project_b.roots.push(b_other.clone());
    let mut db = Database::open(
        dir.path().join("data/main.db"),
        DatabaseKind::Main,
        Default::default(),
    )
    .unwrap();
    let interactive = runtime();
    let model = Arc::new(NoModels(AtomicUsize::new(0)));
    let work = runtime();
    let host = interactive
        .block_on(DaemonHost::open(
            db.clone(),
            options(&root, vec![project("a", a.clone())]),
            model.clone(),
            None,
            "synthetic-restart-token-only-local-fixture-32chars",
            Redactor::default(),
            work.handle().clone(),
        ))
        .unwrap();
    let master = interactive
        .block_on(host.orchestrator.active_agents())
        .unwrap()
        .into_iter()
        .find(|a| a.depth == 0)
        .unwrap();
    let old = interactive
        .block_on(
            host.orchestrator
                .spawn(&master.id, delegation("old-a", "project:a", a.clone())),
        )
        .unwrap();
    interactive.block_on(host.close()).unwrap();
    drop(work);
    drop(db);
    db = Database::open(
        dir.path().join("data/main.db"),
        DatabaseKind::Main,
        Default::default(),
    )
    .unwrap();
    let work = runtime();
    let host = interactive
        .block_on(DaemonHost::open(
            db.clone(),
            options(&root, vec![project("a", a.clone()), project_b]),
            model.clone(),
            None,
            "synthetic-restart-token-only-local-fixture-32chars",
            Redactor::default(),
            work.handle().clone(),
        ))
        .unwrap();
    let rebound = interactive
        .block_on(host.orchestrator.agent(&master.id))
        .unwrap();
    assert!(rebound.token.data_scopes.contains("project:b"));
    assert_eq!(
        interactive
            .block_on(host.orchestrator.agent(&old.id))
            .unwrap()
            .token,
        old.token
    );
    let spawned = interactive
        .block_on(
            host.orchestrator
                .spawn(&master.id, delegation("new-b", "project:b", b.clone())),
        )
        .unwrap();
    let dispatched = interactive.block_on(host.orchestrator.tick()).unwrap();
    assert!(dispatched.contains(&spawned.id));
    assert_eq!(model.0.load(Ordering::SeqCst), 0);
    drop(work);
    interactive.block_on(host.close()).unwrap();
    drop(db);
    db = Database::open(
        dir.path().join("data/main.db"),
        DatabaseKind::Main,
        Default::default(),
    )
    .unwrap();
    let work = runtime();
    let host = interactive
        .block_on(DaemonHost::open(
            db.clone(),
            options(&root, vec![project("a", a.clone())]),
            model.clone(),
            None,
            "synthetic-restart-token-only-local-fixture-32chars",
            Redactor::default(),
            work.handle().clone(),
        ))
        .unwrap();
    let final_master = interactive
        .block_on(host.orchestrator.agent(&master.id))
        .unwrap();
    assert!(!final_master.token.data_scopes.contains("project:b"));
    assert_eq!(
        interactive
            .block_on(host.orchestrator.agent(&old.id))
            .unwrap()
            .token,
        old.token
    );
    assert!(
        interactive
            .block_on(
                host.orchestrator
                    .spawn(&master.id, delegation("removed-b", "project:b", b.clone()))
            )
            .is_err()
    );
    let admitted = interactive.block_on(host.orchestrator.tick()).unwrap();
    assert!(!admitted.contains(&spawned.id));
    let stopped = interactive
        .block_on(host.orchestrator.status(&spawned.id))
        .unwrap();
    assert_eq!(stopped.state, "blocked");
    let notices = interactive
        .block_on(
            host.orchestrator
                .pending_deliveries(excalibur_store::unix_seconds()),
        )
        .unwrap();
    assert!(
        notices
            .iter()
            .any(|notice| notice.notification.agent_id == spawned.id
                && notice.notification.class == "urgent")
    );
    let count = interactive
        .block_on(host.orchestrator.notifications("restore"))
        .unwrap()
        .len();
    interactive.block_on(host.orchestrator.tick()).unwrap();
    assert_eq!(
        interactive
            .block_on(host.orchestrator.notifications("restore"))
            .unwrap()
            .len(),
        count
    );
    assert!(
        interactive
            .block_on(host.orchestrator.mailbox(&master.id))
            .unwrap()
            .iter()
            .any(|message| message.sender == spawned.id && message.kind == "completion")
    );
    interactive.block_on(async {
        let local = Arc::new(
            LocalTools::new(
                db.clone(),
                root.clone(),
                ToolPolicy {
                    read: Access::Allow,
                    ..Default::default()
                },
            )
            .await
            .unwrap(),
        );
        let workers = WorkerTools::new(
            db.clone(),
            local,
            ToolPolicy {
                read: Access::Allow,
                ..Default::default()
            },
            vec![project("a", a.clone())],
            None,
        )
        .await
        .unwrap();
        db.write(Priority::Journal, |c| {
            for id in ["privacy-general", "privacy-a"] {
                c.execute("INSERT INTO runs(id,state) VALUES(?1,'ready')", [id])?;
            }
            Ok(())
        })
        .await
        .unwrap();
        std::fs::write(root.join("general.txt"), "GENERAL").unwrap();
        for (id, scope, allowed, denied) in [
            (
                "privacy-general",
                "general",
                root.join("general.txt"),
                b_other.join("private.txt"),
            ),
            (
                "privacy-a",
                "project:a",
                a.join("own.txt"),
                a.join("nested-b/private.txt"),
            ),
        ] {
            let mut agent = old.clone();
            agent.id = format!("agent:{id}");
            agent.run_id = Some(id.into());
            agent.scope = scope.into();
            agent.token = final_master.token.clone();
            if scope == "general" {
                agent.token.data_scopes = BTreeSet::from(["general".into()]);
            } else {
                agent.token.roots = vec![Root {
                    path: a.clone(),
                    write: false,
                }];
            }
            workers.bind_agent(&agent, None).await.unwrap();
            let call = |path: &std::path::Path| ToolCall {
                id: path.display().to_string(),
                name: "read_file".into(),
                arguments: json!({"path":path}).to_string(),
            };
            let prepared = workers.prepare(id, call(&allowed)).await.unwrap();
            assert!(workers.execute(id, "allowed", &prepared).await.success);
            assert!(workers.prepare(id, call(&denied)).await.is_err());
            assert!(
                workers
                    .verifier_tools(id)
                    .await
                    .unwrap()
                    .prepare(id, call(&denied))
                    .await
                    .is_err()
            );
        }
        workers.shutdown().await;
    });
    drop(work);
    interactive.block_on(host.close()).unwrap();
    assert_eq!(model.0.load(Ordering::SeqCst), 0);
}

#[test]
fn recovery_summary_without_project_agents_enters_project_master_inbox() {
    let dir = tempfile::tempdir().unwrap();
    let db = Database::open(
        dir.path().join("main.db"),
        DatabaseKind::Main,
        Default::default(),
    )
    .unwrap();
    let interactive = runtime();
    let work = runtime();
    let host = interactive
        .block_on(DaemonHost::open(
            db.clone(),
            options(dir.path(), vec![project("a", dir.path().to_path_buf())]),
            Arc::new(NoModels(AtomicUsize::new(0))),
            None,
            "synthetic-scoped-startup-notice-token-32chars",
            Redactor::default(),
            work.handle().clone(),
        ))
        .unwrap();
    interactive.block_on(async {
        let at = excalibur_store::unix_seconds();
        // Use real principal-scoped intake to establish the conversation and
        // its foreign-key owner, rather than assuming Master::open creates it.
        host.master.receive(excalibur_master::Incoming {
            conversation: "scoped-recovery".into(), client_id: "startup-scope-fixture".into(),
            client_msg_id: "establish-conversation".into(), text: "status".into(),
            scope: Some("project:a".into()), timezone: "UTC".into(), channel: "local-test".into(),
            reply_to: None, attachments: vec![],
        }).await.unwrap();
        host.master.stop_turn("scoped-recovery").await.unwrap();
        db.write(Priority::Journal, move |c| {
            c.execute("UPDATE daemon_recovery_state SET safe_until=?1", [at + 1800])?;
            Ok(())
        }).await.unwrap();
        let boot = host.orchestrator.recovery().status(at).await.unwrap().boot_id.unwrap();
        host.orchestrator.recovery().publish_summary(&boot, at).await.unwrap();
        let notes = host.orchestrator.notifications("scoped-recovery").await.unwrap();
        assert_eq!(notes.len(), 1);
        assert_eq!(notes[0].class, "urgent");
        let payload: serde_json::Value = serde_json::from_str(&notes[0].content).unwrap();
        assert_eq!(payload["scope"], "project:a");
        assert_eq!(payload["decisions"], json!([]));
        excalibur_daemon::command::deliver_notifications(&host.master, &host.orchestrator, at).await.unwrap();
        let (scope, queued, envelope): (String, bool, String) = db.read(excalibur_store::ReadLane::Interactive, |c| {
            Ok(c.query_row("SELECT m.scope_id,EXISTS(SELECT 1 FROM master_inbox i WHERE i.message_id=m.id),e.payload FROM master_notices n JOIN messages m ON m.id=n.message_id JOIN master_events e ON e.sequence=n.event_sequence WHERE n.conversation_id='scoped-recovery'", [], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?)))?)
        }).await.unwrap();
        assert_eq!(scope, "project:a");
        assert!(queued, "an urgent project summary must reach the master inbox");
        let envelope: serde_json::Value = serde_json::from_str(&envelope).unwrap();
        assert_eq!(envelope["content"]["scope"], "project:a");
        assert_eq!(envelope["content"]["host"], true);
        assert!(envelope["content"].get("agent_id").is_none());
        // Delivery retries use the existing durable outbox receipt.
        excalibur_daemon::command::deliver_notifications(&host.master, &host.orchestrator, at).await.unwrap();
        let count: u64 = db.read(excalibur_store::ReadLane::Interactive, |c| Ok(c.query_row("SELECT count(*) FROM master_notices WHERE conversation_id='scoped-recovery'", [], |r| r.get(0))?)).await.unwrap();
        assert_eq!(count, 1);
    });
    drop(work);
    interactive.block_on(host.close()).unwrap();
}
