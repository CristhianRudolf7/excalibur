//! Agenda user surface: the master's `reminders` fast tool and the
//! authenticated `agenda.reminders` RPC share this one core (13.2, 21.1–21.2).
//!
//! Authority never comes from model or client JSON. Every call is bound to the
//! host-authenticated principal plus the conversation, original scope and
//! accepted source message of the request; the orchestrator revalidates that
//! source and the master's scope ceiling inside each writer. The reminder
//! namespace is sealed from principal, scope and conversation, so another
//! scope or conversation cannot see or mutate these reminders. Calls without
//! an accepted source are refused; there is no fallback namespace.
use crate::agenda::{Agenda, Error as AgendaError};
use excalibur_master::FastActions;
use excalibur_orchestrator::{HostNotificationBinding, Orchestrator};
use excalibur_scheduler::{
    Timing,
    reminders::{Definition, Operation, Reminder},
};
use excalibur_store::{ReadLane, rusqlite::OptionalExtension};
use excalibur_types::runtime::{BoxFuture, RunContext, ToolDefinition};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::sync::Arc;

/// Model-facing tool name (13.2 "reminders").
pub const REMINDERS_TOOL: &str = "reminders";
const LIST_DEFAULT: u32 = 50;
const LIST_MAX: u32 = 100;

/// Shared Agenda tool core. Hosts construct it with the principal they
/// authenticated and the delivery routes they configured.
pub struct AgendaTools {
    orchestrator: Orchestrator,
    principal: String,
    approved_channels: Vec<String>,
}

fn timing_schema() -> Value {
    json!({
        "type":"object",
        "description":"Exactly one of at, every, local_at or cron.",
        "properties":{
            "at":{"type":"integer","minimum":0,"description":"One-shot UTC Unix seconds."},
            "every":{"type":"object","properties":{"first":{"type":"integer","minimum":0},"seconds":{"type":"integer","minimum":1}},"required":["first","seconds"],"additionalProperties":false},
            "local_at":{"type":"object","properties":{"local":{"type":"string","description":"Civil time YYYY-MM-DDTHH:MM[:SS]"},"timezone":{"type":"string","description":"IANA zone; defaults to the user's zone of the request."}},"required":["local"],"additionalProperties":false},
            "cron":{"type":"object","properties":{"expression":{"type":"string","description":"Numeric 5- or 6-field cron in the user's zone."},"timezone":{"type":"string"},"not_before":{"type":"integer","minimum":0}},"required":["expression"],"additionalProperties":false}
        },
        "additionalProperties":false,
        "minProperties":1,
        "maxProperties":1
    })
}

/// The single `reminders` definition shown to the master model.
pub fn reminders_definition() -> ToolDefinition {
    ToolDefinition {
        name: REMINDERS_TOOL.into(),
        description: "Create, list, snooze, complete or delete the user's reminders in this conversation and scope, or recompute local-time reminders after the user changes time zone. Mutations of an existing reminder need its current generation from list or create.".into(),
        input_schema: json!({
            "type":"object",
            "properties":{
                "action":{"type":"string","enum":["create","list","snooze","complete","delete","change_timezone"]},
                "id":{"type":"string","maxLength":256},
                "generation":{"type":"integer","minimum":1},
                "text":{"type":"string","maxLength":16384},
                "timing":timing_schema(),
                "preferred_channel":{"type":"string","maxLength":512},
                "task":{"type":"string","maxLength":512},
                "timezone":{"type":"string","maxLength":256},
                "after":{"type":"string","maxLength":256},
                "limit":{"type":"integer","minimum":1,"maximum":LIST_MAX}
            },
            "required":["action"],
            "additionalProperties":false
        }),
    }
}

