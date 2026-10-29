use crate::{EventEnvelope, RunMetadata, resources::ResourceSummary};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct TokenTotals {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: Option<u64>,
    pub cache_write_tokens: Option<u64>,
    pub cache_write_by_ttl: BTreeMap<String, u64>,
    pub reasoning_tokens: Option<u64>,
    pub estimated: bool,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct ModelTotals {
    pub calls: u64,
    #[serde(default)]
    pub pending_calls: u64,
    pub provider_calls: u64,
    pub provider_calls_known: bool,
    pub usage_known: bool,
    pub tokens: TokenTotals,
    pub cost_microusd: Option<u64>,
    pub duration_ms: Option<u64>,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct TaskMetrics {
    pub schema_version: String,
    pub run_id: String,
    pub final_state: Option<String>,
    pub exit_code: Option<i32>,
    pub failure_class: Option<String>,
    pub started_at_unix_ms: Option<i64>,
    pub ended_at_unix_ms: Option<i64>,
    pub elapsed_ms: Option<u64>,
    pub model_time_ms: Option<u64>,
    pub tool_time_ms: Option<u64>,
    pub waiting_user_ms: Option<u64>,
    pub tokens: TokenTotals,
    pub recorded_tokens: Option<TokenTotals>,
    pub cost_microusd: Option<u64>,
    pub recorded_cost_microusd: Option<u64>,
    pub cache_hit_rate: Option<f64>,
    pub model_calls: u64,
    #[serde(default)]
    pub pending_model_calls: u64,
    pub provider_calls: u64,
    pub provider_calls_known: bool,
    pub usage_known: bool,
    pub model_calls_by_role: BTreeMap<String, u64>,
    pub model_usage: BTreeMap<String, ModelTotals>,
    pub tool_calls: u64,
    pub tool_calls_by_name: BTreeMap<String, u64>,
    pub gate_attempts: u64,
    pub gate_pending: Vec<String>,
    pub criteria_total: Option<u64>,
    pub criteria_fulfilled: Option<u64>,
    pub interventions: u64,
    pub corrections: u64,
    pub compacted_batches: u64,
    pub masked_batches: u64,
    pub loops_detected: u64,
    pub provider_retries: u64,
    pub tool_retries: u64,
    pub subagents_created: u64,
    pub maximum_subagent_depth: u64,
    pub errors: u64,
    pub price_tables: BTreeMap<String, String>,
    pub unmeasured_model_calls: u64,
    pub verifier_reward: Option<f64>,
    pub verifier_output: Option<Value>,
    pub recovered_events: u64,
    pub resources: ResourceSummary,
    pub limitations: Vec<String>,
}
fn add_optional(total: &mut Option<u64>, value: Option<u64>, first: bool) {
    if first {
        *total = value;
    } else {
        *total = total.zip(value).map(|(a, b)| a.saturating_add(b));
    }
}
impl TokenTotals {
    pub(crate) fn from_usage(v: &Value) -> Self {
        Self {
            input_tokens: number(v, &["input_tokens", "prompt_tokens"]),
            output_tokens: number(v, &["output_tokens", "completion_tokens"]),
            cache_read_tokens: optional(v, &["cache_read_tokens", "cached_tokens"]),
            cache_write_tokens: optional(v, &["cache_write_tokens"]),
            cache_write_by_ttl: v
                .get("cache_write_by_ttl")
                .and_then(Value::as_object)
                .map(|m| {
                    m.iter()
                        .filter_map(|(k, v)| v.as_u64().map(|n| (k.clone(), n)))
                        .collect()
                })
                .unwrap_or_default(),
            reasoning_tokens: optional(v, &["reasoning_tokens"]),
            estimated: v["estimated"].as_bool().unwrap_or(false),
        }
    }
    pub(crate) fn add(&mut self, other: &Self, first: bool) {
        self.input_tokens = self.input_tokens.saturating_add(other.input_tokens);
        self.output_tokens = self.output_tokens.saturating_add(other.output_tokens);
        add_optional(&mut self.cache_read_tokens, other.cache_read_tokens, first);
        add_optional(
            &mut self.cache_write_tokens,
            other.cache_write_tokens,
            first,
        );
        add_optional(&mut self.reasoning_tokens, other.reasoning_tokens, first);
        for (ttl, tokens) in &other.cache_write_by_ttl {
            let value = self.cache_write_by_ttl.entry(ttl.clone()).or_default();
            *value = value.saturating_add(*tokens);
        }
        self.estimated |= other.estimated;
    }
}
fn optional(v: &Value, keys: &[&str]) -> Option<u64> {
    keys.iter().find_map(|k| v.get(k).and_then(Value::as_u64))
}
fn number(v: &Value, keys: &[&str]) -> u64 {
    optional(v, keys).unwrap_or(0)
}
pub(crate) fn call_metrics(payload: &Value) -> &Value {
    payload
        .get("metrics")
        .or_else(|| payload.pointer("/response/metrics"))
        .unwrap_or(&Value::Null)
}
pub(crate) fn response_usage(payload: &Value) -> TokenTotals {
    let metrics = call_metrics(payload);
    if !metrics.is_null() {
        TokenTotals::from_usage(&metrics["usage"])
    } else {
        TokenTotals::from_usage(&payload["response"]["usage"])
    }
}
pub(crate) fn response_cost(payload: &Value) -> Option<u64> {
    let m = call_metrics(payload);
    if !m.is_null() {
        m.get("cost_microusd").and_then(Value::as_u64)
    } else {
        payload["response"]["usage"]["cost_microusd"]
            .as_u64()
            .filter(|n| *n > 0)
    }
}
fn model_operation(key: &str) -> &str {
    [":request", ":dispatch", ":response", ":failure"]
        .iter()
        .find_map(|suffix| key.strip_suffix(suffix))
        .unwrap_or(key)
}
pub(crate) fn aggregate(
    run: &str,
    metadata: &RunMetadata,
    events: &[EventEnvelope],
    snapshot: Option<&Value>,
    resources: ResourceSummary,
) -> TaskMetrics {
    let mut result = TaskMetrics {
        schema_version: "excalibur.metrics.v1".into(),
        run_id: run.into(),
        resources,
        provider_calls_known: true,
        usage_known: true,
        ..Default::default()
    };
    let mut request_at: BTreeMap<String, i64> = BTreeMap::new();
    let mut dispatches: BTreeMap<String, &EventEnvelope> = BTreeMap::new();
    let mut requests: BTreeMap<String, &EventEnvelope> = BTreeMap::new();
    let dispatch_protocol = events.iter().any(|e| {
        e.source == "journal"
            && e.journal_kind == "run_started"
            && e.payload["model_dispatch_protocol"].as_u64() == Some(1)
    });
    let mut tool_at: BTreeMap<String, i64> = BTreeMap::new();
    let mut model_time = 0_u64;
    let mut tool_time = 0_u64;
    let mut measured_models = 0;
    let mut measured_tools = 0;
    let mut waiting = None;
    let mut waiting_ms = 0_u64;
    for e in events.iter().filter(|e| e.source == "journal") {
        result.started_at_unix_ms = Some(
            result
                .started_at_unix_ms
                .map_or(e.timestamp_unix_ms, |old| old.min(e.timestamp_unix_ms)),
        );
        result.ended_at_unix_ms = Some(
            result
                .ended_at_unix_ms
                .map_or(e.timestamp_unix_ms, |old| old.max(e.timestamp_unix_ms)),
        );
        result.recovered_events += u64::from(e.recovered);
        let p = &e.payload;
        match e.journal_kind.as_str() {
            "model_request" => {
                requests.insert(model_operation(&e.key).into(), e);
                if !e.recovered {
                    request_at.insert(
                        e.key.trim_end_matches(":request").into(),
                        e.timestamp_unix_ms,
                    );
                }
            }
            "model_dispatch" => {
                dispatches.insert(model_operation(&e.key).into(), e);
            }
            "model_response" | "model_failure" => {
                requests.remove(model_operation(&e.key));
                let request_start = request_at.remove(model_operation(&e.key));
                let dispatch = dispatches.remove(model_operation(&e.key));
                let started = match dispatch {
                    Some(dispatch) if !dispatch.recovered => Some(dispatch.timestamp_unix_ms),
                    Some(_) => None,
                    None => request_start,
                };
                if e.journal_kind == "model_failure" {
                    result.errors = result.errors.saturating_add(1);
                    result.failure_class = p["failure_class"]
                        .as_str()
                        .map(str::to_owned)
                        .or(result.failure_class);
                }
                if p.get("metrics").is_none() && p.pointer("/response/usage").is_none() {
                    result.unmeasured_model_calls += 1;
                }
                let first = result.model_calls == 0;
                result.model_calls += 1;
                let metrics = call_metrics(p);
                if let (Some(version), Some(hash)) = (
                    metrics
                        .pointer("/metadata/price_table_version")
                        .and_then(Value::as_str),
                    metrics
                        .pointer("/metadata/price_table_hash")
                        .and_then(Value::as_str),
                ) {
                    result.price_tables.insert(version.into(), hash.into());
                }
                let provider_known = metrics["provider_calls_known"].as_bool().unwrap_or(false);
                let usage_known = metrics["usage_known"].as_bool().unwrap_or(false);
                result.provider_calls_known &= provider_known;
                result.usage_known &= usage_known;
                let tokens = response_usage(p);
                result.tokens.add(&tokens, first);
                let cost = response_cost(p);
                add_optional(&mut result.cost_microusd, cost, first);
                let provider_calls = metrics
                    .get("provider_calls")
                    .and_then(Value::as_u64)
                    .unwrap_or(1);
                result.provider_calls = result.provider_calls.saturating_add(provider_calls);
                result.provider_retries = result
                    .provider_retries
                    .saturating_add(provider_calls.saturating_sub(1));
                let role = metrics
                    .pointer("/metadata/role")
                    .and_then(Value::as_str)
                    .or(metadata.role.as_deref())
                    .unwrap_or("unattributed");
                *result.model_calls_by_role.entry(role.into()).or_default() += 1;
                let model = metrics
                    .pointer("/metadata/model")
                    .and_then(Value::as_str)
                    .or(metadata.model.as_deref())
                    .unwrap_or("unattributed");
                let totals = result.model_usage.entry(model.into()).or_default();
                let first_model = totals.calls == 0;
                totals.provider_calls_known = if first_model {
                    provider_known
                } else {
                    totals.provider_calls_known && provider_known
                };
                totals.usage_known = if first_model {
                    usage_known
                } else {
                    totals.usage_known && usage_known
                };
                totals.calls += 1;
                totals.provider_calls = totals.provider_calls.saturating_add(provider_calls);
                totals.tokens.add(&tokens, first_model);
                add_optional(&mut totals.cost_microusd, cost, first_model);
                let duration = metrics
                    .get("duration_ms")
                    .and_then(Value::as_u64)
                    .or_else(|| {
                        if e.recovered {
                            None
                        } else {
                            started.map(|start| {
                                e.timestamp_unix_ms.saturating_sub(start).max(0) as u64
                            })
                        }
                    });
                add_optional(&mut totals.duration_ms, duration, first_model);
                if let Some(duration) = duration {
                    model_time = model_time.saturating_add(duration);
                    measured_models += 1;
                }
                if let Some(recorded) = metrics.get("recorded_usage").filter(|v| !v.is_null()) {
                    let first = result.recorded_tokens.is_none();
                    result
                        .recorded_tokens
                        .get_or_insert_with(Default::default)
                        .add(&TokenTotals::from_usage(recorded), first);
                    let n = metrics
                        .get("recorded_cost_microusd")
                        .and_then(Value::as_u64);
                    add_optional(&mut result.recorded_cost_microusd, n, first);
                }
            }
            "tool_intent" => {
                result.tool_calls += 1;
                let name = p
                    .pointer("/prepared/call/name")
                    .and_then(Value::as_str)
                    .unwrap_or("unattributed");
                *result.tool_calls_by_name.entry(name.into()).or_default() += 1;
                if !e.recovered {
                    tool_at.insert(e.key.clone(), e.timestamp_unix_ms);
                }
            }
            "tool_result" => {
                let key = e.key.trim_end_matches(":result");
                let duration = p
                    .get("duration_ms")
                    .or_else(|| p.pointer("/metrics/duration_ms"))
                    .and_then(Value::as_u64)
                    .or_else(|| {
                        if e.recovered {
                            None
                        } else {
                            tool_at.remove(key).map(|start| {
                                e.timestamp_unix_ms.saturating_sub(start).max(0) as u64
                            })
                        }
                    });
                if let Some(duration) = duration {
                    tool_time = tool_time.saturating_add(duration);
                    measured_tools += 1;
                }
                if p.pointer("/result/success") == Some(&Value::Bool(false)) {
                    result.errors += 1;
                }
                if p.get("reconciliation_evidence").is_some() {
                    result.tool_retries += 1;
                }
            }
            "gate_report" => {
                result.gate_attempts += 1;
                result.gate_pending = p["pending"]
                    .as_array()
                    .map(|v| {
                        v.iter()
                            .filter_map(Value::as_str)
                            .map(str::to_owned)
                            .collect()
                    })
                    .unwrap_or_default();
            }
            "state_change" => {
                let state = p["state"].as_str().unwrap_or("unknown");
                result.final_state = Some(state.into());
                if state == "paused" {
                    waiting.get_or_insert(e.timestamp_unix_ms);
                } else if let Some(start) = waiting.take() {
                    waiting_ms = waiting_ms
                        .saturating_add(e.timestamp_unix_ms.saturating_sub(start).max(0) as u64);
                }
            }
            "directive_received" => {
                result.interventions += 1;
                result.corrections += 1;
            }
            k if k.contains("compact") => result.compacted_batches += 1,
            k if k.contains("mask") => result.masked_batches += 1,
            k if k.contains("loop_detect") => result.loops_detected += 1,
            k if k.contains("tool_retry") => result.tool_retries += 1,
            k if k.contains("provider_retry") => result.provider_retries += 1,
            "subagent_created" => {
                result.subagents_created += 1;
                result.maximum_subagent_depth = result
                    .maximum_subagent_depth
                    .max(p["depth"].as_u64().unwrap_or(0));
            }
            k if k.ends_with("_error") || k == "error" => result.errors += 1,
            _ => {}
        }
    }
    // Dispatch confirms a logical gateway invocation, not an observed HTTP
    // request count. Without a durable outcome all usage/cost remains unknown.
    let mut legacy_pending = 0_u64;
    if !dispatch_protocol {
        for (operation, request) in requests {
            if let std::collections::btree_map::Entry::Vacant(entry) = dispatches.entry(operation) {
                entry.insert(request);
                legacy_pending += 1;
            }
        }
    }
    if legacy_pending > 0 {
        result.limitations.push(format!("{legacy_pending} legacy requests lack dispatch markers and outcomes; dispatch is uncertain"));
    }
    result.pending_model_calls = dispatches.len() as u64;
    for dispatch in dispatches.values() {
        result.model_calls = result.model_calls.saturating_add(1);
        result.unmeasured_model_calls = result.unmeasured_model_calls.saturating_add(1);
        result.provider_calls_known = false;
        result.usage_known = false;
        result.cost_microusd = None;
        result.tokens.cache_read_tokens = None;
        result.tokens.cache_write_tokens = None;
        result.tokens.reasoning_tokens = None;
        let detail = &dispatch.payload["metadata"];
        let role = detail["role"]
            .as_str()
            .or(metadata.role.as_deref())
            .unwrap_or("unattributed");
        *result.model_calls_by_role.entry(role.into()).or_default() += 1;
        let model = detail["model"]
            .as_str()
            .or(metadata.model.as_deref())
            .unwrap_or("unattributed");
        let totals = result.model_usage.entry(model.into()).or_default();
        totals.calls = totals.calls.saturating_add(1);
        totals.pending_calls = totals.pending_calls.saturating_add(1);
        totals.provider_calls_known = false;
        totals.usage_known = false;
        totals.cost_microusd = None;
        totals.duration_ms = None;
        totals.tokens.cache_read_tokens = None;
        totals.tokens.cache_write_tokens = None;
        totals.tokens.reasoning_tokens = None;
        if let (Some(version), Some(hash)) = (
            detail["price_table_version"].as_str(),
            detail["price_table_hash"].as_str(),
        ) {
            result.price_tables.insert(version.into(), hash.into());
        }
    }
    if let Some(snapshot) = snapshot {
        result.final_state = snapshot["status"]
            .as_str()
            .map(str::to_owned)
            .or(result.final_state);
        result.failure_class = snapshot["failure_class"]
            .as_str()
            .map(str::to_owned)
            .or(result.failure_class);
        result.criteria_total = snapshot
            .pointer("/spec/criteria")
            .and_then(Value::as_array)
            .map(|v| v.len() as u64);
        if result.final_state.as_deref() == Some("completed") {
            result.criteria_fulfilled = result.criteria_total;
        }
        result.exit_code = snapshot
            .get("exit_code")
            .and_then(Value::as_i64)
            .and_then(|n| i32::try_from(n).ok());
    }
    if let Some(at) = snapshot.and_then(|s| s["_telemetry_ended_at_ms"].as_i64()) {
        result.ended_at_unix_ms = Some(at);
    }
    if let Some(start) = waiting
        && let Some(end) = result.ended_at_unix_ms
    {
        waiting_ms = waiting_ms.saturating_add(end.saturating_sub(start).max(0) as u64);
    }
    if result.model_calls == 0 {
        result.cost_microusd = Some(0);
        result.tokens.cache_read_tokens = Some(0);
        result.tokens.cache_write_tokens = Some(0);
        result.tokens.reasoning_tokens = Some(0);
    }
    result.elapsed_ms = result
        .started_at_unix_ms
        .zip(result.ended_at_unix_ms)
        .map(|(a, b)| b.saturating_sub(a).max(0) as u64);
    result.model_time_ms = (measured_models == result.model_calls).then_some(model_time);
    result.tool_time_ms = (measured_tools == result.tool_calls).then_some(tool_time);
    result.waiting_user_ms = if result.recovered_events == 0 {
        Some(waiting_ms)
    } else {
        None
    };
    result.cache_hit_rate = result
        .tokens
        .cache_read_tokens
        .filter(|_| result.tokens.input_tokens > 0)
        .map(|n| n as f64 / result.tokens.input_tokens as f64);
    for e in events
        .iter()
        .filter(|e| e.source == "external" && e.journal_kind == "verifier_result")
    {
        result.verifier_reward = e.payload["reward"].as_f64();
        result.verifier_output = Some(e.payload["output"].clone());
    }
    if result.pending_model_calls > 0 {
        result.limitations.push("pending model calls have no confirmed outcome; physical requests, usage, cost and duration remain unknown".into());
    }
    if !result.provider_calls_known {
        result.limitations.push(
            "physical provider-call count is unknown for one or more logical model calls".into(),
        );
    }
    if !result.usage_known && result.model_calls > 0 {
        result
            .limitations
            .push("token totals include estimated or provider-unconfirmed usage".into());
    }
    if result.unmeasured_model_calls > 0 {
        result.limitations.push("one or more model calls have no confirmed provider usage; token totals are known subtotals".into());
    }
    if result.tokens.cache_read_tokens.is_none() {
        result
            .limitations
            .push("cache read usage not reported for every model call".into());
    }
    if result.cost_microusd.is_none() {
        result
            .limitations
            .push("cost is unknown for one or more model calls".into());
    }
    if result.recovered_events > 0 {
        result
            .limitations
            .push("backfilled event timestamps do not measure original execution timing".into());
        result.elapsed_ms = None;
    }
    if result.model_time_ms.is_none() {
        result
            .limitations
            .push("model durations are incomplete".into());
    }
    if result.tool_time_ms.is_none() {
        result
            .limitations
            .push("tool durations are incomplete".into());
    }
    result
}
