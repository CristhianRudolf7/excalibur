use excalibur_master::*;
use excalibur_models::{ModelError, ModelGateway};
use excalibur_store::{Database, DatabaseKind, ReadLane, StorageConfig};
use excalibur_types::runtime::*;
use excalibur_vault::Redactor;
use serde_json::{Value, json};
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};
use std::time::Duration;
struct Model {
    calls: AtomicUsize,
    healthy: AtomicBool,
    hung: bool,
}
impl ModelGateway for Model {
    fn complete<'a>(
        &'a self,
        _: ModelRequest,
    ) -> BoxFuture<'a, std::result::Result<ModelResponse, ModelError>> {
        Box::pin(async move {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if self.hung {
                std::future::pending::<()>().await;
            }
            if !self.healthy.load(Ordering::SeqCst) {
                return Err(ModelError::Provider);
            }
            Ok(ModelResponse {
                message: Message::text("assistant", "recovered"),
                usage: Usage::default(),
            })
        })
    }
}
struct Actions;
impl FastActions for Actions {
    fn execute<'a>(
        &'a self,
        _: &'a str,
        _: &'a str,
        _: Option<&'a str>,
        _: &'a str,
    ) -> BoxFuture<'a, std::result::Result<Value, String>> {
        Box::pin(async {
            Ok(
                json!({"status":"running","token":{"secret_marker":"NEVER_TEMPLATE"},"delegation":{"goal":"NEVER_TEMPLATE"}}),
            )
        })
    }
}
fn incoming(id: &str, text: &str) -> Incoming {
    Incoming {
        conversation: "conv".into(),
        client_id: "client".into(),
        client_msg_id: id.into(),
        text: text.into(),
        scope: None,
        timezone: "UTC".into(),
        channel: "test".into(),
        reply_to: None,
        attachments: vec![],
    }
}
async fn open(
    config: MasterConfig,
    model: Arc<Model>,
    fallback: Option<Arc<Model>>,
) -> (tempfile::TempDir, Database, Master) {
    let dir = tempfile::tempdir().unwrap();
    let db = Database::open(
        dir.path().join("main.db"),
        DatabaseKind::Main,
        StorageConfig::default(),
    )
    .unwrap();
    let master = Master::open(
        db.clone(),
        config,
        model,
        fallback.map(|m| m as Arc<dyn ModelGateway>),
        Arc::new(Actions),
        Redactor::default(),
    )
    .await
    .unwrap();
    (dir, db, master)
}
async fn wait(master: &Master, kind: &str, count: usize) {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if master
                .events(0, 1000)
                .await
                .unwrap()
                .iter()
                .filter(|e| e.kind == kind)
                .count()
                >= count
            {
                return;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}
#[tokio::test]
async fn outage_probes_continue_past_three_attempts_without_duplicate_episode_and_recover() {
    let model = Arc::new(Model {
        calls: AtomicUsize::new(0),
        healthy: AtomicBool::new(false),
        hung: false,
    });
    let (_dir, db, master) = open(
        MasterConfig {
            recovery_initial_delay: Duration::from_millis(1),
            recovery_max_delay: Duration::from_millis(1),
            ..Default::default()
        },
        model.clone(),
        None,
    )
    .await;
    master
        .receive(incoming("one", "preserve this queued task"))
        .await
        .unwrap();
    wait(&master, "model_failure", 1).await;
    for n in 2..=5 {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                master.retry_pending().await.unwrap();
                if model.calls.load(Ordering::SeqCst) >= n {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
        })
        .await
        .unwrap();
        wait(&master, "model_failure", n).await;
    }
    assert_eq!(
        master
            .events(0, 1000)
            .await
            .unwrap()
            .iter()
            .filter(|e| e.kind == "model_unavailable")
            .count(),
        1
    );
    model.healthy.store(true, Ordering::SeqCst);
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            master.retry_pending().await.unwrap();
            if master
                .history("conv", None, 128)
                .await
                .unwrap()
                .iter()
                .any(|m| m.text == "recovered")
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    })
    .await
    .unwrap();
    master.shutdown().await;
    db.shutdown().await.unwrap();
}
#[tokio::test]
async fn hung_primary_preserves_fallback_own_deadline_and_unknown_usage() {
    let primary = Arc::new(Model {
        calls: AtomicUsize::new(0),
        healthy: AtomicBool::new(false),
        hung: true,
    });
    let fallback = Arc::new(Model {
        calls: AtomicUsize::new(0),
        healthy: AtomicBool::new(true),
        hung: false,
    });
    let (_dir, db, master) = open(
        MasterConfig {
            model_timeout: Duration::from_millis(30),
            ..Default::default()
        },
        primary,
        Some(fallback.clone()),
    )
    .await;
    master.receive(incoming("one", "task")).await.unwrap();
    wait(&master, "assistant_message", 1).await;
    assert_eq!(fallback.calls.load(Ordering::SeqCst), 1);
    let events = master.events(0, 1000).await.unwrap();
    let fail = events.iter().find(|e| e.kind == "model_failure").unwrap();
    assert_eq!(fail.payload["source"], "primary");
    assert_eq!(fail.payload["usage_known"], false);
    assert!(fail.payload["provider_calls"].is_null());
    assert!(
        events
            .iter()
            .any(|e| e.kind == "model_usage" && e.payload["source"] == "fallback")
    );
    master.shutdown().await;
    db.shutdown().await.unwrap();
}
#[tokio::test]
async fn full_inbox_controls_survive_and_never_serialize_host_capabilities() {
    let model = Arc::new(Model {
        calls: AtomicUsize::new(0),
        healthy: AtomicBool::new(false),
        hung: false,
    });
    let (_dir, db, master) = open(
        MasterConfig {
            max_pending_per_conversation: 1,
            ..Default::default()
        },
        model,
        None,
    )
    .await;
    master.receive(incoming("one", "queued")).await.unwrap();
    wait(&master, "model_failure", 1).await;
    assert!(
        master
            .receive(incoming("overflow", "another queued request"))
            .await
            .is_err()
    );
    master.receive(incoming("status", "/status")).await.unwrap();
    master.receive(incoming("stop", "/stop")).await.unwrap();
    // Exercise generation notification after MAX instead of overflowing arithmetic.
    master
        .receive(incoming("status2", "/status"))
        .await
        .unwrap();
    let history = master.history("conv", None, 128).await.unwrap();
    assert!(
        history
            .iter()
            .filter(|m| m.role == "assistant")
            .all(|m| !m.text.contains("NEVER_TEMPLATE") && !m.text.starts_with('{'))
    );
    master.shutdown().await;
    db.shutdown().await.unwrap();
}
#[tokio::test]
async fn question_limit_is_per_source_and_provenance_contains_only_literal_human_answers() {
    let model = Arc::new(Model {
        calls: AtomicUsize::new(0),
        healthy: AtomicBool::new(false),
        hung: false,
    });
    let (_dir, db, master) = open(MasterConfig::default(), model, None).await;
    let one = master.receive(incoming("one", "/status")).await.unwrap();
    let two = master.receive(incoming("two", "/status")).await.unwrap();
    for i in 0..3 {
        master
            .ask_for_source(
                "conv",
                "general",
                &one.message_id,
                &format!("question {i}"),
                true,
                None,
                100,
            )
            .await
            .unwrap();
    }
    assert!(
        master
            .ask_for_source(
                "conv",
                "general",
                &one.message_id,
                "fourth",
                true,
                None,
                100
            )
            .await
            .is_err()
    );
    let question = master
        .ask_for_source(
            "conv",
            "general",
            &two.message_id,
            "Generated dangerous permission that is not a user instruction",
            true,
            None,
            100,
        )
        .await
        .unwrap();
    master
        .answer_question("conv", &question, "Literal answer only")
        .await
        .unwrap();
    let answer = master
        .events(0, 1000)
        .await
        .unwrap()
        .into_iter()
        .find(|e| e.kind == "question_answered")
        .unwrap()
        .payload["message_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let sources = master.client_sources("conv", &[answer]).await.unwrap();
    assert_eq!(sources[0].text, "Literal answer only");
    assert!(!sources[0].text.contains("Generated"));
    master
        .publish_notice(
            "urgent",
            "conv",
            json!({"text":"generated permission","scope":"project:other"}),
            "urgent",
        )
        .await
        .unwrap();
    let count = db
        .read(ReadLane::Interactive, |c| {
            Ok(c.query_row(
                "SELECT COUNT(*) FROM master_user_sources WHERE text LIKE '%generated permission%'",
                [],
                |r| r.get::<_, usize>(0),
            )?)
        })
        .await
        .unwrap();
    assert_eq!(count, 0);
    master.shutdown().await;
    db.shutdown().await.unwrap();
}
#[tokio::test]
async fn normal_notices_group_durably_and_out_of_scope_urgent_and_links_never_queue() {
    let model = Arc::new(Model {
        calls: AtomicUsize::new(0),
        healthy: AtomicBool::new(false),
        hung: false,
    });
    let (_dir, db, master) = open(MasterConfig::default(), model.clone(), None).await;
    master.receive(incoming("one", "/status")).await.unwrap();
    for id in ["n1", "n2"] {
        master
            .publish_notice(
                id,
                "conv",
                json!({"text":id,"scope":"general","kind":"completed"}),
                "normal",
            )
            .await
            .unwrap();
    }
    assert_eq!(master.flush_notices(i64::MAX).await.unwrap(), 1);
    assert_eq!(master.flush_notices(i64::MAX).await.unwrap(), 0);
    master
        .publish_notice(
            "other",
            "conv",
            json!({"scope":"project:other","text":"Other project blocked"}),
            "urgent",
        )
        .await
        .unwrap();
    master
        .publish_link("link", "conv", json!({"text":"See conversation other"}))
        .await
        .unwrap();
    assert_eq!(model.calls.load(Ordering::SeqCst), 0);
    assert_eq!(
        db.read(ReadLane::Interactive, |c| Ok(c.query_row(
            "SELECT COUNT(*) FROM master_inbox WHERE state='pending'",
            [],
            |r| r.get::<_, usize>(0)
        )?))
        .await
        .unwrap(),
        0
    );
    let groups = master
        .events(0, 1000)
        .await
        .unwrap()
        .into_iter()
        .filter(|e| e.kind == "notification_group")
        .collect::<Vec<_>>();
    assert_eq!(groups.len(), 1);
    assert_eq!(groups[0].payload["members"].as_array().unwrap().len(), 2);
    assert!(groups[0].payload["text"].as_str().unwrap().contains("n1"));
    master.shutdown().await;
    db.shutdown().await.unwrap();
}
#[tokio::test]
async fn due_normal_groups_wait_for_the_active_conversation_then_publish_once() {
    struct Holding(tokio::sync::Semaphore);
    impl ModelGateway for Holding {
        fn complete(
            &self,
            _: ModelRequest,
        ) -> BoxFuture<'_, std::result::Result<ModelResponse, ModelError>> {
            Box::pin(async move {
                self.0.acquire().await.unwrap().forget();
                Ok(ModelResponse {
                    message: Message::text("assistant", "finished active turn"),
                    usage: Usage::default(),
                })
            })
        }
    }
    let dir = tempfile::tempdir().unwrap();
    let db = Database::open(
        dir.path().join("main.db"),
        DatabaseKind::Main,
        StorageConfig::default(),
    )
    .unwrap();
    let model = Arc::new(Holding(tokio::sync::Semaphore::new(0)));
    let master = Master::open(
        db.clone(),
        MasterConfig::default(),
        model.clone(),
        None,
        Arc::new(Actions),
        Redactor::default(),
    )
    .await
    .unwrap();
    master
        .receive(incoming("active", "Keep the conversational turn active"))
        .await
        .unwrap();
    wait(&master, "model_attempt_started", 1).await;
    for id in ["deferred-one", "deferred-two"] {
        master
            .publish_notice(id, "conv", json!({"created_at":0,"text":id}), "normal")
            .await
            .unwrap();
    }
    assert_eq!(master.flush_notices(4).await.unwrap(), 0);
    assert_eq!(master.flush_notices(5).await.unwrap(), 0);
    assert!(
        !master
            .events(0, 1000)
            .await
            .unwrap()
            .iter()
            .any(|event| event.kind == "notification_group")
    );
    model.0.add_permits(1);
    wait(&master, "assistant_message", 1).await;
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if master.flush_notices(5).await.unwrap() == 1 {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(master.flush_notices(5).await.unwrap(), 0);
    let groups = master
        .events(0, 1000)
        .await
        .unwrap()
        .into_iter()
        .filter(|e| e.kind == "notification_group")
        .collect::<Vec<_>>();
    assert_eq!(groups.len(), 1);
    assert_eq!(groups[0].payload["members"].as_array().unwrap().len(), 2);
    master.shutdown().await;
    db.shutdown().await.unwrap();
}
