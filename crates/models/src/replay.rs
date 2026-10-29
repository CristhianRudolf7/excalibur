//! Exact provider-free replay until the first divergence. No automatic live fallback.
use crate::*;
use excalibur_journal::{Event, Journal};
use std::{path::Path, sync::Mutex};
const MAX_TAPE_BYTES: u64 = 64 * 1024 * 1024;
#[derive(Clone, Serialize, Deserialize, PartialEq)]
pub enum RecordedOutcome {
    Response(ModelResponse),
    Failure(ModelFailure),
}
#[derive(Clone, Serialize, Deserialize, PartialEq)]
pub struct ReplayEntry {
    pub request: ModelRequest,
    pub outcome: RecordedOutcome,
    pub metrics: Option<ModelCallMetrics>,
}
#[derive(Clone, Serialize, Deserialize, PartialEq)]
pub struct ReplayTape {
    pub version: u32,
    pub original_gateway_identity: String,
    #[serde(default)]
    pub seed: Option<Value>,
    pub entries: Vec<ReplayEntry>,
}
impl std::fmt::Debug for ReplayTape {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReplayTape")
            .field("version", &self.version)
            .field("entries", &self.entries.len())
            .finish_non_exhaustive()
    }
}
impl ReplayTape {
    pub fn from_events(events: &[Event]) -> Result<Self, ModelError> {
        let identity = events
            .iter()
            .find(|event| event.kind == "run_started")
            .and_then(|event| event.payload["gateway_identity"].as_str())
            .ok_or_else(|| ModelError::Configuration("recorded gateway identity missing".into()))?
            .to_owned();
        let mut entries = Vec::new();
        let mut pending = None;
        for event in events {
            match event.kind.as_str() {
                "model_request" => {
                    if pending.is_some() {
                        return Err(ModelError::Configuration(
                            "recorded model request has no outcome".into(),
                        ));
                    }
                    pending = Some(
                        serde_json::from_value::<ModelRequest>(event.payload["request"].clone())
                            .map_err(|_| {
                                ModelError::Configuration("invalid recorded model request".into())
                            })?,
                    );
                }
                "model_response" => {
                    let request = pending.take().ok_or_else(|| {
                        ModelError::Configuration("recorded model response lacks request".into())
                    })?;
                    let response = serde_json::from_value(event.payload["response"].clone())
                        .map_err(|_| {
                            ModelError::Configuration("invalid recorded model response".into())
                        })?;
                    let metrics = event
                        .payload
                        .get("metrics")
                        .filter(|v| !v.is_null())
                        .map(|value| serde_json::from_value(value.clone()))
                        .transpose()
                        .map_err(|_| {
                            ModelError::Configuration("invalid recorded metrics".into())
                        })?;
                    entries.push(ReplayEntry {
                        request,
                        outcome: RecordedOutcome::Response(response),
                        metrics,
                    });
                }
                "model_failure" | "model_error" => {
                    let request = pending.take().ok_or_else(|| {
                        ModelError::Configuration("recorded model failure lacks request".into())
                    })?;
                    let metrics = event
                        .payload
                        .get("metrics")
                        .filter(|v| !v.is_null())
                        .map(|value| serde_json::from_value::<ModelCallMetrics>(value.clone()))
                        .transpose()
                        .map_err(|_| {
                            ModelError::Configuration("invalid recorded metrics".into())
                        })?;
                    let failure = event
                        .payload
                        .get("failure")
                        .and_then(|value| {
                            serde_json::from_value::<ModelFailure>(value.clone()).ok()
                        })
                        .or_else(|| {
                            metrics
                                .as_ref()
                                .and_then(|metrics| metrics.attempts.last())
                                .and_then(|attempt| attempt.error.clone())
                        })
                        .or_else(|| {
                            event
                                .payload
                                .get("failure_class")
                                .and_then(|class| {
                                    serde_json::from_value::<FailureClass>(class.clone()).ok()
                                })
                                .map(|class| ModelFailure {
                                    class,
                                    http_status: None,
                                })
                        })
                        .unwrap_or(ModelFailure {
                            class: FailureClass::Unknown,
                            http_status: None,
                        });
                    entries.push(ReplayEntry {
                        request,
                        outcome: RecordedOutcome::Failure(failure),
                        metrics,
                    });
                }
                _ => {}
            }
        }
        if pending.is_some() {
            return Err(ModelError::Configuration(
                "recorded model request is incomplete".into(),
            ));
        }
        let tape = Self {
            version: 1,
            original_gateway_identity: identity,
            seed: events
                .iter()
                .find(|event| event.kind == "run_started")
                .map(|event| event.payload.clone()),
            entries,
        };
        tape.validate()?;
        Ok(tape)
    }
    pub async fn from_journal(journal: &Journal, run: &str) -> Result<Self, ModelError> {
        let events = journal
            .replay(run, 0)
            .await
            .map_err(|_| ModelError::Configuration("recorded journal cannot be read".into()))?;
        Self::from_events(&events)
    }
    pub fn load(path: &Path) -> Result<Self, ModelError> {
        use std::io::Read;
        let file = std::fs::File::open(path)
            .map_err(|_| ModelError::Configuration("replay file cannot be opened".into()))?;
        let mut bytes = Vec::new();
        file.take(MAX_TAPE_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| ModelError::Configuration("replay file cannot be read".into()))?;
        if bytes.len() as u64 > MAX_TAPE_BYTES {
            return Err(ModelError::Configuration(
                "replay file exceeds 64 MiB".into(),
            ));
        }
        let tape: Self = serde_json::from_slice(&bytes)
            .map_err(|_| ModelError::Configuration("invalid replay file".into()))?;
        tape.validate()?;
        Ok(tape)
    }
    /// Create a new file only; callers export from their already-redacted journal.
    pub fn save(&self, path: &Path) -> Result<(), ModelError> {
        use std::io::Write;
        self.validate()?;
        let bytes = serde_json::to_vec(self)
            .map_err(|_| ModelError::Configuration("replay serialization failed".into()))?;
        if bytes.len() as u64 > MAX_TAPE_BYTES {
            return Err(ModelError::Configuration(
                "replay file exceeds 64 MiB".into(),
            ));
        }
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(path).map_err(|_| {
            ModelError::Configuration("replay output cannot be created without overwrite".into())
        })?;
        file.write_all(&bytes)
            .and_then(|_| file.sync_all())
            .map_err(|_| ModelError::Configuration("replay output write failed".into()))
    }
    pub fn validate(&self) -> Result<(), ModelError> {
        if self.version != 1 || self.entries.is_empty() || self.original_gateway_identity.is_empty()
        {
            return Err(ModelError::Configuration(
                "nonempty version-one replay and original identity required".into(),
            ));
        }
        Ok(())
    }
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ReplayDivergence {
    pub index: usize,
    pub pointer: String,
    pub expected_hash: Option<String>,
    pub actual_hash: String,
    pub reason: String,
}
impl std::fmt::Display for ReplayDivergence {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "call {} at {} (expected {}, actual {})",
            self.index,
            self.pointer,
            self.expected_hash.as_deref().unwrap_or("end-of-tape"),
            self.actual_hash
        )
    }
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ReplayLiveBudget {
    pub approval_id: String,
    pub max_calls: u64,
    pub max_tokens: u64,
    pub max_cost_microusd: u64,
}
struct Live {
    gateway: Arc<dyn ModelGateway>,
    budget: ReplayLiveBudget,
    calls: u64,
    tokens: u64,
    cost: u64,
}
struct State {
    cursor: usize,
    divergence: Option<ReplayDivergence>,
    live: Option<Live>,
    in_flight: bool,
}
pub struct ReplayGateway {
    tape: ReplayTape,
    state: Mutex<State>,
}
impl ReplayGateway {
    pub fn new(tape: ReplayTape) -> Result<Self, ModelError> {
        tape.validate()?;
        Ok(Self {
            tape,
            state: Mutex::new(State {
                cursor: 0,
                divergence: None,
                live: None,
                in_flight: false,
            }),
        })
    }
    pub fn divergence(&self) -> Option<ReplayDivergence> {
        self.state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .divergence
            .clone()
    }
    pub fn authorize_live(
        &self,
        divergence: &ReplayDivergence,
        gateway: Arc<dyn ModelGateway>,
        budget: ReplayLiveBudget,
    ) -> Result<(), ModelError> {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        if state.in_flight
            || state.divergence.as_ref() != Some(divergence)
            || state.live.is_some()
            || budget.approval_id.trim().is_empty()
            || budget.max_calls == 0
            || budget.max_tokens == 0
            || budget.max_cost_microusd == 0
            || !gateway.cost_is_known()
        {
            return Err(ModelError::Configuration("live replay requires the exact stopped divergence, a priced gateway and an explicit positive approved budget".into()));
        }
        state.live = Some(Live {
            gateway,
            budget,
            calls: 0,
            tokens: 0,
            cost: 0,
        });
        Ok(())
    }
    async fn observed(
        &self,
        request: ModelRequest,
        sink: Option<StreamSink>,
    ) -> Result<ObservedResponse, ModelCallError> {
        let metadata = self.metadata();
        let fallback = Box::new(ModelCallMetrics::request(&request, metadata));
        let choice = {
            let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
            if state.in_flight {
                return Err(ModelCallError {
                    error: ModelError::Configuration(
                        "replay gateway is already serving a call".into(),
                    ),
                    metrics: fallback,
                });
            }
            if state.divergence.is_none() {
                let expected = self.tape.entries.get(state.cursor);
                if expected.is_some_and(|entry| entry.request == request) {
                    let entry = expected.unwrap().clone();
                    state.cursor += 1;
                    Some(entry)
                } else {
                    let expected_hash = expected.map(|entry| request_hash(&entry.request));
                    let pointer = expected
                        .map(|entry| {
                            difference(
                                &serde_json::to_value(&entry.request).unwrap(),
                                &serde_json::to_value(&request).unwrap(),
                                String::new(),
                            )
                        })
                        .unwrap_or_else(|| "/".into());
                    state.divergence = Some(ReplayDivergence {
                        index: state.cursor,
                        pointer,
                        expected_hash,
                        actual_hash: request_hash(&request),
                        reason: if expected.is_some() {
                            "compiled request changed"
                        } else {
                            "recording exhausted"
                        }
                        .into(),
                    });
                    None
                }
            } else {
                None
            }
        };
        if let Some(entry) = choice {
            let mut metrics = Box::new(ModelCallMetrics::request(
                &request,
                entry
                    .metrics
                    .as_ref()
                    .map(|metrics| metrics.metadata.clone())
                    .unwrap_or_else(|| self.metadata()),
            ));
            metrics.replayed = true;
            metrics.provider_calls_known = true;
            metrics.usage_known = true;
            metrics.usage = UsageBreakdown::zero();
            metrics.cost_microusd = Some(0);
            match entry.outcome {
                RecordedOutcome::Response(response) => {
                    metrics.recorded_usage = Some(
                        entry
                            .metrics
                            .as_ref()
                            .map(|metrics| metrics.usage.clone())
                            .unwrap_or_else(|| UsageBreakdown::from_usage(&response.usage)),
                    );
                    metrics.recorded_cost_microusd =
                        entry.metrics.and_then(|metrics| metrics.cost_microusd);
                    if let Some(sink) = &sink {
                        emit(
                            sink,
                            StreamDelta::Complete {
                                attempt: 0,
                                response: response.clone(),
                            },
                        )
                        .map_err(|error| ModelCallError {
                            error,
                            metrics: metrics.clone(),
                        })?;
                    }
                    return Ok(ObservedResponse {
                        response,
                        metrics: *metrics,
                    });
                }
                RecordedOutcome::Failure(error) => {
                    return Err(ModelCallError {
                        error: ModelError::ProviderFailure(error),
                        metrics,
                    });
                }
            }
        }
        let (gateway, bound) = {
            let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
            let divergence = state.divergence.clone().unwrap();
            let Some(live) = &mut state.live else {
                return Err(ModelCallError {
                    error: ModelError::ReplayDivergence(divergence.to_string()),
                    metrics: fallback,
                });
            };
            if live.calls == 0 && request_hash(&request) != divergence.actual_hash {
                return Err(ModelCallError {
                    error: ModelError::ReplayDivergence(
                        "first live request differs from the explicitly approved divergence hash"
                            .into(),
                    ),
                    metrics: fallback,
                });
            }
            let Some(bound) = live.gateway.call_bound(&request) else {
                return Err(ModelCallError {
                    error: ModelError::ReplayBudget,
                    metrics: fallback,
                });
            };
            if live.calls >= live.budget.max_calls
                || bound.max_tokens > live.budget.max_tokens.saturating_sub(live.tokens)
                || bound.max_cost_microusd > live.budget.max_cost_microusd.saturating_sub(live.cost)
            {
                return Err(ModelCallError {
                    error: ModelError::ReplayBudget,
                    metrics: fallback,
                });
            }
            live.calls += 1;
            live.tokens = live.tokens.saturating_add(bound.max_tokens);
            live.cost = live.cost.saturating_add(bound.max_cost_microusd);
            let gateway = live.gateway.clone();
            state.in_flight = true;
            (gateway, bound)
        };
        let result = if let Some(sink) = sink {
            gateway.complete_stream_observed(request, sink).await
        } else {
            gateway.complete_observed(request).await
        };
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        state.in_flight = false;
        state.cursor += 1;
        let actual = match &result {
            Ok(observed) => &observed.metrics,
            Err(error) => error.metrics.as_ref(),
        };
        let exceeded = actual.usage_known
            && actual
                .usage
                .input_tokens
                .saturating_add(actual.usage.output_tokens)
                > bound.max_tokens
            || actual
                .cost_microusd
                .is_some_and(|cost| cost > bound.max_cost_microusd);
        if exceeded || result.is_ok() && actual.cost_microusd.is_none() {
            if let Some(live) = &mut state.live {
                live.calls = live.budget.max_calls;
            }
            return match result {
                Ok(observed) => Err(ModelCallError::new(
                    ModelError::ReplayBudget,
                    observed.metrics,
                )),
                Err(mut error) => {
                    error.error = ModelError::ReplayBudget;
                    Err(error)
                }
            };
        }
        result
    }
}
impl ModelGateway for ReplayGateway {
    fn context_policy_override(&self) -> Option<Value> {
        self.tape
            .seed
            .as_ref()?
            .get("context_policy")
            .filter(|v| !v.is_null())
            .cloned()
    }
    fn request_cache_override(&self, step: u64) -> Option<excalibur_types::runtime::PromptCache> {
        self.tape
            .entries
            .get(usize::try_from(step).ok()?)?
            .request
            .cache
            .clone()
    }
    fn identity(&self) -> String {
        self.tape.original_gateway_identity.clone()
    }
    fn metadata(&self) -> GatewayMetadata {
        self.tape
            .entries
            .first()
            .and_then(|entry| entry.metrics.as_ref())
            .map(|metrics| metrics.metadata.clone())
            .unwrap_or(GatewayMetadata {
                configuration_hash: metrics::hash(self.tape.original_gateway_identity.as_bytes()),
                ..Default::default()
            })
    }
    fn cost_is_known(&self) -> bool {
        true
    }
    fn request_time_override(&self, step: u64) -> Option<String> {
        let entry = self.tape.entries.get(usize::try_from(step).ok()?)?;
        let last = entry
            .request
            .messages
            .last()?
            .content
            .strip_prefix("[L5]\n")?;
        serde_json::from_str::<Value>(last).ok()?["now_utc"]
            .as_str()
            .map(str::to_owned)
    }
    fn complete<'a>(
        &'a self,
        request: ModelRequest,
    ) -> BoxFuture<'a, Result<ModelResponse, ModelError>> {
        Box::pin(async move {
            self.observed(request, None)
                .await
                .map(|observed| observed.response)
                .map_err(|error| error.error)
        })
    }
    fn complete_stream<'a>(
        &'a self,
        request: ModelRequest,
        sink: StreamSink,
    ) -> BoxFuture<'a, Result<ModelResponse, ModelError>> {
        Box::pin(async move {
            self.observed(request, Some(sink))
                .await
                .map(|observed| observed.response)
                .map_err(|error| error.error)
        })
    }
    fn complete_observed<'a>(
        &'a self,
        request: ModelRequest,
    ) -> BoxFuture<'a, Result<ObservedResponse, ModelCallError>> {
        Box::pin(self.observed(request, None))
    }
    fn complete_stream_observed<'a>(
        &'a self,
        request: ModelRequest,
        sink: StreamSink,
    ) -> BoxFuture<'a, Result<ObservedResponse, ModelCallError>> {
        Box::pin(self.observed(request, Some(sink)))
    }
}
fn difference(expected: &Value, actual: &Value, path: String) -> String {
    if expected == actual {
        return path;
    }
    match (expected, actual) {
        (Value::Object(a), Value::Object(b)) => {
            let keys = a.keys().chain(b.keys()).collect::<BTreeSet<_>>();
            for key in keys {
                if a.get(key) != b.get(key) {
                    let token = if [
                        "messages",
                        "tools",
                        "tools_enabled",
                        "reasoning_effort",
                        "output_schema",
                        "role",
                        "content",
                        "tool_calls",
                        "tool_call_id",
                        "id",
                        "name",
                        "arguments",
                        "description",
                        "input_schema",
                        "type",
                        "properties",
                        "required",
                        "additionalProperties",
                        "enum",
                        "items",
                        "$schema",
                    ]
                    .contains(&key.as_str())
                    {
                        key.clone()
                    } else {
                        format!("key_sha256_{}", &metrics::hash(key.as_bytes())[..16])
                    };
                    let next = format!("{path}/{token}");
                    return match (a.get(key), b.get(key)) {
                        (Some(a), Some(b)) => difference(a, b, next),
                        _ => next,
                    };
                }
            }
        }
        (Value::Array(a), Value::Array(b)) => {
            for index in 0..a.len().max(b.len()) {
                if a.get(index) != b.get(index) {
                    let next = format!("{path}/{index}");
                    return match (a.get(index), b.get(index)) {
                        (Some(a), Some(b)) => difference(a, b, next),
                        _ => next,
                    };
                }
            }
        }
        _ => {}
    }
    if path.is_empty() { "/".into() } else { path }
}
