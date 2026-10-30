//! Provider error resilience (architecture 19.5 and the 19.7 row): Retry-After,
//! bounded retries of the identical request, circuit breaker, failover and
//! shared process state. Local mock HTTP servers and a manual clock only: no
//! network, no paid API and no real sleeping.
use excalibur_models::{
    FailoverGateway, FailureClass, GatewayConfig, ModelError, ModelFailure, ModelGateway,
    ModelRole, OpenAiGateway, ProviderResilience, ResiliencePolicy, StreamDelta, StreamSink,
    resilience::{
        BreakerScope, BreakerState, Outcome, ResilienceClock, ResilienceEvent, is_retryable,
        parse_retry_after,
    },
};
use excalibur_providers::{Provider, ProviderClient, Secret};
use excalibur_types::runtime::{BoxFuture, Message, ModelRequest};
use serde_json::json;
use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, SystemTime},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};

/// Manual time: `sleep` records the request and advances the clock at once.
struct ManualClock {
    now: Mutex<Duration>,
    base: SystemTime,
    sleeps: Mutex<Vec<Duration>>,
}
impl ManualClock {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            now: Mutex::new(Duration::ZERO),
            base: SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000),
            sleeps: Mutex::new(Vec::new()),
        })
    }
    fn advance(&self, by: Duration) {
        *self.now.lock().unwrap() += by;
    }
    fn sleeps(&self) -> Vec<Duration> {
        self.sleeps.lock().unwrap().clone()
    }
}
impl ResilienceClock for ManualClock {
    fn monotonic(&self) -> Duration {
        *self.now.lock().unwrap()
    }
    fn system_time(&self) -> SystemTime {
        self.base + self.monotonic()
    }
    fn sleep(&self, duration: Duration) -> BoxFuture<'_, ()> {
        self.sleeps.lock().unwrap().push(duration);
        self.advance(duration);
        Box::pin(async {})
    }
}

#[derive(Clone)]
enum Reply {
    /// Status line, extra headers, body.
    Http(&'static str, Vec<String>, String),
    /// Read the request and close the connection without answering.
    Drop,
}
fn ok(content: &str) -> Reply {
    Reply::Http(
        "200 OK",
        Vec::new(),
        json!({"choices":[{"finish_reason":"stop","message":{"role":"assistant","content":content}}],"usage":{"prompt_tokens":5,"completion_tokens":2}}).to_string(),
    )
}
fn status(line: &'static str, headers: &[String]) -> Reply {
    Reply::Http(
        line,
        headers.to_vec(),
        json!({"error":{"message":"internal detail sk-live-SECRET-TOKEN-1234 from upstream","type":"server_error"}}).to_string(),
    )
}
struct Mock {
    url: String,
    bodies: Arc<Mutex<Vec<Vec<u8>>>>,
}
/// Serves the scripted replies in order, one connection each, then refuses
/// nothing else: further connections are accepted and dropped unanswered.
async fn mock(replies: Vec<Reply>) -> Mock {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let bodies = Arc::new(Mutex::new(Vec::new()));
    let seen = bodies.clone();
    tokio::spawn(async move {
        let mut replies = replies.into_iter();
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let mut buffer = Vec::new();
            let body = loop {
                let mut chunk = [0u8; 4096];
                let n = socket.read(&mut chunk).await.unwrap_or(0);
                if n == 0 {
                    break None;
                }
                buffer.extend_from_slice(&chunk[..n]);
                if let Some(index) = buffer.windows(4).position(|w| w == b"\r\n\r\n") {
                    let head = String::from_utf8_lossy(&buffer[..index]).to_lowercase();
                    let length: usize = head
                        .lines()
                        .find_map(|l| {
                            l.strip_prefix("content-length:")
                                .map(|n| n.trim().parse().unwrap())
                        })
                        .unwrap_or(0);
                    if buffer.len() >= index + 4 + length {
                        break Some(buffer[index + 4..index + 4 + length].to_vec());
                    }
                }
            };
            let Some(body) = body else { continue };
            seen.lock().unwrap().push(body);
            match replies.next() {
                Some(Reply::Http(line, headers, body)) => {
                    let mut head = format!(
                        "HTTP/1.1 {line}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n",
                        body.len()
                    );
                    for header in headers {
                        head.push_str(&header);
                        head.push_str("\r\n");
                    }
                    head.push_str("\r\n");
                    let _ = socket.write_all(head.as_bytes()).await;
                    let _ = socket.write_all(body.as_bytes()).await;
                }
                Some(Reply::Drop) | None => {}
            }
            let _ = socket.shutdown().await;
        }
    });
    Mock {
        url: format!("http://{address}/v1/"),
        bodies,
    }
}
impl Mock {
    fn count(&self) -> usize {
        self.bodies.lock().unwrap().len()
    }
    fn bodies(&self) -> Vec<Vec<u8>> {
        self.bodies.lock().unwrap().clone()
    }
}
fn request() -> ModelRequest {
    ModelRequest {
        messages: vec![
            Message::text("system", "core"),
            Message::text("user", "task"),
        ],
        tools: Vec::new(),
        tools_enabled: false,
        reasoning_effort: None,
        output_schema: None,
        cache: None,
        deferred_tools: Vec::new(),
    }
}
fn policy() -> ResiliencePolicy {
    let mut policy = ResiliencePolicy::default();
    policy.retry.jitter = false;
    policy
}
fn shared(policy: ResiliencePolicy) -> (Arc<ProviderResilience>, Arc<ManualClock>) {
    let clock = ManualClock::new();
    (
        ProviderResilience::new(policy, clock.clone()).unwrap(),
        clock,
    )
}
fn gateway(
    provider: Provider,
    url: &str,
    model: &str,
    role: ModelRole,
    resilience: &Arc<ProviderResilience>,
) -> OpenAiGateway {
    let client =
        ProviderClient::with_base_url(provider, Secret::new("synthetic-key").unwrap(), url)
            .unwrap();
    OpenAiGateway::new(client, GatewayConfig::new(model, role))
        .unwrap()
        .with_resilience(resilience.clone())
}
fn class(error: &ModelError) -> FailureClass {
    error.classification().class
}

