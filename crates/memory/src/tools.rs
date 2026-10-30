//! Model-facing tool contracts. The host binds each call to a `MemoryGrant`; arguments can
//! narrow scopes but never widen them, and trust is capped by the grant.
use crate::{
    DocEdit, DocName, DocSelector, EditContext, Error, HistoryQuery, IngestRequest, MemoryGrant,
    MemoryInput, MemoryStore, Result, RetrievalQuery, invalid,
};
use excalibur_types::runtime::ToolDefinition;
use serde_json::{Value, json};

pub const MEMORY_TOOLS: &[&str] = &[
    "memory_write",
    "memory_propose",
    "memory_search",
    "history_search",
    "doc_read",
    "doc_edit",
    "library_ingest",
    "library_search",
];

pub fn memory_tool_definitions() -> Vec<ToolDefinition> {
    let input = json!({"type":"object","required":["scope","kind","statement"],"properties":{"scope":{"type":"string"},"kind":{"enum":["fact","preference","event","commitment","belief","relation","procedure"]},"statement":{"type":"string","maxLength":8192},"entities":{"type":"array","items":{"type":"string"},"maxItems":16},"attribute":{"type":"string"},"valid_from":{"type":"integer"},"valid_to":{"type":"integer"},"expires_at":{"type":"integer"},"confidence":{"type":"number"},"importance":{"type":"number"},"sources":{"type":"array","maxItems":16,"items":{"type":"object","required":["kind","id"],"properties":{"kind":{"enum":["message","tool_call","document","page","memory"]},"id":{"type":"string"},"context":{"type":"string"}}}},"temporal_change":{"type":"boolean"}},"additionalProperties":false});
    let def = |name: &str, description: &str, schema: Value| ToolDefinition {
        name: name.into(),
        description: description.into(),
        input_schema: schema,
    };
    vec![
        def(
            "memory_write",
            "Store an explicit user memory now; admission merges duplicates, supersedes refinements and flags contradictions.",
            input.clone(),
        ),
        def(
            "memory_propose",
            "Propose a candidate fact with its source; it is only visible in this conversation or task until consolidated.",
            input,
        ),
        def(
            "memory_search",
            "Search memories and past messages in granted scopes; returns labeled items, validity, sources and notices.",
            json!({"type":"object","required":["query"],"properties":{"query":{"type":"string","maxLength":4096},"scopes":{"type":"array","items":{"type":"string"}},"kinds":{"type":"array","items":{"type":"string"}},"k":{"type":"integer","minimum":1,"maximum":50},"token_budget":{"type":"integer","minimum":64,"maximum":16000},"historical":{"type":"boolean"}},"additionalProperties":false}),
        ),
        def(
            "history_search",
            "Search past messages by text, dates, conversation and role inside granted scopes.",
            json!({"type":"object","properties":{"text":{"type":"string"},"scopes":{"type":"array","items":{"type":"string"}},"from":{"type":"integer"},"to":{"type":"integer"},"conversation":{"type":"string"},"role":{"type":"string"},"cursor":{"type":"integer"},"limit":{"type":"integer","minimum":1,"maximum":20}},"additionalProperties":false}),
        ),
        def(
            "doc_read",
            "Read the heading index, one section or matching fragments of agent_profile, user_profile, notebook or persona:<id>.",
            json!({"type":"object","required":["doc"],"properties":{"doc":{"type":"string"},"section":{"type":"array","items":{"type":"string"}},"search":{"type":"string"}},"additionalProperties":false}),
        ),
        def(
            "doc_edit",
            "Replace, insert after or delete a fragment by id and expected hash, or append to a section. A hash mismatch returns the current fragment.",
            json!({"type":"object","required":["doc","op"],"properties":{"doc":{"type":"string"},"op":{"enum":["replace","insert_after","append","delete"]},"fragment_id":{"type":"string"},"expected_hash":{"type":"string"},"heading_path":{"type":"array","items":{"type":"string"}},"scope":{"type":"string"},"text":{"type":"string"},"provenance":{"type":"string"}},"additionalProperties":false}),
        ),
        def(
            "library_ingest",
            "Add a text document (or a new version) to the private library of a scope.",
            json!({"type":"object","required":["scope","title","text"],"properties":{"scope":{"type":"string"},"title":{"type":"string"},"text":{"type":"string"},"media_type":{"enum":["text/plain","text/markdown"]},"document_id":{"type":"string"},"library":{"type":"boolean"}},"additionalProperties":false}),
        ),
        def(
            "library_search",
            "Search library documents; returns citable chunks. Cite with the returned [[doc:ID#cN]] token, never with page numbers.",
            json!({"type":"object","required":["query"],"properties":{"query":{"type":"string"},"k":{"type":"integer","minimum":1,"maximum":20}},"additionalProperties":false}),
        ),
    ]
}

fn str_field<'a>(args: &'a Value, name: &str) -> Result<&'a str> {
    args[name]
        .as_str()
        .ok_or_else(|| invalid(format!("{name} required")))
}

