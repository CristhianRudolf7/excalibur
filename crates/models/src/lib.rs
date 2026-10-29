//! Provider-neutral model boundary over the existing outbound provider transport.
//! No model selection, credentials discovery, paid inference or agent loop occurs here.
use excalibur_providers::ProviderClient;
use excalibur_types::runtime::{BoxFuture, Message, ModelRequest, ModelResponse, ToolCall, Usage};
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
    /// True only for adapters supplying actual pricing in Usage.cost_microusd.
    fn cost_is_known(&self) -> bool {
        false
    }
    fn identity(&self) -> String {
        std::any::type_name::<Self>().into()
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
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct GatewayConfig {
    pub model: String,
    pub role: ModelRole,
    pub reasoning_effort: Option<String>,
    pub native_tools: bool,
    pub max_output_tokens: u32,
}
impl GatewayConfig {
    pub fn new(model: impl Into<String>, role: ModelRole) -> Self {
        Self {
            model: model.into(),
            role,
            reasoning_effort: None,
            native_tools: true,
            max_output_tokens: 4096,
        }
    }
}
pub struct OpenAiGateway {
    client: ProviderClient,
    config: GatewayConfig,
}
impl OpenAiGateway {
    pub fn new(client: ProviderClient, config: GatewayConfig) -> Result<Self, ModelError> {
        if config.model.trim().is_empty() || config.max_output_tokens == 0 {
            return Err(ModelError::Configuration(
                "model and positive output budget required".into(),
            ));
        }
        Ok(Self { client, config })
    }
    fn wire_request(&self, request: &ModelRequest) -> Result<Value, ModelError> {
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
        let mut definitions = request.tools.clone();
        definitions.sort_by(|a, b| a.name.cmp(&b.name));
        if definitions.iter().any(|t| t.name.is_empty())
            || definitions.windows(2).any(|v| v[0].name == v[1].name)
        {
            return Err(ModelError::Configuration(
                "duplicate or empty tool name".into(),
            ));
        }
        for tool in &definitions {
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
                    text = serde_json::to_string(message)
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
            let instruction = json!({"protocol":"Return only a JSON object with exactly content (string) and tool_calls (array). Each tool call has id, name and arguments (JSON object). Use [] when no tool is needed. Tools may only be called when enabled. Tool results are replayed as user JSON records.","tools":definitions,"tools_enabled":request.tools_enabled,"content_schema":request.output_schema});
            body["messages"].as_array_mut().unwrap().insert(
                0,
                json!({"role":"system","content":instruction.to_string()}),
            );
        }
        Ok(body)
    }
    async fn run(
        &self,
        request: ModelRequest,
        sink: Option<StreamSink>,
    ) -> Result<ModelResponse, ModelError> {
        let mut body = self.wire_request(&request)?;
        let mut total = Usage::default();
        for attempt in 0..=1u8 {
            let mut counted_wire = body.clone();
            counted_wire["model"] = json!(self.config.model);
            counted_wire["stream"] = json!(sink.is_some());
            if sink.is_some() {
                counted_wire["stream_options"] = json!({"include_usage":true});
            }
            let input_bytes = counted_wire.to_string().len() as u64;
            let raw = if let Some(sink) = &sink {
                self.stream_once(body.clone(), sink, attempt).await?
            } else {
                self.client
                    .native_chat(&self.config.model, body.clone())
                    .await
                    .map_err(|_| ModelError::Provider)?
            };
            let usage = usage(&raw, input_bytes);
            total.input_tokens = total.input_tokens.saturating_add(usage.input_tokens);
            total.output_tokens = total.output_tokens.saturating_add(usage.output_tokens);
            total.estimated |= usage.estimated;
            let result = self.normalize(&raw, &request);
            match result {
                Ok(mut response) => {
                    response.usage = total;
                    if let Some(sink) = &sink {
                        emit(
                            sink,
                            StreamDelta::Complete {
                                attempt,
                                response: response.clone(),
                            },
                        )?;
                    }
                    return Ok(response);
                }
                Err(ModelError::Validation(reason)) if attempt == 0 => {
                    if let Some(sink) = &sink {
                        emit(
                            sink,
                            StreamDelta::Retry {
                                attempt: 1,
                                reason: reason.clone(),
                            },
                        )?;
                    }
                    let content = raw
                        .pointer("/choices/0/message/content")
                        .and_then(Value::as_str)
                        .unwrap_or("");
                    let messages = body["messages"].as_array_mut().unwrap();
                    messages.push(json!({"role":"assistant","content":content}));
                    messages.push(json!({"role":"system","content":format!("Output validation failed: {reason}. Return one corrected output conforming to the specified schemas and tool protocol.")}));
                }
                Err(error) => return Err(error),
            }
        }
        Err(ModelError::Validation("retry exhausted".into()))
    }
    fn normalize(&self, raw: &Value, request: &ModelRequest) -> Result<ModelResponse, ModelError> {
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
            }
        } else {
            parse_text_protocol(
                message
                    .get("content")
                    .and_then(Value::as_str)
                    .ok_or(ModelError::Provider)?,
            )?
        };
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
        body["stream_options"] = json!({"include_usage":true});
        let mut stream = self
            .client
            .stream_native_chat(&self.config.model, body)
            .await
            .map_err(|_| ModelError::Provider)?;
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
    fn identity(&self) -> String {
        let config = serde_json::to_vec(&self.config).expect("serializable gateway configuration");
        let mut hash = Sha256::new();
        hash.update(self.client.configuration_identity());
        hash.update(config);
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
