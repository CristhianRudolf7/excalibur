//! Daemon-owned automatic Agenda runtime against the real host, guarded
//! notification path and local master. Clocks are injected; no network.
use excalibur_daemon::{
    DaemonHost, HostOptions, RuntimeSet,
    agenda::{Agenda, AgendaClock, AgendaRuntime, ManualClock},
};
use excalibur_master::Incoming;
use excalibur_models::{ModelError, ModelGateway};
use excalibur_orchestrator::{HostNotificationBinding, Notification};
use excalibur_scheduler::{
    Timing,
    reminders::{Definition, Operation, Reminders},
};
use excalibur_store::{Database, DatabaseKind, Priority};
use excalibur_types::runtime::{BoxFuture, ModelRequest, ModelResponse};
use excalibur_vault::Redactor;
use std::{sync::Arc, time::Duration};
struct Offline;
impl ModelGateway for Offline {
    fn complete(&self, _: ModelRequest) -> BoxFuture<'_, Result<ModelResponse, ModelError>> {
        Box::pin(async { Err(ModelError::Provider) })
    }
}
const TOKEN: &str = "synthetic-agenda-runtime-token-0000000";

fn open_host(rt: &RuntimeSet, db: &Database, root: &std::path::Path) -> DaemonHost {
    rt.block_on(DaemonHost::open(
        db.clone(),
        HostOptions::project(root.to_path_buf()),
        Arc::new(Offline),
        None,
        TOKEN,
        Redactor::default(),
        rt.work(),
    ))
    .unwrap()
}
async fn accepted(host: &DaemonHost, id: &str) -> HostNotificationBinding {
    let receipt = host
        .master
        .receive(Incoming {
            conversation: "agenda".into(),
            client_id: "client".into(),
            client_msg_id: id.into(),
            text: "literal reminder request".into(),
            scope: Some("project:default".into()),
            timezone: "UTC".into(),
            channel: "fixture".into(),
            reply_to: None,
            attachments: vec![],
        })
        .await
        .unwrap();
    host.master.stop_turn("agenda").await.unwrap();
    HostNotificationBinding {
        principal: HostOptions::project(".".into()).master_config.principal,
        scope: "project:default".into(),
        conversation: "agenda".into(),
        source: receipt.message_id,
        approved_channels: vec![],
    }
}
fn create(id: &str, timing: Timing) -> Operation {
    Operation::Create(Definition {
        id: id.into(),
        text: format!("reminder {id}"),
        timing,
        preferred_channel: None,
        task: None,
    })
}
async fn notices(host: &DaemonHost) -> Vec<Notification> {
    // Recovery summaries share the conversation; count reminder notices only.
    let mut all = host.orchestrator.notifications("agenda").await.unwrap();
    all.retain(|n| n.content.starts_with("reminder "));
    all
}
async fn wait_for(host: &DaemonHost, count: usize) -> Vec<Notification> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    loop {
        let current = notices(host).await;
        if current.len() >= count {
            return current;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "expected {count} notices, have {}",
            current.len()
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}
/// Wait until the supervised runtime has no queued wake, then report passes.
async fn settled(runtime: &AgendaRuntime) -> u64 {
    loop {
        let before = runtime.passes();
        tokio::time::sleep(Duration::from_millis(2500)).await;
        if runtime.passes() == before {
            return before;
        }
    }
}
fn with_text<'a>(all: &'a [Notification], id: &str) -> Vec<&'a Notification> {
    let text = format!("reminder {id}");
    all.iter().filter(|n| n.content == text).collect()
}

