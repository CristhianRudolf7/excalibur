use excalibur_models::*;
use excalibur_providers::{Provider, ProviderClient, Secret};
use excalibur_types::runtime::{Message, ModelRequest};
use serde_json::json;
use std::collections::BTreeMap;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};
fn table() -> PriceTable {
    PriceTable {
        version: "synthetic-tariff-v1".into(),
        entries: vec![ModelPrice {
            provider: "OpenAi".into(),
            model: "synthetic".into(),
            input_microusd_per_million: 1_000_000,
            cache_read_microusd_per_million: Some(200_000),
            cache_write_microusd_per_million_by_ttl: [("5m".into(), 1_500_000)].into(),
            output_microusd_per_million: 1_000_000,
            reasoning_microusd_per_million: Some(2_000_000),
        }],
    }
}
#[test]
fn versioned_prices_count_cache_subsets_and_reasoning_once_and_keep_unknowns_unknown() {
    let price = table();
    let usage = UsageBreakdown {
        input_tokens: 100,
        output_tokens: 50,
        cache_read_tokens: Some(30),
        cache_write_tokens: Some(20),
        cache_write_by_ttl: [("5m".into(), 20)].into(),
        reasoning_tokens: Some(10),
        estimated: false,
    };
    assert_eq!(
        price.price("OpenAi", "synthetic", &usage).unwrap(),
        Some(146)
    );
    let mut missing = usage.clone();
    missing.cache_read_tokens = None;
    assert_eq!(price.price("OpenAi", "synthetic", &missing).unwrap(), None);
    let mut missing_ttl = usage.clone();
    missing_ttl.cache_write_by_ttl.clear();
    assert_eq!(
        price.price("OpenAi", "synthetic", &missing_ttl).unwrap(),
        None
    );
    let mut invalid = usage.clone();
    invalid.cache_read_tokens = Some(90);
    assert!(price.price("OpenAi", "synthetic", &invalid).is_err());
    let mut version = price.clone();
    version.version = "synthetic-tariff-v2".into();
    assert_ne!(
        price.content_hash().unwrap(),
        version.content_hash().unwrap()
    );
}
fn request() -> ModelRequest {
    ModelRequest {
        messages: vec![Message::text("user", "synthetic")],
        tools: vec![],
        tools_enabled: false,
        reasoning_effort: None,
        output_schema: None,
        cache: None,
        deferred_tools: Vec::new(),
    }
}
async fn server(status: u16, body: String) -> (String, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let task = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut buffer = Vec::new();
        loop {
            let mut chunk = [0; 4096];
            let n = stream.read(&mut chunk).await.unwrap();
            assert!(n > 0);
            buffer.extend_from_slice(&chunk[..n]);
            if let Some(end) = buffer.windows(4).position(|w| w == b"\r\n\r\n") {
                let text = String::from_utf8_lossy(&buffer[..end]);
                let length: usize = text
                    .lines()
                    .find_map(|line| {
                        line.to_ascii_lowercase()
                            .strip_prefix("content-length:")
                            .map(|value| value.trim().parse().unwrap())
                    })
                    .unwrap_or(0);
                if buffer.len() >= end + 4 + length {
                    break;
                }
            }
        }
        let response = format!(
            "HTTP/1.1 {status} Synthetic\r\nContent-Length: {}\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        stream.write_all(response.as_bytes()).await.unwrap();
    });
    (format!("http://{address}/v1/"), task)
}
#[tokio::test]
async fn gateway_observations_include_actual_provider_counters_tariff_hash_and_classified_http_failure()
 {
    let body=json!({"choices":[{"finish_reason":"stop","message":{"content":"Synthetic result"}}],"usage":{"prompt_tokens":100,"completion_tokens":50,"prompt_tokens_details":{"cached_tokens":30,"cache_write_tokens":20},"completion_tokens_details":{"reasoning_tokens":10}}}).to_string();
    let (url, task) = server(200, body).await;
    let mut price = table();
    price.entries[0].cache_write_microusd_per_million_by_ttl =
        BTreeMap::from([("unspecified".into(), 1_500_000)]);
    let hash = price.content_hash().unwrap();
    let gateway = OpenAiGateway::new(
        ProviderClient::with_base_url(
            Provider::OpenAi,
            Secret::new("synthetic-only").unwrap(),
            &url,
        )
        .unwrap(),
        GatewayConfig::new("synthetic", ModelRole::Worker),
    )
    .unwrap()
    .with_price_table(price)
    .unwrap();
    let result = gateway.complete_observed(request()).await.unwrap();
    assert_eq!(result.response.usage.cost_microusd, 146);
    assert_eq!(result.metrics.usage.reasoning_tokens, Some(10));
    assert_eq!(result.metrics.usage.cache_write_tokens, Some(20));
    assert_eq!(result.metrics.cost_microusd, Some(146));
    assert_eq!(result.metrics.metadata.price_table_hash, Some(hash));
    assert_eq!(result.metrics.provider_calls, 1);
    task.await.unwrap();
    for (status, class) in [
        (401, FailureClass::Authentication),
        (403, FailureClass::Permission),
        (429, FailureClass::RateLimit),
        (503, FailureClass::Server),
        (504, FailureClass::Timeout),
    ] {
        let (url, task) = server(
            status,
            "{\"error\":\"synthetic-secret-value-never-in-diagnostics\"}".into(),
        )
        .await;
        let gateway = OpenAiGateway::new(
            ProviderClient::with_base_url(
                Provider::OpenAi,
                Secret::new("synthetic-only").unwrap(),
                &url,
            )
            .unwrap(),
            GatewayConfig::new("synthetic", ModelRole::Worker),
        )
        .unwrap();
        let error = gateway.complete_observed(request()).await.unwrap_err();
        assert_eq!(
            error.error.classification(),
            ModelFailure {
                class,
                http_status: Some(status)
            }
        );
        assert_eq!(error.metrics.provider_calls, 1);
        assert!(error.metrics.usage.estimated);
        assert!(!error.to_string().contains("secret-value"));
        task.await.unwrap();
    }
}

