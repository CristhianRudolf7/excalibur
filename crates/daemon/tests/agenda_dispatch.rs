//! Host-only authority through the actual local master delivery sink.
use excalibur_daemon::{DaemonHost, HostOptions, RuntimeSet, command::deliver_notifications};
use excalibur_master::Incoming;
use excalibur_models::{ModelError, ModelGateway};
use excalibur_orchestrator::HostNotificationBinding;
use excalibur_store::{Database, DatabaseKind, Priority, ReadLane};
use excalibur_types::runtime::{BoxFuture, ModelRequest, ModelResponse};
use excalibur_vault::Redactor;
use std::sync::Arc;
struct Offline;
impl ModelGateway for Offline {
    fn complete(&self, _: ModelRequest) -> BoxFuture<'_, Result<ModelResponse, ModelError>> {
        Box::pin(async { Err(ModelError::Provider) })
    }
}

use excalibur_daemon::agenda::Agenda;
use excalibur_scheduler::{
    Timing,
    reminders::{Definition, Operation, Reminders},
};
fn definition(id: &str) -> Operation {
    Operation::Create(Definition {
        id: id.into(),
        text: format!("reminder {id}"),
        timing: Timing::At(10),
        preferred_channel: None,
        task: None,
    })
}
#[test]
fn generation_safe_real_host_dispatch() {
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
    let options = HostOptions::project(root);
    let principal = options.master_config.principal.clone();
    let host = rt
        .block_on(DaemonHost::open(
            db.clone(),
            options,
            Arc::new(Offline),
            None,
            "synthetic-agenda-token-00000000000",
            Redactor::default(),
            rt.work(),
        ))
        .unwrap();
    rt.block_on(async {
        let receipt = host
            .master
            .receive(Incoming {
                conversation: "agenda".into(),
                client_id: "client".into(),
                client_msg_id: "original".into(),
                text: "reminder".into(),
                scope: Some("project:default".into()),
                timezone: "UTC".into(),
                channel: "fixture".into(),
                reply_to: None,
                attachments: vec![],
            })
            .await
            .unwrap();
        host.master.stop_turn("agenda").await.unwrap();
        let binding = HostNotificationBinding {
            principal: principal.clone(),
            scope: "project:default".into(),
            conversation: "agenda".into(),
            source: receipt.message_id,
            approved_channels: vec![],
        };
        let agenda = Agenda::open(host.orchestrator.clone(), binding.clone())
            .await
            .unwrap();
        for field in 0..4 {
            let mut b = binding.clone();
            match field {
                0 => b.principal = "other".into(),
                1 => b.scope = "general".into(),
                2 => b.conversation = "other".into(),
                _ => b.source = "invented".into(),
            };
            assert!(Agenda::open(host.orchestrator.clone(), b).await.is_err());
        }
        agenda
            .apply("create".into(), definition("first"))
            .await
            .unwrap();
        let (a, b) = tokio::join!(agenda.tick_once(10, 100), agenda.tick_once(10, 100));
        assert!(a.is_ok() && b.is_ok());
        assert_eq!(
            host.orchestrator
                .notifications("agenda")
                .await
                .unwrap()
                .len(),
            1
        );
        let deliveries = host.orchestrator.pending_deliveries(100).await.unwrap();
        assert!(!deliveries.is_empty());
        agenda
            .apply(
                "done".into(),
                Operation::Complete {
                    id: "first".into(),
                    generation: 1,
                },
            )
            .await
            .unwrap();
        for delivery in &deliveries {
            excalibur_daemon::command::deliver_notification(
                &host.master,
                &host.orchestrator,
                delivery,
            )
            .await
            .unwrap();
        }
        assert_eq!(
            db.read(ReadLane::Interactive, |c| Ok(c.query_row(
                "SELECT count(*) FROM master_notices",
                [],
                |r| r.get::<_, i64>(0)
            )?))
            .await
            .unwrap(),
            0
        );
        assert!(
            host.orchestrator
                .pending_deliveries(100)
                .await
                .unwrap()
                .is_empty()
        );
        // Due fires are persisted during safe mode; a manual exit admits the backlog.
        agenda
            .apply("safe-create".into(), definition("safe"))
            .await
            .unwrap();
        let safe_at = excalibur_store::unix_seconds() + 3;
        for offset in 0..3 {
            let boot = host
                .orchestrator
                .recovery()
                .begin(safe_at - 2 + offset)
                .await
                .unwrap();
            assert!(
                host.orchestrator
                    .recovery()
                    .status(safe_at)
                    .await
                    .unwrap()
                    .recovering
            );
            assert_eq!(agenda.tick_once(safe_at, 100).await.unwrap(), 0);
            host.orchestrator
                .recover_startup(&boot, excalibur_security::Autonomy::Supervised)
                .await
                .unwrap();
        }
        assert!(
            host.orchestrator
                .recovery()
                .status(safe_at)
                .await
                .unwrap()
                .safe_mode
        );
        assert_eq!(agenda.tick_once(safe_at, 100).await.unwrap(), 0);
        let reminders = Reminders::open(
            db.clone(),
            binding.scope.clone(),
            principal.clone(),
            binding.conversation.clone(),
            None,
        )
        .await
        .unwrap();
        assert_eq!(reminders.pending(100).await.unwrap().len(), 1);
        host.orchestrator
            .recovery()
            .exit_safe_mode("manual-exit", safe_at)
            .await
            .unwrap();
        assert_eq!(agenda.tick_once(safe_at, 100).await.unwrap(), 1);
        deliver_notifications(&host.master, &host.orchestrator, 100)
            .await
            .unwrap();
        assert_eq!(
            db.read(ReadLane::Interactive, |c| Ok(c.query_row(
                "SELECT count(*) FROM master_notices",
                [],
                |r| r.get::<_, i64>(0)
            )?))
            .await
            .unwrap(),
            1
        );
        // A later accepted message shares the namespace but cannot replace the creator binding.
        let second = host
            .master
            .receive(Incoming {
                conversation: "agenda".into(),
                client_id: "client".into(),
                client_msg_id: "later".into(),
                text: "snooze".into(),
                scope: Some("project:default".into()),
                timezone: "UTC".into(),
                channel: "fixture".into(),
                reply_to: None,
                attachments: vec![],
            })
            .await
            .unwrap();
        host.master.stop_turn("agenda").await.unwrap();
        let mut later = binding.clone();
        later.source = second.message_id;
        let later_agenda = Agenda::open(host.orchestrator.clone(), later)
            .await
            .unwrap();
        assert_eq!(
            later_agenda
                .list("later-list".into(), None, 100)
                .await
                .unwrap()
                .len(),
            2
        );
        let (tick, snooze) = tokio::join!(
            agenda.tick_once(40, 100),
            later_agenda.apply(
                "later-snooze".into(),
                Operation::Snooze {
                    id: "safe".into(),
                    generation: 1,
                    timing: Timing::At(50)
                }
            )
        );
        tick.unwrap();
        snooze.unwrap();
        reminders.tick(50, false, 100).await.unwrap();
        let inherited = reminders
            .pending(100)
            .await
            .unwrap()
            .into_iter()
            .find(|p| p.reminder.id == "safe")
            .unwrap();
        assert_eq!(
            reminders.metadata(&inherited).await.unwrap().unwrap()["source"],
            binding.source
        );
        later_agenda
            .apply(
                "later-complete".into(),
                Operation::Complete {
                    id: "safe".into(),
                    generation: 2,
                },
            )
            .await
            .unwrap();
        // Admission and snooze race before any sink effect; an old admitted
        // generation, if present, must be suppressed after the winning snooze.
        agenda
            .apply("race-create".into(), definition("racing"))
            .await
            .unwrap();
        let (tick, snooze) = tokio::join!(
            agenda.tick_once(40, 100),
            later_agenda.apply(
                "race-snooze".into(),
                Operation::Snooze {
                    id: "racing".into(),
                    generation: 1,
                    timing: Timing::At(60)
                }
            )
        );
        tick.unwrap();
        snooze.unwrap();
        assert!(
            !host
                .orchestrator
                .pending_deliveries(100)
                .await
                .unwrap()
                .iter()
                .any(|d| d.notification.content == "reminder racing")
        );
        later_agenda
            .apply(
                "race-complete".into(),
                Operation::Complete {
                    id: "racing".into(),
                    generation: 2,
                },
            )
            .await
            .unwrap();
        // A fetched fire is not a permit: complete before admission suppresses it.
        agenda
            .apply("fetch-create".into(), definition("fetched"))
            .await
            .unwrap();
        reminders.tick(30, false, 100).await.unwrap();
        let pending = reminders.pending(100).await.unwrap().remove(0);
        agenda
            .apply(
                "fetch-complete".into(),
                Operation::Complete {
                    id: "fetched".into(),
                    generation: 1,
                },
            )
            .await
            .unwrap();
        let target = host
            .orchestrator
            .bind_host_notification(binding.clone())
            .await
            .unwrap();
        assert!(
            host.orchestrator
                .notify_host_guarded(
                    &target,
                    "fetched-notice",
                    "normal",
                    "never",
                    None,
                    10,
                    reminders.condition(&pending),
                    30
                )
                .await
                .unwrap()
                .is_none()
        );
        // Legacy unbound rows grant no delivery authority and must not hide a
        // later bound row behind the pending page limit.
        reminders
            .apply("unbound-create".into(), definition("aaa-unbound"))
            .await
            .unwrap();
        agenda
            .apply("bound-create".into(), definition("zzz-bound"))
            .await
            .unwrap();
        reminders.tick(70, false, 100).await.unwrap();
        assert_eq!(agenda.tick_once(70, 1).await.unwrap(), 1);
        assert_eq!(reminders.pending(100).await.unwrap().len(), 1);
    });
    rt.block_on(host.close()).unwrap();
}

