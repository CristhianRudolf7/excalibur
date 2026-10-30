//! `memory.*` client methods over the shared memory store: query, correct and inspect
//! provenance. Every scope a client names passes the same scope check as conversations.
//! The client acts for the user, so writes carry user trust and profile edits count as
//! explicit user requests.
use crate::{RpcError, backend::SharedBackend};
use excalibur_memory::{BindingRunTree, MemoryGrant, MemoryStore, Trust, call_memory_tool};
use serde_json::{Value, json};

#[derive(Clone)]
pub struct MemoryAccess {
    pub store: MemoryStore,
    pub principal: String,
    pub timezone: excalibur_memory::chrono_tz::Tz,
}

fn memory_error(error: excalibur_memory::Error) -> RpcError {
    match error {
        excalibur_memory::Error::Scope(_) => RpcError::invalid("memory scope denied"),
        excalibur_memory::Error::NotFound(_) => RpcError::invalid("unknown memory record"),
        excalibur_memory::Error::Invalid(message) => RpcError::invalid(message),
        excalibur_memory::Error::Unsupported(message) => RpcError::invalid(message),
        excalibur_memory::Error::Rejected(message) => RpcError::invalid(message),
        _ => RpcError::internal(),
    }
}

impl SharedBackend {
    async fn memory_grant(
        &self,
        access: &MemoryAccess,
        params: &Value,
    ) -> Result<MemoryGrant, RpcError> {
        let mut scopes: Vec<String> = match (params.get("scopes"), params.get("scope")) {
            (Some(Value::Array(items)), _) => items
                .iter()
                .map(|v| {
                    v.as_str()
                        .map(str::to_owned)
                        .ok_or_else(|| RpcError::invalid("scopes must be strings"))
                })
                .collect::<Result<_, _>>()?,
            (_, Some(Value::String(scope))) => vec![scope.clone()],
            _ => vec!["general".to_owned()],
        };
        scopes.sort();
        scopes.dedup();
        if scopes.is_empty() || scopes.len() > 16 {
            return Err(RpcError::invalid("between 1 and 16 scopes required"));
        }
        for scope in &scopes {
            self.validate_scope(scope).await?;
        }
        Ok(MemoryGrant::new(access.principal.clone(), scopes)
            .with_timezone(access.timezone)
            .with_max_trust(Trust::User)
            .with_run(format!("client:{}", access.principal)))
    }

    pub(crate) async fn memory_method(
        &self,
        method: &str,
        params: Value,
    ) -> Result<Value, RpcError> {
        let access = self.memory.clone().ok_or_else(RpcError::not_found)?;
        let store = &access.store;
        let grant = self.memory_grant(&access, &params).await?;
        let now = excalibur_store::unix_seconds();
        let mut args = params.clone();
        if let Some(object) = args.as_object_mut() {
            if !matches!(
                method,
                "memory.write" | "memory.library_ingest" | "memory.doc_edit"
            ) {
                object.remove("scope");
            }
            if method != "memory.search" && method != "memory.history" {
                object.remove("scopes");
            }
        }
        let tool = |name: &'static str| (name, args.clone());
        let (name, args) = match method {
            "memory.search" => tool("memory_search"),
            "memory.history" => tool("history_search"),
            "memory.write" => tool("memory_write"),
            "memory.doc_read" => tool("doc_read"),
            "memory.doc_edit" => tool("doc_edit"),
            "memory.library_ingest" => tool("library_ingest"),
            "memory.library_search" => tool("library_search"),
            "memory.get" => {
                let id = crate::backend::text(&params, "id")?;
                let memory = store
                    .memory(&grant, id)
                    .await
                    .map_err(memory_error)?
                    .ok_or_else(|| RpcError::invalid("unknown memory record"))?;
                let transitions = store.transitions(&grant, id).await.map_err(memory_error)?;
                let admissions = store.admissions(&grant, id).await.map_err(memory_error)?;
                return Ok(
                    json!({"memory": memory, "transitions": transitions.into_iter().map(|(from, to, reason, at)| json!({"from": from, "to": to, "reason": reason, "at": at})).collect::<Vec<_>>(), "admissions": admissions}),
                );
            }
            "memory.purge" => {
                if params.get("confirmed").and_then(Value::as_bool) != Some(true) {
                    return Err(RpcError::invalid(
                        "purging memory content requires confirmed=true",
                    ));
                }
                store
                    .purge_memory(&grant, crate::backend::text(&params, "id")?, now)
                    .await
                    .map_err(memory_error)?;
                return Ok(json!({"purged": true}));
            }
            "memory.questions" => {
                return Ok(json!(
                    store.open_questions(&grant).await.map_err(memory_error)?
                ));
            }
            "memory.proposals" => {
                return Ok(json!(
                    store
                        .profile_proposals(&grant)
                        .await
                        .map_err(memory_error)?
                ));
            }
            "memory.confirm_proposal" => {
                let applied = store
                    .confirm_profile_proposals(
                        &grant,
                        Some(crate::backend::text(&params, "id")?),
                        now,
                    )
                    .await
                    .map_err(memory_error)?;
                return Ok(json!({"applied": applied}));
            }
            "memory.relation_types" => {
                return Ok(json!(
                    store.relation_types(&grant).await.map_err(memory_error)?
                ));
            }
            "memory.confirm_relation_type" => {
                let confirmed = store
                    .confirm_relation_type(&grant, crate::backend::text(&params, "name")?, now)
                    .await
                    .map_err(memory_error)?;
                return Ok(json!({"confirmed": confirmed}));
            }
            "memory.merge_relation_type" => {
                store
                    .merge_relation_type(
                        &grant,
                        crate::backend::text(&params, "from")?,
                        crate::backend::text(&params, "into")?,
                        now,
                    )
                    .await
                    .map_err(memory_error)?;
                return Ok(json!({"merged": true}));
            }
            "memory.reviews" => {
                return Ok(json!(
                    store
                        .recent_reviews(&grant, crate::backend::limit(&params, 5, 50)? as usize)
                        .await
                        .map_err(memory_error)?
                ));
            }
            "memory.citations" => {
                let conversation = crate::backend::text(&params, "conversation")?;
                let answer = crate::backend::text(&params, "text")?;
                let tree = BindingRunTree {
                    store: store.clone(),
                };
                let cited = store
                    .validate_citations(
                        &grant,
                        &format!(
                            "master:{conversation}:{}",
                            grant
                                .scopes
                                .iter()
                                .find(|s| s.as_str() != "general")
                                .map(String::as_str)
                                .unwrap_or("general")
                        ),
                        &tree,
                        answer,
                        params.get("locale").and_then(Value::as_str).unwrap_or("en"),
                    )
                    .await
                    .map_err(memory_error)?;
                return Ok(json!(cited));
            }
            _ => return Err(RpcError::not_found()),
        };
        call_memory_tool(store, &grant, name, args, true, now)
            .await
            .map_err(memory_error)
    }
}
