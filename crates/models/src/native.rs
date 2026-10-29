//! Native wire adaptation. Cache metadata never becomes prompt text.
use crate::{GatewayConfig, ModelError, StreamDelta, StreamSink, cache, emit, validation};
use excalibur_providers::{Provider, SseStream};
use excalibur_types::runtime::{Message, ModelRequest};
use serde_json::{Value, json};
use std::collections::BTreeMap;

pub(crate) fn request(
    provider: Provider,
    config: &GatewayConfig,
    request: &ModelRequest,
    compatible: Value,
) -> Result<Value, ModelError> {
    match provider {
        Provider::Anthropic => anthropic_request(config, request, compatible),
        Provider::GoogleAi | Provider::Vertex => gemini_request(config, request, compatible),
        _ => Ok(compatible),
    }
}
fn anthropic_request(
    config: &GatewayConfig,
    request: &ModelRequest,
    compatible: Value,
) -> Result<Value, ModelError> {
    if config.seed.is_some() {
        return Err(ModelError::Configuration(
            "Anthropic Messages does not support the requested sampling seed".into(),
        ));
    }
    let native_system = config
        .native_system_updates
        .unwrap_or_else(|| anthropic_system_model(&config.model));
    let mut system = Vec::new();
    let mut messages = Vec::new();
    let offset = usize::from(!config.native_tools);
    for (index, message) in compatible["messages"]
        .as_array()
        .unwrap()
        .iter()
        .enumerate()
    {
        let role = message["role"].as_str().unwrap();
        let mut blocks = Vec::new();
        let text = message["content"].as_str().unwrap_or("");
        if role == "tool" {
            blocks.push(
                json!({"type":"tool_result","tool_use_id":message["tool_call_id"],"content":text}),
            );
        } else {
            if !text.is_empty() {
                blocks.push(json!({"type":"text","text":text}));
            }
            if let Some(calls) = message["tool_calls"].as_array() {
                for call in calls {
                    let input: Value =
                        serde_json::from_str(call["function"]["arguments"].as_str().unwrap_or(""))
                            .map_err(|_| validation("invalid replayed tool JSON"))?;
                    blocks.push(json!({"type":"tool_use","id":call["id"],"name":call["function"]["name"],"input":input}));
                }
            }
        }
        if role == "assistant"
            && let Some(original) = index
                .checked_sub(offset)
                .and_then(|i| request.messages.get(i))
            && let Some(state) = &original.provider_state
        {
            if state["provider"] != "anthropic" {
                return Err(ModelError::Configuration(
                    "provider state belongs to a different native adapter".into(),
                ));
            }
            let saved = state["blocks"]
                .as_array()
                .ok_or_else(|| validation("invalid Anthropic replay blocks"))?;
            let normalized = response(
                Provider::Anthropic,
                json!({"content":saved,"stop_reason":"end_turn","usage":{}}),
            )?;
            let native = &normalized["choices"][0]["message"];
            let calls = native["tool_calls"].as_array().unwrap();
            if !replay_matches(
                original,
                native["content"].as_str().unwrap_or(""),
                calls,
                config.native_tools,
            ) {
                return Err(ModelError::Configuration(
                    "Anthropic replay state does not match its durable assistant message".into(),
                ));
            }
            blocks = saved.clone();
        }
        if index
            .checked_sub(offset)
            .is_some_and(|i| cache::marked(&request.cache, i))
        {
            let block = blocks
                .iter_mut()
                .rev()
                .find(|b| {
                    matches!(
                        b["type"].as_str(),
                        Some("text" | "tool_use" | "tool_result")
                    )
                })
                .ok_or_else(|| {
                    ModelError::Configuration("cannot mark an empty Anthropic content block".into())
                })?;
            block["cache_control"] = cache::control(request.cache.as_ref().unwrap());
        }
        if blocks.is_empty() {
            continue;
        }
        // Initial instructions remain stable system blocks. Later system updates
        // stay at their actual conversation position (Messages API native role).
        if matches!(role, "system" | "developer")
            && messages.is_empty()
            && stable_system_prefix(request, index, offset, text)
        {
            system.extend(blocks);
        } else {
            let late_system = matches!(role, "system" | "developer");
            let placement = messages
                .last()
                .and_then(|m: &Value| m["role"].as_str())
                .is_some_and(|r| matches!(r, "user" | "system"));
            let wire_role = if role == "tool" {
                "user"
            } else if late_system {
                if native_system && placement {
                    "system"
                } else {
                    "user"
                }
            } else {
                role
            };
            if late_system && wire_role == "user" {
                for block in &mut blocks {
                    if block["type"] == "text" {
                        block["text"] =
                            json!(runtime_frame(role, block["text"].as_str().unwrap_or("")));
                    }
                }
            }
            messages.push(json!({"role":wire_role,"content":blocks}));
        }
    }
    let mut body = json!({"max_tokens":config.max_output_tokens,"messages":messages});
    if !system.is_empty() {
        body["system"] = json!(system);
    }
    if let Some(temp) = config.temperature {
        body["temperature"] = json!(temp);
    }
    if let Some(effort) = &config.reasoning_effort {
        if !matches!(effort.as_str(), "low" | "medium" | "high" | "xhigh" | "max") {
            return Err(ModelError::Configuration(
                "unsupported Anthropic effort value".into(),
            ));
        }
        body["output_config"]["effort"] = json!(effort);
    }
    if let Some(tools) = compatible.get("tools").and_then(Value::as_array) {
        body["tools"]=json!(tools.iter().map(|t|json!({"name":t["function"]["name"],"description":t["function"]["description"],"input_schema":t["function"]["parameters"]})).collect::<Vec<_>>());
        body["tool_choice"] = json!({"type":if request.tools_enabled{"auto"}else{"none"}});
    }
    if config.native_tools
        && let Some(schema) = &request.output_schema
    {
        body["output_config"]["format"] = json!({"type":"json_schema","schema":schema});
    }
    Ok(body)
}
/// Compiler L4/L5 are ordered activity, including when the first activity is
/// itself a system delta. They must never join the provider's stable system
/// field merely because there has not yet been a user/assistant turn.
fn stable_system_prefix(request: &ModelRequest, index: usize, offset: usize, text: &str) -> bool {
    if text.starts_with("[L4]\n")
        || text.starts_with("[L5]\n")
        || text.starts_with("Context update v")
    {
        return false;
    }
    let compiled = request
        .messages
        .first()
        .is_some_and(|m| m.role == "system" && m.content.starts_with("[L0]\n"));
    !compiled
        || index
            .checked_sub(offset)
            .is_none_or(|original| original < 4)
}
fn runtime_frame(role: &str, text: &str) -> String {
    format!("[runtime_instruction original_role={role}]\n{text}")
}
fn replay_matches(original: &Message, visible: &str, calls: &[Value], native: bool) -> bool {
    if !native {
        if !calls.is_empty() {
            return false;
        }
        let Ok(parsed) = crate::parse_text_protocol(visible) else {
            return false;
        };
        return parsed.content == original.content
            && parsed.tool_calls.len() == original.tool_calls.len()
            && parsed
                .tool_calls
                .iter()
                .zip(&original.tool_calls)
                .all(|(a, b)| {
                    a.id == b.id
                        && a.name == b.name
                        && serde_json::from_str::<Value>(&a.arguments).ok()
                            == serde_json::from_str::<Value>(&b.arguments).ok()
                });
    }
    visible == original.content
        && calls.len() == original.tool_calls.len()
        && calls.iter().zip(&original.tool_calls).all(|(a, b)| {
            a["id"] == b.id
                && a["function"]["name"] == b.name
                && serde_json::from_str::<Value>(a["function"]["arguments"].as_str().unwrap()).ok()
                    == serde_json::from_str::<Value>(&b.arguments).ok()
        })
}
// Exact aliases and dated variants only; future model capabilities must be
// confirmed by the host instead of guessed from a numeric version comparison.
fn anthropic_system_model(model: &str) -> bool {
    [
        "claude-fable-5-1",
        "claude-mythos-5-1",
        "claude-fable-5",
        "claude-mythos-5",
        "claude-opus-5-5",
        "claude-opus-4-8",
        "claude-opus-5",
        "claude-sonnet-5-5",
        "claude-haiku-5-5",
    ]
    .into_iter()
    .any(|known| {
        model == known
            || model
                .strip_prefix(known)
                .and_then(|s| s.strip_prefix('-'))
                .is_some_and(|suffix| {
                    suffix.len() == 8 && suffix.bytes().all(|b| b.is_ascii_digit())
                })
    })
}
fn gemini_request(
    config: &GatewayConfig,
    request: &ModelRequest,
    compatible: Value,
) -> Result<Value, ModelError> {
    if config.reasoning_effort.is_some() {
        return Err(ModelError::Configuration(
            "Gemini thinking configuration must be selected through a model-specific adapter"
                .into(),
        ));
    }
    let mut system = Vec::new();
    let mut contents = Vec::new();
    let mut identities: BTreeMap<String, (String, Option<String>)> = BTreeMap::new();
    let offset = usize::from(!config.native_tools);
    for (index, message) in compatible["messages"]
        .as_array()
        .unwrap()
        .iter()
        .enumerate()
    {
        let role = message["role"].as_str().unwrap();
        let text = message["content"].as_str().unwrap_or("");
        if matches!(role, "system" | "developer")
            && contents.is_empty()
            && stable_system_prefix(request, index, offset, text)
        {
            system.push(json!({"text":text}));
            continue;
        }
        let original = index
            .checked_sub(offset)
            .and_then(|i| request.messages.get(i));
        let parts = if role == "assistant" {
            if let Some(state) = original.and_then(|m| m.provider_state.as_ref()) {
                if state["provider"] != "gemini" {
                    return Err(ModelError::Configuration(
                        "provider state belongs to a different native adapter".into(),
                    ));
                }
                let parts = state["parts"]
                    .as_array()
                    .ok_or_else(|| validation("invalid Gemini replay parts"))?;
                let (visible, calls) = gemini_parts(parts)?;
                let original = original.unwrap();
                if !replay_matches(original, &visible, &calls, config.native_tools) {
                    return Err(ModelError::Configuration(
                        "Gemini replay state does not match the durable assistant message".into(),
                    ));
                }
                for (part_index, part) in parts.iter().enumerate() {
                    if let Some(call) = part.get("functionCall") {
                        identities.insert(
                            gemini_id(call, part_index),
                            (
                                call["name"].as_str().unwrap().into(),
                                call["id"].as_str().map(String::from),
                            ),
                        );
                    }
                }
                parts.clone()
            } else if config.native_tools && original.is_some_and(|m| !m.tool_calls.is_empty()) {
                return Err(ModelError::Configuration(
                    "Gemini native tool history requires its original durable provider state"
                        .into(),
                ));
            } else {
                vec![json!({"text":text})]
            }
        } else if config.native_tools && role == "tool" {
            let runtime_id = message["tool_call_id"]
                .as_str()
                .ok_or_else(|| validation("missing Gemini tool result identity"))?;
            let (name, native_id) = identities
                .get(runtime_id)
                .ok_or_else(|| validation("Gemini tool result has no original call"))?;
            let mut result = json!({"name":name,"response":{"output":serde_json::from_str::<Value>(text).unwrap_or_else(|_|json!(text))}});
            if let Some(id) = native_id {
                result["id"] = json!(id);
            }
            vec![json!({"functionResponse":result})]
        } else {
            // Gemini has no mid-conversation system role. Preserve ordering and
            // provenance rather than hoisting volatile data into the prefix.
            let text = if matches!(role, "system" | "developer") {
                runtime_frame(role, text)
            } else {
                text.into()
            };
            vec![json!({"text":text})]
        };
        contents.push(json!({"role":if role=="assistant"{"model"}else{"user"},"parts":parts}));
    }
    let mut body = json!({"contents":contents,"generationConfig":{"maxOutputTokens":config.max_output_tokens}});
    if !system.is_empty() {
        body["systemInstruction"] = json!({"parts":system});
    }
    if let Some(seed) = config.seed {
        body["generationConfig"]["seed"] = json!(seed);
    }
    if let Some(temp) = config.temperature {
        body["generationConfig"]["temperature"] = json!(temp);
    }
    if config.native_tools
        && let Some(tools) = compatible["tools"].as_array()
    {
        body["tools"] = json!([{"functionDeclarations":tools.iter().map(|t|json!({"name":t["function"]["name"],"description":t["function"]["description"],"parametersJsonSchema":t["function"]["parameters"]})).collect::<Vec<_>>()}]);
        body["toolConfig"] =
            json!({"functionCallingConfig":{"mode":if request.tools_enabled{"AUTO"}else{"NONE"}}});
    }
    if config.native_tools
        && let Some(schema) = &request.output_schema
    {
        body["generationConfig"]["responseMimeType"] = json!("application/json");
        body["generationConfig"]["responseJsonSchema"] = schema.clone();
    }
    Ok(body)
}
fn gemini_id(call: &Value, index: usize) -> String {
    use sha2::{Digest, Sha256};
    call["id"]
        .as_str()
        .filter(|s| !s.is_empty())
        .map(String::from)
        .unwrap_or_else(|| {
            format!(
                "gemini_{index}_{:x}",
                Sha256::digest(call.to_string().as_bytes())
            )
        })
}
fn gemini_parts(parts: &[Value]) -> Result<(String, Vec<Value>), ModelError> {
    if parts.len() > 128
        || parts.iter().map(|p| p.to_string().len()).sum::<usize>() > 8 * 1024 * 1024
    {
        return Err(ModelError::Truncated);
    }
    let mut text = String::new();
    let mut calls = Vec::new();
    for (index, part) in parts.iter().enumerate() {
        if part.to_string().len() > 8 * 1024 * 1024 {
            return Err(ModelError::Truncated);
        }
        if part.as_object().is_none_or(|p| {
            p.keys().any(|k| {
                !matches!(
                    k.as_str(),
                    "text" | "thought" | "thoughtSignature" | "functionCall"
                )
            })
        }) {
            return Err(ModelError::Configuration(
                "unsupported Gemini response part; this gateway accepts text and function calls"
                    .into(),
            ));
        }
        if let Some(call) = part.get("functionCall") {
            if call["name"].as_str().is_none_or(str::is_empty)
                || !call.get("args").is_none_or(Value::is_object)
            {
                return Err(validation("invalid Gemini function call"));
            }
            calls.push(json!({"id":gemini_id(call,index),"type":"function","function":{"name":call["name"],"arguments":call.get("args").cloned().unwrap_or_else(||json!({})).to_string()}}));
        } else if part["thought"] != true {
            text.push_str(part["text"].as_str().unwrap_or(""));
        }
    }
    Ok((text, calls))
}