#[tokio::test]
async fn retry_after_delta_seconds_is_honoured_and_the_identical_request_is_resent() {
    let (resilience, clock) = shared(policy());
    let server = mock(vec![
        status("429 Too Many Requests", &["Retry-After: 7".into()]),
        ok("done"),
    ])
    .await;
    let gw = gateway(
        Provider::OpenAi,
        &server.url,
        "m",
        ModelRole::Worker,
        &resilience,
    );
    let observed = gw.complete_observed(request()).await.unwrap();
    assert_eq!(observed.response.message.content, "done");
    assert_eq!(clock.sleeps(), vec![Duration::from_secs(7)]);
    let bodies = server.bodies();
    assert_eq!(bodies.len(), 2);
    assert_eq!(bodies[0], bodies[1], "retry must resend the identical body");
    assert_eq!(observed.metrics.provider_calls, 2);
    assert_eq!(
        observed.metrics.attempts[0].error,
        Some(ModelFailure {
            class: FailureClass::RateLimit,
            http_status: Some(429)
        })
    );
    assert!(resilience.events().iter().any(|e| matches!(
        e,
        ResilienceEvent::Retry {
            delay_ms: 7000,
            from_retry_after: true,
            ..
        }
    )));
}

#[tokio::test]
async fn retry_after_http_date_is_honoured_relative_to_the_injected_clock() {
    let (resilience, clock) = shared(policy());
    let at = clock.system_time() + Duration::from_secs(30);
    let server = mock(vec![
        status(
            "503 Service Unavailable",
            &[format!("Retry-After: {}", httpdate::fmt_http_date(at))],
        ),
        ok("done"),
    ])
    .await;
    let gw = gateway(
        Provider::OpenAi,
        &server.url,
        "m",
        ModelRole::Worker,
        &resilience,
    );
    gw.complete(request()).await.unwrap();
    assert_eq!(clock.sleeps(), vec![Duration::from_secs(30)]);
    assert_eq!(server.count(), 2);
}

#[tokio::test]
async fn retry_after_beyond_the_cap_is_not_waited_for() {
    let (resilience, clock) = shared(policy());
    let server = mock(vec![
        status("429 Too Many Requests", &["Retry-After: 3600".into()]),
        ok("never"),
    ])
    .await;
    let gw = gateway(
        Provider::OpenAi,
        &server.url,
        "m",
        ModelRole::Worker,
        &resilience,
    );
    let error = gw.complete(request()).await.unwrap_err();
    assert_eq!(class(&error), FailureClass::RateLimit);
    assert!(clock.sleeps().is_empty());
    assert_eq!(server.count(), 1);
    assert!(resilience.events().iter().any(|e| matches!(
        e,
        ResilienceEvent::RetryAfterExceedsCap {
            requested_ms: 3_600_000,
            ..
        }
    )));
}

#[test]
fn retry_after_parsing_covers_all_http_date_forms() {
    let now = SystemTime::UNIX_EPOCH + Duration::from_secs(784_111_777 - 10);
    assert_eq!(
        parse_retry_after("120", now),
        Some(Duration::from_secs(120))
    );
    assert_eq!(parse_retry_after(" 0 ", now), Some(Duration::ZERO));
    for date in [
        "Sun, 06 Nov 1994 08:49:37 GMT",
        "Sunday, 06-Nov-94 08:49:37 GMT",
        "Sun Nov  6 08:49:37 1994",
    ] {
        assert_eq!(
            parse_retry_after(date, now),
            Some(Duration::from_secs(10)),
            "{date}"
        );
    }
    let later = now + Duration::from_secs(100);
    assert_eq!(
        parse_retry_after("Sun, 06 Nov 1994 08:49:37 GMT", later),
        Some(Duration::ZERO)
    );
    assert_eq!(parse_retry_after("-5", now), None);
    assert_eq!(parse_retry_after("soon", now), None);
    assert_eq!(parse_retry_after("1.5", now), None);
    assert_eq!(
        parse_retry_after("99999999999999999999999", now),
        Some(Duration::MAX)
    );
}

#[tokio::test]
async fn server_and_transport_errors_use_bounded_exponential_backoff() {
    let (resilience, clock) = shared(policy());
    let server = mock(vec![
        status("500 Internal Server Error", &[]),
        Reply::Drop,
        ok("recovered"),
    ])
    .await;
    let gw = gateway(
        Provider::OpenAi,
        &server.url,
        "m",
        ModelRole::Worker,
        &resilience,
    );
    let observed = gw.complete_observed(request()).await.unwrap();
    assert_eq!(observed.response.message.content, "recovered");
    assert_eq!(
        clock.sleeps(),
        vec![Duration::from_millis(500), Duration::from_millis(1000)]
    );
    let bodies = server.bodies();
    assert!(bodies.windows(2).all(|w| w[0] == w[1]));
    let errors = observed
        .metrics
        .attempts
        .iter()
        .filter_map(|a| a.error.as_ref().map(|e| e.class.clone()))
        .collect::<Vec<_>>();
    assert_eq!(errors, vec![FailureClass::Server, FailureClass::Transport]);
    assert_eq!(observed.metrics.provider_calls, 3);

    // Retries are bounded: three failures exhaust max_retries = 2.
    let (resilience, clock) = shared(policy());
    let server = mock(vec![
        status("502 Bad Gateway", &[]),
        status("503 Service Unavailable", &[]),
        status("504 Gateway Timeout", &[]),
        ok("too late"),
    ])
    .await;
    let gw = gateway(
        Provider::OpenAi,
        &server.url,
        "m",
        ModelRole::Worker,
        &resilience,
    );
    let error = gw.complete(request()).await.unwrap_err();
    assert_eq!(class(&error), FailureClass::Timeout);
    assert_eq!(server.count(), 3);
    assert_eq!(clock.sleeps().len(), 2);
}

