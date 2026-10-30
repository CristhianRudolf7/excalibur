//! Observed gateway fixtures: no transport, real pricing or credentials.
use excalibur_master::{Incoming, Master, MasterConfig, NoActions};
use excalibur_models::{
    FailureClass, GatewayMetadata, ModelAttemptMetrics, ModelCallError, ModelCallMetrics,
    ModelError, ModelFailure, ModelGateway, ModelRole, ObservedResponse, UsageBreakdown,
};
use excalibur_store::{Database, DatabaseKind};
use excalibur_types::runtime::*;
use excalibur_vault::{Redactor, SecretValue};
use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
struct Observed {
    provider: &'static str,
    model: String,
    kind: u8,
    calls: AtomicUsize,
}
impl ModelGateway for Observed {
    fn metadata(&self) -> GatewayMetadata {
        GatewayMetadata {
            provider: Some(self.provider.into()),
            model: Some(self.model.clone()),
            role: Some(ModelRole::Master),
            configuration_hash: "a".repeat(64),
            price_table_version: (self.kind == 1).then(|| "synthetic-prices".into()),
            price_table_hash: (self.kind == 1).then(|| "b".repeat(64)),
        }
    }
    fn cost_is_known(&self) -> bool {
        self.kind == 1
    }
    fn complete<'a>(&'a self, _: ModelRequest) -> BoxFuture<'a, Result<ModelResponse, ModelError>> {
        Box::pin(async { panic!("master must use the observed gateway contract") })
    }
    fn complete_observed<'a>(
        &'a self,
        request: ModelRequest,
    ) -> BoxFuture<'a, Result<ObservedResponse, ModelCallError>> {
        Box::pin(async move {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if self.kind == 2 {
                return std::future::pending().await;
            }
            let mut metrics = ModelCallMetrics::request(&request, self.metadata());
            metrics.duration_ms = 8;
            metrics.provider_calls_known = true;
            if self.kind == 0 {
                metrics.provider_calls = 2;
                metrics.usage.estimated = true;
                metrics.attempts = vec![
                    ModelAttemptMetrics {
                        index: 1,
                        duration_ms: 3,
                        usage: UsageBreakdown {
                            estimated: true,
                            ..Default::default()
                        },
                        cost_microusd: None,
                        error: Some(ModelFailure::http(503)),
                    },
                    ModelAttemptMetrics {
                        index: 2,
                        duration_ms: 5,
                        usage: UsageBreakdown {
                            estimated: true,
                            ..Default::default()
                        },
                        cost_microusd: None,
                        error: Some(ModelFailure::http(429)),
                    },
                ];
                return Err(ModelCallError::new(
                    ModelError::ProviderFailure(ModelFailure {
                        class: FailureClass::RateLimit,
                        http_status: Some(429),
                    }),
                    metrics,
                ));
            }
            metrics.provider_calls = 1;
            metrics.usage_known = true;
            metrics.usage = UsageBreakdown {
                input_tokens: 100,
                output_tokens: 20,
                cache_read_tokens: Some(40),
                cache_write_tokens: Some(5),
                cache_write_by_ttl: [("1h".into(), 5)].into(),
                reasoning_tokens: Some(8),
                estimated: false,
            };
            metrics.cost_microusd = Some(42);
            metrics.attempts = vec![ModelAttemptMetrics {
                index: 1,
                duration_ms: 8,
                usage: metrics.usage.clone(),
                cost_microusd: Some(42),
                error: None,
            }];
            Ok(ObservedResponse {
                response: ModelResponse {
                    message: Message::text("assistant", "Observed fallback response"),
                    usage: Usage {
                        input_tokens: 100,
                        output_tokens: 20,
                        cost_microusd: 42,
                        estimated: false,
                    },
                },
                metrics,
            })
        })
    }
}
fn incoming(conversation: &str, id: &str) -> Incoming {
    Incoming {
        conversation: conversation.into(),
        client_id: "accounting-fixture".into(),
        client_msg_id: id.into(),
        text: "Synthetic conversation".into(),
        scope: None,
        timezone: "UTC".into(),
        channel: "test".into(),
        reply_to: None,
        attachments: vec![],
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn observed_primary_fallback_and_cancelled_calls_keep_provider_cost_cache_and_unknowns_distinct()
 {
    let dir = tempfile::tempdir().unwrap();
    let db = Database::open(
        dir.path().join("main.db"),
        DatabaseKind::Main,
        Default::default(),
    )
    .unwrap();
    let redactor = Redactor::default();
    let canary = "synthetic-model-metadata-canary";
    redactor.register_secret(&SecretValue::new(canary.as_bytes().to_vec()).unwrap());
    let primary = Arc::new(Observed {
        provider: "synthetic-primary",
        model: format!("primary-{canary}"),
        kind: 0,
        calls: AtomicUsize::new(0),
    });
    let fallback = Arc::new(Observed {
        provider: "synthetic-fallback",
        model: "fallback-model".into(),
        kind: 1,
        calls: AtomicUsize::new(0),
    });
    let master = Master::open(
        db.clone(),
        MasterConfig::default(),
        primary.clone(),
        Some(fallback.clone()),
        Arc::new(NoActions),
        redactor.clone(),
    )
    .await
    .unwrap();
    let receipt = master.receive(incoming("observed", "first")).await.unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if master
                .history("observed", None, 128)
                .await
                .unwrap()
                .iter()
                .any(|message| {
                    message.role == "assistant" && message.text == "Observed fallback response"
                })
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let events = master.events(0, 128).await.unwrap();
    let failed = events
        .iter()
        .find(|event| event.kind == "model_failure")
        .unwrap();
    let usage = events
        .iter()
        .find(|event| event.kind == "model_usage")
        .unwrap();
    assert_eq!(failed.payload["source"], "primary");
    assert_eq!(failed.payload["source_message_id"], receipt.message_id);
    assert_eq!(
        failed.payload["metrics"]["metadata"]["provider"],
        "synthetic-primary"
    );
    assert_eq!(failed.payload["failure"]["class"], "rate_limit");
    assert_eq!(failed.payload["failure"]["http_status"], 429);
    assert_eq!(failed.payload["provider_calls"], 2);
    assert_eq!(
        failed.payload["metrics"]["attempts"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    assert!(failed.payload["cost_microusd"].is_null());
    assert_eq!(failed.payload["price_known"], false);
    assert_eq!(usage.payload["source"], "fallback");
    assert_eq!(
        usage.payload["metrics"]["metadata"]["provider"],
        "synthetic-fallback"
    );
    assert_eq!(
        usage.payload["metrics"]["metadata"]["model"],
        "fallback-model"
    );
    assert_eq!(
        usage.payload["metrics"]["metadata"]["price_table_version"],
        "synthetic-prices"
    );
    assert_eq!(usage.payload["price_known"], true);
    assert_eq!(usage.payload["cost_microusd"], 42);
    assert_eq!(usage.payload["usage_known"], true);
    assert_eq!(usage.payload["provider_calls"], 1);
    assert_eq!(usage.payload["metrics"]["usage"]["cache_read_tokens"], 40);
    assert_eq!(
        usage.payload["metrics"]["usage"]["cache_write_by_ttl"]["1h"],
        5
    );
    assert_eq!(usage.payload["metrics"]["usage"]["reasoning_tokens"], 8);
    assert!(failed.sequence < usage.sequence);
    assert!(!serde_json::to_string(&events).unwrap().contains(canary));
    assert_eq!(primary.calls.load(Ordering::SeqCst), 1);
    assert_eq!(fallback.calls.load(Ordering::SeqCst), 1);
    master.shutdown().await;
    let pending = Arc::new(Observed {
        provider: "synthetic-pending",
        model: "pending-model".into(),
        kind: 2,
        calls: AtomicUsize::new(0),
    });
    let pending_master = Master::open(
        db.clone(),
        MasterConfig::default(),
        pending.clone(),
        None,
        Arc::new(NoActions),
        redactor,
    )
    .await
    .unwrap();
    let cancelled = pending_master
        .receive(incoming("cancelled", "pending"))
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        while pending.calls.load(Ordering::SeqCst) == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    pending_master.stop_turn("cancelled").await.unwrap();
    pending_master.shutdown().await;
    let events = pending_master.events(0, 128).await.unwrap();
    let started = events
        .iter()
        .find(|event| {
            event.kind == "model_attempt_started"
                && event.payload["source_message_id"] == cancelled.message_id
        })
        .unwrap();
    assert_eq!(started.payload["metadata"]["provider"], "synthetic-pending");
    assert_eq!(started.payload["usage_known"], false);
    assert!(started.payload["usage"].is_null());
    assert!(started.payload["provider_calls"].is_null());
    assert!(started.payload["cost_microusd"].is_null());
    assert!(!events.iter().any(|event| matches!(
        event.kind.as_str(),
        "model_usage" | "model_failure"
    ) && event.payload["attempt_id"]
        == started.payload["attempt_id"]));
    db.shutdown().await.unwrap();
}
