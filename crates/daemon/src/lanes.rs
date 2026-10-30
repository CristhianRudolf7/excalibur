use excalibur_models::{ModelError, ModelGateway};
use excalibur_types::runtime::{BoxFuture, ModelRequest, ModelResponse};
use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{sync::Notify, time::Instant};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Lane {
    Master = 0,
    User = 1,
    Background = 2,
    Maintenance = 3,
}
#[derive(Clone, Debug)]
pub struct LaneLimits {
    pub concurrency: usize,
    pub requests_per_minute: u64,
    pub tokens_per_minute: u64,
}
impl Default for LaneLimits {
    fn default() -> Self {
        Self {
            concurrency: 8,
            requests_per_minute: 1000,
            tokens_per_minute: 1_000_000,
        }
    }
}
struct State {
    next: u64,
    waiting: [VecDeque<u64>; 4],
    active: usize,
    background_active: usize,
    window: Instant,
    requests: u64,
    background_requests: u64,
    tokens: u64,
    background_tokens: u64,
    user_active: bool,
    provider_requests: u64,
    provider_tokens: u64,
    refill_at: Instant,
    request_balance: f64,
    token_balance: f64,
    background_request_balance: f64,
    background_token_balance: f64,
}
struct Shared {
    limits: LaneLimits,
    state: Mutex<State>,
    wake: Notify,
}
#[derive(Clone)]
pub struct LaneScheduler(Arc<Shared>);
impl LaneScheduler {
    pub fn new(limits: LaneLimits) -> Result<Self, &'static str> {
        if limits.concurrency < 2
            || limits.requests_per_minute < 10
            || limits.tokens_per_minute < 10
            || limits.requests_per_minute > 1_000_000_000
            || limits.tokens_per_minute > 1_000_000_000_000
        {
            return Err("lane limits require at least two slots and ten quota units");
        }
        Ok(Self(Arc::new(Shared {
            limits: limits.clone(),
            state: Mutex::new(State {
                next: 0,
                waiting: std::array::from_fn(|_| VecDeque::new()),
                active: 0,
                background_active: 0,
                window: Instant::now(),
                requests: 0,
                background_requests: 0,
                tokens: 0,
                background_tokens: 0,
                user_active: false,
                provider_requests: limits.requests_per_minute,
                provider_tokens: limits.tokens_per_minute,
                refill_at: Instant::now(),
                request_balance: limits.requests_per_minute as f64,
                token_balance: limits.tokens_per_minute as f64,
                background_request_balance: (limits.requests_per_minute / 10 * 9) as f64,
                background_token_balance: (limits.tokens_per_minute / 10 * 9) as f64,
            }),
            wake: Notify::new(),
        })))
    }
    pub fn observe_provider_limits(&self, limits: Option<(u64, u64)>) {
        if let Some((requests, tokens)) = limits.filter(|(r, t)| *r > 0 && *t > 0) {
            let mut state = self.0.state.lock().unwrap_or_else(|p| p.into_inner());
            if state.provider_requests == requests.min(self.0.limits.requests_per_minute)
                && state.provider_tokens == tokens.min(self.0.limits.tokens_per_minute)
            {
                return;
            }
            state.provider_requests = requests.min(self.0.limits.requests_per_minute);
            state.provider_tokens = tokens.min(self.0.limits.tokens_per_minute);
            state.request_balance = state
                .request_balance
                .min(state.provider_requests.saturating_sub(state.requests) as f64);
            state.token_balance = state
                .token_balance
                .min(state.provider_tokens.saturating_sub(state.tokens) as f64);
            state.background_request_balance = state.background_request_balance.min(
                (state.provider_requests / 10 * 9).saturating_sub(state.background_requests) as f64,
            );
            state.background_token_balance = state.background_token_balance.min(
                (state.provider_tokens / 10 * 9).saturating_sub(state.background_tokens) as f64,
            );
            drop(state);
            self.0.wake.notify_waiters();
        }
    }
    pub fn gateway(&self, lane: Lane, delegate: Arc<dyn ModelGateway>) -> Arc<dyn ModelGateway> {
        Arc::new(PriorityGateway {
            scheduler: self.clone(),
            lane,
            delegate,
        })
    }
    pub fn set_user_active(&self, active: bool) {
        self.0
            .state
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .user_active = active;
        self.0.wake.notify_waiters();
    }
    pub fn waiting(&self) -> [usize; 4] {
        let s = self.0.state.lock().unwrap_or_else(|p| p.into_inner());
        std::array::from_fn(|i| s.waiting[i].len())
    }
    pub async fn acquire(&self, lane: Lane, tokens: u64) -> Result<LanePermit, &'static str> {
        let limits = &self.0.limits;
        let background = lane != Lane::Master;
        let quota = if background {
            limits.tokens_per_minute / 10 * 9
        } else {
            limits.tokens_per_minute
        };
        if tokens > quota {
            return Err("request exceeds lane token quota");
        }
        let id = {
            let mut s = self.0.state.lock().unwrap_or_else(|p| p.into_inner());
            s.next = s.next.checked_add(1).ok_or("lane sequence exhausted")?;
            let id = s.next;
            s.waiting[lane as usize].push_back(id);
            id
        };
        let mut waiter = Waiter {
            shared: self.0.clone(),
            lane,
            id,
            queued: true,
        };
        loop {
            let notified = self.0.wake.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            let deadline = {
                let mut s = self.0.state.lock().unwrap_or_else(|p| p.into_inner());
                if s.window.elapsed() >= Duration::from_secs(60) {
                    s.window = Instant::now();
                    s.requests = 0;
                    s.background_requests = 0;
                    s.tokens = 0;
                    s.background_tokens = 0;
                }
                let elapsed = s.refill_at.elapsed().as_secs_f64() / 60.0;
                s.refill_at = Instant::now();
                s.request_balance = (s.request_balance + elapsed * s.provider_requests as f64)
                    .min(s.provider_requests as f64);
                s.token_balance = (s.token_balance + elapsed * s.provider_tokens as f64)
                    .min(s.provider_tokens as f64);
                s.background_request_balance = (s.background_request_balance
                    + elapsed * (s.provider_requests / 10 * 9) as f64)
                    .min((s.provider_requests / 10 * 9) as f64);
                s.background_token_balance = (s.background_token_balance
                    + elapsed * (s.provider_tokens / 10 * 9) as f64)
                    .min((s.provider_tokens / 10 * 9) as f64);
                let available_quota = if background {
                    s.provider_tokens / 10 * 9
                } else {
                    s.provider_tokens
                };
                if tokens > available_quota || background && s.provider_requests / 10 * 9 == 0 {
                    return Err("request exceeds observed provider lane quota");
                }
                let ready = s.waiting[lane as usize].front() == Some(&id)
                    && s.waiting[..lane as usize].iter().all(VecDeque::is_empty)
                    && s.active < limits.concurrency
                    && (!background || s.background_active < limits.concurrency - 1)
                    && s.request_balance >= 1.0
                    && s.token_balance >= tokens as f64
                    && (!background
                        || s.background_request_balance >= 1.0
                            && s.background_token_balance >= tokens as f64)
                    && !(lane == Lane::Maintenance && s.user_active);
                if ready {
                    s.waiting[lane as usize].pop_front();
                    s.active += 1;
                    s.requests += 1;
                    s.tokens += tokens;
                    s.request_balance -= 1.0;
                    s.token_balance -= tokens as f64;
                    if background {
                        s.background_active += 1;
                        s.background_requests += 1;
                        s.background_tokens += tokens;
                        s.background_request_balance -= 1.0;
                        s.background_token_balance -= tokens as f64;
                    }
                    waiter.queued = false;
                    self.0.wake.notify_waiters();
                    return Ok(LanePermit {
                        shared: self.0.clone(),
                        background,
                    });
                }
                let mut delay =
                    ((1.0 - s.request_balance).max(0.0) * 60.0 / s.provider_requests as f64).max(
                        (tokens as f64 - s.token_balance).max(0.0) * 60.0
                            / s.provider_tokens as f64,
                    );
                if background {
                    delay = delay
                        .max(
                            (1.0 - s.background_request_balance).max(0.0) * 60.0
                                / (s.provider_requests / 10 * 9) as f64,
                        )
                        .max(
                            (tokens as f64 - s.background_token_balance).max(0.0) * 60.0
                                / (s.provider_tokens / 10 * 9) as f64,
                        );
                }
                Instant::now()
                    + Duration::from_secs_f64(if delay > 0.0 { delay.max(0.001) } else { 60.0 })
            };
            tokio::select! {_=notified=>{},_=tokio::time::sleep_until(deadline)=>{}}
        }
    }
}
struct Waiter {
    shared: Arc<Shared>,
    lane: Lane,
    id: u64,
    queued: bool,
}
impl Drop for Waiter {
    fn drop(&mut self) {
        if self.queued {
            self.shared
                .state
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .waiting[self.lane as usize]
                .retain(|id| *id != self.id);
            self.shared.wake.notify_waiters();
        }
    }
}
pub struct LanePermit {
    shared: Arc<Shared>,
    background: bool,
}
impl Drop for LanePermit {
    fn drop(&mut self) {
        let mut s = self.shared.state.lock().unwrap_or_else(|p| p.into_inner());
        s.active -= 1;
        if self.background {
            s.background_active -= 1;
        }
        drop(s);
        self.shared.wake.notify_waiters();
    }
}
struct PriorityGateway {
    scheduler: LaneScheduler,
    lane: Lane,
    delegate: Arc<dyn ModelGateway>,
}
impl PriorityGateway {
    fn request_lane(&self, request: &ModelRequest) -> Lane {
        if self.lane != Lane::User {
            return self.lane;
        }
        let core = request
            .messages
            .first()
            .filter(|message| message.role == "system")
            .map(|message| message.content.as_str())
            .unwrap_or("");
        if core.contains("\n[excalibur-host-lane:3]\n") {
            Lane::Maintenance
        } else if core.contains("\n[excalibur-host-lane:2]\n") {
            Lane::Background
        } else {
            Lane::User
        }
    }
    fn tokens(&self, request: &ModelRequest) -> u64 {
        self.delegate
            .call_bound(request)
            .map(|b| b.max_tokens)
            .unwrap_or_else(|| {
                serde_json::to_vec(request)
                    .map(|b| b.len().div_ceil(3) as u64)
                    .unwrap_or(u64::MAX)
            })
    }
    async fn permit(
        &self,
        request: &ModelRequest,
    ) -> Result<LanePermit, excalibur_models::ModelCallError> {
        self.scheduler
            .observe_provider_limits(self.delegate.rate_limits());
        self.scheduler
            .acquire(self.request_lane(request), self.tokens(request))
            .await
            .map_err(|e| {
                excalibur_models::ModelCallError::new(
                    ModelError::Configuration(e.into()),
                    excalibur_models::ModelCallMetrics::request(request, self.delegate.metadata()),
                )
            })
    }
}
impl ModelGateway for PriorityGateway {
    fn rate_limits(&self) -> Option<(u64, u64)> {
        self.delegate.rate_limits()
    }
    fn request_time_override(&self, step: u64) -> Option<String> {
        self.delegate.request_time_override(step)
    }
    fn context_policy_override(&self) -> Option<serde_json::Value> {
        self.delegate.context_policy_override()
    }
    fn request_cache_override(&self, step: u64) -> Option<excalibur_types::runtime::PromptCache> {
        self.delegate.request_cache_override(step)
    }
    fn complete_observed<'a>(
        &'a self,
        request: ModelRequest,
    ) -> BoxFuture<'a, Result<excalibur_models::ObservedResponse, excalibur_models::ModelCallError>>
    {
        Box::pin(async move {
            let _permit = self.permit(&request).await?;
            {
                let result = self.delegate.complete_observed(request).await;
                self.scheduler
                    .observe_provider_limits(self.delegate.rate_limits());
                result
            }
        })
    }
    fn complete_stream_observed<'a>(
        &'a self,
        request: ModelRequest,
        sink: excalibur_models::StreamSink,
    ) -> BoxFuture<'a, Result<excalibur_models::ObservedResponse, excalibur_models::ModelCallError>>
    {
        Box::pin(async move {
            let _permit = self.permit(&request).await?;
            {
                let result = self.delegate.complete_stream_observed(request, sink).await;
                self.scheduler
                    .observe_provider_limits(self.delegate.rate_limits());
                result
            }
        })
    }
    fn complete_cache_warm_observed<'a>(
        &'a self,
        request: ModelRequest,
        sink: excalibur_models::StreamSink,
    ) -> BoxFuture<'a, Result<excalibur_models::ObservedResponse, excalibur_models::ModelCallError>>
    {
        Box::pin(async move {
            let _permit = self.permit(&request).await?;
            let result = self
                .delegate
                .complete_cache_warm_observed(request, sink)
                .await;
            self.scheduler
                .observe_provider_limits(self.delegate.rate_limits());
            result
        })
    }
    fn identity(&self) -> String {
        self.delegate.identity()
    }
    fn cost_is_known(&self) -> bool {
        self.delegate.cost_is_known()
    }
    fn metadata(&self) -> excalibur_models::GatewayMetadata {
        self.delegate.metadata()
    }
    fn cache_settings(&self) -> excalibur_models::CacheSettings {
        self.delegate.cache_settings()
    }
    fn native_deferred_tools(&self) -> bool {
        self.delegate.native_deferred_tools()
    }
    fn deferred_catalog_override(&self) -> Option<Vec<excalibur_types::runtime::ToolDefinition>> {
        self.delegate.deferred_catalog_override()
    }
    fn call_bound(&self, r: &ModelRequest) -> Option<excalibur_models::ModelCallBound> {
        self.delegate.call_bound(r)
    }
    fn complete<'a>(
        &'a self,
        request: ModelRequest,
    ) -> BoxFuture<'a, Result<ModelResponse, ModelError>> {
        Box::pin(async move {
            self.scheduler
                .observe_provider_limits(self.delegate.rate_limits());
            let tokens = self.tokens(&request);
            let _permit = self
                .scheduler
                .acquire(self.request_lane(&request), tokens)
                .await
                .map_err(|e| ModelError::Configuration(e.into()))?;
            {
                let result = self.delegate.complete(request).await;
                self.scheduler
                    .observe_provider_limits(self.delegate.rate_limits());
                result
            }
        })
    }
    fn complete_stream<'a>(
        &'a self,
        request: ModelRequest,
        sink: excalibur_models::StreamSink,
    ) -> BoxFuture<'a, Result<ModelResponse, ModelError>> {
        Box::pin(async move {
            self.scheduler
                .observe_provider_limits(self.delegate.rate_limits());
            let tokens = self.tokens(&request);
            let _permit = self
                .scheduler
                .acquire(self.request_lane(&request), tokens)
                .await
                .map_err(|e| ModelError::Configuration(e.into()))?;
            {
                let result = self.delegate.complete_stream(request, sink).await;
                self.scheduler
                    .observe_provider_limits(self.delegate.rate_limits());
                result
            }
        })
    }
}
