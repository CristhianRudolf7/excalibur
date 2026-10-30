//! Bounded current-run utilities. No cross-task memory, retrieval service or model.
use excalibur_journal::Journal;
use excalibur_types::runtime::{
    Effect, PreparedTool, ToolCall, ToolDefinition, ToolResult, ToolRuntime,
};
use serde_json::{Value, json};
use std::collections::BTreeMap;
const PAGE: usize = 16;
const SNIPPET_BYTES: usize = 512;
pub(crate) fn is_builtin(name: &str) -> bool {
    matches!(
        name,
        "history_search" | "run_history_search" | "pin_observation" | "ack_directive"
    )
}
pub(crate) fn definitions() -> Vec<ToolDefinition> {
    let schema = |required: Vec<&str>, properties: Value| json!({"$schema":"https://json-schema.org/draft/2020-12/schema","type":"object","required":required,"properties":properties,"additionalProperties":false});
    let mut definitions=vec![ToolDefinition{name:"history_search".into(),description:"Search one bounded page of this run's confirmed journal, with sanitized snippets and a continuation cursor.".into(),input_schema:schema(vec![],json!({"query":{"type":"string","maxLength":256},"from_sequence":{"type":"integer","minimum":1},"to_sequence":{"type":"integer","minimum":1},"cursor":{"type":"integer","minimum":0},"limit":{"type":"integer","minimum":1,"maximum":16}}))},ToolDefinition{name:"pin_observation".into(),description:"Pin or unpin a confirmed tool observation in this run by call ID and optional source sequence; retain its durable source reference.".into(),input_schema:schema(vec!["call_id"],json!({"call_id":{"type":"string","minLength":1,"maxLength":256},"source_sequence":{"type":"integer","minimum":1},"note":{"type":"string","maxLength":2048},"pinned":{"type":"boolean"}}))},ToolDefinition{name:"ack_directive".into(),description:"Acknowledge an existing directive received in this run. This cannot create a directive or approve permissions.".into(),input_schema:schema(vec!["id"],json!({"id":{"type":"string","minLength":1,"maxLength":256}}))}];
    let mut run_history = definitions[0].clone();
    run_history.name = "run_history_search".into();
    definitions.push(run_history);
    definitions
}
pub(crate) fn prepare(call: ToolCall) -> Result<PreparedTool, String> {
    let definition = definitions()
        .into_iter()
        .find(|definition| definition.name == call.name)
        .ok_or("unknown episode utility")?;
    let data: Value = serde_json::from_str(&call.arguments)
        .or_else(|_| json5::from_str(&call.arguments))
        .map_err(|_| "invalid utility JSON")?;
    let validator = jsonschema::validator_for(&definition.input_schema)
        .map_err(|_| "invalid built-in utility schema")?;
    if !validator.is_valid(&data) {
        return Err("invalid episode utility arguments; use the advertised schema".into());
    }
    for field in [
        "cursor",
        "limit",
        "source_sequence",
        "from_sequence",
        "to_sequence",
    ] {
        if data
            .get(field)
            .is_some_and(|value| value.as_u64().is_none_or(|number| number > i64::MAX as u64))
        {
            return Err(format!(
                "{field} must be a supported unsigned journal integer"
            ));
        }
    }
    for (field, max) in [
        ("query", 256),
        ("call_id", 256),
        ("id", 256),
        ("note", 2048),
    ] {
        if data[field].as_str().is_some_and(|text| text.len() > max) {
            return Err(format!("{field} exceeds its UTF-8 byte limit"));
        }
    }
    if let (Some(from), Some(to)) = (data["from_sequence"].as_u64(), data["to_sequence"].as_u64())
        && from > to
    {
        return Err("from_sequence must not exceed to_sequence".into());
    }
    Ok(PreparedTool {
        call,
        effect: Effect::Pure,
        resources: Vec::new(),
        exclusive: false,
        data,
    })
}
pub(crate) async fn execute(
    journal: &Journal,
    tools: &dyn ToolRuntime,
    run: &str,
    _key: &str,
    prepared: &PreparedTool,
) -> ToolResult {
    let result = match prepared.call.name.as_str() {
        "history_search" | "run_history_search" => {
            history(journal, tools, run, &prepared.data).await
        }
        "pin_observation" => pin(journal, run, &prepared.data).await,
        "ack_directive" => ack(journal, run, &prepared.data).await,
        _ => Err("unknown episode utility".into()),
    };
    match result {
        Ok(output) => match tools.sanitize_durable(&output) {
            Ok(safe) => ToolResult::ok(safe),
            Err(_) => ToolResult::error("episode utility output sanitization failed"),
        },
        Err(reason) => ToolResult::error(reason),
    }
}
fn snippet(text: &str) -> String {
    if text.len() <= SNIPPET_BYTES {
        return text.into();
    }
    let mut end = SNIPPET_BYTES - "…".len();
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &text[..end])
}
async fn history(
    journal: &Journal,
    tools: &dyn ToolRuntime,
    run: &str,
    args: &Value,
) -> Result<Value, String> {
    let first = args["from_sequence"].as_u64().unwrap_or(1);
    let last = args["to_sequence"].as_u64().unwrap_or(u64::MAX);
    let after = args["cursor"].as_u64().unwrap_or(0).max(first - 1);
    let limit = args["limit"].as_u64().unwrap_or(PAGE as u64) as usize;
    let query = args["query"].as_str().unwrap_or("").to_lowercase();
    let page = journal
        .entries(run, after, PAGE as u32)
        .await
        .map_err(|_| "episode journal cannot be read")?;
    let mut results = Vec::new();
    let mut cursor = after;
    let mut scanned = 0;
    for event in &page {
        if event.sequence > last {
            break;
        }
        cursor = event.sequence;
        scanned += 1;
        // Sanitize the COMPLETE event before matching or truncating. In particular a
        // secret spanning the snippet boundary cannot escape as a visible prefix.
        let safe=tools.sanitize_durable(&json!({"sequence":event.sequence,"kind":event.kind,"key":event.key,"payload":event.payload})).map_err(|_|"episode history sanitization failed")?;
        let text = serde_json::to_string(&safe).map_err(|_| "episode history encoding failed")?;
        if text.to_lowercase().contains(&query) {
            results.push(json!({"sequence":event.sequence,"kind":safe["kind"],"key":safe["key"],"snippet":snippet(&text)}));
            if results.len() == limit {
                break;
            }
        }
    }
    let more = cursor < last
        && (scanned < page.len() || page.len() == PAGE)
        && page.get(scanned).is_none_or(|event| event.sequence <= last);
    Ok(
        json!({"results":results,"next_cursor":more.then_some(cursor),"scanned":scanned,"scope":"current_run","from_sequence":first,"to_sequence":args["to_sequence"]}),
    )
}
struct Observation {
    sequence: u64,
    key: String,
    name: String,
}
async fn pin(journal: &Journal, run: &str, args: &Value) -> Result<Value, String> {
    let call_id = args["call_id"].as_str().ok_or("call_id required")?;
    let selected = args["source_sequence"].as_u64();
    let mut cursor = 0;
    let mut intents = BTreeMap::<String, String>::new();
    let mut observations = Vec::new();
    loop {
        let page = journal
            .entries(run, cursor, PAGE as u32)
            .await
            .map_err(|_| "episode journal cannot be read")?;
        if page.is_empty() {
            break;
        }
        for event in &page {
            cursor = event.sequence;
            if event.kind == "tool_intent"
                && event
                    .payload
                    .pointer("/prepared/call/id")
                    .and_then(Value::as_str)
                    == Some(call_id)
            {
                let name = event
                    .payload
                    .pointer("/prepared/call/name")
                    .and_then(Value::as_str)
                    .ok_or("confirmed intention has no tool name")?;
                intents.insert(format!("{}:result", event.key), name.into());
            } else if event.kind == "tool_result"
                && selected.is_none_or(|sequence| sequence == event.sequence)
                && let Some(name) = intents.get(&event.key)
                && event.payload["name"].as_str() == Some(name)
            {
                serde_json::from_value::<ToolResult>(event.payload["result"].clone())
                    .map_err(|_| "invalid confirmed observation")?;
                observations.push(Observation {
                    sequence: event.sequence,
                    key: event.key.clone(),
                    name: name.clone(),
                });
            }
        }
    }
    if observations.len() != 1 {
        return Err(if observations.is_empty() {
            "confirmed observation not found in this run"
        } else {
            "call_id is ambiguous; select its confirmed source_sequence"
        }
        .into());
    }
    let source = observations.pop().unwrap();
    Ok(
        json!({"id":call_id,"call_id":call_id,"source_sequence":source.sequence,"source_key":source.key,"name":source.name,"note":args["note"].as_str().unwrap_or(""),"pinned":args["pinned"].as_bool().unwrap_or(true)}),
    )
}
async fn ack(journal: &Journal, run: &str, args: &Value) -> Result<Value, String> {
    let id = args["id"].as_str().ok_or("directive id required")?;
    let mut cursor = 0;
    loop {
        let page = journal
            .entries(run, cursor, PAGE as u32)
            .await
            .map_err(|_| "episode journal cannot be read")?;
        if page.is_empty() {
            return Err("directive not found in this run".into());
        }
        for event in &page {
            cursor = event.sequence;
            if matches!(event.kind.as_str(), "directive_received" | "directive")
                && event.payload["id"].as_str() == Some(id)
            {
                return Ok(json!({"id":id,"acknowledged":true,"source_sequence":event.sequence}));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use excalibur_store::{Database, DatabaseKind, StorageConfig};
    use excalibur_types::runtime::{BoxFuture, Reconcile};
    struct Sanitizer;
    impl ToolRuntime for Sanitizer {
        fn definitions(&self) -> Vec<ToolDefinition> {
            Vec::new()
        }
        fn sanitize_durable(&self, value: &Value) -> Result<Value, String> {
            serde_json::from_str(
                &value
                    .to_string()
                    .replace("SENSITIVE-FIXTURE-VALUE", "[REDACTED]"),
            )
            .map_err(|_| "synthetic encoding failure".into())
        }
        fn prepare<'a>(
            &'a self,
            _: &'a str,
            _: ToolCall,
        ) -> BoxFuture<'a, Result<PreparedTool, String>> {
            Box::pin(async { Err("unused".into()) })
        }
        fn execute<'a>(
            &'a self,
            _: &'a str,
            _: &'a str,
            _: &'a PreparedTool,
        ) -> BoxFuture<'a, ToolResult> {
            Box::pin(async { ToolResult::error("unused") })
        }
        fn reconcile<'a>(
            &'a self,
            _: &'a str,
            _: &'a str,
            _: &'a PreparedTool,
        ) -> BoxFuture<'a, Reconcile> {
            Box::pin(async { Reconcile::Retry })
        }
    }
    fn call(name: &str, args: Value) -> PreparedTool {
        prepare(ToolCall {
            id: "utility-call".into(),
            name: name.into(),
            arguments: args.to_string(),
        })
        .unwrap()
    }
    async fn fixture() -> (tempfile::TempDir, Database, Journal, String) {
        let temp = tempfile::tempdir().unwrap();
        let db = Database::open(
            temp.path().join("main.db"),
            DatabaseKind::Main,
            StorageConfig::default(),
        )
        .unwrap();
        let journal = Journal::new(db.clone()).await.unwrap();
        let run = journal
            .create_run("utility-fixture", json!({}))
            .await
            .unwrap();
        (temp, db, journal, run)
    }
    #[tokio::test]
    async fn history_is_run_local_paged_and_sanitized_before_snippets() {
        let (_temp, db, journal, run) = fixture().await;
        for index in 0..20 {
            let empty=json!({"sequence":index+2,"kind":"fixture","key":format!("fixture:{index}"),"payload":{"fixture":"","index":index}}).to_string();
            let start = empty.find("\"fixture\":\"\"").unwrap() + "\"fixture\":\"".len();
            let filler = "x".repeat(SNIPPET_BYTES - "…".len() - start - 4);
            let payload = json!({"fixture":filler+"SENSITIVE-FIXTURE-VALUE","index":index});
            let raw=json!({"sequence":index+2,"kind":"fixture","key":format!("fixture:{index}"),"payload":payload}).to_string();
            assert!(
                snippet(&raw).contains("SENS"),
                "fixture must expose a prefix if truncation happens first"
            );
            journal
                .append(&run, "fixture", &format!("fixture:{index}"), payload)
                .await
                .unwrap();
        }
        let other = journal.create_run("other-run", json!({})).await.unwrap();
        journal
            .append(
                &other,
                "fixture",
                "other-only",
                json!({"fixture":"other-only"}),
            )
            .await
            .unwrap();
        let result = execute(
            &journal,
            &Sanitizer,
            &run,
            "search",
            &call("history_search", json!({"query":"fixture","limit":2})),
        )
        .await;
        assert!(result.success);
        let alias = execute(
            &journal,
            &Sanitizer,
            &run,
            "alias-search",
            &call("run_history_search", json!({"query":"fixture","limit":2})),
        )
        .await;
        assert_eq!(alias, result);
        assert_eq!(result.output["results"].as_array().unwrap().len(), 2);
        assert!(result.output["scanned"].as_u64().unwrap() <= 16);
        let text = result.output.to_string();
        assert!(!text.contains("SENS"));
        assert!(
            result.output["results"]
                .as_array()
                .unwrap()
                .iter()
                .all(|row| row["snippet"].as_str().unwrap().len() <= SNIPPET_BYTES)
        );
        assert!(!text.contains("other-only"));
        let next = result.output["next_cursor"].as_u64().unwrap();
        let second = execute(
            &journal,
            &Sanitizer,
            &run,
            "next",
            &call(
                "history_search",
                json!({"query":"fixture","cursor":next,"limit":16}),
            ),
        )
        .await;
        assert!(
            second.output["results"]
                .as_array()
                .unwrap()
                .iter()
                .all(|row| row["sequence"].as_u64().unwrap() > next)
        );
        db.shutdown().await.unwrap();
    }
    #[tokio::test]
    async fn history_recovers_an_exact_discarded_range_without_another_runs_events() {
        let (_temp, db, journal, run) = fixture().await;
        for index in 0..40 {
            journal
                .append(
                    &run,
                    "discarded",
                    &format!("key-{index}"),
                    json!({"index":index}),
                )
                .await
                .unwrap();
        }
        let other = journal
            .create_run("foreign-history", json!({}))
            .await
            .unwrap();
        journal
            .append(&other, "discarded", "foreign", json!({"index":"foreign"}))
            .await
            .unwrap();
        let selected = execute(
            &journal,
            &Sanitizer,
            &run,
            "range",
            &call(
                "history_search",
                json!({"from_sequence":12,"to_sequence":14}),
            ),
        )
        .await;
        assert!(selected.success);
        assert_eq!(
            selected.output["results"]
                .as_array()
                .unwrap()
                .iter()
                .map(|row| row["sequence"].as_u64().unwrap())
                .collect::<Vec<_>>(),
            vec![12, 13, 14]
        );
        assert!(selected.output["next_cursor"].is_null());
        assert!(!selected.output.to_string().contains("foreign"));
        assert!(
            prepare(ToolCall {
                id: "invalid".into(),
                name: "history_search".into(),
                arguments: json!({"from_sequence":14,"to_sequence":12}).to_string()
            })
            .is_err()
        );
        db.shutdown().await.unwrap();
    }
    #[tokio::test]
    async fn pin_requires_matching_confirmed_intent_result_and_ack_requires_existing_run_directive()
    {
        let (_temp, db, journal, run) = fixture().await;
        let tool = PreparedTool {
            call: ToolCall {
                id: "source-call".into(),
                name: "read_file".into(),
                arguments: "{}".into(),
            },
            effect: Effect::Pure,
            resources: vec![],
            exclusive: false,
            data: Value::Null,
        };
        journal
            .append(
                &run,
                "tool_intent",
                "source",
                json!({"index":0,"prepared":tool}),
            )
            .await
            .unwrap();
        assert!(
            !execute(
                &journal,
                &Sanitizer,
                &run,
                "before",
                &call("pin_observation", json!({"call_id":"source-call"}))
            )
            .await
            .success
        );
        let sequence=journal.append(&run,"tool_result","source:result",json!({"index":0,"name":"read_file","result":ToolResult::ok(json!({"text":"synthetic"}))})).await.unwrap();
        let pinned = execute(
            &journal,
            &Sanitizer,
            &run,
            "pin",
            &call(
                "pin_observation",
                json!({"call_id":"source-call","note":"synthetic note"}),
            ),
        )
        .await;
        assert!(pinned.success);
        assert_eq!(pinned.output["source_sequence"], sequence);
        assert_eq!(pinned.output["pinned"], true);
        let unpinned = execute(
            &journal,
            &Sanitizer,
            &run,
            "unpin",
            &call(
                "pin_observation",
                json!({"call_id":"source-call","pinned":false}),
            ),
        )
        .await;
        assert_eq!(unpinned.output["pinned"], false);
        assert!(
            !execute(
                &journal,
                &Sanitizer,
                &run,
                "absent",
                &call("ack_directive", json!({"id":"absent"}))
            )
            .await
            .success
        );
        journal
            .append(
                &run,
                "directive_received",
                "directive:1",
                json!({"id":"directive-1","text":"synthetic"}),
            )
            .await
            .unwrap();
        assert!(
            execute(
                &journal,
                &Sanitizer,
                &run,
                "ack",
                &call("ack_directive", json!({"id":"directive-1"}))
            )
            .await
            .success
        );
        db.shutdown().await.unwrap();
    }
    #[test]
    fn utility_argument_schemas_reject_scope_escape_and_unbounded_requests() {
        for (name, args) in [
            ("history_search", json!({"query":"x","limit":17})),
            ("history_search", json!({"query":"x","run":"other"})),
            ("pin_observation", json!({"call_id":"x","pinned":"yes"})),
            ("ack_directive", json!({"id":""})),
        ] {
            assert!(
                prepare(ToolCall {
                    id: "invalid".into(),
                    name: name.into(),
                    arguments: args.to_string()
                })
                .is_err()
            );
        }
        assert!(is_builtin("history_search"));
        assert!(!is_builtin("memory_search"));
    }
}
