//! Provider-neutral model boundary over the existing outbound provider transport.
//! No model selection, credentials discovery, paid inference or agent loop occurs here.
mod cache;
mod metrics;
mod native;
mod replay;
pub use cache::*;
use excalibur_providers::ProviderClient;
use excalibur_types::runtime::{BoxFuture, Message, ModelRequest, ModelResponse, ToolCall, Usage};
pub use metrics::*;
pub use replay::*;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};

#[derive(Clone, Debug, thiserror::Error)]
pub enum ModelError {
    #[error("provider transport or response failed")]
    Provider,
    #[error("invalid model configuration: {0}")]
    Configuration(String),
    #[error("model output failed validation: {0}")]
    Validation(String),
    #[error("model response was truncated or filtered")]
    Truncated,
    #[error("stream consumer failed")]
    Consumer,
    #[error("model accounting: {0}")]
    Accounting(String),
    #[error("provider request failed: {0:?}")]
    ProviderFailure(ModelFailure),
    #[error("replay request diverged: {0}")]
    ReplayDivergence(String),
    #[error("approved replay live budget exhausted")]
    ReplayBudget,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub enum StreamDelta {
    Text {
        attempt: u8,
        delta: String,
    },
    Tool {
        attempt: u8,
        index: usize,
        id: String,
        name: String,
        arguments: String,
    },
    Retry {
        attempt: u8,
        reason: String,
    },
    Complete {
        attempt: u8,
        response: ModelResponse,
    },
}
pub type StreamSink = Arc<dyn Fn(StreamDelta) + Send + Sync>;
/// Stream deltas are provisional UI data. Only the returned complete response may
/// enter the durable execution protocol; callbacks must never execute tool fragments.
pub trait ModelGateway: Send + Sync {
    /// Release only idle host-owned local inference resources. Remote-only
    /// adapters have nothing resident and return zero without making a request.
    fn unload_idle(&self) -> BoxFuture<'_, Result<usize, ModelError>> {
        Box::pin(async { Ok(0) })
    }
    fn rate_limits(&self) -> Option<(u64, u64)> {
        None
    }
    fn native_deferred_tools(&self) -> bool {
        false
    }
    fn deferred_catalog_override(&self) -> Option<Vec<excalibur_types::runtime::ToolDefinition>> {
        None
    }
    fn cache_settings(&self) -> CacheSettings {
        CacheSettings::default()
    }
    /// True only for adapters supplying actual pricing in Usage.cost_microusd.
    fn cost_is_known(&self) -> bool {
        false
    }
    fn identity(&self) -> String {
        std::any::type_name::<Self>().into()
    }
    fn metadata(&self) -> GatewayMetadata {
        GatewayMetadata {
            configuration_hash: metrics::hash(self.identity().as_bytes()),
            ..Default::default()
        }
    }
    fn request_time_override(&self, _step: u64) -> Option<String> {
        None
    }
    /// Recorded host context policy as neutral JSON; the engine validates it.
    fn context_policy_override(&self) -> Option<Value> {
        None
    }
    /// Exact cache metadata for provider-free durable replay, when recorded.
    fn request_cache_override(&self, _step: u64) -> Option<excalibur_types::runtime::PromptCache> {
        None
    }
    fn call_bound(&self, _request: &ModelRequest) -> Option<ModelCallBound> {
        None
    }
    fn complete_observed<'a>(
        &'a self,
        request: ModelRequest,
    ) -> BoxFuture<'a, Result<ObservedResponse, ModelCallError>> {
        Box::pin(async move {
            let started = std::time::Instant::now();
            let mut metrics = Box::new(ModelCallMetrics::request(&request, self.metadata()));
            match self.complete(request).await {
                Ok(response) => {
                    metrics.duration_ms =
                        started.elapsed().as_millis().try_into().unwrap_or(u64::MAX);
                    metrics.usage_known = !response.usage.estimated;
                    metrics.usage = UsageBreakdown::from_usage(&response.usage);
                    metrics.cost_microusd =
                        self.cost_is_known().then_some(response.usage.cost_microusd);
                    Ok(ObservedResponse {
                        response,
                        metrics: *metrics,
                    })
                }
                Err(error) => {
                    metrics.duration_ms = elapsed_ms(started);
                    metrics.usage.estimated = true;
                    Err(ModelCallError { error, metrics })
                }
            }
        })
    }
    /// Internal cache-warming stream for the first real sibling. UI buffering
    /// policy remains the responsibility of the ordinary streaming methods.
    fn complete_cache_warm_observed<'a>(
        &'a self,
        request: ModelRequest,
        sink: StreamSink,
    ) -> BoxFuture<'a, Result<ObservedResponse, ModelCallError>> {
        self.complete_stream_observed(request, sink)
    }
    fn complete_stream_observed<'a>(
        &'a self,
        request: ModelRequest,
        sink: StreamSink,
    ) -> BoxFuture<'a, Result<ObservedResponse, ModelCallError>> {
        Box::pin(async move {
            let started = std::time::Instant::now();
            let mut metrics = Box::new(ModelCallMetrics::request(&request, self.metadata()));
            match self.complete_stream(request, sink).await {
                Ok(response) => {
                    metrics.duration_ms =
                        started.elapsed().as_millis().try_into().unwrap_or(u64::MAX);
                    metrics.usage_known = !response.usage.estimated;
                    metrics.usage = UsageBreakdown::from_usage(&response.usage);
                    metrics.cost_microusd =
                        self.cost_is_known().then_some(response.usage.cost_microusd);
                    Ok(ObservedResponse {
                        response,
                        metrics: *metrics,
                    })
                }
                Err(error) => {
                    metrics.duration_ms = elapsed_ms(started);
                    metrics.usage.estimated = true;
                    Err(ModelCallError { error, metrics })
                }
            }
        })
    }
    fn complete<'a>(
        &'a self,
        request: ModelRequest,
    ) -> BoxFuture<'a, Result<ModelResponse, ModelError>>;
    fn complete_stream<'a>(
        &'a self,
        request: ModelRequest,
        sink: StreamSink,
    ) -> BoxFuture<'a, Result<ModelResponse, ModelError>> {
        Box::pin(async move {
            let response = self.complete(request).await?;
            emit(
                &sink,
                StreamDelta::Complete {
                    attempt: 0,
                    response: response.clone(),
                },
            )?;
            Ok(response)
        })
    }
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum ModelRole {
    Master,
    Worker,
    Expert,
    Extractor,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct GatewayConfig {
    pub model: String,
    pub role: ModelRole,
    pub reasoning_effort: Option<String>,
    /// Requested sampling seed; backend determinism is not guaranteed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seed: Option<u64>,
    /// Requested sampling temperature; the selected model must support it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    pub native_tools: bool,
    /// Override only after the host verifies this model's native mid-conversation
    /// system role. None uses the adapter's conservative documented model list.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub native_system_updates: Option<bool>,
    /// Host override for verified native deferred-tool search support.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub native_deferred_tools: Option<bool>,
    pub max_output_tokens: u32,
}
impl GatewayConfig {
    pub fn new(model: impl Into<String>, role: ModelRole) -> Self {
        Self {
            model: model.into(),
            role,
            reasoning_effort: None,
            seed: None,
            temperature: None,
            native_tools: true,
            native_system_updates: None,
            native_deferred_tools: None,
            max_output_tokens: 4096,
        }
    }
}
/// Backward-compatible name for the shared native/compatible provider gateway.
pub type ProviderGateway = OpenAiGateway;
pub struct OpenAiGateway {
    client: ProviderClient,
    config: GatewayConfig,
    pricing: Option<Arc<PriceTable>>,
}
impl OpenAiGateway {
    pub fn new(client: ProviderClient, config: GatewayConfig) -> Result<Self, ModelError> {
        if config.model.trim().is_empty() || config.max_output_tokens == 0 {
            return Err(ModelError::Configuration(
                "model and positive output budget required".into(),
            ));
        }
        if config.seed.is_some_and(|seed| seed > i64::MAX as u64) {
            return Err(ModelError::Configuration(
                "sampling seed must fit a nonnegative signed 64-bit integer".into(),
            ));
        }
        if config.temperature.is_some_and(|temperature| {
            !temperature.is_finite() || !(0.0..=2.0).contains(&temperature)
        }) {
            return Err(ModelError::Configuration(
                "sampling temperature must be finite and between 0 and 2".into(),
            ));
        }
        Ok(Self {
            client,
            config,
            pricing: None,
        })
    }
    /// The trusted host supplies the versioned tariff; no prices are downloaded.
    pub fn with_price_table(mut self, table: PriceTable) -> Result<Self, ModelError> {
        table.validate()?;
        table.price(
            &format!("{:?}", self.client.provider()),
            &self.config.model,
            &UsageBreakdown::zero(),
        )?;
        self.pricing = Some(Arc::new(table));
        Ok(self)
    }
    fn wire_request(&self, request: &ModelRequest) -> Result<Value, ModelError> {
        cache::validate_cache(request)?;
        if request
            .reasoning_effort
            .as_ref()
            .is_some_and(|effort| Some(effort) != self.config.reasoning_effort.as_ref())
        {
            return Err(ModelError::Configuration(
                "reasoning effort is fixed by role".into(),
            ));
        }
        if let Some(schema) = &request.output_schema {
            jsonschema::validator_for(schema).map_err(|_| {
                ModelError::Configuration("invalid or externally-referencing output schema".into())
            })?;
        }
        if !request.deferred_tools.is_empty() && !self.native_deferred_tools() {
            return Err(ModelError::Configuration(
                "native deferred tools are unavailable for this provider adapter".into(),
            ));
        }
        let mut definitions = request.tools.clone();
        definitions.sort_by(|a, b| a.name.cmp(&b.name));
        if definitions.iter().any(|t| t.name.is_empty())
            || definitions.windows(2).any(|v| v[0].name == v[1].name)
        {
            return Err(ModelError::Configuration(
                "duplicate or empty tool name".into(),
            ));
        }
        let mut all_definitions = definitions
            .iter()
            .chain(&request.deferred_tools)
            .collect::<Vec<_>>();
        all_definitions.sort_by(|a, b| a.name.cmp(&b.name));
        if all_definitions.iter().any(|d| {
            d.name.is_empty()
                || (!request.deferred_tools.is_empty() && d.name == "tool_search_tool_bm25")
        }) || all_definitions.windows(2).any(|v| v[0].name == v[1].name)
        {
            return Err(ModelError::Configuration(
                "duplicate, empty or reserved deferred tool name".into(),
            ));
        }
        for tool in all_definitions {
            jsonschema::validator_for(&tool.input_schema).map_err(|_| {
                ModelError::Configuration(format!("invalid schema for tool {}", tool.name))
            })?;
        }
        let mut messages = Vec::new();
        for message in &request.messages {
            if !matches!(
                message.role.as_str(),
                "system" | "developer" | "user" | "assistant" | "tool"
            ) {
                return Err(ModelError::Configuration("unsupported message role".into()));
            }
            if self.config.native_tools {
                let mut wire = json!({"role":message.role,"content":message.content});
                if !message.tool_calls.is_empty() {
                    wire["tool_calls"]=json!(message.tool_calls.iter().map(|call|json!({"id":call.id,"type":"function","function":{"name":call.name,"arguments":call.arguments}})).collect::<Vec<_>>());
                }
                if let Some(id) = &message.tool_call_id {
                    wire["tool_call_id"] = json!(id);
                }
                messages.push(wire);
            } else {
                // Tool-less models receive the exact durable call/result records in a textual envelope.
                let mut text = message.content.clone();
                if !message.tool_calls.is_empty() || message.tool_call_id.is_some() {
                    let mut record = message.clone();
                    record.provider_state = None;
                    text = serde_json::to_string(&record)
                        .map_err(|_| ModelError::Configuration("history serialization".into()))?;
                }
                let role = if message.role == "tool" {
                    "user"
                } else {
                    message.role.as_str()
                };
                messages.push(json!({"role":role,"content":text}));
            }
        }
        let mut body =
            json!({"messages":messages,"max_completion_tokens":self.config.max_output_tokens});
        if let Some(effort) = &self.config.reasoning_effort {
            body["reasoning_effort"] = json!(effort);
        }
        if let Some(seed) = self.config.seed {
            body["seed"] = json!(seed);
        }
        if let Some(temperature) = self.config.temperature {
            body["temperature"] = json!(temperature);
        }
        if self.config.native_tools {
            if !definitions.is_empty() {
                body["tools"]=json!(definitions.iter().map(|t|json!({"type":"function","function":{"name":t.name,"description":t.description,"parameters":t.input_schema}})).collect::<Vec<_>>());
                body["tool_choice"] = json!(if request.tools_enabled {
                    "auto"
                } else {
                    "none"
                });
            }
            if let Some(schema) = &request.output_schema {
                body["response_format"] = json!({"type":"json_schema","json_schema":{"name":"excalibur_output","strict":true,"schema":schema}});
            }
        } else {
            let instruction = json!({"protocol":"Return only a JSON object with exactly content (string) and tool_calls (array). Each tool call has id, name and arguments (JSON object). Use [] when no tool is needed. Tools may only be called when enabled. Tool results are replayed as user JSON records.","tools":definitions,"content_schema":request.output_schema});
            body["messages"].as_array_mut().unwrap().insert(
                0,
                json!({"role":"system","content":instruction.to_string()}),
            );
            let enabled = format!(
                "Tool calls enabled for this response: {}",
                request.tools_enabled
            );
            let messages = body["messages"].as_array_mut().unwrap();
            if let Some(last) = messages.last_mut()
                && let Some(text) = last["content"].as_str()
                && text.starts_with("[L5]\n")
            {
                last["content"] = json!(format!("{text}\n{enabled}"));
            } else {
                messages.push(json!({"role":"system","content":enabled}));
            }
        }
        if self.client.provider() == excalibur_providers::Provider::OpenAi
            && let Some(cache) = &request.cache
        {
            body["prompt_cache_key"] = json!(cache.namespace_key);
        }
        native::request(self.client.provider(), &self.config, request, body)
    }
    async fn run(
        &self,
        request: ModelRequest,
        sink: Option<StreamSink>,
    ) -> Result<ModelResponse, ModelError> {
        self.run_observed(request, sink)
            .await
            .map(|observed| observed.response)
            .map_err(|failure| failure.error)
    }
    async fn run_observed(
        &self,
        request: ModelRequest,
        sink: Option<StreamSink>,
    ) -> Result<ObservedResponse, ModelCallError> {
        let started = std::time::Instant::now();
        let mut metrics = Box::new(ModelCallMetrics::request(&request, self.metadata()));
        metrics.provider_calls_known = true;
        metrics.usage_known = true;
        let mut body = match self.wire_request(&request) {
            Ok(body) => body,
            Err(error) => return Err(ModelCallError { error, metrics }),
        };
        let mut total = Usage::default();
        metrics.usage = UsageBreakdown::zero();
        metrics.cost_microusd = self.pricing.as_ref().map(|_| 0);
        for attempt in 0..=1_u8 {
            let attempt_started = std::time::Instant::now();
            let mut counted_wire = body.clone();
            counted_wire["model"] = json!(self.config.model);
            counted_wire["stream"] = json!(sink.is_some());
            if sink.is_some()
                && !matches!(
                    self.client.provider(),
                    excalibur_providers::Provider::Anthropic
                        | excalibur_providers::Provider::GoogleAi
                        | excalibur_providers::Provider::Vertex
                )
            {
                counted_wire["stream_options"] = json!({"include_usage":true});
            }
            let input_bytes = counted_wire.to_string().len() as u64;
            metrics.provider_calls += 1;
            let raw = if let Some(sink) = &sink {
                self.stream_once(body.clone(), sink, attempt).await
            } else {
                self.client
                    .native_chat(&self.config.model, body.clone())
                    .await
                    .map_err(provider_error)
                    .and_then(|raw| native::response(self.client.provider(), raw))
            };
            let raw = match raw {
                Ok(raw) => raw,
                Err(error) => {
                    metrics.duration_ms = elapsed_ms(started);
                    metrics.cost_microusd = None;
                    metrics.usage_known = false;
                    let unreported = UsageBreakdown {
                        input_tokens: input_bytes.saturating_add(100),
                        estimated: true,
                        ..Default::default()
                    };
                    metrics.usage.add(&unreported);
                    metrics.attempts.push(ModelAttemptMetrics {
                        index: attempt,
                        duration_ms: elapsed_ms(attempt_started),
                        usage: unreported,
                        cost_microusd: None,
                        error: Some(error.classification()),
                    });
                    return Err(ModelCallError { error, metrics });
                }
            };
            let used = usage(&raw, input_bytes);
            let detail = usage_breakdown(&raw, &used);
            if let Err(error) = detail.validate() {
                let reported = UsageBreakdown::from_usage(&used);
                metrics.usage.add(&reported);
                metrics.cost_microusd = None;
                metrics.usage_known &= !used.estimated;
                metrics.duration_ms = elapsed_ms(started);
                metrics.attempts.push(ModelAttemptMetrics {
                    index: attempt,
                    duration_ms: elapsed_ms(attempt_started),
                    usage: reported,
                    cost_microusd: None,
                    error: Some(error.classification()),
                });
                return Err(ModelCallError { error, metrics });
            }
            metrics.usage_known &= !used.estimated;
            let price = match &self.pricing {
                Some(table) => match table.price(
                    &format!("{:?}", self.client.provider()),
                    &self.config.model,
                    &detail,
                ) {
                    Ok(cost) => cost,
                    Err(error) => {
                        metrics.duration_ms = elapsed_ms(started);
                        return Err(ModelCallError { error, metrics });
                    }
                },
                None => None,
            };
            total.input_tokens = total.input_tokens.saturating_add(used.input_tokens);
            total.output_tokens = total.output_tokens.saturating_add(used.output_tokens);
            total.estimated |= used.estimated;
            total.cost_microusd = total.cost_microusd.saturating_add(price.unwrap_or(0));
            metrics.usage.add(&detail);
            metrics.cost_microusd = metrics
                .cost_microusd
                .zip(price)
                .map(|(a, b)| a.saturating_add(b));
            let result = self.normalize(&raw, &request);
            metrics.attempts.push(ModelAttemptMetrics {
                index: attempt,
                duration_ms: elapsed_ms(attempt_started),
                usage: detail,
                cost_microusd: price,
                error: result.as_ref().err().map(ModelError::classification),
            });
            metrics.duration_ms = elapsed_ms(started);
            match result {
                Ok(mut response) => {
                    response.usage = total;
                    if self.pricing.is_some() && metrics.cost_microusd.is_none() {
                        return Err(ModelCallError {
                            error: ModelError::Accounting(
                                "provider usage is insufficient for the approved tariff".into(),
                            ),
                            metrics,
                        });
                    }
                    if let Some(sink) = &sink
                        && let Err(error) = emit(
                            sink,
                            StreamDelta::Complete {
                                attempt,
                                response: response.clone(),
                            },
                        )
                    {
                        return Err(ModelCallError { error, metrics });
                    }
                    return Ok(ObservedResponse {
                        response,
                        metrics: *metrics,
                    });
                }
                Err(ModelError::Validation(reason)) if attempt == 0 => {
                    if let Some(sink) = &sink
                        && let Err(error) = emit(
                            sink,
                            StreamDelta::Retry {
                                attempt: 1,
                                reason: reason.clone(),
                            },
                        )
                    {
                        return Err(ModelCallError { error, metrics });
                    }
                    native::retry(
                        self.client.provider(),
                        &mut body,
                        &raw["choices"][0]["message"],
                        &reason,
                    );
                }
                Err(error) => return Err(ModelCallError { error, metrics }),
            }
        }
        Err(ModelCallError {
            error: ModelError::Validation("retry exhausted".into()),
            metrics,
        })
    }
    fn normalize(&self, raw: &Value, request: &ModelRequest) -> Result<ModelResponse, ModelError> {
        native::validate_deferred_response(raw, request)?;
        let reason = raw
            .pointer("/choices/0/finish_reason")
            .and_then(Value::as_str);
        if !matches!(reason, Some("stop" | "tool_calls")) {
            return Err(ModelError::Truncated);
        }
        let message = raw
            .pointer("/choices/0/message")
            .ok_or(ModelError::Provider)?;
        if message.get("refusal").is_some_and(|v| !v.is_null()) {
            return Err(ModelError::Validation("provider refused the output".into()));
        }
        let mut normalized = if self.config.native_tools {
            let content = message
                .get("content")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let mut calls = Vec::new();
            if let Some(native) = message.get("tool_calls") {
                for call in native
                    .as_array()
                    .ok_or_else(|| validation("tool_calls must be an array"))?
                {
                    if call.get("type").and_then(Value::as_str) != Some("function") {
                        return Err(validation("unsupported native tool-call type"));
                    }
                    calls.push(ToolCall {
                        id: required_string(call, "/id")?,
                        name: required_string(call, "/function/name")?,
                        arguments: required_string(call, "/function/arguments")?,
                    });
                }
            }
            Message {
                role: "assistant".into(),
                content,
                tool_calls: calls,
                tool_call_id: None,
                provider_state: message.get("provider_state").cloned(),
            }
        } else {
            parse_text_protocol(
                message
                    .get("content")
                    .and_then(Value::as_str)
                    .ok_or(ModelError::Provider)?,
            )?
        };
        if normalized.provider_state.is_none() {
            normalized.provider_state = message.get("provider_state").cloned();
        }
        validate_calls(&mut normalized, request)?;
        if normalized.tool_calls.is_empty()
            && let Some(schema) = &request.output_schema
        {
            let value: Value = parse_json_minor(&normalized.content)?;
            validate_schema(schema, &value)?;
            normalized.content = serde_json::to_string(&value).unwrap();
        }
        Ok(ModelResponse {
            message: normalized,
            usage: Usage::default(),
        })
    }
    async fn stream_once(
        &self,
        mut body: Value,
        sink: &StreamSink,
        attempt: u8,
    ) -> Result<Value, ModelError> {
        if matches!(
            self.client.provider(),
            excalibur_providers::Provider::Anthropic
                | excalibur_providers::Provider::GoogleAi
                | excalibur_providers::Provider::Vertex
        ) {
            let stream = self
                .client
                .stream_native_chat(&self.config.model, body)
                .await
                .map_err(provider_error)?;
            return native::stream(self.client.provider(), stream, sink, attempt).await;
        }
        body["stream_options"] = json!({"include_usage":true});
        let mut stream = self
            .client
            .stream_native_chat(&self.config.model, body)
            .await
            .map_err(provider_error)?;
        let mut content = String::new();
        let mut calls: BTreeMap<usize, ToolCall> = BTreeMap::new();
        let mut finish = None;
        let mut used = Value::Null;
        while let Some(event) = stream
            .next_event()
            .await
            .map_err(|_| ModelError::Truncated)?
        {
            let data = event.data;
            if let Some(value) = data.get("usage").filter(|v| !v.is_null()) {
                used = value.clone();
            }
            let Some(choice) = data.pointer("/choices/0") else {
                continue;
            };
            if let Some(reason) = choice.get("finish_reason").and_then(Value::as_str) {
                finish = Some(reason.to_string());
            }
            if let Some(text) = choice.pointer("/delta/content").and_then(Value::as_str) {
                content.push_str(text);
                emit(
                    sink,
                    StreamDelta::Text {
                        attempt,
                        delta: text.into(),
                    },
                )?;
            }
            if let Some(deltas) = choice
                .pointer("/delta/tool_calls")
                .and_then(Value::as_array)
            {
                for delta in deltas {
                    let index = delta
                        .get("index")
                        .and_then(Value::as_u64)
                        .filter(|v| *v < 128)
                        .ok_or_else(|| validation("invalid stream tool index"))?
                        as usize;
                    let id = delta.get("id").and_then(Value::as_str).unwrap_or("");
                    let name = delta
                        .pointer("/function/name")
                        .and_then(Value::as_str)
                        .unwrap_or("");
                    let arguments = delta
                        .pointer("/function/arguments")
                        .and_then(Value::as_str)
                        .unwrap_or("");
                    let call = calls.entry(index).or_insert_with(|| ToolCall {
                        id: String::new(),
                        name: String::new(),
                        arguments: String::new(),
                    });
                    call.id.push_str(id);
                    call.name.push_str(name);
                    call.arguments.push_str(arguments);
                    emit(
                        sink,
                        StreamDelta::Tool {
                            attempt,
                            index,
                            id: id.into(),
                            name: name.into(),
                            arguments: arguments.into(),
                        },
                    )?;
                }
            }
            if content.len()
                + calls
                    .values()
                    .map(|c| c.arguments.len() + c.id.len() + c.name.len())
                    .sum::<usize>()
                > 8 * 1024 * 1024
            {
                return Err(ModelError::Truncated);
            }
        }
        if !matches!(finish.as_deref(), Some("stop" | "tool_calls")) {
            return Err(ModelError::Truncated);
        }
        let native=calls.values().map(|c|json!({"id":c.id,"type":"function","function":{"name":c.name,"arguments":c.arguments}})).collect::<Vec<_>>();
        Ok(
            json!({"choices":[{"finish_reason":finish,"message":{"role":"assistant","content":content,"tool_calls":native}}],"usage":used}),
        )
    }
}
impl ModelGateway for OpenAiGateway {
    fn rate_limits(&self) -> Option<(u64, u64)> {
        self.client.rate_limits()
    }
    fn native_deferred_tools(&self) -> bool {
        self.config.native_tools
            && self.client.provider() == excalibur_providers::Provider::Anthropic
            && self
                .config
                .native_deferred_tools
                .unwrap_or_else(|| native::anthropic_search_model(&self.config.model))
    }
    fn cache_settings(&self) -> CacheSettings {
        CacheSettings::provider(self.client.provider())
    }
    fn cost_is_known(&self) -> bool {
        self.pricing.is_some()
    }
    fn metadata(&self) -> GatewayMetadata {
        GatewayMetadata {
            provider: Some(format!("{:?}", self.client.provider())),
            model: Some(self.config.model.clone()),
            role: Some(self.config.role),
            configuration_hash: metrics::hash(self.identity().as_bytes()),
            price_table_version: self.pricing.as_ref().map(|table| table.version.clone()),
            price_table_hash: self
                .pricing
                .as_ref()
                .and_then(|table| table.content_hash().ok()),
        }
    }
    fn call_bound(&self, request: &ModelRequest) -> Option<ModelCallBound> {
        let wire = self.wire_request(request).ok()?;
        let input = (wire.to_string().len() as u64)
            .saturating_add(self.config.model.len() as u64)
            .saturating_add(1024);
        let output = u64::from(self.config.max_output_tokens);
        Some(ModelCallBound {
            max_tokens: input.saturating_add(output).saturating_mul(2),
            // A token-only bound is useful before an unpriced call. This zero
            // is not an invoice value: cost_is_known remains false and observed
            // cost remains None; cost budgets cannot use an unpriced gateway.
            max_cost_microusd: self
                .pricing
                .as_ref()
                .map(|table| {
                    table
                        .upper_bound(
                            &format!("{:?}", self.client.provider()),
                            &self.config.model,
                            input.saturating_mul(3).saturating_add(output),
                            output.saturating_mul(2),
                        )
                        .unwrap_or(u64::MAX)
                })
                .unwrap_or(0),
        })
    }
    fn complete_observed<'a>(
        &'a self,
        request: ModelRequest,
    ) -> BoxFuture<'a, Result<ObservedResponse, ModelCallError>> {
        Box::pin(self.run_observed(request, None))
    }
    fn complete_cache_warm_observed<'a>(
        &'a self,
        request: ModelRequest,
        sink: StreamSink,
    ) -> BoxFuture<'a, Result<ObservedResponse, ModelCallError>> {
        if self.cache_settings().first_delta_warming {
            Box::pin(self.run_observed(request, Some(sink)))
        } else {
            self.complete_stream_observed(request, sink)
        }
    }
    fn complete_stream_observed<'a>(
        &'a self,
        request: ModelRequest,
        sink: StreamSink,
    ) -> BoxFuture<'a, Result<ObservedResponse, ModelCallError>> {
        if self.config.role == ModelRole::Master {
            Box::pin(self.run_observed(request, Some(sink)))
        } else {
            Box::pin(async move {
                let observed = self.run_observed(request, None).await?;
                emit(
                    &sink,
                    StreamDelta::Complete {
                        attempt: 0,
                        response: observed.response.clone(),
                    },
                )
                .map_err(|error| ModelCallError {
                    error,
                    metrics: Box::new(observed.metrics.clone()),
                })?;
                Ok(observed)
            })
        }
    }

    fn identity(&self) -> String {
        let config = serde_json::to_vec(&self.config).expect("serializable gateway configuration");
        let mut hash = Sha256::new();
        hash.update(self.client.configuration_identity());
        hash.update(config);
        if let Some(table) = &self.pricing {
            hash.update(table.content_hash().expect("validated price table"));
        }
        format!("openai-compatible:{:x}", hash.finalize())
    }
    fn complete<'a>(
        &'a self,
        request: ModelRequest,
    ) -> BoxFuture<'a, Result<ModelResponse, ModelError>> {
        Box::pin(self.run(request, None))
    }
    fn complete_stream<'a>(
        &'a self,
        request: ModelRequest,
        sink: StreamSink,
    ) -> BoxFuture<'a, Result<ModelResponse, ModelError>> {
        if self.config.role == ModelRole::Master {
            Box::pin(self.run(request, Some(sink)))
        } else {
            Box::pin(async move {
                let response = self.run(request, None).await?;
                emit(
                    &sink,
                    StreamDelta::Complete {
                        attempt: 0,
                        response: response.clone(),
                    },
                )?;
                Ok(response)
            })
        }
    }
}
fn validation(reason: &str) -> ModelError {
    ModelError::Validation(reason.into())
}
fn required_string(value: &Value, path: &str) -> Result<String, ModelError> {
    value
        .pointer(path)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(String::from)
        .ok_or_else(|| validation(&format!("missing string at {path}")))
}
fn emit(sink: &StreamSink, delta: StreamDelta) -> Result<(), ModelError> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| sink(delta)))
        .map_err(|_| ModelError::Consumer)
}
/// UTF-8 byte counts plus input framing overhead are deliberately conservative
/// heuristics when usage is omitted. They are not a universal tokenizer guarantee;
/// actual provider counts take precedence whenever both fields are present.
fn usage(raw: &Value, input_bytes: u64) -> Usage {
    let input = raw.pointer("/usage/prompt_tokens").and_then(Value::as_u64);
    let output = raw
        .pointer("/usage/completion_tokens")
        .and_then(Value::as_u64);
    let estimated = input.is_none() || output.is_none();
    Usage {
        input_tokens: if estimated {
            input_bytes.saturating_add(100).max(input.unwrap_or(0))
        } else {
            input.unwrap()
        },
        output_tokens: if estimated {
            (raw.to_string().len() as u64)
                .max(1)
                .max(output.unwrap_or(0))
        } else {
            output.unwrap()
        },
        cost_microusd: 0,
        estimated,
    }
}
fn validate_schema(schema: &Value, value: &Value) -> Result<(), ModelError> {
    let validator = jsonschema::validator_for(schema)
        .map_err(|_| ModelError::Configuration("invalid schema".into()))?;
    if let Some(error) = validator.iter_errors(value).next() {
        return Err(validation(&format!(
            "schema constraint {} failed at {}",
            error.schema_path(),
            error.instance_path()
        )));
    }
    Ok(())
}
fn validate_calls(message: &mut Message, request: &ModelRequest) -> Result<(), ModelError> {
    if !request.tools_enabled && !message.tool_calls.is_empty() {
        return Err(validation("tool calls are disabled for this turn"));
    }
    let mut ids = BTreeSet::new();
    for call in &mut message.tool_calls {
        if call.id.is_empty() || !ids.insert(call.id.clone()) {
            return Err(validation("tool call IDs must be nonempty and unique"));
        }
        let tool = request
            .tools
            .iter()
            .chain(&request.deferred_tools)
            .find(|t| t.name == call.name)
            .ok_or_else(|| validation("unknown tool name"))?;
        let arguments = parse_json_minor(&call.arguments)?;
        if !arguments.is_object() {
            return Err(validation("tool arguments must be an object"));
        }
        validate_schema(&tool.input_schema, &arguments)?;
        call.arguments = arguments.to_string();
    }
    Ok(())
}
fn parse_text_protocol(text: &str) -> Result<Message, ModelError> {
    let envelope = parse_json_minor(text)?;
    let object = envelope
        .as_object()
        .ok_or_else(|| validation("text protocol must be an object"))?;
    if object.keys().any(|k| k != "content" && k != "tool_calls") {
        return Err(validation("unexpected text protocol field"));
    }
    let content = required_string_allow_empty(&envelope, "content")?;
    let calls = envelope
        .get("tool_calls")
        .and_then(Value::as_array)
        .ok_or_else(|| validation("text protocol requires tool_calls array"))?;
    let mut normalized = Vec::new();
    for call in calls {
        let o = call
            .as_object()
            .ok_or_else(|| validation("invalid textual tool call"))?;
        if o.len() != 3 || !o.contains_key("arguments") {
            return Err(validation("textual tool call requires id/name/arguments"));
        }
        normalized.push(ToolCall {
            id: required_string(call, "/id")?,
            name: required_string(call, "/name")?,
            arguments: call["arguments"].to_string(),
        });
    }
    Ok(Message {
        role: "assistant".into(),
        content,
        tool_calls: normalized,
        tool_call_id: None,
        provider_state: None,
    })
}
fn required_string_allow_empty(v: &Value, key: &str) -> Result<String, ModelError> {
    v.get(key)
        .and_then(Value::as_str)
        .map(String::from)
        .ok_or_else(|| validation("text protocol requires content string"))
}
/// Only trim one complete JSON fence and trailing commas outside strings. Never extract
/// a guessed object from prose, repair keys/values or turn invalid arguments into defaults.
fn parse_json_minor(text: &str) -> Result<Value, ModelError> {
    let mut text = text.trim();
    if let Some(inner) = text
        .strip_prefix("```json\n")
        .or_else(|| text.strip_prefix("```\n"))
    {
        text = inner
            .strip_suffix("```")
            .ok_or_else(|| validation("unclosed JSON fence"))?
            .trim();
    }
    if let Ok(value) = serde_json::from_str(text) {
        return Ok(value);
    }
    let chars = text.chars().collect::<Vec<_>>();
    let (mut quote, mut escaped) = (false, false);
    let mut repaired = String::new();
    for (index, c) in chars.iter().copied().enumerate() {
        if quote {
            repaired.push(c);
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                quote = false;
            }
            continue;
        }
        if c == '"' {
            quote = true;
        }
        if c == ','
            && chars[index + 1..]
                .iter()
                .find(|c| !c.is_whitespace())
                .is_some_and(|c| *c == '}' || *c == ']')
        {
            continue;
        }
        repaired.push(c);
    }
    serde_json::from_str(&repaired)
        .map_err(|_| validation("invalid JSON after bounded minor repair"))
}