#[test]
fn startup_policy_min_wake_mutations_safe_mode_and_shutdown() {
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
    let host = open_host(&rt, &db, &root);
    let base = excalibur_store::unix_seconds();
    rt.block_on(async {
        let binding = accepted(&host, "creator").await;
        let agenda = Agenda::open(host.orchestrator.clone(), binding.clone())
            .await
            .unwrap();
        // Ten intervals were missed while no runtime existed.
        agenda
            .apply(
                "c-every".into(),
                create(
                    "every",
                    Timing::Every {
                        first: base - 1000,
                        seconds: 100,
                    },
                ),
            )
            .await
            .unwrap();
        agenda
            .apply("c-later".into(), create("later", Timing::At(base + 50)))
            .await
            .unwrap();
        let clock = ManualClock::new(base);
        let runtime = AgendaRuntime::new(host.orchestrator.clone(), clock.clone());
        let first = runtime.step().await.unwrap();
        assert!(first.catch_up && !first.blocked);
        assert_eq!((first.recorded, first.queued), (1, 1), "run_once_now");
        assert_eq!(first.wake_at, base + 50, "min(next_fire_utc)");
        let all = notices(&host).await;
        let every = with_text(&all, "every");
        assert_eq!(every.len(), 1);
        assert_eq!(every[0].created, base, "late delivery keeps scheduled time");
        let again = runtime.step().await.unwrap();
        assert!(!again.catch_up);
        assert_eq!((again.recorded, again.queued), (0, 0));

        // Supervised task: wakes at the earliest timer on the injected clock.
        let (stop, stopping) = tokio::sync::watch::channel(false);
        let shutdown = Arc::new(tokio::sync::Notify::new());
        let task = runtime.clone().spawn(
            &tokio::runtime::Handle::current(),
            shutdown.clone(),
            stopping,
        );
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(notices(&host).await.len(), 1, "nothing due before wake");
        clock.set(base + 50);
        let all = wait_for(&host, 2).await;
        assert_eq!(with_text(&all, "later")[0].created, base + 50);

        // A mutation wakes the sleeping runtime without advancing the clock.
        agenda
            .apply("c-now".into(), create("now", Timing::At(base + 50)))
            .await
            .unwrap();
        wait_for(&host, 3).await;

        // Complete and delete also wake the runtime (no clock movement; the
        // next durable deadline is far away on the manual clock).
        for (key, operation) in [
            (
                "done-later",
                Operation::Complete {
                    id: "later".into(),
                    generation: 1,
                },
            ),
            ("c-spare", create("spare", Timing::At(base + 100_000))),
            (
                "d-spare",
                Operation::Delete {
                    id: "spare".into(),
                    generation: 1,
                },
            ),
        ] {
            let before = settled(&runtime).await;
            agenda.apply(key.into(), operation).await.unwrap();
            let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
            while runtime.passes() == before {
                assert!(tokio::time::Instant::now() < deadline, "{key} did not wake");
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }

        // Snooze moves to a new generation; dispatch still uses the creator binding.
        let later = accepted(&host, "snoozer").await;
        let later_agenda = Agenda::open(host.orchestrator.clone(), later.clone())
            .await
            .unwrap();
        later_agenda
            .apply(
                "c-snoozed".into(),
                create("snoozed", Timing::At(base + 10_000)),
            )
            .await
            .unwrap();
        agenda
            .apply(
                "s-snoozed".into(),
                Operation::Snooze {
                    id: "snoozed".into(),
                    generation: 1,
                    timing: Timing::At(base + 60),
                },
            )
            .await
            .unwrap();
        clock.set(base + 60);
        let all = wait_for(&host, 4).await;
        let snoozed = with_text(&all, "snoozed")[0].id.clone();
        let origin = host
            .orchestrator
            .host_notification_origin(&snoozed)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(origin.source, later.source, "creator source kept on snooze");
        for notice in &all {
            assert!(
                host.orchestrator
                    .host_notification_origin(&notice.id)
                    .await
                    .unwrap()
                    .is_some(),
                "every automatic notice carries a host receipt"
            );
        }

        // Safe mode: due fires are recorded, never admitted, then released.
        let safe_at = clock.now();
        for offset in 0..3 {
            let boot = host
                .orchestrator
                .recovery()
                .begin(safe_at - 2 + offset)
                .await
                .unwrap();
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
        clock.set(base + 100);
        let reminders = Reminders::open(
            db.clone(),
            binding.scope.clone(),
            binding.principal.clone(),
            binding.conversation.clone(),
            None,
        )
        .await
        .unwrap();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
        while reminders.pending(10).await.unwrap().is_empty() {
            assert!(tokio::time::Instant::now() < deadline, "fire not recorded");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        // A second interval elapses while still safe: recorded, then coalesced.
        clock.set(base + 300);
        let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
        while !reminders
            .pending(10)
            .await
            .unwrap()
            .iter()
            .any(|p| p.fire.scheduled_at == base + 300)
        {
            assert!(tokio::time::Instant::now() < deadline, "fire not recorded");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        // The runtime itself does not attempt admission while blocked; this
        // does not rely on the notification writer's own readiness check.
        let blocked = runtime.step().await.unwrap();
        assert!(blocked.blocked && blocked.catch_up);
        assert_eq!((blocked.attempted, blocked.queued), (0, 0));
        assert_eq!(reminders.pending(10).await.unwrap().len(), 1, "coalesced");
        assert!(
            reminders
                .dead_letters(10)
                .await
                .unwrap()
                .iter()
                .any(|d| d.reason == "superseded")
        );
        tokio::time::sleep(Duration::from_millis(1500)).await;
        assert_eq!(notices(&host).await.len(), 4, "safe mode admits nothing");
        host.orchestrator
            .recovery()
            .exit_safe_mode("manual-exit", base + 300)
            .await
            .unwrap();
        clock.advance(excalibur_daemon::agenda::BLOCKED_RECHECK_SECONDS);
        let all = wait_for(&host, 5).await;
        let every = with_text(&all, "every");
        assert_eq!(every.len(), 2, "safe-mode backlog admitted once");
        assert!(every.iter().any(|n| n.created == base + 300));
        tokio::time::sleep(Duration::from_millis(1500)).await;
        assert_eq!(notices(&host).await.len(), 5, "no burst after safe mode");
        assert!(reminders.pending(10).await.unwrap().is_empty());

        // Shutdown cancels the wait and returns cleanly without a panic signal.
        stop.send(true).unwrap();
        tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(10), shutdown.notified())
                .await
                .is_err()
        );
    });
    rt.block_on(host.close()).unwrap();
}

#[test]
fn restart_and_concurrent_runtimes_deduplicate_and_forged_namespaces_stay_inert() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("project");
    std::fs::create_dir(&root).unwrap();
    let path = temp.path().join("main.db");
    let rt = RuntimeSet::new(2, 1).unwrap();
    let base = excalibur_store::unix_seconds();
    let db = Database::open(&path, DatabaseKind::Main, Default::default()).unwrap();
    let host = open_host(&rt, &db, &root);
    let binding = rt.block_on(async {
        let binding = accepted(&host, "creator").await;
        let agenda = Agenda::open(host.orchestrator.clone(), binding.clone())
            .await
            .unwrap();
        agenda
            .apply(
                "c-every".into(),
                create(
                    "every",
                    Timing::Every {
                        first: base - 1000,
                        seconds: 100,
                    },
                ),
            )
            .await
            .unwrap();
        let clock = ManualClock::new(base);
        let step = AgendaRuntime::new(host.orchestrator.clone(), clock)
            .step()
            .await
            .unwrap();
        assert_eq!((step.recorded, step.queued), (1, 1));
        // Reproduce the crash window: the notice committed, the scheduler
        // acknowledgement was lost before the process died.
        let reset = db
            .write(Priority::Journal, |c| {
                Ok(c.execute("UPDATE scheduler_fires SET delivered=0", [])?)
            })
            .await
            .unwrap();
        assert_eq!(reset, 1);
        binding
    });
    rt.block_on(host.close()).unwrap();
    drop(db);

    let db = Database::open(&path, DatabaseKind::Main, Default::default()).unwrap();
    let host = open_host(&rt, &db, &root);
    rt.block_on(async {
        // Restart later: the runtime replays the existing notice by key and
        // completes the lost acknowledgement without a second notice.
        let clock = ManualClock::new(base + 50);
        let restarted = AgendaRuntime::new(host.orchestrator.clone(), clock.clone());
        let step = restarted.step().await.unwrap();
        assert!(step.catch_up);
        assert_eq!((step.recorded, step.attempted, step.queued), (0, 1, 1));
        assert_eq!(notices(&host).await.len(), 1, "replayed, not duplicated");
        let reminders = Reminders::open(
            db.clone(),
            binding.scope.clone(),
            binding.principal.clone(),
            binding.conversation.clone(),
            None,
        )
        .await
        .unwrap();
        assert!(reminders.pending(10).await.unwrap().is_empty(), "ack done");
        assert_eq!(
            restarted.step().await.unwrap().attempted,
            0,
            "no further attempt once acknowledged"
        );

        // A namespace whose metadata does not derive to its sealed scope is
        // never opened, ticked or admitted, even with a valid binding body.
        let forged = serde_json::to_string(&binding).unwrap();
        db.write(Priority::Journal, move |c| {
            let definition = serde_json::json!({"id":"forged-timer","scope":"reminder:forged","timing":{"At":0},"missed":"RunOnceNow","payload":null}).to_string();
            c.execute("INSERT INTO scheduler_timers(scope,id,definition,next_at) VALUES('reminder:forged','forged-timer',?1,0)", [definition])?;
            c.execute("INSERT INTO scheduler_reminder_timers VALUES('reminder:forged','forged-timer','forged',1)", [])?;
            c.execute("INSERT INTO scheduler_reminder_metadata VALUES('reminder:forged','forged',1,?1)", [forged])?;
            Ok(())
        })
        .await
        .unwrap();

        // Two independent runtimes race on the next interval: one notice.
        clock.set(base + 100);
        let other = AgendaRuntime::new(host.orchestrator.clone(), clock.clone());
        let (a, b) = tokio::join!(restarted.step(), other.step());
        let (a, b) = (a.unwrap(), b.unwrap());
        assert_eq!(a.recorded + b.recorded, 1);
        let all = notices(&host).await;
        assert_eq!(with_text(&all, "every").len(), 2);
        assert_eq!(all.len(), 2, "forged namespace produced nothing");
        let forged_next: Option<i64> = db
            .read(excalibur_store::ReadLane::Interactive, |c| {
                Ok(c.query_row(
                    "SELECT next_at FROM scheduler_timers WHERE scope='reminder:forged'",
                    [],
                    |r| r.get(0),
                )?)
            })
            .await
            .unwrap();
        assert_eq!(forged_next, Some(0), "forged timer never ticked");
        assert!(a.wake_at > base + 100 && b.wake_at > base + 100);
    });
    rt.block_on(host.close()).unwrap();
}