#[tokio::test]
async fn known_provider_failure_classes_survive_metrics_and_replay_without_raw_error_details() {
    for (status, code, class) in [
        (429, "credit_balance_exhausted", FailureClass::Quota),
        (
            400,
            "context_length_exceeded",
            FailureClass::ContextWindowExceeded,
        ),
        (503, "server_is_overloaded", FailureClass::Overloaded),
    ] {
        let (url, task) = server(
            status,
            json!({"error":{"code":code,"message":"synthetic-private-provider-message"}})
                .to_string(),
        )
        .await;
        let gateway = OpenAiGateway::new(
            ProviderClient::with_base_url(
                Provider::OpenAi,
                Secret::new("synthetic-only").unwrap(),
                &url,
            )
            .unwrap(),
            GatewayConfig::new("synthetic", ModelRole::Worker),
        )
        .unwrap();
        let failure = gateway.complete_observed(request()).await.unwrap_err();
        let expected = ModelFailure {
            class,
            http_status: Some(status),
        };
        assert_eq!(failure.error.classification(), expected);
        assert_eq!(failure.metrics.provider_calls, 1);
        assert_eq!(failure.metrics.attempts.len(), 1);
        assert_eq!(failure.metrics.attempts[0].error, Some(expected.clone()));
        let serialized = serde_json::to_string(&failure.metrics).unwrap();
        assert!(!serialized.contains("synthetic-private"));
        assert!(!serialized.contains(code));
        task.await.unwrap();
        let replay = ReplayGateway::new(ReplayTape {
            version: 1,
            original_gateway_identity: gateway.identity(),
            seed: None,
            entries: vec![ReplayEntry {
                request: request(),
                outcome: RecordedOutcome::Failure(expected.clone()),
                metrics: Some(*failure.metrics),
            }],
        })
        .unwrap();
        let replayed = replay.complete_observed(request()).await.unwrap_err();
        assert_eq!(replayed.error.classification(), expected);
        assert!(replayed.metrics.replayed);
        assert_eq!(replayed.metrics.provider_calls, 0);
    }
}
#[test]
fn integer_cost_overflow_and_cache_overflow_are_rejected() {
    let mut price = table();
    price.entries[0].input_microusd_per_million = u64::MAX;
    assert!(
        price
            .upper_bound("OpenAi", "synthetic", u64::MAX, u64::MAX)
            .is_err()
    );
    let usage = UsageBreakdown {
        input_tokens: u64::MAX,
        cache_read_tokens: Some(u64::MAX),
        cache_write_tokens: Some(1),
        ..Default::default()
    };
    assert!(usage.validate().is_err());
}

#[tokio::test]
async fn actual_stream_usage_preserves_cache_and_reasoning_details() {
    let use_value = json!({"prompt_tokens":100,"completion_tokens":50,"prompt_tokens_details":{"cached_tokens":30,"cache_write_tokens":20},"completion_tokens_details":{"reasoning_tokens":10}});
    let body = format!(
        "data: {}\n\ndata: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
        json!({"choices":[{"delta":{"content":"synthetic"},"finish_reason":null}]}),
        json!({"choices":[{"delta":{},"finish_reason":"stop"}]}),
        json!({"choices":[],"usage":use_value})
    );
    let (url, task) = server(200, body).await;
    let mut price = table();
    price.entries[0].cache_write_microusd_per_million_by_ttl =
        BTreeMap::from([("unspecified".into(), 1_500_000)]);
    let gateway = OpenAiGateway::new(
        ProviderClient::with_base_url(
            Provider::OpenAi,
            Secret::new("synthetic-only").unwrap(),
            &url,
        )
        .unwrap(),
        GatewayConfig::new("synthetic", ModelRole::Master),
    )
    .unwrap()
    .with_price_table(price)
    .unwrap();
    let result = gateway
        .complete_stream_observed(request(), std::sync::Arc::new(|_| {}))
        .await
        .unwrap();
    assert_eq!(result.metrics.usage.cache_read_tokens, Some(30));
    assert_eq!(result.metrics.usage.reasoning_tokens, Some(10));
    assert_eq!(result.metrics.cost_microusd, Some(146));
    assert!(result.metrics.provider_calls_known);
    assert!(result.metrics.usage_known);
    task.await.unwrap();
}

#[test]
fn unpriced_gateway_has_token_preflight_without_claiming_a_zero_invoice() {
    let gateway = OpenAiGateway::new(
        ProviderClient::with_base_url(
            Provider::OpenAi,
            Secret::new("synthetic-only").unwrap(),
            "http://127.0.0.1:9/v1/",
        )
        .unwrap(),
        GatewayConfig::new("synthetic", ModelRole::Worker),
    )
    .unwrap();
    let bound = gateway.call_bound(&request()).unwrap();
    assert!(bound.max_tokens > 0);
    assert_eq!(bound.max_cost_microusd, 0);
    assert!(!gateway.cost_is_known());
    assert!(gateway.metadata().price_table_hash.is_none());
}