fn elapsed_ms(start: std::time::Instant) -> u64 {
    start.elapsed().as_millis().try_into().unwrap_or(u64::MAX)
}
fn provider_error(error: excalibur_providers::Error) -> ModelError {
    use excalibur_providers::{Error, KnownFailure};
    ModelError::ProviderFailure(match error {
        Error::Http(status) => ModelFailure::http(status.as_u16()),
        Error::ClassifiedHttp { status, kind } => ModelFailure {
            class: match kind {
                KnownFailure::Quota => FailureClass::Quota,
                KnownFailure::ContextWindow => FailureClass::ContextWindowExceeded,
                KnownFailure::Overloaded => FailureClass::Overloaded,
            },
            http_status: Some(status.as_u16()),
        },
        Error::Transport => ModelFailure {
            class: FailureClass::Transport,
            http_status: None,
        },
        Error::InvalidResponse => ModelFailure {
            class: FailureClass::InvalidResponse,
            http_status: None,
        },
        Error::Configuration(_) | Error::Unsupported => ModelFailure {
            class: FailureClass::InvalidRequest,
            http_status: None,
        },
    })
}
impl ModelError {
    pub fn classification(&self) -> ModelFailure {
        ModelFailure {
            class: match self {
                Self::ProviderFailure(error) => return error.clone(),
                Self::Provider => FailureClass::InvalidResponse,
                Self::Configuration(_) => FailureClass::InvalidRequest,
                Self::Validation(_) => FailureClass::Validation,
                Self::Truncated => FailureClass::Truncated,
                Self::Consumer => FailureClass::Consumer,
                Self::Accounting(_) => FailureClass::Accounting,
                Self::ReplayDivergence(_) => FailureClass::ReplayDivergence,
                Self::ReplayBudget => FailureClass::Budget,
            },
            http_status: None,
        }
    }
}
fn usage_breakdown(raw: &Value, used: &Usage) -> UsageBreakdown {
    let count = |path| raw.pointer(path).and_then(Value::as_u64);
    UsageBreakdown {
        input_tokens: used.input_tokens,
        output_tokens: used.output_tokens,
        estimated: used.estimated,
        cache_read_tokens: count("/usage/prompt_tokens_details/cached_tokens")
            .or_else(|| count("/usage/prompt_cache_hit_tokens"))
            .or_else(|| count("/usage/cached_tokens")),
        cache_write_tokens: count("/usage/prompt_tokens_details/cache_write_tokens"),
        reasoning_tokens: count("/usage/completion_tokens_details/reasoning_tokens"),
        cache_write_by_ttl: [
            ("5m", "/usage/cache_creation/ephemeral_5m_input_tokens"),
            ("1h", "/usage/cache_creation/ephemeral_1h_input_tokens"),
        ]
        .into_iter()
        .filter_map(|(ttl, path)| count(path).map(|tokens| (ttl.into(), tokens)))
        .collect(),
    }
}
