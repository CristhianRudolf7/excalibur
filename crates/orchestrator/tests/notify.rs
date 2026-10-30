mod support;
use excalibur_orchestrator::*;
use support::*;
#[tokio::test]
async fn urgent_cross_conversation_normal_batch_quiet_hours_and_unique_ack() {
    let f = fixture().await;
    let p = f
        .host
        .spawn(&f.master.id, delegation("p", "general", 300))
        .await
        .unwrap();
    f.host
        .set_notification_policy(
            "conversation:test",
            NotificationPolicy {
                preferred_channel: Some("synthetic:preferred".into()),
                quiet_start_minute: Some(0),
                quiet_end_minute: Some(60),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    f.host
        .connection_activity("conversation:other", "synthetic:web", true, 20)
        .await
        .unwrap();
    let normal = f
        .host
        .notify("normal", &p.id, "normal", "Done", 10)
        .await
        .unwrap();
    let urgent = f
        .host
        .notify("urgent", &p.id, "urgent", "Needs approval", 10)
        .await
        .unwrap();
    let routes = f.host.pending_deliveries(20).await.unwrap();
    assert_eq!(routes.len(), 3);
    assert!(routes.iter().all(|d| d.notification.id == urgent.id));
    assert!(
        routes
            .iter()
            .any(|d| d.cross_conversation.as_deref() == Some("conversation:other"))
    );
    for r in &routes {
        f.host
            .confirm_delivery(&r.notification.id, &r.channel)
            .await
            .unwrap();
    }
    assert!(f.host.pending_deliveries(20).await.unwrap().is_empty());
    f.host.acknowledge_notification(&urgent.id).await.unwrap();
    let routes = f.host.pending_deliveries(3601).await.unwrap();
    assert_eq!(routes.len(), 2);
    assert!(routes.iter().all(|d| d.notification.id == normal.id));
    f.host.acknowledge_notification(&normal.id).await.unwrap();
    assert!(f.host.pending_deliveries(3602).await.unwrap().is_empty());
}

struct StopAtAdmission {
    journal: excalibur_journal::Journal,
    state: &'static str,
    reason: &'static str,
}
impl RunAuthority for StopAtAdmission {
    fn bind<'a>(
        &'a self,
        agent: &'a Agent,
    ) -> excalibur_types::runtime::BoxFuture<'a, std::result::Result<(), String>> {
        Box::pin(async move {
            let run = agent.run_id.as_ref().ok_or("run required")?;
            self.journal
                .set_state_if(run, &["ready"], "running", None, "fixture-admitted")
                .await
                .map_err(|e| e.to_string())?;
            self.journal
                .set_state_if(
                    run,
                    &["running"],
                    self.state,
                    Some(self.reason.into()),
                    "fixture-human-stop",
                )
                .await
                .map_err(|e| e.to_string())?;
            Ok(())
        })
    }
}
#[tokio::test]
async fn reaper_delivers_human_and_budget_stops_urgently_but_not_ordinary_waits() {
    for (state, reason, class) in [
        ("paused", "approval required", Some("urgent")),
        ("waiting", "user", Some("urgent")),
        ("waiting", "approval", Some("urgent")),
        ("paused", "budget", Some("urgent")),
        (
            "paused",
            "wall time budget exhausted during verification",
            Some("urgent"),
        ),
        ("paused", "user", Some("normal")),
        ("waiting", "children", None),
        ("waiting", "tool", None),
    ] {
        let f = fixture().await;
        let host = f
            .host
            .clone()
            .with_authority(std::sync::Arc::new(StopAtAdmission {
                journal: f.host.engine().journal().clone(),
                state,
                reason,
            }));
        let agent = host
            .spawn(&f.master.id, delegation("p", "general", 300))
            .await
            .unwrap();
        assert_eq!(host.tick().await.unwrap(), vec![agent.id]);
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            loop {
                if host.reap().await.unwrap() == 1 {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let notices = host.notifications("conversation:test").await.unwrap();
        assert_eq!(
            notices.len(),
            usize::from(class.is_some()),
            "{state}/{reason}"
        );
        assert_eq!(host.reap().await.unwrap(), 0);
        if let Some(class) = class {
            let n = &notices[0];
            assert_eq!(n.class, class, "{state}/{reason}");
            let immediate = host.pending_deliveries(n.created).await.unwrap();
            assert_eq!(
                immediate.len(),
                usize::from(class == "urgent"),
                "{state}/{reason}"
            );
            if class == "urgent" {
                let retry = host.pending_deliveries(n.created).await.unwrap();
                assert_eq!(retry[0].notification.id, immediate[0].notification.id);
                host.confirm_delivery(&n.id, &immediate[0].channel)
                    .await
                    .unwrap();
                assert!(host.pending_deliveries(n.created).await.unwrap().is_empty());
            }
            let reopened =
                Orchestrator::open(f.db.clone(), host.engine().clone(), Limits::default())
                    .await
                    .unwrap();
            assert_eq!(reopened.reap().await.unwrap(), 0);
            let persisted = reopened.notifications("conversation:test").await.unwrap();
            assert_eq!(persisted.len(), 1);
            assert_eq!(persisted[0].id, n.id);
        }
        host.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn n3_progress_defaults_on_and_explicit_conversation_and_task_policies_take_precedence() {
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
    f.db.write(excalibur_store::Priority::Journal, |c| {
        c.execute("INSERT INTO tasks(id,level,scope_id) VALUES('task:p','N3','general'),('task:q','N1','general')", [])?;
        Ok(())
    }).await.unwrap();
    let report = f
        .host
        .send_message(
            "progress-n3",
            &p.id,
            &f.master.id,
            "report",
            serde_json::json!({"text":"Confirmed milestone"}),
            "normal",
        )
        .await
        .unwrap();
    f.host
        .send_message(
            "progress-n3",
            &p.id,
            &f.master.id,
            "report",
            serde_json::json!({"text":"Confirmed milestone"}),
            "normal",
        )
        .await
        .unwrap();
    let all = f.host.notifications("conversation:test").await.unwrap();
    assert_eq!(all.len(), 1);
    let n3 = all[0].clone();
    assert_eq!(n3.class, "informational");
    assert!(!report.id.is_empty());
    let n1 = f
        .host
        .notify(
            "progress-n1",
            &q.id,
            "informational",
            "Confirmed milestone",
            n3.created,
        )
        .await
        .unwrap();
    let at = n3.created;
    let routes = f.host.pending_deliveries(at).await.unwrap();
    assert_eq!(routes.len(), 1);
    assert_eq!(routes[0].notification.id, n3.id);
    f.host
        .set_notification_policy(
            "conversation:test",
            NotificationPolicy {
                progress: Some(false),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert!(f.host.pending_deliveries(at).await.unwrap().is_empty());
    f.db.write(excalibur_store::Priority::Journal, |c| {
        c.execute("UPDATE tasks SET notification_policy='{\"progress\":true}' WHERE id IN('task:p','task:q')", [])?;
        Ok(())
    }).await.unwrap();
    let routes = f.host.pending_deliveries(at).await.unwrap();
    assert_eq!(routes.len(), 2);
    assert!(routes.iter().any(|d| d.notification.id == n3.id));
    assert!(routes.iter().any(|d| d.notification.id == n1.id));
    f.db.write(excalibur_store::Priority::Journal, |c| {
        c.execute(
            "UPDATE tasks SET notification_policy='{\"progress\":\"invalid\"}' WHERE id='task:p'",
            [],
        )?;
        Ok(())
    })
    .await
    .unwrap();
    assert_eq!(f.host.pending_deliveries(at).await.unwrap().len(), 1);
    assert!(
        f.host
            .set_notification_policy(
                "conversation:test",
                NotificationPolicy {
                    utc_offset_minutes: i32::MIN,
                    ..Default::default()
                }
            )
            .await
            .is_err()
    );
    f.host.acknowledge_notification(&n3.id).await.unwrap();
    f.host.acknowledge_notification(&n1.id).await.unwrap();
    assert!(f.host.pending_deliveries(at).await.unwrap().is_empty());
}