#[tokio::test]
async fn jittered_backoff_stays_within_the_equal_jitter_band() {
    let mut policy = ResiliencePolicy::default();
    policy.retry.max_retries = 4;
    let (resilience, clock) = shared(policy);
    let server = mock(vec![
        status("500 Internal Server Error", &[]),
        status("500 Internal Server Error", &[]),
        status("500 Internal Server Error", &[]),
        status("500 Internal Server Error", &[]),
        ok("ok"),
    ])
    .await;
    let gw = gateway(
        Provider::OpenAi,
        &server.url,
        "m",
        ModelRole::Worker,
        &resilience,
    );
    gw.complete(request()).await.unwrap();
    let sleeps = clock.sleeps();
    assert_eq!(sleeps.len(), 4);
    for (retry, sleep) in sleeps.iter().enumerate() {
        let full = (500u64 << retry).min(8_000);
        let ms = sleep.as_millis() as u64;
        assert!((full / 2..=full).contains(&ms), "retry {retry}: {ms} ms");
    }
}

#[tokio::test]
async fn client_and_authentication_errors_are_not_retried_and_do_not_trip_the_breaker() {
    for line in [
        "400 Bad Request",
        "401 Unauthorized",
        "403 Forbidden",
        "404 Not Found",
        "422 Unprocessable Entity",
    ] {
        let (resilience, clock) = shared(policy());
        let server = mock(vec![status(line, &["Retry-After: 1".into()]); 6]).await;
        let gw = gateway(
            Provider::OpenAi,
            &server.url,
            "m",
            ModelRole::Worker,
            &resilience,
        );
        for _ in 0..6 {
            gw.complete(request()).await.unwrap_err();
        }
        assert!(clock.sleeps().is_empty(), "{line}");
        let key = resilience.key("OpenAi", "m");
        let snapshot = resilience.snapshot(&key);
        assert_eq!(snapshot.state, BreakerState::Closed, "{line}");
        assert_eq!(snapshot.consecutive_failures, 0, "{line}");
    }
    // 408 is the one retryable 4xx besides 429.
    let (resilience, clock) = shared(policy());
    let server = mock(vec![status("408 Request Timeout", &[]), ok("ok")]).await;
    let gw = gateway(
        Provider::OpenAi,
        &server.url,
        "m",
        ModelRole::Worker,
        &resilience,
    );
    gw.complete(request()).await.unwrap();
    assert_eq!(clock.sleeps().len(), 1);
    assert!(!is_retryable(&ModelFailure {
        class: FailureClass::Quota,
        http_status: Some(429)
    }));
    assert!(!is_retryable(&ModelFailure {
        class: FailureClass::Server,
        http_status: Some(501)
    }));
}