fn crash_host(path: &std::path::Path, rt: &RuntimeSet, db: Database) -> DaemonHost {
    rt.block_on(DaemonHost::open(
        db,
        HostOptions::project(path.join("project")),
        Arc::new(Offline),
        None,
        "synthetic-agenda-token-00000000000",
        Redactor::default(),
        rt.work(),
    ))
    .unwrap()
}
#[test]
#[ignore = "subprocess interruption helper"]
fn agenda_post_notice_pre_ack_helper() {
    let path = std::path::PathBuf::from(std::env::var("EXCALIBUR_AGENDA_CRASH").unwrap());
    let db = Database::open(path.join("main.db"), DatabaseKind::Main, Default::default()).unwrap();
    let rt = RuntimeSet::new(2, 1).unwrap();
    let host = crash_host(&path, &rt, db.clone());
    rt.block_on(async {
        let principal=HostOptions::project(path.join("project")).master_config.principal;
        let binding=host.orchestrator.host_notification_bindings(&principal).await.unwrap().remove(0);
        let agenda=Agenda::open(host.orchestrator.clone(),binding).await.unwrap();
        db.write(Priority::Journal,|c| {c.execute_batch("CREATE TRIGGER interrupt_agenda_ack BEFORE UPDATE OF delivered ON scheduler_fires BEGIN SELECT RAISE(ABORT,'post notice pre ack interruption'); END;")?;Ok(())}).await.unwrap();
        assert!(agenda.tick_once(10,100).await.is_err());
        assert_eq!(host.orchestrator.notifications("agenda").await.unwrap().iter().filter(|n|n.content=="reminder crash").count(),1);
    });
    std::process::exit(73);
}
#[test]
fn post_commit_pre_ack_reopens_with_exactly_one_notice() {
    let temp = tempfile::tempdir().unwrap();
    std::fs::create_dir(temp.path().join("project")).unwrap();
    let rt = RuntimeSet::new(2, 1).unwrap();
    let db = Database::open(
        temp.path().join("main.db"),
        DatabaseKind::Main,
        Default::default(),
    )
    .unwrap();
    let host = crash_host(temp.path(), &rt, db.clone());
    rt.block_on(async {
        let receipt = host
            .master
            .receive(Incoming {
                conversation: "agenda".into(),
                client_id: "client".into(),
                client_msg_id: "original".into(),
                text: "reminder".into(),
                scope: Some("project:default".into()),
                timezone: "UTC".into(),
                channel: "fixture".into(),
                reply_to: None,
                attachments: vec![],
            })
            .await
            .unwrap();
        host.master.stop_turn("agenda").await.unwrap();
        let binding = HostNotificationBinding {
            principal: HostOptions::project(temp.path().join("project"))
                .master_config
                .principal,
            scope: "project:default".into(),
            conversation: "agenda".into(),
            source: receipt.message_id,
            approved_channels: vec![],
        };
        Agenda::open(host.orchestrator.clone(), binding)
            .await
            .unwrap()
            .apply("create".into(), definition("crash"))
            .await
            .unwrap();
    });
    rt.block_on(host.close()).unwrap();
    drop(db);
    let status = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--ignored", "--exact", "agenda_post_notice_pre_ack_helper"])
        .env("EXCALIBUR_AGENDA_CRASH", temp.path())
        .status()
        .unwrap();
    assert_eq!(status.code(), Some(73));
    let db = Database::open(
        temp.path().join("main.db"),
        DatabaseKind::Main,
        Default::default(),
    )
    .unwrap();
    let host = crash_host(temp.path(), &rt, db.clone());
    rt.block_on(async {
        db.write(Priority::Journal, |c| {
            c.execute_batch("DROP TRIGGER interrupt_agenda_ack")?;
            Ok(())
        })
        .await
        .unwrap();
        let principal = HostOptions::project(temp.path().join("project"))
            .master_config
            .principal;
        let binding = host
            .orchestrator
            .host_notification_bindings(&principal)
            .await
            .unwrap()
            .remove(0);
        let agenda = Agenda::open(host.orchestrator.clone(), binding)
            .await
            .unwrap();
        let before = host
            .orchestrator
            .notifications("agenda")
            .await
            .unwrap()
            .into_iter()
            .find(|n| n.content == "reminder crash")
            .unwrap();
        assert_eq!(agenda.tick_once(99, 100).await.unwrap(), 1);
        assert_eq!(agenda.tick_once(99, 100).await.unwrap(), 0);
        let notices = host
            .orchestrator
            .notifications("agenda")
            .await
            .unwrap()
            .into_iter()
            .filter(|n| n.content == "reminder crash")
            .collect::<Vec<_>>();
        assert_eq!(notices.len(), 1);
        assert_eq!(notices[0].id, before.id);
        assert_eq!(notices[0].created, before.created);
        assert!(!notices[0].acknowledged);
        deliver_notifications(&host.master, &host.orchestrator, 100)
            .await
            .unwrap();
        deliver_notifications(&host.master, &host.orchestrator, 100)
            .await
            .unwrap();
        assert_eq!(
            db.read(ReadLane::Interactive, |c| Ok(c.query_row(
                "SELECT count(*) FROM master_notices",
                [],
                |r| r.get::<_, i64>(0)
            )?))
            .await
            .unwrap(),
            1
        );
    });
    rt.block_on(host.close()).unwrap();
}

