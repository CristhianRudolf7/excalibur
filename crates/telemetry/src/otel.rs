//! Local OTLP JSON export; no collector or network side effects.
use crate::{
    Error, EventEnvelope, Result, RunMetadata, TaskMetrics, digest, metrics::call_metrics,
};
use serde_json::{Value, json};
use std::collections::BTreeMap;
pub const GENAI_COMMIT: &str = "3fe84604a581b3d50dd5b4bd29383b462d2a5018";
fn attr(key: &str, value: impl Into<Value>) -> Value {
    let value = value.into();
    json!({"key":key,"value":if let Some(n)=value.as_u64(){json!({"intValue":n.to_string()})}else{json!({"stringValue":value.as_str().unwrap_or_default()})}})
}
#[allow(clippy::too_many_arguments)]
fn span(
    run: &str,
    key: &str,
    name: &str,
    kind: u8,
    start: i64,
    end: i64,
    attributes: Vec<Value>,
    parent: Option<&str>,
    failed: bool,
) -> Value {
    let mut value = json!({"traceId":&digest(run.as_bytes())[..32],"spanId":&digest(format!("{run}\0{key}").as_bytes())[..16],"name":name,"kind":kind,"startTimeUnixNano":(start.max(0) as u64).saturating_mul(1_000_000).to_string(),"endTimeUnixNano":(end.max(start).max(0) as u64).saturating_mul(1_000_000).to_string(),"attributes":attributes,"status":{"code":if failed{2}else{0}}});
    if let Some(parent) = parent {
        value["parentSpanId"] = json!(parent);
    }
    value
}
pub(crate) fn export(
    run: &str,
    metadata: &RunMetadata,
    events: &[EventEnvelope],
    metrics: &TaskMetrics,
) -> Value {
    let projected = crate::auxiliary::events(events);
    let events = projected
        .iter()
        .map(|event| event.as_ref())
        .collect::<Vec<_>>();
    let mut spans = Vec::new();
    let mut omissions = Vec::new();
    let root = digest(format!("{run}\0agent").as_bytes());
    let has_root = metrics.recovered_events == 0
        && metrics.started_at_unix_ms.is_some()
        && metrics.ended_at_unix_ms.is_some();
    if has_root {
        spans.push(span(
            run,
            "agent",
            &format!("invoke_agent {}", metadata.agent_name),
            1,
            metrics.started_at_unix_ms.unwrap(),
            metrics.ended_at_unix_ms.unwrap(),
            {
                let mut attrs = vec![
                    attr("gen_ai.operation.name", "invoke_agent"),
                    attr("gen_ai.agent.name", metadata.agent_name.clone()),
                    attr("gen_ai.conversation.id", run),
                    attr("excalibur.run.id", run),
                ];
                if metrics.final_state.as_deref() == Some("blocked") {
                    attrs.push(attr("error.type", "run_blocked"));
                }
                attrs
            },
            None,
            metrics.final_state.as_deref() == Some("blocked"),
        ));
    } else {
        omissions.push("agent timing unavailable");
    }
    let mut requests: BTreeMap<String, i64> = BTreeMap::new();
    let mut tools: BTreeMap<String, (i64, String, Option<String>)> = BTreeMap::new();
    for event in events.iter().filter(|e| e.source == "journal") {
        let p = &event.payload;
        match event.journal_kind.as_str() {
            "model_request" | "model_dispatch" if !event.recovered => {
                requests.insert(
                    event
                        .key
                        .trim_end_matches(":request")
                        .trim_end_matches(":dispatch")
                        .trim_end_matches(":started")
                        .to_owned(),
                    event.timestamp_unix_ms,
                );
            }
            "model_response" | "model_failure" => {
                let m = call_metrics(p);
                if p["_context_summary_unconfirmed"] == true {
                    omissions.push("context-summary response and provider timing unconfirmed");
                    continue;
                }
                if event.recovered
                    || (m["provider_calls_known"] == true
                        && m["provider_calls"].as_u64() == Some(0))
                {
                    omissions.push("recovered or replayed model call");
                    continue;
                }
                let provider = m
                    .pointer("/metadata/provider")
                    .and_then(Value::as_str)
                    .or(metadata.provider.as_deref());
                let Some(provider) = provider else {
                    omissions.push("model provider unavailable");
                    continue;
                };
                let key = event
                    .key
                    .trim_end_matches(":response")
                    .trim_end_matches(":failure")
                    .trim_end_matches(":completed");
                let start = m["duration_ms"]
                    .as_u64()
                    .and_then(|n| i64::try_from(n).ok())
                    .map(|n| event.timestamp_unix_ms.saturating_sub(n))
                    .or_else(|| requests.remove(key));
                let Some(start) = start else {
                    omissions.push("model timing unavailable");
                    continue;
                };
                let model = m
                    .pointer("/metadata/model")
                    .and_then(Value::as_str)
                    .or(metadata.model.as_deref());
                let mut attrs = vec![
                    attr("gen_ai.operation.name", "chat"),
                    attr("gen_ai.provider.name", provider),
                    attr("gen_ai.conversation.id", run),
                    attr("excalibur.journal.sequence", event.sequence),
                ];
                if p["_context_summary"] == true {
                    attrs.push(attr("excalibur.model.purpose", "context-summary"));
                }
                if let Some(model) = model {
                    attrs.push(attr("gen_ai.request.model", model));
                }
                for (source, target) in [
                    ("input_tokens", "gen_ai.usage.input_tokens"),
                    ("output_tokens", "gen_ai.usage.output_tokens"),
                ] {
                    if let Some(n) = m["usage"][source].as_u64() {
                        attrs.push(attr(target, n));
                    }
                }
                let failed = event.journal_kind == "model_failure"
                    || (p["_context_summary"] == true && m["failure_class"].is_string());
                if failed {
                    attrs.push(attr(
                        "error.type",
                        m.pointer("/failure_class")
                            .or_else(|| m.pointer("/attempts/0/error/class"))
                            .and_then(Value::as_str)
                            .unwrap_or("model_failure"),
                    ));
                }
                spans.push(span(
                    run,
                    &event.key,
                    &format!("chat {}", model.unwrap_or("")),
                    3,
                    start,
                    event.timestamp_unix_ms,
                    attrs,
                    has_root.then_some(&root[..16]),
                    failed,
                ));
            }
            "tool_intent" if !event.recovered => {
                tools.insert(
                    event.key.clone(),
                    (
                        event.timestamp_unix_ms,
                        p.pointer("/prepared/call/name")
                            .and_then(Value::as_str)
                            .unwrap_or("unattributed")
                            .into(),
                        p.pointer("/prepared/call/id")
                            .and_then(Value::as_str)
                            .map(str::to_owned),
                    ),
                );
            }
            "tool_result" if !event.recovered => {
                let key = event.key.trim_end_matches(":result");
                let Some((start, name, call_id)) = tools.remove(key) else {
                    omissions.push("tool timing unavailable");
                    continue;
                };
                let start = p
                    .get("duration_ms")
                    .or_else(|| p.pointer("/metrics/duration_ms"))
                    .and_then(Value::as_u64)
                    .and_then(|n| i64::try_from(n).ok())
                    .map(|n| event.timestamp_unix_ms.saturating_sub(n))
                    .unwrap_or(start);
                let failed = p.pointer("/result/success") == Some(&Value::Bool(false));
                let mut attrs = vec![
                    attr("gen_ai.operation.name", "execute_tool"),
                    attr("gen_ai.tool.name", name.clone()),
                    attr("gen_ai.tool.type", "function"),
                    attr("gen_ai.agent.name", metadata.agent_name.clone()),
                    attr("gen_ai.conversation.id", run),
                    attr("excalibur.journal.sequence", event.sequence),
                ];
                if let Some(id) = call_id {
                    attrs.push(attr("gen_ai.tool.call.id", id));
                }
                if failed {
                    attrs.push(attr("error.type", "tool_failure"));
                }
                spans.push(span(
                    run,
                    &event.key,
                    &format!("execute_tool {name}"),
                    1,
                    start,
                    event.timestamp_unix_ms,
                    attrs,
                    has_root.then_some(&root[..16]),
                    failed,
                ));
            }
            _ => {}
        }
    }
    if metrics.pending_model_calls > 0 {
        omissions.push("model calls awaiting confirmed outcome");
    }
    omissions.sort();
    omissions.dedup();
    json!({"resourceSpans":[{"resource":{"attributes":[attr("service.name","excalibur"),attr("service.version",metadata.agent_version.clone()),attr("excalibur.otel.omissions",omissions.join("; "))]},"scopeSpans":[{"scope":{"name":"excalibur.telemetry","version":env!("CARGO_PKG_VERSION"),"attributes":[attr("excalibur.gen_ai.conventions.commit",GENAI_COMMIT)]},"spans":spans}]}]})
}
/// Checks emitted OTLP JSON identifiers, timing, operation attributes and the
/// pinned GenAI scope. The reference Python validator additionally parses the
/// official generated protobuf message; this function does not contact a collector.
pub fn validate(document: &Value) -> Result<()> {
    let groups = document["resourceSpans"]
        .as_array()
        .ok_or(Error::Invalid("OTLP resourceSpans missing"))?;
    for resource in groups {
        for scope in resource["scopeSpans"]
            .as_array()
            .ok_or(Error::Invalid("OTLP scopeSpans missing"))?
        {
            let pin = scope["scope"]["attributes"].as_array().is_some_and(|a| {
                a.iter().any(|a| {
                    a["key"] == "excalibur.gen_ai.conventions.commit"
                        && a["value"]["stringValue"] == GENAI_COMMIT
                })
            });
            if !pin {
                return Err(Error::Invalid("OTLP GenAI convention pin missing"));
            }
            for span in scope["spans"]
                .as_array()
                .ok_or(Error::Invalid("OTLP spans missing"))?
            {
                for (key, len) in [("traceId", 32), ("spanId", 16)] {
                    let id = span[key]
                        .as_str()
                        .ok_or(Error::Invalid("OTLP id missing"))?;
                    if id.len() != len
                        || !id.bytes().all(|b| b.is_ascii_hexdigit())
                        || id.bytes().all(|b| b == b'0')
                    {
                        return Err(Error::Invalid("OTLP id invalid"));
                    }
                }
                let time = |key: &str| {
                    span[key]
                        .as_str()
                        .and_then(|s| s.parse::<u64>().ok())
                        .ok_or(Error::Invalid("OTLP timestamp invalid"))
                };
                if time("endTimeUnixNano")? < time("startTimeUnixNano")? {
                    return Err(Error::Invalid("OTLP timing invalid"));
                }
                let attrs = span["attributes"]
                    .as_array()
                    .ok_or(Error::Invalid("OTLP attributes missing"))?;
                let attr = |key: &str| {
                    attrs
                        .iter()
                        .find(|v| v["key"] == key)
                        .and_then(|v| v["value"]["stringValue"].as_str())
                };
                match attr("gen_ai.operation.name") {
                    Some("invoke_agent")
                        if span["kind"] == 1 && attr("gen_ai.agent.name").is_some() => {}
                    Some("execute_tool")
                        if span["kind"] == 1 && attr("gen_ai.tool.name").is_some() => {}
                    Some("chat") if span["kind"] == 3 && attr("gen_ai.provider.name").is_some() => {
                    }
                    _ => return Err(Error::Invalid("OTLP GenAI operation attributes invalid")),
                }
            }
        }
    }
    Ok(())
}