pub(crate) fn retry(provider: Provider, body: &mut Value, previous: &Value, reason: &str) {
    let content = previous["content"].as_str().unwrap_or("");
    let correction = format!(
        "Output validation failed: {reason}. Return one corrected output conforming to the specified schemas and tool protocol."
    );
    match provider {
        Provider::GoogleAi | Provider::Vertex => {
            let messages = body["contents"].as_array_mut().unwrap();
            let parts = previous
                .pointer("/provider_state/parts")
                .cloned()
                .unwrap_or_else(|| json!([{"text":content}]));
            messages.push(json!({"role":"model","parts":parts}));
            messages.push(json!({"role":"user","parts":[{"text":correction}]}));
        }
        Provider::Anthropic => {
            let messages = body["messages"].as_array_mut().unwrap();
            let blocks = previous
                .pointer("/provider_state/blocks")
                .cloned()
                .unwrap_or_else(|| json!([{"type":"text","text":content}]));
            messages.push(json!({"role":"assistant","content":blocks}));
            messages.push(json!({"role":"user","content":[{"type":"text","text":correction}]}));
        }
        _ => {
            let messages = body["messages"].as_array_mut().unwrap();
            messages.push(json!({"role":"assistant","content":content}));
            messages.push(json!({"role":"system","content":correction}));
        }
    }
}
/// Normalize native accounting to inclusive input and output counts without
/// converting an absent cache counter to a measured zero.
pub(crate) fn response(provider: Provider, raw: Value) -> Result<Value, ModelError> {
    match provider {
        Provider::Anthropic => {
            let blocks = raw["content"].as_array().ok_or(ModelError::Provider)?;
            let mut text = String::new();
            let mut calls = Vec::new();
            for block in blocks {
                match block["type"].as_str(){Some("text")=>text.push_str(block["text"].as_str().unwrap_or("")),Some("tool_use")=>calls.push(json!({"id":block["id"],"type":"function","function":{"name":block["name"],"arguments":block["input"].to_string()}})),_=>{}}
            }
            let u = &raw["usage"];
            let count = |name: &str| u[name].as_u64();
            let input = count("input_tokens")
                .zip(count("cache_read_input_tokens"))
                .zip(count("cache_creation_input_tokens"))
                .map(|((uncached, read), write)| {
                    uncached.saturating_add(read).saturating_add(write)
                });
            let mut usage = json!({"prompt_tokens":input,"completion_tokens":u["output_tokens"],"prompt_tokens_details":{"cached_tokens":u["cache_read_input_tokens"],"cache_write_tokens":u["cache_creation_input_tokens"]},"completion_tokens_details":{"reasoning_tokens":u["output_tokens_details"]["thinking_tokens"]}});
            if let Some(ttls) = u.get("cache_creation") {
                usage["cache_creation"] = ttls.clone();
            }
            Ok(
                json!({"choices":[{"finish_reason":match raw["stop_reason"].as_str(){Some("end_turn"|"stop_sequence")=>"stop",Some("tool_use")=>"tool_calls",_=>"length"},"message":{"role":"assistant","content":text,"tool_calls":calls,"provider_state":{"provider":"anthropic","blocks":blocks}}}],"usage":usage}),
            )
        }
        Provider::GoogleAi | Provider::Vertex => {
            let candidate = raw.pointer("/candidates/0").ok_or(ModelError::Truncated)?;
            let parts = candidate
                .pointer("/content/parts")
                .and_then(Value::as_array)
                .ok_or(ModelError::Provider)?;
            let (text, calls) = gemini_parts(parts)?;
            let u = &raw["usageMetadata"];
            let input = u["promptTokenCount"]
                .as_u64()
                .map(|v| v.saturating_add(u["toolUsePromptTokenCount"].as_u64().unwrap_or(0)));
            let output = u["candidatesTokenCount"]
                .as_u64()
                .map(|v| v.saturating_add(u["thoughtsTokenCount"].as_u64().unwrap_or(0)));
            Ok(
                json!({"choices":[{"finish_reason":if candidate["finishReason"]=="STOP"{if calls.is_empty(){"stop"}else{"tool_calls"}}else{"length"},"message":{"role":"assistant","content":text,"tool_calls":calls,"provider_state":{"provider":"gemini","parts":parts}}}],"usage":{"prompt_tokens":input,"completion_tokens":output,"prompt_tokens_details":{"cached_tokens":u["cachedContentTokenCount"]},"completion_tokens_details":{"reasoning_tokens":u["thoughtsTokenCount"]}}}),
            )
        }
        _ => Ok(raw),
    }
}
pub(crate) async fn stream(
    provider: Provider,
    mut stream: SseStream,
    sink: &StreamSink,
    attempt: u8,
) -> Result<Value, ModelError> {
    let mut content = String::new();
    let mut calls: BTreeMap<usize, Value> = BTreeMap::new();
    let mut usage = json!({});
    let mut finish = Value::Null;
    let mut saw_start = false;
    let mut gemini_response_parts = Vec::new();
    let mut anthropic_blocks: BTreeMap<usize, Value> = BTreeMap::new();
    while let Some(event) = stream
        .next_event()
        .await
        .map_err(|_| ModelError::Truncated)?
    {
        let data = event.data;
        match provider {
            Provider::Anthropic => match data["type"].as_str().or(event.event.as_deref()) {
                Some("message_start") => {
                    saw_start = true;
                    usage = data["message"]["usage"].clone();
                }
                Some("content_block_start") => {
                    let index = data["index"]
                        .as_u64()
                        .filter(|i| *i < 128)
                        .ok_or(ModelError::Truncated)? as usize;
                    let block = &data["content_block"];
                    anthropic_blocks.insert(index, block.clone());
                    if block["type"] == "tool_use" {
                        let call = json!({"id":block["id"],"name":block["name"],"arguments":""});
                        emit(
                            sink,
                            StreamDelta::Tool {
                                attempt,
                                index,
                                id: block["id"].as_str().unwrap_or("").into(),
                                name: block["name"].as_str().unwrap_or("").into(),
                                arguments: String::new(),
                            },
                        )?;
                        calls.insert(index, call);
                    } else if block["type"] == "text" {
                        let text = block["text"].as_str().unwrap_or("");
                        content.push_str(text);
                        if !text.is_empty() {
                            emit(
                                sink,
                                StreamDelta::Text {
                                    attempt,
                                    delta: text.into(),
                                },
                            )?;
                        }
                    }
                }
                Some("content_block_delta") => {
                    let delta = &data["delta"];
                    if delta["type"] == "text_delta" {
                        let text = delta["text"].as_str().ok_or(ModelError::Truncated)?;
                        content.push_str(text);
                        let index = data["index"].as_u64().ok_or(ModelError::Truncated)? as usize;
                        let block = anthropic_blocks
                            .get_mut(&index)
                            .ok_or(ModelError::Truncated)?;
                        let mut full = block["text"].as_str().unwrap_or("").to_string();
                        full.push_str(text);
                        block["text"] = json!(full);
                        emit(
                            sink,
                            StreamDelta::Text {
                                attempt,
                                delta: text.into(),
                            },
                        )?;
                    } else if matches!(
                        delta["type"].as_str(),
                        Some("thinking_delta" | "signature_delta")
                    ) {
                        let index = data["index"].as_u64().ok_or(ModelError::Truncated)? as usize;
                        let field = if delta["type"] == "thinking_delta" {
                            "thinking"
                        } else {
                            "signature"
                        };
                        let block = anthropic_blocks
                            .get_mut(&index)
                            .ok_or(ModelError::Truncated)?;
                        let mut full = block[field].as_str().unwrap_or("").to_string();
                        full.push_str(delta[field].as_str().ok_or(ModelError::Truncated)?);
                        block[field] = json!(full);
                    } else if delta["type"] == "input_json_delta" {
                        let index = data["index"].as_u64().ok_or(ModelError::Truncated)? as usize;
                        let part = delta["partial_json"]
                            .as_str()
                            .ok_or(ModelError::Truncated)?;
                        let call = calls.get_mut(&index).ok_or(ModelError::Truncated)?;
                        let mut args = call["arguments"].as_str().unwrap().to_owned();
                        args.push_str(part);
                        call["arguments"] = json!(args);
                        emit(
                            sink,
                            StreamDelta::Tool {
                                attempt,
                                index,
                                id: String::new(),
                                name: String::new(),
                                arguments: part.into(),
                            },
                        )?;
                    }
                }
                Some("message_delta") => {
                    finish = data["delta"]["stop_reason"].clone();
                    if let Some(updates) = data["usage"].as_object() {
                        for (k, v) in updates {
                            usage[k] = v.clone();
                        }
                    }
                }
                _ => {}
            },
            Provider::GoogleAi | Provider::Vertex => {
                if let Some(u) = data.get("usageMetadata") {
                    usage = u.clone();
                }
                if let Some(candidate) = data.pointer("/candidates/0") {
                    saw_start = true;
                    if let Some(reason) = candidate.get("finishReason") {
                        finish = reason.clone();
                    }
                    for part in candidate
                        .pointer("/content/parts")
                        .and_then(Value::as_array)
                        .into_iter()
                        .flatten()
                    {
                        let index = gemini_response_parts.len();
                        gemini_response_parts.push(part.clone());
                        if let Some(call) = part.get("functionCall") {
                            emit(
                                sink,
                                StreamDelta::Tool {
                                    attempt,
                                    index,
                                    id: gemini_id(call, index),
                                    name: call["name"].as_str().unwrap_or("").into(),
                                    arguments: call
                                        .get("args")
                                        .cloned()
                                        .unwrap_or_else(|| json!({}))
                                        .to_string(),
                                },
                            )?;
                        } else if part["thought"] != true
                            && let Some(text) = part["text"].as_str()
                        {
                            content.push_str(text);
                            emit(
                                sink,
                                StreamDelta::Text {
                                    attempt,
                                    delta: text.into(),
                                },
                            )?;
                        }
                    }
                }
            }
            _ => {
                return Err(ModelError::Configuration(
                    "unsupported native stream adapter".into(),
                ));
            }
        }
        if content.len()
            + calls.values().map(|v| v.to_string().len()).sum::<usize>()
            + anthropic_blocks
                .values()
                .map(|v| v.to_string().len())
                .sum::<usize>()
            + gemini_response_parts
                .iter()
                .map(|v| v.to_string().len())
                .sum::<usize>()
            > 8 * 1024 * 1024
        {
            return Err(ModelError::Truncated);
        }
    }
    if !saw_start || finish.is_null() {
        return Err(ModelError::Truncated);
    }
    let raw = if provider == Provider::Anthropic {
        for (index, call) in &calls {
            let args = call["arguments"].as_str().unwrap();
            let input: Value = if args.is_empty() {
                json!({})
            } else {
                serde_json::from_str(args)
                    .map_err(|_| validation("invalid streamed Anthropic tool JSON"))?
            };
            anthropic_blocks
                .get_mut(index)
                .ok_or(ModelError::Truncated)?["input"] = input;
        }
        let blocks = anthropic_blocks.into_values().collect::<Vec<_>>();
        json!({"content":blocks,"stop_reason":finish,"usage":usage})
    } else {
        json!({"candidates":[{"content":{"parts":gemini_response_parts},"finishReason":finish}],"usageMetadata":usage})
    };
    response(provider, raw)
}