fn hashed(label: &str, key: &str) -> String {
    let digest =
        Sha256::digest(serde_json::to_vec(&("agenda-tool-v1", label, key)).unwrap_or_default());
    format!("{label}_{digest:x}")
}
fn string<'a>(args: &'a Value, name: &str) -> Result<&'a str, String> {
    args.get(name)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| format!("{name} is required"))
}
fn optional(args: &Value, name: &str) -> Result<Option<String>, String> {
    match args.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) if !s.is_empty() => Ok(Some(s.clone())),
        _ => Err(format!("{name} must be a nonempty string")),
    }
}
fn generation(args: &Value) -> Result<i64, String> {
    args.get("generation")
        .and_then(Value::as_i64)
        .filter(|g| *g >= 1)
        .ok_or_else(|| "generation is required".into())
}
/// Errors carry fixed messages only: reminder text never reaches diagnostics.
fn describe(error: AgendaError) -> String {
    match error {
        AgendaError::Store(excalibur_store::Error::Invalid(message)) => message.into(),
        AgendaError::Store(excalibur_store::Error::PrincipalOnly) => {
            "reminders need the principal database".into()
        }
        AgendaError::Orchestrator(excalibur_orchestrator::Error::Storage(
            excalibur_store::Error::Invalid(message),
        )) => message.into(),
        AgendaError::Orchestrator(excalibur_orchestrator::Error::Invalid(_)) => {
            "reminder request rejected".into()
        }
        _ => "agenda storage unavailable; retry with the same operation key".into(),
    }
}