#[test]
fn durable_sink_guards_survive_reopen_and_legacy_flush_cannot_bypass_them() {
    let temp = tempfile::tempdir().unwrap();
    std::fs::create_dir(temp.path().join("project")).unwrap();
    let rt = RuntimeSet::new(2, 1).unwrap();
    let db = Database::open(
        temp.path().join("main.db"),
        DatabaseKind::Main,
        Default::default(),
    )
    .unwrap();
    let host = crash_host(temp.path(), &rt, db.clone());
    let (binding, revoked_source) = rt.block_on(async {
        let principal = HostOptions::project(temp.path().join("project"))
            .master_config
            .principal;
        let mut sources = vec![];
        for msg in ["first", "revoked"] {
            let receipt = host
                .master
                .receive(Incoming {
                    conversation: "agenda".into(),
                    client_id: "client".into(),
                    client_msg_id: msg.into(),
                    text: "reminder".into(),
                    scope: Some("project:default".into()),
                    timezone: "UTC".into(),
                    channel: "fixture".into(),
                    reply_to: None,
                    attachments: vec![],
                })
                .await
                .unwrap();
            host.master.stop_turn("agenda").await.unwrap();
            sources.push(receipt.message_id);
        }
        let binding = HostNotificationBinding {
            principal,
            scope: "project:default".into(),
            conversation: "agenda".into(),
            source: sources[0].clone(),
            approved_channels: vec![],
        };
        let agenda = Agenda::open(host.orchestrator.clone(), binding.clone())
            .await
            .unwrap();
        for id in ["completed-before-flush", "valid"] {
            agenda
                .apply(format!("create-{id}"), definition(id))
                .await
                .unwrap();
        }
        let mut revoke_binding = binding.clone();
        revoke_binding.source = sources[1].clone();
        Agenda::open(host.orchestrator.clone(), revoke_binding)
            .await
            .unwrap()
            .apply("create-revoked".into(), definition("revoked-before-flush"))
            .await
            .unwrap();
        assert_eq!(agenda.tick_once(10, 100).await.unwrap(), 3);
        for delivery in host.orchestrator.pending_deliveries(100).await.unwrap() {
            excalibur_daemon::command::deliver_notification(
                &host.master,
                &host.orchestrator,
                &delivery,
            )
            .await
            .unwrap();
        }
        // Sink insertion is already a durable visible event. The later grouped
        // publication is an independent effect that needs its own durable guard.
        assert_eq!(
            db.read(ReadLane::Interactive, |c| Ok(c.query_row(
                "SELECT count(*) FROM master_notice_guards",
                [],
                |r| r.get::<_, i64>(0)
            )?))
            .await
            .unwrap(),
            3
        );
        (binding, sources[1].clone())
    });
    rt.block_on(host.close()).unwrap();
    drop(db);
    let db = Database::open(
        temp.path().join("main.db"),
        DatabaseKind::Main,
        Default::default(),
    )
    .unwrap();
    let host = crash_host(temp.path(), &rt, db.clone());
    rt.block_on(async {
        let agenda = Agenda::open(host.orchestrator.clone(), binding)
            .await
            .unwrap();
        agenda
            .apply(
                "complete-after-reopen".into(),
                Operation::Complete {
                    id: "completed-before-flush".into(),
                    generation: 1,
                },
            )
            .await
            .unwrap();
        db.write(Priority::Journal, move |c| {
            c.execute(
                "DELETE FROM master_client_receipts WHERE message_id=?1",
                [revoked_source],
            )?;
            Ok(())
        })
        .await
        .unwrap();
        assert_eq!(host.master.flush_notices(100).await.unwrap(), 0);
        assert_eq!(
            db.read(ReadLane::Interactive, |c| Ok(c.query_row(
                "SELECT count(*) FROM master_events WHERE kind='notification_group'",
                [],
                |r| r.get::<_, i64>(0)
            )?))
            .await
            .unwrap(),
            0
        );
        let old_key = db
            .read(ReadLane::Interactive, |c| {
                Ok(
                    c.query_row("SELECT id FROM master_notices LIMIT 1", [], |r| {
                        r.get::<_, String>(0)
                    })?,
                )
            })
            .await
            .unwrap();
        assert!(
            host.master
                .publish_notice(
                    &old_key,
                    "agenda",
                    serde_json::json!({"scope":"project:default","notification_id":"forged"}),
                    "normal"
                )
                .await
                .is_err()
        );
        assert!(
            host.master
                .publish_notice_checked(
                    "unknown",
                    "agenda",
                    serde_json::json!({"scope":"project:default"}),
                    "normal",
                    "unknown-origin",
                    |c| excalibur_orchestrator::host_notification_effect_allowed(
                        c,
                        "unknown-origin",
                        100
                    )
                )
                .await
                .is_err()
        );
        assert_eq!(
            host.master
                .flush_notices_checked(100, |c, id| {
                    excalibur_orchestrator::host_notification_effect_allowed(c, id, 100)
                })
                .await
                .unwrap(),
            1
        );
        assert_eq!(
            host.master
                .flush_notices_checked(100, |c, id| {
                    excalibur_orchestrator::host_notification_effect_allowed(c, id, 100)
                })
                .await
                .unwrap(),
            0
        );
        let (group, pending, receipts) = db
            .read(ReadLane::Interactive, |c| {
                Ok((
                    c.query_row(
                        "SELECT payload FROM master_events WHERE kind='notification_group'",
                        [],
                        |r| r.get::<_, String>(0),
                    )?,
                    c.query_row(
                        "SELECT count(*) FROM master_notice_outbox WHERE flushed=0",
                        [],
                        |r| r.get::<_, i64>(0),
                    )?,
                    c.query_row("SELECT count(*) FROM master_notices", [], |r| {
                        r.get::<_, i64>(0)
                    })?,
                ))
            })
            .await
            .unwrap();
        assert!(group.contains("reminder valid"));
        assert!(!group.contains("completed-before-flush"));
        assert!(!group.contains("revoked-before-flush"));
        assert_eq!(pending, 2);
        assert_eq!(receipts, 3);
    });
    rt.block_on(host.close()).unwrap();
}