#[tokio::test]
async fn five_consecutive_failures_open_the_breaker_then_half_open_probe_closes_it() {
    let mut policy = policy();
    policy.retry.max_retries = 0;
    let (resilience, clock) = shared(policy);
    let mut replies = vec![status("500 Internal Server Error", &[]); 5];
    replies.push(ok("probe ok"));
    let server = mock(replies).await;
    let gw = gateway(
        Provider::OpenAi,
        &server.url,
        "m",
        ModelRole::Worker,
        &resilience,
    );
    let key = resilience.key("OpenAi", "m");
    for i in 0..5 {
        assert_eq!(resilience.snapshot(&key).state, BreakerState::Closed, "{i}");
        assert_eq!(
            class(&gw.complete(request()).await.unwrap_err()),
            FailureClass::Server
        );
    }
    let open = resilience.snapshot(&key);
    assert_eq!(open.state, BreakerState::Open);
    assert_eq!(open.consecutive_failures, 5);
    assert_eq!(open.open_remaining_ms, Some(60_000));
    assert_eq!(open.times_opened, 1);
    // While open no request reaches the provider.
    let error = gw.complete(request()).await.unwrap_err();
    assert_eq!(class(&error), FailureClass::CircuitOpen);
    assert_eq!(server.count(), 5);
    clock.advance(Duration::from_secs(59));
    assert_eq!(
        class(&gw.complete(request()).await.unwrap_err()),
        FailureClass::CircuitOpen
    );
    assert_eq!(server.count(), 5);
    // After 60 s exactly one probe is admitted; its success closes the breaker.
    clock.advance(Duration::from_secs(1));
    assert_eq!(resilience.snapshot(&key).state, BreakerState::HalfOpen);
    assert_eq!(
        gw.complete(request()).await.unwrap().message.content,
        "probe ok"
    );
    assert_eq!(resilience.snapshot(&key).state, BreakerState::Closed);
    let events = resilience.events();
    let order = events
        .iter()
        .filter_map(|e| match e {
            ResilienceEvent::Opened { .. } => Some("opened"),
            ResilienceEvent::HalfOpen { .. } => Some("half_open"),
            ResilienceEvent::Closed { .. } => Some("closed"),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(order, vec!["opened", "half_open", "closed"]);
}

#[tokio::test]
async fn failed_probe_reopens_and_only_one_probe_is_in_flight() {
    let mut policy = policy();
    policy.breaker.consecutive_failures = 1;
    let (resilience, clock) = shared(policy);
    let key = resilience.key("Synthetic", "m");
    resilience.admit(&key).unwrap().record(Outcome::Failure);
    assert_eq!(resilience.snapshot(&key).state, BreakerState::Open);
    clock.advance(Duration::from_secs(60));
    let probe = resilience.admit(&key).unwrap();
    assert_eq!(
        resilience.admit(&key).err().map(|f| f.class),
        Some(FailureClass::CircuitOpen),
        "a second concurrent probe must be rejected"
    );
    // A cancelled (dropped) probe releases its slot without deciding.
    drop(probe);
    assert_eq!(resilience.snapshot(&key).state, BreakerState::HalfOpen);
    let probe = resilience.admit(&key).unwrap();
    probe.record(Outcome::Failure);
    let reopened = resilience.snapshot(&key);
    assert_eq!(reopened.state, BreakerState::Open);
    assert_eq!(reopened.times_opened, 2);
    assert_eq!(reopened.open_remaining_ms, Some(60_000));
}

#[test]
fn failure_rate_above_half_in_the_window_opens_with_a_minimum_sample() {
    let mut policy = policy();
    policy.breaker.consecutive_failures = 100;
    let (resilience, clock) = shared(policy);
    let key = resilience.key("Synthetic", "m");
    let record = |outcome| resilience.admit(&key).unwrap().record(outcome);
    // 5 of 10 is exactly 50 %: not above the threshold.
    for i in 0..10 {
        record(if i % 2 == 0 {
            Outcome::Failure
        } else {
            Outcome::Success
        });
        clock.advance(Duration::from_secs(1));
    }
    let snapshot = resilience.snapshot(&key);
    assert_eq!((snapshot.window_samples, snapshot.window_failures), (10, 5));
    assert_eq!(snapshot.state, BreakerState::Closed);
    record(Outcome::Failure);
    let snapshot = resilience.snapshot(&key);
    assert_eq!(snapshot.state, BreakerState::Open, "6 of 11 is above 50 %");

    // Below the minimum sample the rate rule cannot open the breaker.
    let (resilience, clock) = shared({
        let mut p = ResiliencePolicy::default();
        p.breaker.consecutive_failures = 100;
        p
    });
    let key = resilience.key("Synthetic", "m");
    for _ in 0..4 {
        resilience.admit(&key).unwrap().record(Outcome::Failure);
    }
    assert_eq!(resilience.snapshot(&key).state, BreakerState::Closed);
    // Outcomes older than the 1-minute window no longer count.
    clock.advance(Duration::from_secs(61));
    for i in 0..10 {
        resilience.admit(&key).unwrap().record(if i < 5 {
            Outcome::Failure
        } else {
            Outcome::Success
        });
    }
    let snapshot = resilience.snapshot(&key);
    assert_eq!((snapshot.window_samples, snapshot.window_failures), (10, 5));
    assert_eq!(snapshot.state, BreakerState::Closed);
}

#[tokio::test]
async fn open_breaker_switches_the_role_to_its_fallback_route() {
    let mut policy = policy();
    policy.retry.max_retries = 1;
    let (resilience, clock) = shared(policy);
    let primary_server = mock(vec![status("500 Internal Server Error", &[]); 6]).await;
    let fallback_server = mock(vec![ok("fallback"); 5]).await;
    let primary: Arc<dyn ModelGateway> = Arc::new(gateway(
        Provider::OpenAi,
        &primary_server.url,
        "primary-model",
        ModelRole::Worker,
        &resilience,
    ));
    let fallback: Arc<dyn ModelGateway> = Arc::new(gateway(
        Provider::DeepSeek,
        &fallback_server.url,
        "fallback-model",
        ModelRole::Worker,
        &resilience,
    ));
    let role = FailoverGateway::new(vec![primary, fallback])
        .unwrap()
        .with_resilience(resilience.clone());
    // Each call: two primary attempts (one retry), then the fallback.
    for _ in 0..2 {
        let observed = role.complete_observed(request()).await.unwrap();
        assert_eq!(observed.response.message.content, "fallback");
        assert_eq!(
            observed.metrics.metadata.model.as_deref(),
            Some("fallback-model")
        );
        assert_eq!(observed.metrics.provider_calls, 3);
        assert_eq!(observed.metrics.attempts.len(), 3);
        // The primary's failed requests are charged, not hidden.
        assert!(!observed.metrics.usage_known);
        assert!(observed.response.usage.estimated);
        assert!(observed.response.usage.input_tokens > 5);
        assert_eq!(
            observed.metrics.usage.input_tokens,
            observed
                .metrics
                .attempts
                .iter()
                .map(|a| a.usage.input_tokens)
                .sum::<u64>()
        );
    }
    let primary_key = resilience.key("OpenAi", "primary-model");
    assert_eq!(resilience.snapshot(&primary_key).consecutive_failures, 4);
    // Fifth consecutive failure opens the primary breaker.
    role.complete(request()).await.unwrap();
    assert_eq!(resilience.snapshot(&primary_key).state, BreakerState::Open);
    assert_eq!(primary_server.count(), 5);
    // Now requests go straight to the fallback without touching the primary.
    let observed = role.complete_observed(request()).await.unwrap();
    assert_eq!(observed.response.message.content, "fallback");
    assert_eq!(primary_server.count(), 5);
    assert_eq!(
        observed.metrics.attempts[0]
            .error
            .as_ref()
            .map(|e| e.class.clone()),
        Some(FailureClass::CircuitOpen),
    );
    assert_eq!(
        observed.metrics.provider_calls, 1,
        "a rejected open route sends no provider request"
    );
    assert_eq!(fallback_server.count(), 4);
    assert!(resilience.events().iter().any(|e| matches!(
        e,
        ResilienceEvent::Failover {
            failure: ModelFailure {
                class: FailureClass::CircuitOpen,
                ..
            },
            ..
        }
    )));
    // The primary slept once in each of the first two calls; the failure that
    // opened its breaker in the third call failed over without sleeping.
    assert_eq!(clock.sleeps().len(), 2);
}

#[tokio::test]
async fn client_errors_never_fail_over() {
    let (resilience, _clock) = shared(policy());
    let primary_server = mock(vec![status("400 Bad Request", &[])]).await;
    let fallback_server = mock(vec![ok("must not be used")]).await;
    let role = FailoverGateway::new(vec![
        Arc::new(gateway(
            Provider::OpenAi,
            &primary_server.url,
            "p",
            ModelRole::Worker,
            &resilience,
        )),
        Arc::new(gateway(
            Provider::DeepSeek,
            &fallback_server.url,
            "f",
            ModelRole::Worker,
            &resilience,
        )),
    ])
    .unwrap();
    let error = role.complete(request()).await.unwrap_err();
    assert_eq!(class(&error), FailureClass::InvalidRequest);
    assert_eq!(fallback_server.count(), 0);
    assert!(FailoverGateway::new(Vec::new()).is_err());
}

#[tokio::test]
async fn breaker_state_is_shared_by_master_and_worker_gateways() {
    let mut policy = policy();
    policy.retry.max_retries = 0;
    let (resilience, _clock) = shared(policy);
    let server = mock(vec![status("503 Service Unavailable", &[]); 5]).await;
    let worker = gateway(
        Provider::OpenAi,
        &server.url,
        "worker-model",
        ModelRole::Worker,
        &resilience,
    );
    let master = gateway(
        Provider::OpenAi,
        &server.url,
        "master-model",
        ModelRole::Master,
        &resilience,
    );
    for _ in 0..5 {
        worker.complete(request()).await.unwrap_err();
    }
    // Spec scope is per provider: the master's model on the same provider is
    // rejected without a request, by the same in-process state.
    let sink: StreamSink = Arc::new(|_| {});
    let error = master.complete_stream(request(), sink).await.unwrap_err();
    assert_eq!(class(&error), FailureClass::CircuitOpen);
    assert_eq!(server.count(), 5);
    assert!(Arc::ptr_eq(
        &ProviderResilience::process(),
        &ProviderResilience::process()
    ));
    assert_eq!(
        ProviderResilience::process().policy(),
        &ResiliencePolicy::default()
    );
}

#[tokio::test]
async fn provider_model_scope_keeps_models_independent() {
    let mut policy = policy();
    policy.breaker.scope = BreakerScope::ProviderModel;
    let (resilience, _clock) = shared(policy);
    let a = resilience.key("OpenAi", "a");
    let b = resilience.key("OpenAi", "b");
    for _ in 0..5 {
        resilience.admit(&a).unwrap().record(Outcome::Failure);
    }
    assert_eq!(resilience.snapshot(&a).state, BreakerState::Open);
    assert_eq!(resilience.snapshot(&b).state, BreakerState::Closed);
    assert!(resilience.admit(&b).is_ok());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_callers_update_one_breaker_consistently() {
    let mut policy = policy();
    policy.breaker.consecutive_failures = 50;
    // Disable the rate rule (it could open at the 10th failure) so only the
    // consecutive rule under test can open the breaker: then at least 50
    // permits are always admitted, whatever the scheduling.
    policy.breaker.min_samples = 4_096;
    let (resilience, _clock) = shared(policy);
    let key = resilience.key("OpenAi", "m");
    let admitted = Arc::new(AtomicUsize::new(0));
    let mut tasks = Vec::new();
    for _ in 0..64 {
        let resilience = resilience.clone();
        let key = key.clone();
        let admitted = admitted.clone();
        tasks.push(tokio::spawn(async move {
            for _ in 0..4 {
                if let Ok(permit) = resilience.admit(&key) {
                    admitted.fetch_add(1, Ordering::SeqCst);
                    tokio::task::yield_now().await;
                    permit.record(Outcome::Failure);
                }
            }
        }));
    }
    for task in tasks {
        task.await.unwrap();
    }
    let snapshot = resilience.snapshot(&key);
    assert_eq!(snapshot.state, BreakerState::Open);
    assert_eq!(snapshot.times_opened, 1, "the breaker opens exactly once");
    assert_eq!(
        snapshot.consecutive_failures as usize,
        admitted.load(Ordering::SeqCst)
    );
    assert!(admitted.load(Ordering::SeqCst) >= 50);
}

#[tokio::test]
async fn provider_error_bodies_and_secrets_never_reach_callers_or_evidence() {
    let (resilience, _clock) = shared(policy());
    let server = mock(vec![
        status(
            "500 Internal Server Error",
            &["Retry-After: sk-live-SECRET".into()],
        ),
        status("500 Internal Server Error", &[]),
        status("500 Internal Server Error", &[]),
    ])
    .await;
    let gw = gateway(
        Provider::OpenAi,
        &server.url,
        "m",
        ModelRole::Worker,
        &resilience,
    );
    let failure = gw.complete_observed(request()).await.unwrap_err();
    let key = resilience.key("OpenAi", "m");
    let rendered = [
        format!("{:?}", failure.error),
        failure.error.to_string(),
        serde_json::to_string(&failure.metrics).unwrap(),
        serde_json::to_string(&resilience.events()).unwrap(),
        serde_json::to_string(&resilience.snapshot(&key)).unwrap(),
    ];
    for text in rendered {
        assert!(!text.contains("SECRET"), "{text}");
        assert!(!text.contains("synthetic-key"), "{text}");
        assert!(!text.contains("internal detail"), "{text}");
    }
}

#[tokio::test]
async fn master_stream_retries_only_while_opening_the_stream() {
    let (resilience, clock) = shared(policy());
    let chunks = [
        json!({"choices":[{"delta":{"content":"hello"},"finish_reason":"stop"}]}),
        json!({"choices":[],"usage":{"prompt_tokens":8,"completion_tokens":3}}),
    ];
    let stream = chunks
        .iter()
        .map(|v| format!("data: {v}\n\n"))
        .collect::<String>()
        + "data: [DONE]\n\n";
    let server = mock(vec![
        status("429 Too Many Requests", &["retry-after-ms: 1500".into()]),
        Reply::Http("200 OK", Vec::new(), stream),
    ])
    .await;
    let master = gateway(
        Provider::OpenAi,
        &server.url,
        "m",
        ModelRole::Master,
        &resilience,
    );
    let deltas = Arc::new(Mutex::new(Vec::new()));
    let capture = deltas.clone();
    let sink: StreamSink = Arc::new(move |d| capture.lock().unwrap().push(d));
    let response = master.complete_stream(request(), sink).await.unwrap();
    assert_eq!(response.message.content, "hello");
    assert_eq!(clock.sleeps(), vec![Duration::from_millis(1500)]);
    let bodies = server.bodies();
    assert_eq!(bodies[0], bodies[1]);
    assert!(matches!(
        deltas.lock().unwrap().last(),
        Some(StreamDelta::Complete { .. })
    ));
}

#[tokio::test]
async fn gateways_without_resilience_keep_their_previous_single_attempt_behaviour() {
    let server = mock(vec![status("500 Internal Server Error", &[]), ok("unused")]).await;
    let client = ProviderClient::with_base_url(
        Provider::OpenAi,
        Secret::new("synthetic-key").unwrap(),
        &server.url,
    )
    .unwrap();
    let plain = OpenAiGateway::new(client, GatewayConfig::new("m", ModelRole::Worker)).unwrap();
    assert!(plain.resilience().is_none());
    assert_eq!(
        class(&plain.complete(request()).await.unwrap_err()),
        FailureClass::Server
    );
    assert_eq!(server.count(), 1);
}

#[test]
fn invalid_policies_are_rejected() {
    let clock: Arc<dyn ResilienceClock> = ManualClock::new();
    let mut bad = Vec::new();
    let mut p = ResiliencePolicy::default();
    p.retry.max_retries = 11;
    bad.push(p);
    let mut p = ResiliencePolicy::default();
    p.retry.base_delay_ms = 0;
    bad.push(p);
    let mut p = ResiliencePolicy::default();
    p.breaker.failure_rate_percent = 100;
    bad.push(p);
    let mut p = ResiliencePolicy::default();
    p.breaker.min_samples = 0;
    bad.push(p);
    let mut p = ResiliencePolicy::default();
    p.breaker.open_ms = 0;
    bad.push(p);
    for policy in bad {
        assert!(ProviderResilience::new(policy, clock.clone()).is_err());
    }
    let defaults = ResiliencePolicy::default();
    assert_eq!(defaults.breaker.consecutive_failures, 5);
    assert_eq!(defaults.breaker.window_ms, 60_000);
    assert_eq!(defaults.breaker.failure_rate_percent, 50);
    assert_eq!(defaults.breaker.open_ms, 60_000);
    assert_eq!(defaults.breaker.scope, BreakerScope::Provider);
}

// ---- Correction cycle 1 (audit of d42da58: PR-2 .. PR-7) ----

/// Mirror of the failure branch of `crates/engine/src/context_runtime.rs`
/// (summary budget charging, lines 367-381 at 570823d): nothing charged when
/// the call provably sent nothing, recorded usage when known, otherwise the
/// pre-dispatch bound. Kept literal so a change in either side is visible.
fn context_runtime_charge(
    failure: &excalibur_models::ModelCallError,
    bound: &excalibur_models::ModelCallBound,
) -> excalibur_types::runtime::Usage {
    let metrics = &failure.metrics;
    if metrics.provider_calls_known && metrics.provider_calls == 0 {
        excalibur_types::runtime::Usage::default()
    } else if metrics.usage_known {
        excalibur_types::runtime::Usage {
            input_tokens: metrics.usage.input_tokens,
            output_tokens: metrics.usage.output_tokens,
            cost_microusd: metrics.cost_microusd.unwrap_or(0),
            estimated: metrics.usage.estimated,
        }
    } else {
        excalibur_types::runtime::Usage {
            input_tokens: bound.max_tokens,
            cost_microusd: bound.max_cost_microusd,
            estimated: true,
            ..Default::default()
        }
    }
}

#[tokio::test]
async fn retried_attempts_are_charged_like_final_failures() {
    let (resilience, _clock) = shared(policy());
    let server = mock(vec![status("500 Internal Server Error", &[]), ok("done")]).await;
    let gw = gateway(
        Provider::OpenAi,
        &server.url,
        "m",
        ModelRole::Worker,
        &resilience,
    );
    let observed = gw.complete_observed(request()).await.unwrap();
    assert!(
        !observed.metrics.usage_known,
        "a retried attempt's usage is unknown"
    );
    let retried = &observed.metrics.attempts[0];
    assert!(retried.usage.estimated);
    assert!(
        retried.usage.input_tokens > 100,
        "conservative input estimate"
    );
    assert_eq!(
        observed.metrics.usage.input_tokens,
        retried.usage.input_tokens + 5
    );
    // The durable response usage that budgets charge on success includes it.
    assert!(observed.response.usage.estimated);
    assert_eq!(
        observed.response.usage.input_tokens,
        retried.usage.input_tokens + 5
    );
    // The pre-dispatch bound covers the resends allowed by the policy.
    let plain = OpenAiGateway::new(
        ProviderClient::with_base_url(
            Provider::OpenAi,
            Secret::new("synthetic-key").unwrap(),
            &server.url,
        )
        .unwrap(),
        GatewayConfig::new("m", ModelRole::Worker),
    )
    .unwrap();
    let with = gw.call_bound(&request()).unwrap().max_tokens;
    let without = plain.call_bound(&request()).unwrap().max_tokens;
    assert!(with > without + 2 * retried.usage.input_tokens);
}

#[tokio::test]
async fn retried_priced_success_keeps_a_known_conservative_cost() {
    use excalibur_models::{ModelPrice, PriceTable};
    let (resilience, _clock) = shared(policy());
    let server = mock(vec![status("503 Service Unavailable", &[]), ok("done")]).await;
    let table = PriceTable {
        version: "synthetic-tariff-v1".into(),
        entries: vec![ModelPrice {
            provider: "OpenAi".into(),
            model: "m".into(),
            input_microusd_per_million: 1_000_000,
            cache_read_microusd_per_million: None,
            cache_write_microusd_per_million_by_ttl: Default::default(),
            output_microusd_per_million: 1_000_000,
            reasoning_microusd_per_million: None,
        }],
    };
    let gw = gateway(
        Provider::OpenAi,
        &server.url,
        "m",
        ModelRole::Worker,
        &resilience,
    )
    .with_price_table(table)
    .unwrap();
    let observed = gw.complete_observed(request()).await.unwrap();
    let retried_cost = observed.metrics.attempts[0].cost_microusd.unwrap();
    assert!(retried_cost > 0, "the unreported resend is priced");
    assert_eq!(
        observed.metrics.cost_microusd,
        Some(retried_cost + observed.metrics.attempts[1].cost_microusd.unwrap())
    );
    assert_eq!(
        observed.response.usage.cost_microusd,
        observed.metrics.cost_microusd.unwrap()
    );
}

#[tokio::test]
async fn context_runtime_budget_charging_sees_sent_and_unsent_calls_correctly() {
    let mut policy = policy();
    policy.retry.max_retries = 2;
    let (resilience, clock) = shared(policy);
    let key = resilience.key("OpenAi", "m");
    for _ in 0..4 {
        resilience.admit(&key).unwrap().record(Outcome::Failure);
    }
    let server = mock(vec![status("500 Internal Server Error", &[]); 2]).await;
    let gw = gateway(
        Provider::OpenAi,
        &server.url,
        "m",
        ModelRole::Worker,
        &resilience,
    );
    let bound = gw.call_bound(&request()).unwrap();
    // A sent request whose failure opens the breaker: a real Server failure,
    // never CircuitOpen, and charged at the conservative bound.
    let sent = gw.complete_observed(request()).await.unwrap_err();
    assert_eq!(class(&sent.error), FailureClass::Server);
    assert_eq!(sent.metrics.provider_calls, 1);
    assert!(!sent.metrics.usage_known);
    let charged = context_runtime_charge(&sent, &bound);
    assert_eq!(charged.input_tokens, bound.max_tokens);
    assert!(charged.estimated);
    assert!(
        clock.sleeps().is_empty(),
        "no backoff once the breaker opened"
    );
    // A call rejected before sending: CircuitOpen, provably nothing sent,
    // nothing charged.
    let unsent = gw.complete_observed(request()).await.unwrap_err();
    assert_eq!(class(&unsent.error), FailureClass::CircuitOpen);
    assert!(unsent.metrics.provider_calls_known);
    assert_eq!(unsent.metrics.provider_calls, 0);
    assert_eq!(
        context_runtime_charge(&unsent, &bound),
        excalibur_types::runtime::Usage::default()
    );
    assert_eq!(server.count(), 1);
}

#[tokio::test]
async fn breaker_opening_skips_the_backoff_and_fails_over_immediately() {
    let (resilience, clock) = shared(policy());
    let key = resilience.key("OpenAi", "p");
    for _ in 0..4 {
        resilience.admit(&key).unwrap().record(Outcome::Failure);
    }
    let primary_server = mock(vec![status(
        "429 Too Many Requests",
        &["Retry-After: 45".into()],
    )])
    .await;
    let fallback_server = mock(vec![ok("fallback")]).await;
    let role = FailoverGateway::new(vec![
        Arc::new(gateway(
            Provider::OpenAi,
            &primary_server.url,
            "p",
            ModelRole::Worker,
            &resilience,
        )),
        Arc::new(gateway(
            Provider::DeepSeek,
            &fallback_server.url,
            "f",
            ModelRole::Worker,
            &resilience,
        )),
    ])
    .unwrap();
    let observed = role.complete_observed(request()).await.unwrap();
    assert_eq!(observed.response.message.content, "fallback");
    assert!(
        clock.sleeps().is_empty(),
        "the 45 s Retry-After is not waited"
    );
    assert_eq!(primary_server.count(), 1);
    assert_eq!(observed.metrics.provider_calls, 2);
}

#[tokio::test]
async fn failover_aggregates_metrics_when_every_route_fails() {
    let mut policy = policy();
    policy.retry.max_retries = 0;
    let (resilience, _clock) = shared(policy);
    let primary_server = mock(vec![status("500 Internal Server Error", &[])]).await;
    let fallback_server = mock(vec![status("503 Service Unavailable", &[])]).await;
    let routes: Vec<Arc<dyn ModelGateway>> = vec![
        Arc::new(gateway(
            Provider::OpenAi,
            &primary_server.url,
            "p",
            ModelRole::Worker,
            &resilience,
        )),
        Arc::new(gateway(
            Provider::DeepSeek,
            &fallback_server.url,
            "f",
            ModelRole::Worker,
            &resilience,
        )),
    ];
    let role = FailoverGateway::new(routes.clone()).unwrap();
    let failure = role.complete_observed(request()).await.unwrap_err();
    assert_eq!(class(&failure.error), FailureClass::Server);
    assert_eq!(failure.metrics.provider_calls, 2);
    assert!(failure.metrics.provider_calls_known);
    assert!(!failure.metrics.usage_known);
    assert_eq!(failure.metrics.attempts.len(), 2);
    assert_eq!(
        failure.metrics.usage.input_tokens,
        failure
            .metrics
            .attempts
            .iter()
            .map(|a| a.usage.input_tokens)
            .sum::<u64>()
    );
    assert_eq!(failure.metrics.metadata.model.as_deref(), Some("f"));
    // The reservation is the sum of the routes: one call may spend on each.
    let sum = routes
        .iter()
        .map(|r| r.call_bound(&request()).unwrap().max_tokens)
        .sum::<u64>();
    assert_eq!(role.call_bound(&request()).unwrap().max_tokens, sum);
}

#[tokio::test]
async fn failover_reports_sent_primary_requests_when_the_fallback_is_open() {
    let mut policy = policy();
    policy.retry.max_retries = 0;
    let (resilience, _clock) = shared(policy);
    let fallback_key = resilience.key("DeepSeek", "f");
    for _ in 0..5 {
        resilience
            .admit(&fallback_key)
            .unwrap()
            .record(Outcome::Failure);
    }
    let primary_server = mock(vec![status("500 Internal Server Error", &[])]).await;
    let fallback_server = mock(vec![ok("never")]).await;
    let role = FailoverGateway::new(vec![
        Arc::new(gateway(
            Provider::OpenAi,
            &primary_server.url,
            "p",
            ModelRole::Worker,
            &resilience,
        )),
        Arc::new(gateway(
            Provider::DeepSeek,
            &fallback_server.url,
            "f",
            ModelRole::Worker,
            &resilience,
        )),
    ])
    .unwrap();
    let failure = role.complete_observed(request()).await.unwrap_err();
    // CircuitOpen is reserved for calls that sent nothing: the primary's
    // real failure is reported.
    assert_eq!(class(&failure.error), FailureClass::Server);
    assert_eq!(
        failure.metrics.provider_calls, 1,
        "the primary's sent request must not be reported as none"
    );
    assert!(!failure.metrics.usage_known);
    let bound = role.call_bound(&request()).unwrap();
    assert_eq!(
        context_runtime_charge(&failure, &bound).input_tokens,
        bound.max_tokens
    );
    assert_eq!(fallback_server.count(), 0);
}

/// Scripted route: streams one delta and then fails, or answers.
struct Scripted {
    fail: bool,
    calls: AtomicUsize,
    unload: usize,
}
impl Scripted {
    fn new(fail: bool, unload: usize) -> Arc<Self> {
        Arc::new(Self {
            fail,
            calls: AtomicUsize::new(0),
            unload,
        })
    }
}
impl ModelGateway for Scripted {
    fn unload_idle(&self) -> BoxFuture<'_, Result<usize, ModelError>> {
        let unload = self.unload;
        Box::pin(async move { Ok(unload) })
    }
    fn complete<'a>(
        &'a self,
        _request: ModelRequest,
    ) -> BoxFuture<'a, Result<excalibur_types::runtime::ModelResponse, ModelError>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let fail = self.fail;
        Box::pin(async move {
            if fail {
                Err(ModelError::ProviderFailure(ModelFailure {
                    class: FailureClass::Server,
                    http_status: Some(502),
                }))
            } else {
                Ok(excalibur_types::runtime::ModelResponse {
                    message: Message::text("assistant", "scripted"),
                    usage: Default::default(),
                })
            }
        })
    }
    fn complete_stream<'a>(
        &'a self,
        request: ModelRequest,
        sink: StreamSink,
    ) -> BoxFuture<'a, Result<excalibur_types::runtime::ModelResponse, ModelError>> {
        Box::pin(async move {
            sink(StreamDelta::Text {
                attempt: 0,
                delta: "partial".into(),
            });
            self.complete(request).await
        })
    }
}

