//! ATIF-v1.8, pinned to the vendored Harbor reference validator. JSON schema is
//! supplemented with the reference model's relational validation rules.
use crate::{
    Error, EventEnvelope, Result, RunMetadata, TaskMetrics,
    metrics::{call_metrics, response_cost, response_usage},
};
use serde_json::{Value, json};
use std::{collections::BTreeSet, sync::OnceLock};
pub const HARBOR_COMMIT: &str = "509b119db0d97b141dcd9c80f4f7ab861c47c2d6";
pub const HARBOR_VERSION: &str = "0.25.0";
pub const ATIF_VERSION: &str = "ATIF-v1.8";
fn timestamp(milliseconds: i64) -> Option<String> {
    time::OffsetDateTime::from_unix_timestamp_nanos(milliseconds as i128 * 1_000_000)
        .ok()?
        .format(&time::format_description::well_known::Rfc3339)
        .ok()
}
fn step(id: usize, source: &str, message: Value, event: Option<&EventEnvelope>) -> Value {
    let mut value = json!({"step_id":id,"source":source,"message":message});
    if let Some(event) = event {
        value["extra"] = json!({"journal_sequence":event.sequence,"event_key":event.key,"recovered":event.recovered});
        if !event.recovered
            && let Some(time) = timestamp(event.timestamp_unix_ms)
        {
            value["timestamp"] = json!(time);
        }
    }
    value
}
pub(crate) fn trajectory(
    run: &str,
    metadata: &RunMetadata,
    events: &[EventEnvelope],
    metrics: &TaskMetrics,
    finalized: bool,
) -> Result<Value> {
    let projected = crate::auxiliary::events(events);
    let events = projected
        .iter()
        .map(|event| event.as_ref())
        .collect::<Vec<_>>();
    let mut initial = metadata.instruction.clone();
    if initial.is_empty() {
        initial = events
            .iter()
            .find(|e| e.journal_kind == "run_started")
            .and_then(|e| e.payload.pointer("/spec/context/layers/3"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .into();
    }
    let mut steps = vec![step(
        1,
        "user",
        json!(initial),
        events
            .iter()
            .find(|e| e.journal_kind == "run_started")
            .copied(),
    )];
    let mut current = None;
    let mut intent_calls = std::collections::BTreeMap::new();
    for event in events.iter().filter(|e| e.source == "journal") {
        match event.journal_kind.as_str() {
            "model_response" | "model_failure" => {
                let response = &event.payload["response"];
                let message = &response["message"];
                let mut value = step(
                    steps.len() + 1,
                    "agent",
                    json!(
                        message["content"]
                            .as_str()
                            .or_else(|| event.payload["error"].as_str())
                            .unwrap_or_default()
                    ),
                    Some(event),
                );
                let detail = call_metrics(&event.payload);
                let usage = response_usage(&event.payload);
                let cost = response_cost(&event.payload);
                if detail["provider_calls_known"] == true {
                    value["llm_call_count"] = detail["provider_calls"].clone();
                }
                // Replayed responses are deterministic dispatch in the current
                // execution; historical LLM usage remains in extra, not metrics.
                if value["llm_call_count"] != 0
                    && event.payload["_context_summary_usage_unknown"] != true
                {
                    value["metrics"] = json!({"prompt_tokens":usage.input_tokens,"completion_tokens":usage.output_tokens,"cached_tokens":usage.cache_read_tokens,"cost_usd":cost.map(|n|n as f64/1_000_000.0),"extra":{"cache_write_tokens":usage.cache_write_tokens,"cache_write_by_ttl":usage.cache_write_by_ttl,"reasoning_tokens":usage.reasoning_tokens,"estimated":usage.estimated}});
                }
                if !detail.is_null() {
                    value["extra"]["model_call_metrics"] = detail.clone();
                }
                if let Some(name) = detail
                    .pointer("/metadata/model")
                    .and_then(Value::as_str)
                    .or(metadata.model.as_deref())
                {
                    value["model_name"] = json!(name);
                }
                if let Some(effort) = metadata.reasoning_effort.as_deref() {
                    value["reasoning_effort"] = json!(effort);
                }
                if let Some(calls) = message["tool_calls"]
                    .as_array()
                    .filter(|calls| !calls.is_empty())
                {
                    value["tool_calls"]=Value::Array(calls.iter().map(|call| {
                        let raw=call["arguments"].as_str().unwrap_or("{}");let parsed=serde_json::from_str::<Value>(raw).ok().filter(Value::is_object);
                        json!({"tool_call_id":call["id"],"function_name":call["name"],"arguments":parsed.unwrap_or_else(||json!({"_unparsed_arguments":raw}))})
                    }).collect());
                }
                let auxiliary = event.payload["_context_summary"] == true;
                if auxiliary {
                    value["extra"]["purpose"] = json!("context-summary");
                    value["extra"]["outcome"] = event.payload["outcome"].clone();
                }
                steps.push(value);
                if !auxiliary {
                    current = Some(steps.len() - 1);
                    intent_calls.clear();
                }
            }
            "tool_intent" => {
                let call = &event.payload["prepared"]["call"];
                if let (Some(index), Some(id)) =
                    (event.payload["index"].as_u64(), call["id"].as_str())
                {
                    intent_calls.insert(index, id.to_owned());
                }
                if current.is_none() && call["id"].is_string() {
                    let mut value = step(steps.len() + 1, "agent", json!(""), Some(event));
                    value["llm_call_count"] = json!(0);
                    value["tool_calls"] = json!([{"tool_call_id":call["id"],"function_name":call["name"],"arguments":serde_json::from_str::<Value>(call["arguments"].as_str().unwrap_or("{}")).ok().filter(Value::is_object).unwrap_or_else(||json!({"_unparsed_arguments":call["arguments"]}))}]);
                    steps.push(value);
                    current = Some(steps.len() - 1);
                }
            }
            "tool_result" => {
                let index = event.payload["index"].as_u64().unwrap_or(0) as usize;
                if let Some(position) = current {
                    let id = intent_calls.get(&(index as u64)).cloned().or_else(|| {
                        steps[position]
                            .pointer(&format!("/tool_calls/{index}/tool_call_id"))
                            .and_then(Value::as_str)
                            .map(str::to_owned)
                    });
                    let observation = json!({"source_call_id":id,"content":serde_json::to_string(&event.payload["result"])?,"extra":{"journal_sequence":event.sequence,"event_key":event.key}});
                    if steps[position]["observation"].is_null() {
                        steps[position]["observation"] = json!({"results":[]});
                    }
                    steps[position]["observation"]["results"]
                        .as_array_mut()
                        .unwrap()
                        .push(observation);
                } else {
                    let mut value = step(steps.len() + 1, "system", json!(""), Some(event));
                    value["observation"] = json!({"results":[{"source_call_id":null,"content":serde_json::to_string(&event.payload["result"])?}]});
                    steps.push(value);
                }
            }
            "directive_received" => steps.push(step(
                steps.len() + 1,
                "user",
                json!(event.payload["text"].as_str().unwrap_or_default()),
                Some(event),
            )),
            "step_completed" => {
                current = None;
                intent_calls.clear();
            }
            _ => {}
        }
    }
    let definitions=events.iter().find(|e|e.journal_kind=="model_request").and_then(|e|e.payload.pointer("/request/tools")).and_then(Value::as_array).map(|tools|tools.iter().map(|tool|json!({"type":"function","function":{"name":tool["name"],"description":tool["description"],"parameters":tool["input_schema"]}})).collect::<Vec<_>>());
    let value = json!({"schema_version":ATIF_VERSION,"session_id":run,"trajectory_id":format!("excalibur:{run}"),"agent":{"name":metadata.agent_name,"version":metadata.agent_version,"model_name":metadata.model,"tool_definitions":definitions,"extra":{"commit":metadata.excalibur_commit,"profile":metadata.profile,"configuration_hash":metadata.configuration_hash}},"steps":steps,"final_metrics":{"total_prompt_tokens":(metrics.unmeasured_model_calls==0).then_some(metrics.tokens.input_tokens),"total_completion_tokens":(metrics.unmeasured_model_calls==0).then_some(metrics.tokens.output_tokens),"total_cached_tokens":metrics.tokens.cache_read_tokens,"total_cost_usd":metrics.cost_microusd.map(|n|n as f64/1_000_000.0),"total_steps":steps.len(),"extra":{"cache_write_tokens":metrics.tokens.cache_write_tokens,"reasoning_tokens":metrics.tokens.reasoning_tokens,"provider_calls":metrics.provider_calls,"provider_calls_known":metrics.provider_calls_known,"usage_known":metrics.usage_known,"pending_model_calls":metrics.pending_model_calls,"known_input_token_subtotal":metrics.tokens.input_tokens,"known_output_token_subtotal":metrics.tokens.output_tokens}},"extra":{"metadata":metadata,"finalized":finalized,"final_state":metrics.final_state,"harbor_reference_version":HARBOR_VERSION,"harbor_reference_commit":HARBOR_COMMIT,"resources":metrics.resources}});
    validate(&value)?;
    Ok(value)
}
pub fn validate(value: &Value) -> Result<()> {
    static VALIDATOR: OnceLock<jsonschema::Validator> = OnceLock::new();
    let validator = VALIDATOR.get_or_init(|| {
        jsonschema::validator_for(
            &serde_json::from_str::<Value>(include_str!("../reference/atif-v1.8.schema.json"))
                .expect("pinned schema JSON"),
        )
        .expect("pinned schema validity")
    });
    if value["schema_version"] != ATIF_VERSION || !validator.is_valid(value) {
        return Err(Error::Invalid("ATIF JSON schema validation failed"));
    }
    validate_relations(value)
}
fn validate_relations(value: &Value) -> Result<()> {
    let steps = value["steps"]
        .as_array()
        .ok_or(Error::Invalid("ATIF steps missing"))?;
    for (index, step) in steps.iter().enumerate() {
        validate_content(&step["message"])?;
        if step["step_id"].as_u64() != Some(index as u64 + 1) {
            return Err(Error::Invalid("ATIF step IDs must be sequential"));
        }
        if step["source"] != "agent"
            && [
                "model_name",
                "reasoning_effort",
                "reasoning_content",
                "tool_calls",
                "metrics",
            ]
            .iter()
            .any(|field| step.get(*field).is_some_and(|v| !v.is_null()))
        {
            return Err(Error::Invalid(
                "ATIF non-agent step contains agent-only fields",
            ));
        }
        if step["source"] == "agent"
            && step["llm_call_count"] == 0
            && ["metrics", "reasoning_content"]
                .iter()
                .any(|field| step.get(*field).is_some_and(|v| !v.is_null()))
        {
            return Err(Error::Invalid(
                "ATIF deterministic step contains LLM-only fields",
            ));
        }
        let ids = step["tool_calls"]
            .as_array()
            .map(|v| {
                v.iter()
                    .filter_map(|call| call["tool_call_id"].as_str())
                    .collect::<BTreeSet<_>>()
            })
            .unwrap_or_default();
        if let Some(results) = step
            .pointer("/observation/results")
            .and_then(Value::as_array)
        {
            for result in results {
                validate_content(&result["content"])?;
                if let Some(refs) = result["subagent_trajectory_ref"].as_array() {
                    for reference in refs {
                        if reference["trajectory_id"].is_null()
                            && reference["trajectory_path"].is_null()
                        {
                            return Err(Error::Invalid(
                                "ATIF subagent reference is not resolvable",
                            ));
                        }
                    }
                }
                if let Some(id) = result["source_call_id"].as_str()
                    && !ids.contains(id)
                {
                    return Err(Error::Invalid(
                        "ATIF observation references an unknown tool call",
                    ));
                }
            }
        }
        if let Some(stamp) = step["timestamp"].as_str()
            && time::OffsetDateTime::parse(stamp, &time::format_description::well_known::Rfc3339)
                .is_err()
        {
            return Err(Error::Invalid("ATIF timestamp is invalid"));
        }
    }
    let mut ids = BTreeSet::new();
    if let Some(children) = value["subagent_trajectories"].as_array() {
        for child in children {
            let id = child["trajectory_id"]
                .as_str()
                .ok_or(Error::Invalid("ATIF embedded trajectory ID missing"))?;
            if !ids.insert(id) {
                return Err(Error::Invalid(
                    "ATIF embedded trajectory IDs must be unique",
                ));
            }
            validate_relations(child)?;
        }
    }
    Ok(())
}
fn validate_content(value: &Value) -> Result<()> {
    if let Some(parts) = value.as_array() {
        for part in parts {
            match part["type"].as_str() {
                Some("text") if part["text"].is_string() && part["source"].is_null() => {}
                Some(kind @ ("image" | "audio"))
                    if part["text"].is_null()
                        && part["source"]["media_type"]
                            .as_str()
                            .is_some_and(|m| m.starts_with(&format!("{kind}/"))) => {}
                _ => {
                    return Err(Error::Invalid(
                        "ATIF content kind and source are inconsistent",
                    ));
                }
            }
        }
    }
    Ok(())
}
pub(crate) fn agent_context(metadata: &RunMetadata, metrics: &TaskMetrics) -> Value {
    let models=metrics.model_usage.iter().filter(|(_,totals)|totals.tokens.cache_read_tokens.is_some()).map(|(name,totals)|(name.clone(),json!({"n_input_tokens":totals.tokens.input_tokens,"n_cache_tokens":totals.tokens.cache_read_tokens,"n_output_tokens":totals.tokens.output_tokens,"cost_usd":totals.cost_microusd.map(|n|n as f64/1_000_000.0)}))).collect::<serde_json::Map<_,_>>();
    json!({"n_input_tokens":(metrics.unmeasured_model_calls==0).then_some(metrics.tokens.input_tokens),"n_cache_tokens":metrics.tokens.cache_read_tokens,"n_output_tokens":(metrics.unmeasured_model_calls==0).then_some(metrics.tokens.output_tokens),"cost_usd":metrics.cost_microusd.map(|n|n as f64/1_000_000.0),"model_usage":models,"metadata":{"excalibur":metadata,"final_state":metrics.final_state,"exit_code":metrics.exit_code,"limitations":metrics.limitations,"cache_read_known":metrics.tokens.cache_read_tokens.is_some(),"pending_model_calls":metrics.pending_model_calls,"usage_known":metrics.usage_known,"known_input_token_subtotal":metrics.tokens.input_tokens,"known_output_token_subtotal":metrics.tokens.output_tokens,"complete_model_usage":metrics.model_usage.values().all(|v|v.tokens.cache_read_tokens.is_some()),"model_usage_with_unknown_cache":metrics.model_usage.iter().filter(|(_,v)|v.tokens.cache_read_tokens.is_none()).collect::<std::collections::BTreeMap<_,_>>()}})
}
