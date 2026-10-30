//! Provider error resilience shared by every gateway user in a process.
//!
//! Architecture section 19.5: transient errors are retried with exponential
//! backoff and jitter, `Retry-After` is honoured, and a per-provider circuit
//! breaker opens after 5 consecutive failures or a failure rate above 50 %
//! within one minute, stays open for 60 s and then admits a single half-open
//! probe. While a breaker is open, [`FailoverGateway`] sends the request to the
//! role's next configured route.
//!
//! Only closed failure classifications are stored; no provider response body,
//! header other than the retry hint, or credential ever reaches this module.
use crate::{
    FailureClass, ModelCallError, ModelError, ModelFailure, ModelGateway, ObservedResponse,
    StreamDelta, StreamSink,
};
use excalibur_types::runtime::{BoxFuture, ModelRequest, ModelResponse};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, VecDeque},
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant, SystemTime},
};

/// Time source and sleeper. Tests inject a manual clock so no test sleeps.
pub trait ResilienceClock: Send + Sync {
    /// Monotonic time since an arbitrary fixed origin.
    fn monotonic(&self) -> Duration;
    /// Wall-clock time, used only to interpret `Retry-After` HTTP-dates.
    fn system_time(&self) -> SystemTime;
    fn sleep(&self, duration: Duration) -> BoxFuture<'_, ()>;
}
/// Real clock backed by `Instant`, `SystemTime` and the tokio timer.
pub struct SystemClock {
    origin: Instant,
}
impl Default for SystemClock {
    fn default() -> Self {
        Self {
            origin: Instant::now(),
        }
    }
}
impl ResilienceClock for SystemClock {
    fn monotonic(&self) -> Duration {
        self.origin.elapsed()
    }
    fn system_time(&self) -> SystemTime {
        SystemTime::now()
    }
    fn sleep(&self, duration: Duration) -> BoxFuture<'_, ()> {
        Box::pin(tokio::time::sleep(duration))
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RetryPolicy {
    /// Retries after the first attempt; the identical request is resent.
    pub max_retries: u32,
    pub base_delay_ms: u64,
    pub max_delay_ms: u64,
    /// A server `Retry-After` longer than this is not waited for: the call
    /// fails (and may fail over) instead of sleeping less than requested.
    pub max_retry_after_ms: u64,
    /// Equal jitter: the backoff is uniformly drawn from [d/2, d].
    pub jitter: bool,
}
impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_retries: 2,
            base_delay_ms: 500,
            max_delay_ms: 8_000,
            max_retry_after_ms: 60_000,
            jitter: true,
        }
    }
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum BreakerScope {
    /// One breaker per provider, as specified in section 19.5.
    Provider,
    /// One breaker per provider and model.
    ProviderModel,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct BreakerPolicy {
    pub consecutive_failures: u32,
    pub window_ms: u64,
    /// The breaker opens when failures exceed this percentage of the window.
    pub failure_rate_percent: u8,
    /// Minimum outcomes in the window before the rate rule may open it.
    pub min_samples: u32,
    pub open_ms: u64,
    pub scope: BreakerScope,
}
impl Default for BreakerPolicy {
    fn default() -> Self {
        Self {
            consecutive_failures: 5,
            window_ms: 60_000,
            failure_rate_percent: 50,
            min_samples: 10,
            open_ms: 60_000,
            scope: BreakerScope::Provider,
        }
    }
}
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ResiliencePolicy {
    pub retry: RetryPolicy,
    pub breaker: BreakerPolicy,
}
impl ResiliencePolicy {
    pub fn validate(&self) -> Result<(), ModelError> {
        let r = &self.retry;
        let b = &self.breaker;
        let hour = 3_600_000;
        let invalid = |m: &str| Err(ModelError::Configuration(m.into()));
        if r.max_retries > 10 {
            return invalid("at most 10 provider retries");
        }
        if r.base_delay_ms == 0 || r.max_delay_ms < r.base_delay_ms || r.max_delay_ms > hour {
            return invalid("retry delays must be positive, ordered and at most one hour");
        }
        if r.max_retry_after_ms > hour {
            return invalid("Retry-After cap must be at most one hour");
        }
        if b.consecutive_failures == 0 || b.consecutive_failures > 1_000 {
            return invalid("consecutive failure threshold must be between 1 and 1000");
        }
        if b.window_ms == 0 || b.window_ms > hour || b.open_ms == 0 || b.open_ms > hour {
            return invalid("breaker window and open time must be positive and at most one hour");
        }
        if b.failure_rate_percent >= 100 || b.min_samples == 0 || b.min_samples > 10_000 {
            return invalid("failure rate must be below 100 % with 1 to 10000 minimum samples");
        }
        Ok(())
    }
}

/// Retryable failures are transient: the identical request may succeed later.
pub fn is_retryable(failure: &ModelFailure) -> bool {
    match failure.class {
        FailureClass::Transport
        | FailureClass::Timeout
        | FailureClass::RateLimit
        | FailureClass::Overloaded => true,
        // 501 and 505 are permanent server answers.
        FailureClass::Server => !matches!(failure.http_status, Some(501 | 505)),
        _ => false,
    }
}
/// Failures that indicate the provider is unavailable to this process and
/// therefore count against its breaker. Client errors, authentication and
/// output validation do not: they are surfaced to the caller unchanged.
pub fn is_availability_failure(failure: &ModelFailure) -> bool {
    is_retryable(failure)
        || matches!(
            failure.class,
            FailureClass::Server | FailureClass::Quota | FailureClass::CircuitOpen
        )
}

/// Parse a `Retry-After` value: delta-seconds or an HTTP-date (IMF-fixdate,
/// RFC 850 or asctime). Past dates mean "retry now". Invalid values are None.
pub fn parse_retry_after(value: &str, now: SystemTime) -> Option<Duration> {
    let value = value.trim();
    if !value.is_empty() && value.bytes().all(|b| b.is_ascii_digit()) {
        return Some(
            value
                .parse::<u64>()
                .map(Duration::from_secs)
                .unwrap_or(Duration::MAX),
        );
    }
    let at = httpdate::parse_http_date(value).ok()?;
    Some(at.duration_since(now).unwrap_or(Duration::ZERO))
}
/// Interpret a provider hint, preferring the millisecond header when valid.
pub fn retry_hint_delay(
    hint: &excalibur_providers::RetryHint,
    now: SystemTime,
) -> Option<Duration> {
    hint.retry_after_ms
        .as_deref()
        .filter(|v| !v.is_empty() && v.bytes().all(|b| b.is_ascii_digit()))
        .map(|v| {
            v.parse::<u64>()
                .map(Duration::from_millis)
                .unwrap_or(Duration::MAX)
        })
        .or_else(|| {
            hint.retry_after
                .as_deref()
                .and_then(|v| parse_retry_after(v, now))
        })
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct BreakerKey {
    pub provider: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum BreakerState {
    Closed,
    Open,
    HalfOpen,
}
/// Evidence view of one breaker (section 19.7: breaker state).
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct BreakerSnapshot {
    pub key: BreakerKey,
    pub state: BreakerState,
    pub consecutive_failures: u32,
    pub window_samples: u32,
    pub window_failures: u32,
    /// Remaining open time, when open.
    pub open_remaining_ms: Option<u64>,
    pub times_opened: u64,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case", tag = "event")]
pub enum ResilienceEvent {
    Retry {
        key: BreakerKey,
        failure: ModelFailure,
        delay_ms: u64,
        from_retry_after: bool,
    },
    RetryAfterExceedsCap {
        key: BreakerKey,
        requested_ms: u64,
    },
    Opened {
        key: BreakerKey,
        consecutive_failures: u32,
        window_samples: u32,
        window_failures: u32,
    },
    HalfOpen {
        key: BreakerKey,
    },
    Closed {
        key: BreakerKey,
    },
    Rejected {
        key: BreakerKey,
    },
    Failover {
        from: BreakerKey,
        to: BreakerKey,
        failure: ModelFailure,
    },
}
const MAX_EVENTS: usize = 512;
const MAX_WINDOW: usize = 4_096;

#[derive(Debug)]
enum State {
    Closed,
    Open { until: Duration },
    HalfOpen { probe: Option<u64> },
}
#[derive(Debug)]
struct Breaker {
    state: State,
    consecutive: u32,
    window: VecDeque<(Duration, bool)>,
    times_opened: u64,
}
impl Breaker {
    fn new() -> Self {
        Self {
            state: State::Closed,
            consecutive: 0,
            window: VecDeque::new(),
            times_opened: 0,
        }
    }
    fn trim(&mut self, now: Duration, window: Duration) {
        while self
            .window
            .front()
            .is_some_and(|(at, _)| now.saturating_sub(*at) >= window)
            || self.window.len() > MAX_WINDOW
        {
            self.window.pop_front();
        }
    }
    fn counts(&self) -> (u32, u32) {
        let failures = self.window.iter().filter(|(_, failed)| *failed).count();
        (self.window.len() as u32, failures as u32)
    }
}

/// Admission to one provider attempt. Dropping it without recording an
/// outcome (cancellation) releases a half-open probe slot.
pub struct Permit {
    owner: Arc<ProviderResilience>,
    key: BreakerKey,
    probe: Option<u64>,
    done: bool,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    Success,
    Failure,
    /// The provider answered but the result says nothing about availability.
    Neutral,
}
impl Permit {
    pub fn key(&self) -> &BreakerKey {
        &self.key
    }
    pub fn record(mut self, outcome: Outcome) {
        self.done = true;
        self.owner.record(&self.key, self.probe, outcome);
    }
}
impl Drop for Permit {
    fn drop(&mut self) {
        if !self.done {
            self.owner.record(&self.key, self.probe, Outcome::Neutral);
        }
    }
}

/// Process-wide, thread-safe retry and breaker state. Clone the `Arc` into
/// every gateway (master and workers) so all callers share the breakers.
pub struct ProviderResilience {
    policy: ResiliencePolicy,
    clock: Arc<dyn ResilienceClock>,
    breakers: Mutex<BTreeMap<BreakerKey, Breaker>>,
    events: Mutex<VecDeque<ResilienceEvent>>,
    rng: AtomicU64,
    probes: AtomicU64,
}
static PROCESS: OnceLock<Arc<ProviderResilience>> = OnceLock::new();
static PROCESS_CUSTOM: AtomicBool = AtomicBool::new(false);
impl ProviderResilience {
    pub fn new(
        policy: ResiliencePolicy,
        clock: Arc<dyn ResilienceClock>,
    ) -> Result<Arc<Self>, ModelError> {
        policy.validate()?;
        let seed = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0x9e37_79b9_7f4a_7c15);
        Ok(Arc::new(Self {
            policy,
            clock,
            breakers: Mutex::new(BTreeMap::new()),
            events: Mutex::new(VecDeque::new()),
            rng: AtomicU64::new(seed | 1),
            probes: AtomicU64::new(1),
        }))
    }
    /// The single process instance, with the default policy and real clock
    /// unless [`Self::install_process`] ran first.
    pub fn process() -> Arc<Self> {
        PROCESS
            .get_or_init(|| {
                Self::new(
                    ResiliencePolicy::default(),
                    Arc::new(SystemClock::default()),
                )
                .expect("default resilience policy is valid")
            })
            .clone()
    }
    /// Configure the process instance once, before any gateway uses it.
    pub fn install_process(
        policy: ResiliencePolicy,
        clock: Arc<dyn ResilienceClock>,
    ) -> Result<Arc<Self>, ModelError> {
        let instance = Self::new(policy, clock)?;
        if PROCESS_CUSTOM.swap(true, Ordering::SeqCst) || PROCESS.set(instance.clone()).is_err() {
            return Err(ModelError::Configuration(
                "process resilience state is already initialized".into(),
            ));
        }
        Ok(instance)
    }
    pub fn policy(&self) -> &ResiliencePolicy {
        &self.policy
    }
    pub fn clock(&self) -> &Arc<dyn ResilienceClock> {
        &self.clock
    }
    pub fn key(&self, provider: &str, model: &str) -> BreakerKey {
        BreakerKey {
            provider: provider.into(),
            model: (self.policy.breaker.scope == BreakerScope::ProviderModel).then(|| model.into()),
        }
    }
    fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
        mutex.lock().unwrap_or_else(|p| p.into_inner())
    }
    fn event(&self, event: ResilienceEvent) {
        let mut events = Self::lock(&self.events);
        if events.len() == MAX_EVENTS {
            events.pop_front();
        }
        events.push_back(event);
    }
    /// Bounded recent event log for evidence and telemetry.
    pub fn events(&self) -> Vec<ResilienceEvent> {
        Self::lock(&self.events).iter().cloned().collect()
    }
    pub fn note_failover(&self, from: BreakerKey, to: BreakerKey, failure: ModelFailure) {
        self.event(ResilienceEvent::Failover { from, to, failure });
    }
    pub fn snapshot(&self, key: &BreakerKey) -> BreakerSnapshot {
        let now = self.clock.monotonic();
        let window = Duration::from_millis(self.policy.breaker.window_ms);
        let mut breakers = Self::lock(&self.breakers);
        let Some(breaker) = breakers.get_mut(key) else {
            return BreakerSnapshot {
                key: key.clone(),
                state: BreakerState::Closed,
                consecutive_failures: 0,
                window_samples: 0,
                window_failures: 0,
                open_remaining_ms: None,
                times_opened: 0,
            };
        };
        breaker.trim(now, window);
        let (samples, failures) = breaker.counts();
        let (state, remaining) = match breaker.state {
            State::Closed => (BreakerState::Closed, None),
            State::Open { until } if until > now => (
                BreakerState::Open,
                Some(until.saturating_sub(now).as_millis() as u64),
            ),
            State::Open { .. } | State::HalfOpen { .. } => (BreakerState::HalfOpen, None),
        };
        BreakerSnapshot {
            key: key.clone(),
            state,
            consecutive_failures: breaker.consecutive,
            window_samples: samples,
            window_failures: failures,
            open_remaining_ms: remaining,
            times_opened: breaker.times_opened,
        }
    }
    /// Ask to send one provider attempt. Rejected while open, and while a
    /// half-open probe is already in flight.
    pub fn admit(self: &Arc<Self>, key: &BreakerKey) -> Result<Permit, ModelFailure> {
        let now = self.clock.monotonic();
        let mut breakers = Self::lock(&self.breakers);
        let breaker = breakers.entry(key.clone()).or_insert_with(Breaker::new);
        let probe = match breaker.state {
            State::Closed => None,
            State::Open { until } if until > now => {
                drop(breakers);
                self.event(ResilienceEvent::Rejected { key: key.clone() });
                return Err(circuit_open());
            }
            State::Open { .. } | State::HalfOpen { probe: None } => {
                if matches!(breaker.state, State::Open { .. }) {
                    self.event(ResilienceEvent::HalfOpen { key: key.clone() });
                }
                let id = self.probes.fetch_add(1, Ordering::Relaxed);
                breaker.state = State::HalfOpen { probe: Some(id) };
                Some(id)
            }
            State::HalfOpen { probe: Some(_) } => {
                drop(breakers);
                self.event(ResilienceEvent::Rejected { key: key.clone() });
                return Err(circuit_open());
            }
        };
        Ok(Permit {
            owner: self.clone(),
            key: key.clone(),
            probe,
            done: false,
        })
    }
    fn record(&self, key: &BreakerKey, probe: Option<u64>, outcome: Outcome) {
        let policy = &self.policy.breaker;
        let now = self.clock.monotonic();
        let mut event = None;
        {
            let mut breakers = Self::lock(&self.breakers);
            let breaker = breakers.entry(key.clone()).or_insert_with(Breaker::new);
            let is_probe = probe.is_some()
                && matches!(breaker.state, State::HalfOpen { probe: current } if current == probe);
            match outcome {
                Outcome::Neutral => {
                    if is_probe {
                        breaker.state = State::HalfOpen { probe: None };
                    }
                }
                Outcome::Success => {
                    breaker.consecutive = 0;
                    breaker.window.push_back((now, false));
                    if is_probe {
                        breaker.state = State::Closed;
                        breaker.window.clear();
                        event = Some(ResilienceEvent::Closed { key: key.clone() });
                    }
                }
                Outcome::Failure => {
                    breaker.consecutive = breaker.consecutive.saturating_add(1);
                    breaker.window.push_back((now, true));
                    breaker.trim(now, Duration::from_millis(policy.window_ms));
                    let (samples, failures) = breaker.counts();
                    let rate_trip = samples >= policy.min_samples
                        && u64::from(failures) * 100
                            > u64::from(samples) * u64::from(policy.failure_rate_percent);
                    let trip = is_probe
                        || (matches!(breaker.state, State::Closed)
                            && (breaker.consecutive >= policy.consecutive_failures || rate_trip));
                    if trip {
                        breaker.state = State::Open {
                            until: now + Duration::from_millis(policy.open_ms),
                        };
                        breaker.times_opened += 1;
                        event = Some(ResilienceEvent::Opened {
                            key: key.clone(),
                            consecutive_failures: breaker.consecutive,
                            window_samples: samples,
                            window_failures: failures,
                        });
                    }
                }
            }
            breaker.trim(now, Duration::from_millis(policy.window_ms));
        }
        if let Some(event) = event {
            self.event(event);
        }
    }
    fn unit(&self) -> f64 {
        // SplitMix64 over a shared atomic counter: lock-free jitter.
        let mut z = self
            .rng
            .fetch_add(0x9e37_79b9_7f4a_7c15, Ordering::Relaxed)
            .wrapping_add(0x9e37_79b9_7f4a_7c15);
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^= z >> 31;
        (z >> 11) as f64 / (1u64 << 53) as f64
    }
    /// Delay before retry number `retry` (0-based), or None when no retry is
    /// allowed: retries exhausted, non-retryable failure or a server
    /// `Retry-After` beyond the configured cap.
    pub fn retry_delay(
        &self,
        key: &BreakerKey,
        retry: u32,
        failure: &ModelFailure,
        hint: Option<&excalibur_providers::RetryHint>,
    ) -> Option<Duration> {
        let policy = &self.policy.retry;
        if retry >= policy.max_retries || !is_retryable(failure) {
            return None;
        }
        if let Some(requested) = hint.and_then(|h| retry_hint_delay(h, self.clock.system_time())) {
            if requested > Duration::from_millis(policy.max_retry_after_ms) {
                self.event(ResilienceEvent::RetryAfterExceedsCap {
                    key: key.clone(),
                    requested_ms: requested.as_millis().min(u128::from(u64::MAX)) as u64,
                });
                return None;
            }
            self.event(ResilienceEvent::Retry {
                key: key.clone(),
                failure: failure.clone(),
                delay_ms: requested.as_millis() as u64,
                from_retry_after: true,
            });
            return Some(requested);
        }
        let exponential = policy
            .base_delay_ms
            .saturating_mul(1u64.checked_shl(retry).unwrap_or(u64::MAX))
            .min(policy.max_delay_ms);
        let delay = if policy.jitter {
            let half = exponential / 2;
            half + ((exponential - half) as f64 * self.unit()) as u64
        } else {
            exponential
        };
        self.event(ResilienceEvent::Retry {
            key: key.clone(),
            failure: failure.clone(),
            delay_ms: delay,
            from_retry_after: false,
        });
        Some(Duration::from_millis(delay))
    }
}
fn circuit_open() -> ModelFailure {
    ModelFailure {
        class: FailureClass::CircuitOpen,
        http_status: None,
    }
}

/// Ordered routes for one role (primary first). A request moves to the next
/// route when the current one is rejected by its open breaker or fails with
/// an availability failure after its own bounded retries. Client errors,
/// authentication failures and output validation never fail over. A stream
/// that already delivered a delta is never restarted on another route.
pub struct FailoverGateway {
    routes: Vec<Arc<dyn ModelGateway>>,
    resilience: Option<Arc<ProviderResilience>>,
}
impl FailoverGateway {
    pub fn new(routes: Vec<Arc<dyn ModelGateway>>) -> Result<Self, ModelError> {
        if routes.is_empty() || routes.len() > 9 {
            return Err(ModelError::Configuration(
                "a failover chain has a primary and at most 8 fallbacks".into(),
            ));
        }
        Ok(Self {
            routes,
            resilience: None,
        })
    }
    /// Record failover events in the shared state's evidence log.
    pub fn with_resilience(mut self, resilience: Arc<ProviderResilience>) -> Self {
        self.resilience = Some(resilience);
        self
    }
    fn key(&self, index: usize) -> BreakerKey {
        let metadata = self.routes[index].metadata();
        let provider = metadata
            .provider
            .unwrap_or_else(|| self.routes[index].identity());
        let model = metadata.model.unwrap_or_default();
        match &self.resilience {
            Some(resilience) => resilience.key(&provider, &model),
            None => BreakerKey {
                provider,
                model: Some(model),
            },
        }
    }
    fn should_fail_over(&self, index: usize, error: &ModelError) -> bool {
        if index + 1 >= self.routes.len() {
            return false;
        }
        let ModelError::ProviderFailure(failure) = error else {
            return false;
        };
        if !is_availability_failure(failure) {
            return false;
        }
        if let Some(resilience) = &self.resilience {
            resilience.note_failover(self.key(index), self.key(index + 1), failure.clone());
        }
        true
    }
    async fn run(
        &self,
        request: ModelRequest,
        sink: Option<StreamSink>,
        warm: bool,
    ) -> Result<ObservedResponse, ModelCallError> {
        let mut previous: Vec<crate::ModelAttemptMetrics> = Vec::new();
        let mut previous_calls = 0_u64;
        for (index, route) in self.routes.iter().enumerate() {
            let emitted = Arc::new(AtomicBool::new(false));
            let result = match &sink {
                None => route.complete_observed(request.clone()).await,
                Some(sink) => {
                    let inner = sink.clone();
                    let flag = emitted.clone();
                    let wrapped: StreamSink = Arc::new(move |delta: StreamDelta| {
                        flag.store(true, Ordering::SeqCst);
                        inner(delta)
                    });
                    if warm {
                        route
                            .complete_cache_warm_observed(request.clone(), wrapped)
                            .await
                    } else {
                        route
                            .complete_stream_observed(request.clone(), wrapped)
                            .await
                    }
                }
            };
            match result {
                Ok(mut observed) => {
                    if !previous.is_empty() {
                        let mut attempts = std::mem::take(&mut previous);
                        attempts.append(&mut observed.metrics.attempts);
                        for (i, attempt) in attempts.iter_mut().enumerate() {
                            attempt.index = i.min(u8::MAX as usize) as u8;
                        }
                        observed.metrics.attempts = attempts;
                        observed.metrics.provider_calls = observed
                            .metrics
                            .provider_calls
                            .saturating_add(previous_calls);
                    }
                    return Ok(observed);
                }
                Err(failure) => {
                    if emitted.load(Ordering::SeqCst)
                        || !self.should_fail_over(index, &failure.error)
                    {
                        return Err(failure);
                    }
                    previous_calls = previous_calls.saturating_add(failure.metrics.provider_calls);
                    previous.extend(failure.metrics.attempts.iter().cloned());
                }
            }
        }
        unreachable!("the last route never fails over")
    }
}
impl ModelGateway for FailoverGateway {
    fn rate_limits(&self) -> Option<(u64, u64)> {
        self.routes[0].rate_limits()
    }
    fn native_deferred_tools(&self) -> bool {
        self.routes.iter().all(|r| r.native_deferred_tools())
    }
    fn cache_settings(&self) -> crate::CacheSettings {
        self.routes[0].cache_settings()
    }
    fn cost_is_known(&self) -> bool {
        self.routes.iter().all(|r| r.cost_is_known())
    }
    fn identity(&self) -> String {
        use sha2::{Digest, Sha256};
        let mut hash = Sha256::new();
        for route in &self.routes {
            hash.update(route.identity().as_bytes());
            hash.update([0]);
        }
        format!("failover:{:x}", hash.finalize())
    }
    fn metadata(&self) -> crate::GatewayMetadata {
        let mut metadata = self.routes[0].metadata();
        metadata.configuration_hash = crate::metrics::hash(self.identity().as_bytes());
        metadata
    }
    fn call_bound(&self, request: &ModelRequest) -> Option<crate::ModelCallBound> {
        let mut bound = crate::ModelCallBound {
            max_tokens: 0,
            max_cost_microusd: 0,
        };
        for route in &self.routes {
            let next = route.call_bound(request)?;
            bound.max_tokens = bound.max_tokens.max(next.max_tokens);
            bound.max_cost_microusd = bound.max_cost_microusd.max(next.max_cost_microusd);
        }
        Some(bound)
    }
    fn complete_observed<'a>(
        &'a self,
        request: ModelRequest,
    ) -> BoxFuture<'a, Result<ObservedResponse, ModelCallError>> {
        Box::pin(self.run(request, None, false))
    }
    fn complete_stream_observed<'a>(
        &'a self,
        request: ModelRequest,
        sink: StreamSink,
    ) -> BoxFuture<'a, Result<ObservedResponse, ModelCallError>> {
        Box::pin(self.run(request, Some(sink), false))
    }
    fn complete_cache_warm_observed<'a>(
        &'a self,
        request: ModelRequest,
        sink: StreamSink,
    ) -> BoxFuture<'a, Result<ObservedResponse, ModelCallError>> {
        Box::pin(self.run(request, Some(sink), true))
    }
    fn complete<'a>(
        &'a self,
        request: ModelRequest,
    ) -> BoxFuture<'a, Result<ModelResponse, ModelError>> {
        Box::pin(async move {
            self.run(request, None, false)
                .await
                .map(|o| o.response)
                .map_err(|e| e.error)
        })
    }
    fn complete_stream<'a>(
        &'a self,
        request: ModelRequest,
        sink: StreamSink,
    ) -> BoxFuture<'a, Result<ModelResponse, ModelError>> {
        Box::pin(async move {
            self.run(request, Some(sink), false)
                .await
                .map(|o| o.response)
                .map_err(|e| e.error)
        })
    }
}
