//! Host-only authority through the actual local master delivery sink.
use excalibur_daemon::{DaemonHost, HostOptions, RuntimeSet, command::deliver_notifications};
use excalibur_master::Incoming;
use excalibur_models::{ModelError, ModelGateway};
use excalibur_orchestrator::{HostNotificationBinding, NotificationPolicy};
use excalibur_store::{Database, DatabaseKind, Priority, ReadLane};
use excalibur_types::runtime::{BoxFuture, ModelRequest, ModelResponse};
use excalibur_vault::Redactor;
use std::{sync::Arc, time::Duration};
struct Offline;
impl ModelGateway for Offline {
    fn generate(&self, _: ModelRequest) -> BoxFuture<'_, Result<ModelResponse, ModelError>> {
        Box::pin(async { Err(ModelError::Provider) })
    }
}
#[test]
#[ignore = "subprocess crash helper"]
fn host_notification_crash_helper() {
    let path = std::path::PathBuf::from(std::env::var("EXCALIBUR_HOST_NOTICE_CRASH_DIR").unwrap());
    let db = Database::open(path.join("main.db"), DatabaseKind::Main, Default::default()).unwrap();
    let options = HostOptions::project(path.join("project"));
    let principal = options.master_config.principal.clone();
    let rt = RuntimeSet::new(2, 1).unwrap();
    let host = rt
        .block_on(DaemonHost::open(
            db,
            options,
            Arc::new(Offline),
            None,
            "synthetic-host-notices-token-000000",
            Redactor::default(),
            rt.work(),
        ))
        .unwrap();
    rt.block_on(async {
        let binding = host
            .orchestrator
            .host_notification_bindings(&principal)
            .await
            .unwrap()
            .remove(0);
        let target = host
            .orchestrator
            .bind_host_notification(binding)
            .await
            .unwrap();
        host.orchestrator
            .notify_host(&target, "crash-key", "normal", "crash reminder", None, 10)
            .await
            .unwrap();
    });
    // Abrupt process termination after commit, without host shutdown or destructors.
    std::process::exit(73);
}
#[test]
fn host_notices_use_original_authority_without_worker_and_replay_across_reopen() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("project");
    std::fs::create_dir(&root).unwrap();
    let db = Database::open(
        temp.path().join("main.db"),
        DatabaseKind::Main,
        Default::default(),
    )
    .unwrap();
    let rt = RuntimeSet::new(2, 1).unwrap();
    let options = HostOptions::project(root.clone());
    let principal = options.master_config.principal.clone();
    let host = rt
        .block_on(DaemonHost::open(
            db.clone(),
            options,
            Arc::new(Offline),
            None,
            "synthetic-host-notices-token-000000",
            Redactor::default(),
            rt.work(),
        ))
        .unwrap();
    let binding=rt.block_on(async {
        let receipt=host.master.receive(Incoming{conversation:"agenda".into(),client_id:"client".into(),client_msg_id:"original".into(),text:"Remind me about this project".into(),scope:Some("project:default".into()),timezone:"UTC".into(),channel:"fixture".into(),reply_to:None,attachments:vec![]}).await.unwrap();
        host.master.stop_turn("agenda").await.unwrap();
        let binding=HostNotificationBinding{principal:principal.clone(),scope:"project:default".into(),conversation:"agenda".into(),source:receipt.message_id,approved_channels:vec!["local:approved".into()]};
        let target=host.orchestrator.bind_host_notification(binding.clone()).await.unwrap();
        for field in 0..4 {
            let mut bad=binding.clone();match field {0=>bad.principal="other".into(),1=>bad.scope="general".into(),2=>bad.conversation="other".into(),_=>bad.source="invented".into()};
            assert!(host.orchestrator.bind_host_notification(bad).await.is_err());
        }
        let mut changed=binding.clone();changed.approved_channels.push("local:another-approved".into());
        let alternate=host.orchestrator.bind_host_notification(changed).await.unwrap();
        host.master.set_scope("agenda","general").await.unwrap();
        host.orchestrator.set_notification_policy("agenda",NotificationPolicy{preferred_channel:Some("local:global".into()),..Default::default()}).await.unwrap();
        assert!(host.orchestrator.notify_host(&target,"bad-channel","normal","hello",Some("unapproved"),10).await.is_err());
        db.write(Priority::Journal,|c|{c.execute_batch("CREATE TRIGGER abort_host_receipt BEFORE INSERT ON orchestration_host_notification_origins BEGIN SELECT RAISE(ABORT,'simulated interruption'); END;")?;Ok(())}).await.unwrap();
        assert!(host.orchestrator.notify_host(&target,"rollback","normal","must roll back",None,10).await.is_err());
        let partial=db.read(ReadLane::Interactive,|c|Ok(c.query_row("SELECT count(*) FROM orchestration_notifications WHERE content='must roll back'",[],|r|r.get::<_,i64>(0))?)).await.unwrap();assert_eq!(partial,0);
        db.write(Priority::Journal,|c|{c.execute_batch("DROP TRIGGER abort_host_receipt")?;Ok(())}).await.unwrap();
        let (a,b)=tokio::join!(host.orchestrator.notify_host(&target,"key","normal","project reminder",Some("local:approved"),10),host.orchestrator.notify_host(&target,"key","normal","project reminder",Some("local:approved"),99));
        let (a,b)=(a.unwrap(),b.unwrap());assert_eq!(a.created,b.created);assert_eq!(a.id,b.id);assert!(a.agent_id.is_empty());
        assert!(host.orchestrator.notify_host(&alternate,"key","normal","project reminder",Some("local:approved"),10).await.is_err());
        let pending=host.orchestrator.pending_deliveries(100).await.unwrap();
        assert!(pending.iter().any(|d|d.channel=="local:approved"));assert!(!pending.iter().any(|d|d.channel=="local:global"));
        for (class,text,channel) in [("urgent","project reminder",Some("local:approved")),("normal","changed",Some("local:approved")),("normal","project reminder",None)] {
            assert!(host.orchestrator.notify_host(&target,"key",class,text,channel,10).await.is_err());
        }
        deliver_notifications(&host.master,&host.orchestrator,100).await.unwrap();
        deliver_notifications(&host.master,&host.orchestrator,100).await.unwrap();
        let id=a.id.clone();
        let (scope,count,agents,policy)=db.read(ReadLane::Interactive,move|c|Ok((c.query_row("SELECT m.scope_id FROM messages m JOIN master_notices n ON n.message_id=m.id WHERE n.id=?1",[format!("{id}:conversation:agenda")],|r|r.get::<_,String>(0))?,c.query_row("SELECT count(*) FROM master_notices",[],|r|r.get::<_,i64>(0))?,c.query_row("SELECT count(*) FROM orchestration_agents WHERE depth>0",[],|r|r.get::<_,i64>(0))?,c.query_row("SELECT body FROM orchestration_notify_policy WHERE conversation='agenda'",[],|r|r.get::<_,String>(0))?))).await.unwrap();
        assert_eq!(scope,"project:default");assert_eq!(count,1);assert_eq!(agents,0);assert!(policy.contains("local:global"));
        db.write(Priority::Journal,|c|{
            c.execute("INSERT INTO principals(id) VALUES('other-principal')",[])?;
            c.execute("INSERT INTO conversations(id,principal_id,active_scope) VALUES('other-principal-conversation','other-principal','general')",[])?;
            Ok(())
        }).await.unwrap();
        host.orchestrator.connection_activity("other-principal-conversation","fixture",true,100).await.unwrap();
        let urgent=host.orchestrator.notify_host(&target,"urgent-key","urgent","private content",None,100).await.unwrap();
        let attempts=host.orchestrator.pending_deliveries(100).await.unwrap();
        assert!(!attempts.iter().any(|d|d.notification.id==urgent.id&&d.cross_conversation.is_some()));
        let owned_principal=principal.clone();
        db.write(Priority::Journal,move|c|{
            c.execute("INSERT INTO conversations(id,principal_id,active_scope) VALUES('same-principal-conversation',?1,'general')",[owned_principal])?;
            Ok(())
        }).await.unwrap();
        host.orchestrator.connection_activity("same-principal-conversation","fixture",true,100).await.unwrap();
        let attempts=host.orchestrator.pending_deliveries(100).await.unwrap();
        assert!(attempts.iter().any(|d|d.notification.id==urgent.id&&d.cross_conversation.as_deref()==Some("same-principal-conversation")));
        deliver_notifications(&host.master,&host.orchestrator,100).await.unwrap();
        let link=db.read(ReadLane::Interactive,|c|Ok(c.query_row("SELECT payload FROM master_events WHERE conversation_id='same-principal-conversation' AND kind='notification'",[],|r|r.get::<_,String>(0))?)).await.unwrap();
        assert!(link.contains("Urgent update in the origin conversation"));assert!(!link.contains("private content"));assert!(!link.contains("project reminder"));
        host.orchestrator.acknowledge_notification(&urgent.id).await.unwrap();
        host.orchestrator.acknowledge_notification(&a.id).await.unwrap();
        let replay=host.orchestrator.notify_host(&target,"key","normal","project reminder",Some("local:approved"),999).await.unwrap();assert_eq!(replay.created,a.created);assert!(!replay.acknowledged);
        // A message generated by the notice sink is not an accepted client source.
        let source=db.read(ReadLane::Interactive,|c|Ok(c.query_row("SELECT message_id FROM master_notices LIMIT 1",[],|r|r.get::<_,String>(0))?)).await.unwrap();
        let mut forged=binding.clone();forged.source=source;assert!(host.orchestrator.bind_host_notification(forged).await.is_err());
        binding
    });
    rt.block_on(host.close()).unwrap();
    drop(host);
    drop(db);
    let status = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--ignored", "--exact", "host_notification_crash_helper"])
        .env("EXCALIBUR_HOST_NOTICE_CRASH_DIR", temp.path())
        .status()
        .unwrap();
    assert_eq!(status.code(), Some(73));
    let db = Database::open(
        temp.path().join("main.db"),
        DatabaseKind::Main,
        Default::default(),
    )
    .unwrap();
    let host = rt
        .block_on(DaemonHost::open(
            db.clone(),
            HostOptions::project(root),
            Arc::new(Offline),
            None,
            "synthetic-host-notices-token-000000",
            Redactor::default(),
            rt.work(),
        ))
        .unwrap();
    rt.block_on(async {
        let bindings = host
            .orchestrator
            .host_notification_bindings(&principal)
            .await
            .unwrap();
        assert_eq!(bindings.len(), 2);
        assert!(bindings.contains(&binding));
        let target = host
            .orchestrator
            .bind_host_notification(binding)
            .await
            .unwrap();
        let replay = host
            .orchestrator
            .notify_host(
                &target,
                "key",
                "normal",
                "project reminder",
                Some("local:approved"),
                1000,
            )
            .await
            .unwrap();
        assert!(replay.created == 10 || replay.created == 99);
        let bindings = host
            .orchestrator
            .host_notification_bindings(&principal)
            .await
            .unwrap();
        let crash_origin = host
            .orchestrator
            .notifications("agenda")
            .await
            .unwrap()
            .into_iter()
            .find(|n| n.content == "crash reminder")
            .unwrap();
        let authority = host
            .orchestrator
            .host_notification_origin(&crash_origin.id)
            .await
            .unwrap()
            .unwrap();
        assert!(bindings.contains(&authority));
        let crash_target = host
            .orchestrator
            .bind_host_notification(authority)
            .await
            .unwrap();
        let crash_replay = host
            .orchestrator
            .notify_host(
                &crash_target,
                "crash-key",
                "normal",
                "crash reminder",
                None,
                9000,
            )
            .await
            .unwrap();
        assert_eq!(crash_replay.created, 10);
        deliver_notifications(&host.master, &host.orchestrator, 1000)
            .await
            .unwrap();
        let count = db
            .read(ReadLane::Interactive, |c| {
                Ok(c.query_row("SELECT count(*) FROM master_notices", [], |r| {
                    r.get::<_, i64>(0)
                })?)
            })
            .await
            .unwrap();
        assert_eq!(count, 4);
    });
    rt.block_on(host.close()).unwrap();
    rt.shutdown(Duration::from_secs(3));
}