impl AgendaTools {
    pub fn new(
        orchestrator: Orchestrator,
        principal: String,
        approved_channels: Vec<String>,
    ) -> Result<Self, String> {
        if principal.is_empty() || principal.len() > 512 || approved_channels.len() > 32 {
            return Err("invalid agenda tool principal or routes".into());
        }
        Ok(Self {
            orchestrator,
            principal,
            approved_channels,
        })
    }
    /// The user's zone recorded with the accepted source message; civil
    /// timings without an explicit zone are expressed in it.
    async fn source_timezone(&self, conversation: &str, source: &str) -> Result<String, String> {
        let (conversation, source) = (conversation.to_owned(), source.to_owned());
        self.orchestrator
            .database()
            .read(ReadLane::Interactive, move |c| {
                Ok(c.query_row(
                    "SELECT user_timezone FROM messages WHERE id=?1 AND conversation_id=?2",
                    [source, conversation],
                    |r| r.get::<_, Option<String>>(0),
                )
                .optional()?
                .flatten())
            })
            .await
            .map_err(|_| "agenda storage unavailable; retry with the same operation key")?
            .filter(|zone| !zone.is_empty())
            .ok_or_else(|| "reminder scope or source is not authorized".into())
    }
    async fn timing(
        &self,
        value: &Value,
        conversation: &str,
        source: &str,
        now: i64,
    ) -> Result<Timing, String> {
        let object = value
            .as_object()
            .filter(|o| o.len() == 1)
            .ok_or("timing needs exactly one of at, every, local_at or cron")?;
        let (kind, body) = object.iter().next().ok_or("timing is required")?;
        let zone = |body: &Value| optional(body, "timezone");
        Ok(match kind.as_str() {
            "at" => Timing::At(body.as_i64().filter(|t| *t >= 0).ok_or("invalid at")?),
            "every" => Timing::Every {
                first: body
                    .get("first")
                    .and_then(Value::as_i64)
                    .filter(|t| *t >= 0)
                    .ok_or("invalid every.first")?,
                seconds: body
                    .get("seconds")
                    .and_then(Value::as_u64)
                    .and_then(|s| u32::try_from(s).ok())
                    .filter(|s| *s > 0)
                    .ok_or("invalid every.seconds")?,
            },
            "local_at" => Timing::LocalAt {
                local: string(body, "local")?.into(),
                timezone: match zone(body)? {
                    Some(zone) => zone,
                    None => self.source_timezone(conversation, source).await?,
                },
            },
            "cron" => Timing::Cron {
                expression: string(body, "expression")?.into(),
                timezone: match zone(body)? {
                    Some(zone) => zone,
                    None => self.source_timezone(conversation, source).await?,
                },
                not_before: match body.get("not_before") {
                    None | Some(Value::Null) => now,
                    Some(v) => v
                        .as_i64()
                        .filter(|t| *t >= 0)
                        .ok_or("invalid cron.not_before")?,
                },
            },
            _ => return Err("timing needs exactly one of at, every, local_at or cron".into()),
        })
    }
    async fn snapshot(&self, agenda: &Agenda, reminder: Reminder) -> Result<Value, String> {
        let next = agenda
            .next_fire(&reminder)
            .await
            .map_err(|_| "agenda storage unavailable; retry with the same operation key")?;
        Ok(json!({
            "id":reminder.id,
            "generation":reminder.generation,
            "state":reminder.state,
            "text":reminder.text,
            "timing":reminder.timing,
            "preferred_channel":reminder.preferred_channel,
            "task":reminder.task,
            "next_fire_utc":next,
        }))
    }
    /// One authorized call. `key` is the caller's durable operation key; an
    /// exact retry with the same key and arguments returns the original
    /// snapshot, and the same key with different arguments is refused.
    pub async fn call(
        &self,
        conversation: &str,
        scope: &str,
        source: &str,
        args: Value,
        key: &str,
    ) -> Result<Value, String> {
        self.call_at(
            conversation,
            scope,
            source,
            args,
            key,
            excalibur_store::unix_seconds(),
        )
        .await
    }
    /// As `call`, with an explicit host clock for civil recomputation.
    pub async fn call_at(
        &self,
        conversation: &str,
        scope: &str,
        source: &str,
        args: Value,
        key: &str,
        now: i64,
    ) -> Result<Value, String> {
        if key.is_empty() || key.len() > 1024 {
            return Err("an operation key is required".into());
        }
        let binding = HostNotificationBinding {
            principal: self.principal.clone(),
            scope: scope.into(),
            conversation: conversation.into(),
            source: source.into(),
            approved_channels: self.approved_channels.clone(),
        };
        let agenda = Agenda::open(self.orchestrator.clone(), binding)
            .await
            .map_err(|_| "reminder scope or source is not authorized")?;
        let action = string(&args, "action")?;
        let operation_key = hashed("op", key);
        let operation = match action {
            "list" => {
                let limit = match args.get("limit") {
                    None => LIST_DEFAULT,
                    Some(v) => v
                        .as_u64()
                        .filter(|n| (1..=LIST_MAX as u64).contains(n))
                        .ok_or("invalid limit")? as u32,
                };
                let after = optional(&args, "after")?;
                let page = agenda
                    .list(operation_key, after, limit)
                    .await
                    .map_err(describe)?;
                let full = page.len() == limit as usize;
                let next_after = if full {
                    page.last().map(|r| r.id.clone())
                } else {
                    None
                };
                let mut reminders = Vec::with_capacity(page.len());
                for reminder in page {
                    reminders.push(self.snapshot(&agenda, reminder).await?);
                }
                return Ok(json!({"reminders":reminders,"next_after":next_after}));
            }
            "change_timezone" => {
                let report = agenda
                    .change_timezone(operation_key, string(&args, "timezone")?.into(), now)
                    .await
                    .map_err(describe)?;
                return serde_json::to_value(report).map_err(|_| "invalid report".into());
            }
            "create" => Operation::Create(Definition {
                id: match optional(&args, "id")? {
                    Some(id) => id,
                    // Deterministic per operation key, so a retry is an exact replay.
                    None => hashed("rem", key)[..24].to_owned(),
                },
                text: string(&args, "text")?.into(),
                timing: self
                    .timing(
                        args.get("timing").ok_or("timing is required")?,
                        conversation,
                        source,
                        now,
                    )
                    .await?,
                preferred_channel: optional(&args, "preferred_channel")?,
                task: optional(&args, "task")?,
            }),
            "snooze" => Operation::Snooze {
                id: string(&args, "id")?.into(),
                generation: generation(&args)?,
                timing: self
                    .timing(
                        args.get("timing").ok_or("timing is required")?,
                        conversation,
                        source,
                        now,
                    )
                    .await?,
            },
            "complete" => Operation::Complete {
                id: string(&args, "id")?.into(),
                generation: generation(&args)?,
            },
            "delete" => Operation::Delete {
                id: string(&args, "id")?.into(),
                generation: generation(&args)?,
            },
            _ => return Err("unknown reminders action".into()),
        };
        let reminder = agenda
            .apply_at(operation_key, operation, now)
            .await
            .map_err(describe)?;
        Ok(json!({"reminder":self.snapshot(&agenda, reminder).await?}))
    }
}