#[tokio::test]
async fn a_stream_that_emitted_a_delta_never_fails_over() {
    let primary = Scripted::new(true, 2);
    let fallback = Scripted::new(false, 3);
    let role = FailoverGateway::new(vec![primary.clone(), fallback.clone()]).unwrap();
    let sink: StreamSink = Arc::new(|_| {});
    let error = role.complete_stream(request(), sink).await.unwrap_err();
    assert_eq!(class(&error), FailureClass::Server);
    assert_eq!(fallback.calls.load(Ordering::SeqCst), 0);
    // The same failure without an emitted delta does fail over.
    let response = role.complete(request()).await.unwrap();
    assert_eq!(response.message.content, "scripted");
    assert_eq!(fallback.calls.load(Ordering::SeqCst), 1);
    // unload_idle reaches every route.
    assert_eq!(role.unload_idle().await.unwrap(), 5);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_callers_get_exactly_one_half_open_probe() {
    let mut policy = policy();
    policy.breaker.consecutive_failures = 1;
    let (resilience, clock) = shared(policy);
    let key = resilience.key("OpenAi", "m");
    resilience.admit(&key).unwrap().record(Outcome::Failure);
    clock.advance(Duration::from_secs(60));
    let barrier = Arc::new(tokio::sync::Barrier::new(32));
    let mut tasks = Vec::new();
    for _ in 0..32 {
        let resilience = resilience.clone();
        let key = key.clone();
        let barrier = barrier.clone();
        tasks.push(tokio::spawn(async move {
            barrier.wait().await;
            // Hold any granted permit until every caller has tried.
            resilience.admit(&key).ok()
        }));
    }
    let mut granted = Vec::new();
    for task in tasks {
        if let Some(permit) = task.await.unwrap() {
            granted.push(permit);
        }
    }
    assert_eq!(granted.len(), 1, "exactly one concurrent probe");
    granted.pop().unwrap().record(Outcome::Success);
    assert_eq!(resilience.snapshot(&key).state, BreakerState::Closed);
}

#[test]
fn minimum_sample_cannot_exceed_the_window_capacity() {
    let clock: Arc<dyn ResilienceClock> = ManualClock::new();
    let mut policy = ResiliencePolicy::default();
    policy.breaker.min_samples = 4_096;
    assert!(ProviderResilience::new(policy.clone(), clock.clone()).is_ok());
    policy.breaker.min_samples = 4_097;
    assert!(ProviderResilience::new(policy, clock).is_err());
}
