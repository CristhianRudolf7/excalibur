use serde_json::Value;
fn safe(value: Option<&Value>) -> String {
    value
        .and_then(Value::as_str)
        .unwrap_or("unknown")
        .chars()
        .take(160)
        .filter(|c| !c.is_control() || *c == '\n')
        .collect()
}
pub(super) fn control(command: &str, value: &Value) -> String {
    if value.get("selection_required").and_then(Value::as_bool) == Some(true)
        || value.get("candidates").is_some()
    {
        let choices = value
            .get("candidates")
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .take(16)
                    .map(|item| format!("{} ({})", safe(item.get("name")), safe(item.get("id"))))
                    .collect::<Vec<_>>()
                    .join(", ")
            })
            .unwrap_or_default();
        return format!(
            "Choose the intended agent or task by its identifier; no target was selected automatically. {choices}"
        );
    }
    if value.get("confirmation_required").and_then(Value::as_bool) == Some(true) {
        return format!(
            "Cancellation needs your confirmation for {}. No work has been cancelled.",
            safe(value.get("target"))
        );
    }
    if value
        .get("error")
        .and_then(Value::as_str)
        .is_some_and(|e| e.starts_with("uncertain interrupted master action"))
    {
        return "This is an uncertain interrupted master action. Inspect its durable operation before resolving it; the effect was not repeated.".into();
    }
    if value.get("error").is_some() {
        return format!(
            "The {command} request could not be completed. Inspect the task status before retrying."
        );
    }
    if command == "status" {
        let items = value
            .as_array()
            .or_else(|| value.get("agents").and_then(Value::as_array))
            .or_else(|| value.get("results").and_then(Value::as_array));
        if let Some(items) = items {
            if items.is_empty() {
                return "No active agents.".into();
            }
            return items
                .iter()
                .take(32)
                .map(|v| {
                    let progress = v
                        .get("last_progress")
                        .and_then(|p| {
                            p.as_str().map(str::to_string).or_else(|| {
                                p.get("text")
                                    .or_else(|| p.get("message"))
                                    .and_then(Value::as_str)
                                    .map(str::to_string)
                            })
                        })
                        .unwrap_or_default();
                    let progress = safe(Some(&Value::String(progress)));
                    let plan = v
                        .get("plan")
                        .and_then(|p| {
                            p.as_array()
                                .map(Vec::len)
                                .or_else(|| p.get("nodes").and_then(Value::as_array).map(Vec::len))
                        })
                        .unwrap_or(0);
                    format!(
                        "{}: {} (step {}; plan {} items). Progress: {}. {}",
                        safe(
                            v.get("name")
                                .or_else(|| v.get("agent").and_then(|agent| agent.get("name")))
                                .or_else(|| v.get("id"))
                                .or_else(|| v.get("agent").and_then(|agent| agent.get("id")))
                        ),
                        safe(v.get("state")),
                        v.get("step").and_then(Value::as_u64).unwrap_or(0),
                        plan,
                        progress,
                        v.get("reason")
                            .filter(|r| !r.is_null())
                            .map(|r| format!("Attention: {}.", safe(Some(r))))
                            .unwrap_or_default()
                    )
                })
                .collect::<Vec<_>>()
                .join("\n");
        }
        return format!(
            "Task {} status: {}.",
            safe(value.get("target").or_else(|| value.get("id"))),
            safe(value.get("state").or_else(|| value.get("status")))
        );
    }
    match command {
        "pause" => "Pause requested. You can resume or cancel the selected work.",
        "resume" => "Resume requested for the selected work.",
        "cancel" => "Cancellation requested for the selected work.",
        "stop" => "The conversational turn was stopped; background tasks are unchanged.",
        "project" => "The conversation project selection was updated.",
        "approve" => "The approval response was recorded.",
        "deny" => "The denial response was recorded.",
        _ => "The requested control operation was recorded.",
    }
    .into()
}
pub(super) fn notice(content: &Value) -> String {
    let inner = content
        .get("content")
        .and_then(Value::as_str)
        .and_then(|text| serde_json::from_str::<Value>(text).ok());
    let body = inner.as_ref().unwrap_or(content);
    let kind = body
        .get("kind")
        .and_then(Value::as_str)
        .or_else(|| body.get("state").and_then(Value::as_str))
        .unwrap_or("update");
    let name = safe(
        body.get("name")
            .or_else(|| body.get("agent"))
            .or_else(|| body.get("agent_id"))
            .or_else(|| body.get("task"))
            .or_else(|| content.get("name"))
            .or_else(|| content.get("agent_id")),
    );
    let detail = safe(
        body.get("text")
            .or_else(|| body.get("message"))
            .or_else(|| body.get("summary"))
            .or_else(|| {
                body.get("final_text")
                    .filter(|v| v.as_str().is_some_and(|s| !s.is_empty()))
            })
            .or_else(|| body.get("reason").filter(|v| !v.is_null()))
            .or_else(|| {
                if inner.is_none() {
                    content.get("content")
                } else {
                    None
                }
            }),
    );
    let origin = content
        .get("origin")
        .map(|value| format!(" [conversation {}]", safe(Some(value))))
        .unwrap_or_default();
    format!("{name} — {kind}: {detail}{origin}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn cancellation_waiting_for_confirmation_is_never_reported_as_performed() {
        let text = control(
            "cancel",
            &json!({"confirmation_required":true,"target":"worker-7"}),
        );
        assert!(text.contains("needs your confirmation"));
        assert!(text.contains("worker-7"));
        assert!(text.contains("No work has been cancelled"));
    }
    #[test]
    fn stop_notice_uses_reason_when_there_is_no_final_deliverable() {
        let text = notice(
            &json!({"name":"Build worker","content":json!({"state":"blocked","reason":"compiler failed","final_text":null}).to_string()}),
        );
        assert!(text.contains("Build worker"));
        assert!(text.contains("compiler failed"));
    }
}