/// Dispatch one model tool call. `explicit_user_request` is supplied by the host (never by the model).
pub async fn call_memory_tool(
    store: &MemoryStore,
    grant: &MemoryGrant,
    name: &str,
    args: Value,
    explicit_user_request: bool,
    now: i64,
) -> Result<Value> {
    match name {
        "memory_write" | "memory_propose" => {
            let input: MemoryInput =
                serde_json::from_value(args).map_err(|e| invalid(e.to_string()))?;
            let mut input = input;
            input.trust = grant.max_trust;
            if name == "memory_write" {
                Ok(serde_json::to_value(
                    store.memory_write(grant, input, now).await?,
                )?)
            } else {
                Ok(json!({"candidate": store.memory_propose(grant, input, now).await?}))
            }
        }
        "memory_search" => {
            let mut query = RetrievalQuery::new(str_field(&args, "query")?);
            if let Some(scopes) = args["scopes"].as_array() {
                query.scopes = scopes
                    .iter()
                    .filter_map(|s| s.as_str().map(str::to_string))
                    .collect();
            }
            if let Some(kinds) = args["kinds"].as_array() {
                query.kinds = kinds
                    .iter()
                    .filter_map(|k| k.as_str())
                    .map(crate::MemoryKind::parse)
                    .collect::<Result<_>>()?;
            }
            if let Some(k) = args["k"].as_u64() {
                query.k = k as usize;
            }
            if let Some(budget) = args["token_budget"].as_u64() {
                query.token_budget = budget as usize;
            }
            if args["historical"].as_bool() == Some(true) {
                query.time_mode = Some(crate::TimeMode::Historical);
            }
            query.allow_rerank = true;
            let result = store.retrieve(grant, query, now).await?;
            Ok(
                json!({"packed": result.packed, "notices": result.notices, "items": result.items.iter().map(|i| json!({"id": i.id, "kind": i.kind, "scope": i.scope, "text": i.text, "status": i.status, "valid_from": i.valid_from, "valid_to": i.valid_to, "sources": i.sources})).collect::<Vec<_>>()}),
            )
        }
        "history_search" => {
            let query: HistoryQuery =
                serde_json::from_value(args).map_err(|e| invalid(e.to_string()))?;
            Ok(serde_json::to_value(
                store.history_search(grant, query).await?,
            )?)
        }
        "doc_read" => {
            let doc = DocName::parse(str_field(&args, "doc")?)?;
            let selector = if let Some(search) = args["search"].as_str() {
                DocSelector::Search(search.into())
            } else if let Some(section) = args["section"].as_array() {
                DocSelector::Section(
                    section
                        .iter()
                        .filter_map(|s| s.as_str().map(str::to_string))
                        .collect(),
                )
            } else {
                DocSelector::Index
            };
            Ok(serde_json::to_value(
                store.doc_read(grant, &doc, selector).await?,
            )?)
        }
        "doc_edit" => {
            let doc = DocName::parse(str_field(&args, "doc")?)?;
            let text = || str_field(&args, "text").map(str::to_string);
            let fragment = || str_field(&args, "fragment_id").map(str::to_string);
            let hash = || str_field(&args, "expected_hash").map(str::to_string);
            let edit = match str_field(&args, "op")? {
                "replace" => DocEdit::Replace {
                    fragment_id: fragment()?,
                    expected_hash: hash()?,
                    text: text()?,
                },
                "insert_after" => DocEdit::InsertAfter {
                    fragment_id: fragment()?,
                    expected_hash: hash()?,
                    text: text()?,
                },
                "delete" => DocEdit::Delete {
                    fragment_id: fragment()?,
                    expected_hash: hash()?,
                },
                "append" => DocEdit::Append {
                    heading_path: args["heading_path"]
                        .as_array()
                        .map(|a| {
                            a.iter()
                                .filter_map(|s| s.as_str().map(str::to_string))
                                .collect()
                        })
                        .unwrap_or_default(),
                    scope: args["scope"].as_str().map(str::to_string),
                    text: text()?,
                },
                other => return Err(invalid(format!("unknown op {other}"))),
            };
            let actor = grant
                .run_id
                .clone()
                .ok_or_else(|| invalid("doc_edit requires a run"))?;
            let ctx = EditContext {
                actor,
                provenance: args["provenance"].as_str().map(str::to_string),
                explicit_user_request,
            };
            Ok(serde_json::to_value(
                store.doc_edit(grant, &doc, edit, ctx, now).await?,
            )?)
        }
        "library_ingest" => {
            let request = IngestRequest {
                scope: str_field(&args, "scope")?.into(),
                title: str_field(&args, "title")?.into(),
                media_type: args["media_type"].as_str().unwrap_or("text/plain").into(),
                origin: Some("tool".into()),
                bytes: str_field(&args, "text")?.as_bytes().to_vec(),
                document_id: args["document_id"].as_str().map(str::to_string),
                library: args["library"].as_bool().unwrap_or(false),
            };
            Ok(serde_json::to_value(
                store.library_ingest(grant, request, now).await?,
            )?)
        }
        "library_search" => {
            let k = args["k"].as_u64().unwrap_or(8) as usize;
            let (items, notices) = store
                .library_search(grant, str_field(&args, "query")?, k.min(20), now)
                .await?;
            Ok(
                json!({"notices": notices, "chunks": items.iter().map(|i| json!({"cite": i.cite, "location": i.location, "text": i.text, "scope": i.scope})).collect::<Vec<_>>()}),
            )
        }
        other => Err(Error::Unsupported(format!("{other} is not a memory tool"))),
    }
}
