//! Failure isolation, backlog bounds, paging and clock regressions for the
//! automatic Agenda runtime (audit AR-1, AR-2, AR-3, AR-7). Local fixtures,
//! offline provider, injected clocks.
use excalibur_daemon::{
    DaemonHost, HostOptions, RuntimeSet,
    agenda::{
        Agenda, AgendaClock, AgendaRuntime, IDLE_RECHECK_SECONDS, MAX_NAMESPACE_BACKOFF_SECONDS,
        ManualClock, SystemClock,
    },
};
use excalibur_master::Incoming;
use excalibur_models::{ModelError, ModelGateway};
use excalibur_orchestrator::HostNotificationBinding;
use excalibur_scheduler::{
    Timing,
    reminders::{self, Definition, Operation, Reminders},
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
const TOKEN: &str = "synthetic-agenda-isolation-token-000000";

struct Fixture {
    _temp: tempfile::TempDir,
    rt: RuntimeSet,
    db: Database,
    host: DaemonHost,
}
fn fixture() -> Fixture {
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
    let host = rt
        .block_on(DaemonHost::open(
            db.clone(),
            HostOptions::project(root),
            Arc::new(Offline),
            None,
            TOKEN,
            Redactor::default(),
            rt.work(),
        ))
        .unwrap();
    Fixture {
        _temp: temp,
        rt,
        db,
        host,
    }
}
async fn bind(host: &DaemonHost, conversation: &str) -> (HostNotificationBinding, Agenda) {
    let receipt = host
        .master
        .receive(Incoming {
            conversation: conversation.into(),
            client_id: "client".into(),
            client_msg_id: format!("m-{conversation}"),
            text: "literal reminder request".into(),
            scope: Some("project:default".into()),
            timezone: "UTC".into(),
            channel: "fixture".into(),
            reply_to: None,
            attachments: vec![],
        })
        .await
        .unwrap();
    host.master.stop_turn(conversation).await.unwrap();
    let binding = HostNotificationBinding {
        principal: HostOptions::project(".".into()).master_config.principal,
        scope: "project:default".into(),
        conversation: conversation.into(),
        source: receipt.message_id,
        approved_channels: vec![],
    };
    let agenda = Agenda::open(host.orchestrator.clone(), binding.clone())
        .await
        .unwrap();
    (binding, agenda)
}
async fn handle(db: &Database, binding: &HostNotificationBinding) -> Reminders {
    Reminders::open(
        db.clone(),
        binding.scope.clone(),
        binding.principal.clone(),
        binding.conversation.clone(),
        None,
    )
    .await
    .unwrap()
}
fn create(id: &str, text: &str, timing: Timing) -> Operation {
    Operation::Create(Definition {
        id: id.into(),
        text: text.into(),
        timing,
        preferred_channel: None,
        task: None,
    })
}
async fn reminder_notices(host: &DaemonHost, conversation: &str) -> usize {
    host.orchestrator
        .notifications(conversation)
        .await
        .unwrap()
        .iter()
        .filter(|n| n.content.starts_with("reminder "))
        .count()
}

#[test]
fn poisoned_fire_is_dead_lettered_without_starving_other_fires_or_namespaces() {
    let f = fixture();
    let base = excalibur_store::unix_seconds();
    let secret = "reminder change wifi password: hunter2";
    f.rt.block_on(async {
        let mut bound = Vec::new();
        for i in 0..8 {
            bound.push(bind(&f.host, &format!("conv{i}")).await);
        }
        // Poison the namespace that sorts first in the runtime index.
        bound.sort_by_key(|(b, _)| {
            reminders::namespace_of(&b.scope, &b.principal, &b.conversation, None).unwrap()
        });
        let (first, first_agenda) = &bound[0];
        // The host API rejects redactor-refused text up front.
        assert!(
            first_agenda
                .apply(
                    "p-direct".into(),
                    create("direct", secret, Timing::At(base - 30))
                )
                .await
                .is_err()
        );
        // A row stored before that check existed (legacy path): the earliest
        // fire of the first namespace, ahead of an ordinary fire.
        handle(&f.db, first)
            .await
            .apply_host(
                "p".into(),
                create("poison", secret, Timing::At(base - 20)),
                Some(serde_json::to_value(first).unwrap()),
            )
            .await
            .unwrap();
        for (i, (b, agenda)) in bound.iter().enumerate() {
            agenda
                .apply(
                    format!("v{i}"),
                    create(
                        "victim",
                        &format!("reminder victim {}", b.conversation),
                        Timing::At(base - 10),
                    ),
                )
                .await
                .unwrap();
        }
        let runtime = AgendaRuntime::new(f.host.orchestrator.clone(), ManualClock::new(base));
        let step = runtime.step().await.unwrap();
        assert_eq!(step.failed_namespaces, 0);
        assert_eq!(step.queued, 8, "every namespace admitted");
        assert_eq!(step.dead_lettered, 1);
        for (b, _) in &bound {
            assert_eq!(reminder_notices(&f.host, &b.conversation).await, 1);
            let all = f
                .host
                .orchestrator
                .notifications(&b.conversation)
                .await
                .unwrap();
            assert!(!all.iter().any(|n| n.content.contains("hunter2")));
        }
        let poisoned = handle(&f.db, first).await;
        let letters = poisoned.dead_letters(10).await.unwrap();
        assert_eq!(letters.len(), 1);
        assert_eq!(letters[0].reason, "content_rejected");
        assert!(!format!("{letters:?}").contains("hunter2"));
        // The retired fire is not retried: later passes are clean and quiet.
        let again = runtime.step().await.unwrap();
        assert_eq!(
            (
                again.attempted,
                again.dead_lettered,
                again.failed_namespaces
            ),
            (0, 0, 0)
        );
        assert!(poisoned.pending(10).await.unwrap().is_empty());
    });
    f.rt.block_on(f.host.close()).unwrap();
}

#[test]
fn more_than_one_index_page_of_namespaces_is_dispatched() {
    let f = fixture();
    let base = excalibur_store::unix_seconds();
    f.rt.block_on(async {
        let mut conversations = Vec::new();
        for i in 0..260 {
            let conversation = format!("page{i:03}");
            let (_, agenda) = bind(&f.host, &conversation).await;
            agenda
                .apply(
                    "c".into(),
                    create("page", "reminder page", Timing::At(base - 5)),
                )
                .await
                .unwrap();
            conversations.push(conversation);
        }
        let runtime = AgendaRuntime::new(f.host.orchestrator.clone(), ManualClock::new(base));
        let step = runtime.step().await.unwrap();
        assert_eq!((step.recorded, step.queued), (260, 260));
        for conversation in &conversations {
            assert_eq!(reminder_notices(&f.host, conversation).await, 1);
        }
    });
    f.rt.block_on(f.host.close()).unwrap();
}

#[test]
fn undeliverable_recurring_fires_stay_bounded_and_restore_without_burst() {
    let f = fixture();
    let base = excalibur_store::unix_seconds();
    f.rt.block_on(async {
        let (binding, agenda) = bind(&f.host, "agenda").await;
        agenda
            .apply(
                "c".into(),
                create(
                    "often",
                    "reminder often",
                    Timing::Every {
                        first: base,
                        seconds: 10,
                    },
                ),
            )
            .await
            .unwrap();
        // Revoke the creator's accepted source.
        let (source, owner) = (binding.source.clone(), binding.principal.clone());
        f.db.write(Priority::Journal, move |c| {
            c.execute(
                "INSERT INTO principals(id) VALUES('revoked-source-owner')",
                [],
            )?;
            c.execute(
                "UPDATE messages SET principal_id='revoked-source-owner' WHERE id=?1",
                [source],
            )?;
            Ok(())
        })
        .await
        .unwrap();
        let clock = ManualClock::new(base);
        let runtime = AgendaRuntime::new(f.host.orchestrator.clone(), clock.clone());
        let reminders = handle(&f.db, &binding).await;
        for _ in 0..40 {
            let step = runtime.step().await.unwrap();
            assert_eq!(step.queued, 0);
            assert!(reminders.pending(100).await.unwrap().len() <= 1, "bounded");
            clock.advance(10);
        }
        let retired = reminders.dead_letters(1000).await.unwrap();
        assert_eq!(retired.len(), 39);
        assert!(retired.iter().all(|d| d.reason == "superseded"));
        // Between new fires, the deferred namespace is not retried every pass.
        let idle = runtime.step().await.unwrap();
        assert_eq!(idle.attempted, 0);
        let source = binding.source.clone();
        f.db.write(Priority::Journal, move |c| {
            c.execute(
                "UPDATE messages SET principal_id=?2 WHERE id=?1",
                [source, owner],
            )?;
            Ok(())
        })
        .await
        .unwrap();
        // Authority restored: one notice for the latest interval, no burst,
        // within the documented maximum backoff.
        clock.advance(MAX_NAMESPACE_BACKOFF_SECONDS);
        let restored = runtime.step().await.unwrap();
        assert_eq!(restored.queued, 1);
        assert_eq!(reminder_notices(&f.host, "agenda").await, 1);
        assert!(reminders.pending(100).await.unwrap().is_empty());
        let quiet = runtime.step().await.unwrap();
        assert_eq!((quiet.queued, quiet.attempted), (0, 0));
    });
    f.rt.block_on(f.host.close()).unwrap();
}

#[test]
fn a_backwards_clock_step_ends_the_wait_for_reevaluation() {
    let f = fixture();
    let base = excalibur_store::unix_seconds();
    f.rt.block_on(async {
        let clock = ManualClock::new(base);
        let runtime = AgendaRuntime::new(f.host.orchestrator.clone(), clock.clone());
        let step = runtime.step().await.unwrap();
        assert_eq!(step.wake_at, base + IDLE_RECHECK_SECONDS);
        let waiting = tokio::spawn({
            let runtime = runtime.clone();
            async move { runtime.wait(step.wake_at).await }
        });
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(!waiting.is_finished());
        // Without re-evaluation the wait would last until the old deadline,
        // 1060 seconds later on the new timeline.
        clock.set(base - 1000);
        tokio::time::timeout(Duration::from_secs(5), waiting)
            .await
            .expect("backwards step must end the wait")
            .unwrap();
        let after = runtime.step().await.unwrap();
        assert_eq!(after.wake_at, base - 1000 + IDLE_RECHECK_SECONDS);
    });
    f.rt.block_on(f.host.close()).unwrap();
}

#[tokio::test(start_paused = true)]
async fn system_clock_sleep_is_bounded_by_the_recheck_interval() {
    let clock = SystemClock;
    let start = tokio::time::Instant::now();
    // A deadline far ahead, as after the wall clock stepped back by hours.
    clock.sleep_until(clock.now() + 10_000).await;
    assert!(start.elapsed() <= Duration::from_secs(IDLE_RECHECK_SECONDS as u64 + 1));
}
