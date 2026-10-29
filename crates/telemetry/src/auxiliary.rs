//! Account for auxiliary inference without changing immutable journal records or
//! associating extractor text with executor tool calls.
use crate::{EventEnvelope, EventKind};
use serde_json::json;
use std::borrow::Cow;

pub(crate) fn events(events: &[EventEnvelope]) -> Vec<Cow<'_, EventEnvelope>> {
    events.iter().map(project).collect()
}
fn project(event: &EventEnvelope) -> Cow<'_, EventEnvelope> {
    let started = event.journal_kind == "context_summary_started";
    let completed = event.journal_kind == "context_summary_completed";
    if !started && !completed {
        return Cow::Borrowed(event);
    }
    let mut projected = event.clone();
    projected.payload["_context_summary"] = json!(true);
    if started {
        projected.journal_kind = "model_dispatch".into();
        projected.kind = EventKind::ModelDispatched;
        // The extractor may use another gateway: do not invent its model from
        // the executor's run metadata while no receipt exists.
        projected.payload["metadata"] = json!({"role":"context-summary","model":"unattributed"});
        if let Some(payload) = projected.payload.as_object_mut() {
            payload.remove("request");
        }
    } else {
        projected.journal_kind = "model_response".into();
        projected.kind = EventKind::ModelCompleted;
        let confirmed = projected
            .payload
            .get("metrics")
            .is_some_and(|m| m.is_object());
        if !confirmed {
            let reservation = projected.payload["usage"].clone();
            projected.payload["metrics"] = json!({"metadata":{"role":"context-summary","model":"unattributed"},"provider_calls":0,"provider_calls_known":false,"usage_known":false,"usage":{},"cost_microusd":null,"reservation":reservation});
            projected.payload["_context_summary_unconfirmed"] = json!(true);
        } else {
            // Preserve measured identity/usage and distinguish purpose even if
            // the host reused the executor gateway for this auxiliary request.
            let metadata = &mut projected.payload["metrics"]["metadata"];
            if !metadata.is_object() {
                *metadata = json!({});
            }
            metadata["role"] = json!("context-summary");
        }
        projected.payload["_context_summary_usage_unknown"] =
            json!(projected.payload["metrics"]["usage_known"] != true);
        projected.payload["response"] = json!({"message":{"role":"assistant","content":"Auxiliary context summary completed","tool_calls":[]},"usage":projected.payload["usage"]});
    }
    Cow::Owned(projected)
}