/// Master fast actions plus the `reminders` tool, called only with the
/// turn's accepted source and original scope.
pub struct AgendaFastActions {
    inner: Arc<dyn FastActions>,
    tools: Arc<AgendaTools>,
}
impl AgendaFastActions {
    pub fn new(inner: Arc<dyn FastActions>, tools: Arc<AgendaTools>) -> Self {
        Self { inner, tools }
    }
}
impl FastActions for AgendaFastActions {
    fn deferred_schemas<'a>(
        &'a self,
        request: excalibur_master::MasterContextRequest<'a>,
    ) -> BoxFuture<'a, Result<Vec<ToolDefinition>, String>> {
        self.inner.deferred_schemas(request)
    }
    fn session_context<'a>(
        &'a self,
        request: excalibur_master::MasterContextRequest<'a>,
    ) -> BoxFuture<'a, Result<RunContext, String>> {
        self.inner.session_context(request)
    }
    fn definitions(&self) -> Vec<ToolDefinition> {
        let mut definitions = self.inner.definitions();
        definitions.retain(|d| d.name != REMINDERS_TOOL);
        definitions.push(reminders_definition());
        definitions
    }
    fn execute<'a>(
        &'a self,
        conversation: &'a str,
        command: &'a str,
        target: Option<&'a str>,
        key: &'a str,
    ) -> BoxFuture<'a, Result<Value, String>> {
        self.inner.execute(conversation, command, target, key)
    }
    fn confirm_cancel<'a>(
        &'a self,
        conversation: &'a str,
        scope: &'a str,
        target: &'a str,
        key: &'a str,
    ) -> BoxFuture<'a, Result<Value, String>> {
        self.inner.confirm_cancel(conversation, scope, target, key)
    }
    fn invoke<'a>(
        &'a self,
        conversation: &'a str,
        name: &'a str,
        arguments: Value,
        key: &'a str,
    ) -> BoxFuture<'a, Result<Value, String>> {
        if name != REMINDERS_TOOL {
            return self.inner.invoke(conversation, name, arguments, key);
        }
        Box::pin(async { Err("reminders require an accepted source and original scope".into()) })
    }
    fn invoke_scoped<'a>(
        &'a self,
        conversation: &'a str,
        scope: &'a str,
        name: &'a str,
        arguments: Value,
        key: &'a str,
    ) -> BoxFuture<'a, Result<Value, String>> {
        if name != REMINDERS_TOOL {
            return self
                .inner
                .invoke_scoped(conversation, scope, name, arguments, key);
        }
        Box::pin(async { Err("reminders require an accepted source message".into()) })
    }
    fn invoke_source_scoped<'a>(
        &'a self,
        conversation: &'a str,
        scope: &'a str,
        source_message_id: &'a str,
        name: &'a str,
        arguments: Value,
        key: &'a str,
    ) -> BoxFuture<'a, Result<Value, String>> {
        if name != REMINDERS_TOOL {
            return self.inner.invoke_source_scoped(
                conversation,
                scope,
                source_message_id,
                name,
                arguments,
                key,
            );
        }
        Box::pin(async move {
            self.tools
                .call(conversation, scope, source_message_id, arguments, key)
                .await
        })
    }
    fn context<'a>(&'a self, conversation: &'a str) -> BoxFuture<'a, Result<Value, String>> {
        self.inner.context(conversation)
    }
    fn stable_context<'a>(
        &'a self,
        conversation: &'a str,
        scope: &'a str,
    ) -> BoxFuture<'a, (String, String)> {
        self.inner.stable_context(conversation, scope)
    }
    fn finalize_response_scoped<'a>(
        &'a self,
        conversation: &'a str,
        scope: &'a str,
        text: &'a str,
    ) -> BoxFuture<'a, String> {
        self.inner
            .finalize_response_scoped(conversation, scope, text)
    }
    fn finalize_response<'a>(
        &'a self,
        conversation: &'a str,
        text: &'a str,
    ) -> BoxFuture<'a, String> {
        self.inner.finalize_response(conversation, text)
    }
}
