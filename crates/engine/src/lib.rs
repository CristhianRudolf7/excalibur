//! A durable single-run step engine. Applications embed it; no CLI or HTTP server.
mod episode_utilities;
use excalibur_context::{ContextInput, compile};
use excalibur_journal::{Event, Journal};
use excalibur_models::ModelGateway;
use excalibur_store::unix_seconds;
use excalibur_types::runtime::*;
use futures::{FutureExt, future::join_all};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    panic::AssertUnwindSafe,
    sync::{Arc, Mutex, OnceLock, Weak},
    time::Duration,
};
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("durable journal: {0}")]
    Journal(#[from] excalibur_journal::Error),
    #[error("state serialization: {0}")]
    Json(#[from] serde_json::Error),
    #[error("invalid engine state: {0}")]
    Invalid(&'static str),
    #[error("model gateway: {0}")]
    Model(String),
}
pub type Result<T> = std::result::Result<T, Error>;
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Budget {
    pub max_steps: u64,
    pub max_tokens: u64,
    pub max_cost_microusd: Option<u64>,
    pub max_wall_seconds: u64,
    pub max_tool_calls: u64,
    pub tool_timeout_seconds: u64,
}
impl Default for Budget {
    fn default() -> Self {
        Self {
            max_steps: 100,
            max_tokens: 100_000,
            max_cost_microusd: None,
            max_wall_seconds: 3600,
            max_tool_calls: 1000,
            tool_timeout_seconds: 300,
        }
    }
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Criterion {
    FinalResponse {
        schema: Option<Value>,
    },
    ToolSuccess {
        name: String,
    },
    ToolOutput {
        name: String,
        pointer: String,
        expected: Value,
    },
    HostCheck {
        id: String,
    },
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct RunSpec {
    pub context: ContextInput,
    pub budget: Budget,
    pub criteria: Vec<Criterion>,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Running,
    Paused,
    Blocked,
    Completed,
    CompletedWithPending,
    Cancelled,
    Failed,
}
impl Status {
    fn label(&self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Paused => "paused",
            Self::Blocked => "blocked",
            Self::Completed => "completed",
            Self::CompletedWithPending => "completed_with_pending",
            Self::Cancelled => "cancelled",
            Self::Failed => "failed",
        }
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CallState {
    pub prepared: Option<PreparedTool>,
    pub result: Option<ToolResult>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Snapshot {
    pub spec: RunSpec,
    pub status: Status,
    pub reason: Option<String>,
    pub step: u64,
    pub started_at: i64,
    pub usage: Usage,
    pub tool_calls: u64,
    pub gate_attempts: u32,
    pub final_text: Option<String>,
    pub request: Option<ModelRequest>,
    pub response: Option<ModelResponse>,
    pub calls: BTreeMap<usize, CallState>,
    pub history: Vec<Message>,
    pub continuation: String,
    #[serde(default)]
    pub pinned_observations: BTreeMap<String, Value>,
    pub gate_report: Vec<String>,
    pub host_checks: BTreeMap<String, bool>,
    pub revision: u64,
    pub pending_directives: BTreeMap<String, String>,
    pub last_sequence: u64,
    pub gateway_identity: String,
}
impl Snapshot {
    fn started(seed: &Value) -> Result<Self> {
        Ok(Self {
            spec: serde_json::from_value(seed["spec"].clone())?,
            status: Status::Running,
            reason: None,
            step: 0,
            started_at: seed["started_at"]
                .as_i64()
                .ok_or(Error::Invalid("missing start time"))?,
            usage: Usage::default(),
            tool_calls: 0,
            gate_attempts: 0,
            final_text: None,
            request: None,
            response: None,
            calls: BTreeMap::new(),
            history: Vec::new(),
            continuation: String::new(),
            pinned_observations: BTreeMap::new(),
            gate_report: Vec::new(),
            host_checks: BTreeMap::new(),
            revision: 0,
            pending_directives: BTreeMap::new(),
            last_sequence: 1,
            gateway_identity: seed["gateway_identity"]
                .as_str()
                .ok_or(Error::Invalid("gateway identity missing"))?
                .into(),
        })
    }
    fn apply(&mut self, event: &Event) -> Result<()> {
        let p = &event.payload;
        match event.kind.as_str() {
            "run_started" => {}
            "model_request" => {
                self.request = Some(serde_json::from_value(p["request"].clone())?);
            }
            "model_response" => {
                let response: ModelResponse = serde_json::from_value(p["response"].clone())?;
                self.usage.estimated |= response.usage.estimated;
                self.usage.input_tokens = self
                    .usage
                    .input_tokens
                    .saturating_add(response.usage.input_tokens);
                self.usage.output_tokens = self
                    .usage
                    .output_tokens
                    .saturating_add(response.usage.output_tokens);
                self.usage.cost_microusd = self
                    .usage
                    .cost_microusd
                    .saturating_add(response.usage.cost_microusd);
                self.response = Some(response);
            }
            "tool_intent" => {
                let index = index(p)?;
                let prepared = serde_json::from_value(p["prepared"].clone())?;
                self.calls
                    .entry(index)
                    .or_insert(CallState {
                        prepared: None,
                        result: None,
                    })
                    .prepared = Some(prepared);
                self.tool_calls += 1;
                if self.calls[&index]
                    .prepared
                    .as_ref()
                    .is_some_and(|p| p.effect != Effect::Pure)
                {
                    self.host_checks.clear();
                }
            }
            "tool_result" => {
                let index = index(p)?;
                if !self.calls.contains_key(&index) {
                    self.tool_calls += 1;
                }
                let result: ToolResult = serde_json::from_value(p["result"].clone())?;
                if self
                    .calls
                    .get(&index)
                    .and_then(|c| c.prepared.as_ref())
                    .is_some_and(|p| p.effect != Effect::Pure)
                {
                    self.host_checks.clear();
                }
                if p["name"] == "leave_continuation_note" && result.success {
                    self.continuation = result.output["note"].as_str().unwrap_or_default().into();
                }
                if p["name"] == "pin_observation" && result.success {
                    let id = format!(
                        "{:020}:{}",
                        result.output["source_sequence"].as_u64().unwrap_or(0),
                        result.output["id"].as_str().unwrap_or("")
                    );
                    if result.output["pinned"] == false {
                        self.pinned_observations.remove(&id);
                    } else {
                        self.pinned_observations.insert(id, result.output.clone());
                    }
                    while self.pinned_observations.len() > 8
                        || serde_json::to_vec(&self.pinned_observations)?.len() > 4096
                    {
                        self.pinned_observations.pop_first();
                    }
                }
                self.calls
                    .entry(index)
                    .or_insert(CallState {
                        prepared: None,
                        result: None,
                    })
                    .result = Some(result);
            }
            "step_completed" => {
                let response = self
                    .response
                    .take()
                    .ok_or(Error::Invalid("completed step lacks response"))?;
                self.history.push(response.message.clone());
                for (index, call) in response.message.tool_calls.iter().enumerate() {
                    let result = self
                        .calls
                        .get(&index)
                        .and_then(|c| c.result.as_ref())
                        .ok_or(Error::Invalid("completed step lacks result"))?;
                    self.history.push(Message {
                        role: "tool".into(),
                        content: serde_json::to_string(result)?,
                        tool_call_id: Some(call.id.clone()),
                        tool_calls: Vec::new(),
                    });
                }
                self.request = None;
                self.calls.clear();
                self.step += 1;
            }
            "gate_report" => {
                self.gate_attempts += 1;
                self.gate_report = serde_json::from_value(p["pending"].clone())?;
                self.final_text = p["final_text"].as_str().map(str::to_owned);
            }
            "state_change" => {
                self.status = serde_json::from_value(p["state"].clone())?;
                self.reason = p["reason"].as_str().map(str::to_owned);
            }
            "directive_received" => {
                self.pending_directives.insert(
                    p["id"]
                        .as_str()
                        .ok_or(Error::Invalid("directive ID"))?
                        .into(),
                    p["text"]
                        .as_str()
                        .ok_or(Error::Invalid("directive text"))?
                        .into(),
                );
            }
            "directive_applied" => {
                self.pending_directives
                    .remove(p["id"].as_str().ok_or(Error::Invalid("directive ID"))?);
                let text = p["text"].as_str().ok_or(Error::Invalid("directive text"))?;
                self.revision += 1;
                self.history.push(Message::text(
                    "system",
                    format!("Context update v{}: {text}", self.revision),
                ));
                self.host_checks.clear();
            }
            "host_check" => {
                let id = p["id"].as_str().ok_or(Error::Invalid("host check ID"))?;
                if p["revision"].as_u64() == Some(self.revision) {
                    self.host_checks
                        .insert(id.into(), p["passed"].as_bool().unwrap_or(false));
                }
            }
            "budget_updated" => {
                self.spec.budget = serde_json::from_value(p["budget"].clone())?;
            }
            "checkpoint" | "model_failure" | "model_dispatch" => {}
            _ => return Err(Error::Invalid("unknown durable event")),
        }
        self.last_sequence = event.sequence;
        Ok(())
    }
    pub fn pending_criteria(&self) -> Vec<String> {
        self.spec
            .criteria
            .iter()
            .filter_map(|criterion| {
                let ok = match criterion {
                    Criterion::FinalResponse { schema } => {
                        self.response.as_ref().is_some_and(|r| {
                            if !r.message.tool_calls.is_empty()
                                || r.message.content.trim().is_empty()
                            {
                                return false;
                            }
                            match schema {
                                None => true,
                                Some(schema) => serde_json::from_str::<Value>(&r.message.content)
                                    .ok()
                                    .is_some_and(|value| {
                                        jsonschema::validator_for(schema)
                                            .is_ok_and(|validator| validator.is_valid(&value))
                                    }),
                            }
                        })
                    }
                    Criterion::HostCheck { id } => self.host_checks.get(id) == Some(&true),
                    Criterion::ToolSuccess { name } => {
                        self.latest_tool(name).is_some_and(|r| r.success)
                    }
                    Criterion::ToolOutput {
                        name,
                        pointer,
                        expected,
                    } => self
                        .latest_tool(name)
                        .is_some_and(|r| r.success && r.output.pointer(pointer) == Some(expected)),
                };
                (!ok).then(|| format!("Unverified criterion: {criterion:?}"))
            })
            .collect()
    }
    fn latest_tool(&self, name: &str) -> Option<ToolResult> {
        // The append-only history pairs assistant calls with results, in original call order.
        let mut latest = None;
        let mut names = BTreeMap::new();
        for m in &self.history {
            for call in &m.tool_calls {
                names.insert(call.id.clone(), call.name.clone());
            }
            if m.role == "tool"
                && m.tool_call_id
                    .as_ref()
                    .and_then(|id| names.get(id))
                    .is_some_and(|n| n == name)
            {
                latest = serde_json::from_str(&m.content).ok();
            }
        }
        latest
    }
}
fn index(p: &Value) -> Result<usize> {
    p["index"]
        .as_u64()
        .map(|n| n as usize)
        .ok_or(Error::Invalid("tool index missing"))
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Boundary {
    BeforeModelRequest,
    ModelRequestConfirmed,
    ModelReturned,
    ModelResponseConfirmed,
    GateReportConfirmed,
    BeforeToolIntent,
    ToolIntentConfirmed,
    ToolEffectReturned,
    ToolResultConfirmed,
    CheckpointConfirmed,
}
/// Fallible incremental durable projection. Returning an error stops before the
/// next model/tool effect; replaying journal events must be idempotent.
pub trait DurableSink: Send + Sync {
    fn record<'a>(
        &'a self,
        run: &'a str,
        event: &'a Event,
    ) -> BoxFuture<'a, std::result::Result<(), String>>;
}
#[derive(Clone)]
struct Projection {
    sink: Arc<dyn DurableSink>,
    cursors: Arc<tokio::sync::Mutex<BTreeMap<String, u64>>>,
}
pub trait Observer: Send + Sync {
    fn reached(&self, phase: Boundary, run: &str, step: u64, key: Option<&str>);
}
#[derive(Clone)]
pub struct Engine {
    journal: Journal,
    model: Arc<dyn ModelGateway>,
    tools: Arc<dyn ToolRuntime>,
    observer: Option<Arc<dyn Observer>>,
    projection: Option<Projection>,
    stream_sink: Option<excalibur_models::StreamSink>,
}
impl Engine {
    pub fn new(
        journal: Journal,
        model: Arc<dyn ModelGateway>,
        tools: Arc<dyn ToolRuntime>,
    ) -> Self {
        Self {
            journal,
            model,
            tools,
            observer: None,
            projection: None,
            stream_sink: None,
        }
    }
    pub fn with_durable_sink(mut self, sink: Arc<dyn DurableSink>) -> Self {
        self.projection = Some(Projection {
            sink,
            cursors: Arc::new(tokio::sync::Mutex::new(BTreeMap::new())),
        });
        self
    }
    pub async fn flush_events(&self, run: &str) -> Result<()> {
        let Some(projection) = &self.projection else {
            return Ok(());
        };
        let mut cursors = projection.cursors.lock().await;
        let cursor = cursors.entry(run.to_owned()).or_default();
        loop {
            let events = self.journal.entries(run, *cursor, 256).await?;
            if events.is_empty() {
                break;
            }
            for event in events {
                projection
                    .sink
                    .record(run, &event)
                    .await
                    .map_err(|_| Error::Invalid("durable event projection failed"))?;
                *cursor = event.sequence;
            }
        }
        Ok(())
    }
    pub fn with_observer(mut self, observer: Arc<dyn Observer>) -> Self {
        self.observer = Some(observer);
        self
    }
    pub fn with_stream_sink(mut self, sink: excalibur_models::StreamSink) -> Self {
        self.stream_sink = Some(sink);
        self
    }
    pub fn journal(&self) -> &Journal {
        &self.journal
    }
    fn point(&self, phase: Boundary, run: &str, step: u64, key: Option<&str>) {
        if let Some(o) = &self.observer {
            o.reached(phase, run, step, key);
        }
    }
    pub async fn create(&self, operation: &str, mut spec: RunSpec) -> Result<String> {
        let _creation = self
            .journal
            .lock_run(&format!("operation:{operation}"))
            .await;
        if spec.criteria.is_empty()
            || spec.budget.max_steps == 0
            || spec.budget.max_tokens == 0
            || spec.budget.max_wall_seconds == 0
            || spec.budget.tool_timeout_seconds == 0
        {
            return Err(Error::Invalid(
                "nonempty criteria and positive budgets required",
            ));
        }
        if spec.budget.max_cost_microusd.is_some() && !self.model.cost_is_known() {
            return Err(Error::Invalid(
                "cost budget requires a priced gateway; unknown cost is not zero",
            ));
        }
        spec.context.tools = self.tools.definitions();
        spec.context.tools.extend(builtin_definitions());
        spec.context.tools.sort_by(|a, b| a.name.cmp(&b.name));
        compile(&spec.context).map_err(|_| Error::Invalid("invalid initial context"))?;
        let spec_value = serde_json::to_value(&spec)?;
        if self.sanitize(&spec_value)? != spec_value {
            return Err(Error::Invalid(
                "secret literal in initial run specification; use a secret reference",
            ));
        }
        // Creation time belongs to the durable seed, not to a stable prompt prefix.
        // Reusing an operation returns its existing run, preserving its original time.
        let existing = self
            .journal
            .database()
            .read(excalibur_store::ReadLane::Interactive, {
                let operation = operation.to_owned();
                move |c| {
                    use excalibur_store::rusqlite::OptionalExtension;
                    Ok(c.query_row(
                        "SELECT run_id FROM durable_operations WHERE operation_id=?1",
                        [operation],
                        |r| r.get::<_, String>(0),
                    )
                    .optional()?)
                }
            })
            .await
            .map_err(excalibur_journal::Error::from)?;
        if let Some(run) = existing {
            let state = self.load(&run).await?;
            let seed = self
                .journal
                .entries(&run, 0, 1)
                .await?
                .pop()
                .ok_or(Error::Invalid("run seed missing"))?;
            let original: RunSpec = serde_json::from_value(seed.payload["spec"].clone())?;
            if original != spec || state.gateway_identity != self.model.identity() {
                return Err(Error::Invalid(
                    "operation reused with different run specification",
                ));
            }
            self.flush_events(&run).await?;
            return Ok(run);
        }
        let seed = json!({"spec":spec,"started_at":unix_seconds(),"gateway_identity":self.model.identity(),"model_dispatch_protocol":1});
        if self.sanitize(&seed)? != seed {
            return Err(Error::Invalid("secret literal in durable run identity"));
        }
        let run = self.journal.create_run(operation, seed).await?;
        self.flush_events(&run).await?;
        Ok(run)
    }
    pub async fn load(&self, run: &str) -> Result<Snapshot> {
        let checkpoint = self.journal.latest_checkpoint(run).await?;
        let (mut state, after) = if let Some(seq) = checkpoint {
            let event = self
                .journal
                .entries(run, seq - 1, 1)
                .await?
                .pop()
                .ok_or(Error::Invalid("checkpoint missing"))?;
            let mut state: Snapshot = serde_json::from_value(event.payload)?;
            state.last_sequence = seq;
            (state, seq)
        } else {
            let event = self
                .journal
                .entries(run, 0, 1)
                .await?
                .pop()
                .ok_or(Error::Invalid("run missing"))?;
            if event.kind != "run_started" {
                return Err(Error::Invalid("run seed missing"));
            }
            (Snapshot::started(&event.payload)?, event.sequence)
        };
        for event in self.journal.replay(run, after).await? {
            state.apply(&event)?;
        }
        Ok(state)
    }
    fn sanitize(&self, value: &Value) -> Result<Value> {
        self.tools
            .sanitize_durable(value)
            .map_err(|_| Error::Invalid("durable data sanitization failed"))
    }
    async fn append_safe(&self, run: &str, kind: &str, key: &str, payload: Value) -> Result<u64> {
        let sequence = self
            .journal
            .append(run, kind, key, self.sanitize(&payload)?)
            .await?;
        self.flush_events(run).await?;
        Ok(sequence)
    }
    async fn event(
        &self,
        run: &str,
        state: &mut Snapshot,
        kind: &str,
        key: String,
        payload: Value,
    ) -> Result<()> {
        let payload = self.sanitize(&payload)?;
        let sequence = self.append_safe(run, kind, &key, payload.clone()).await?;
        if sequence > state.last_sequence {
            state.apply(&Event {
                sequence,
                kind: kind.into(),
                key,
                payload,
            })?;
        }
        Ok(())
    }
    async fn transition(
        &self,
        run: &str,
        state: &mut Snapshot,
        status: Status,
        reason: Option<String>,
    ) -> Result<()> {
        self.transition_from(run, state, &["running"], status, reason)
            .await?;
        Ok(())
    }
    async fn transition_from(
        &self,
        run: &str,
        state: &mut Snapshot,
        expected: &[&str],
        status: Status,
        reason: Option<String>,
    ) -> Result<bool> {
        let (mut status, mut reason) = (status, reason);
        // Hold the security runtime's barrier through the durable completion CAS.
        // Background brokers serialize new approval requests with this same gate.
        let _finalization_guard =
            if matches!(status, Status::Completed | Status::CompletedWithPending) {
                let guard = self
                    .tools
                    .finalization_guard(run)
                    .await
                    .map_err(|_| Error::Invalid("completion authority unavailable"))?;
                match guard.state() {
                    excalibur_types::runtime::FinalizationState::Ready => {}
                    excalibur_types::runtime::FinalizationState::Pending => {
                        status = Status::Paused;
                        reason = Some("approval required".into());
                    }
                    excalibur_types::runtime::FinalizationState::Blocked(cause) => {
                        status = Status::Blocked;
                        reason = Some(cause);
                    }
                }
                Some(guard)
            } else {
                None
            };
        // The read gives the caller the latest facts; the writer-side guard closes
        // the race between this read and a concurrent user control transaction.
        *state = self.load(run).await?;
        if !expected.contains(&state.status.label()) {
            return Ok(false);
        }
        let key = format!(
            "state:{}:{}:{}",
            state.step,
            state.last_sequence,
            status.label()
        );
        let reason = reason
            .map(|text| self.sanitize(&json!(text)))
            .transpose()?
            .and_then(|value| value.as_str().map(str::to_owned));
        let applied = self
            .journal
            .set_state_if(run, expected, status.label(), reason, &key)
            .await?;
        self.flush_events(run).await?;
        *state = self.load(run).await?;
        Ok(applied)
    }
    /// Host finalization of an unrecoverable execution error; never overwrites
    /// user controls or an already completed run.
    pub async fn fail(&self, run: &str, reason: &str) -> Result<()> {
        let mut state = self.load(run).await?;
        self.transition_from(
            run,
            &mut state,
            &["running"],
            Status::Failed,
            Some(reason.into()),
        )
        .await?;
        Ok(())
    }
    pub async fn block(&self, run: &str, reason: &str) -> Result<()> {
        let mut state = self.load(run).await?;
        self.transition(run, &mut state, Status::Blocked, Some(reason.into()))
            .await
    }
    /// Controls are persisted without acquiring the long-running run lock.
    pub async fn pause(&self, run: &str) -> Result<()> {
        let mut s = self.load(run).await?;
        if s.status == Status::Paused && s.reason.as_deref() == Some("user") {
            return Ok(());
        }
        if !self
            .transition_from(
                run,
                &mut s,
                &["running", "paused", "blocked"],
                Status::Paused,
                Some("user".into()),
            )
            .await?
        {
            return Err(Error::Invalid("terminal execution cannot be paused"));
        }
        Ok(())
    }
    pub async fn cancel(&self, run: &str) -> Result<()> {
        let mut s = self.load(run).await?;
        if s.status == Status::Cancelled {
            return Ok(());
        }
        let applied = self
            .transition_from(
                run,
                &mut s,
                &["running", "paused", "blocked"],
                Status::Cancelled,
                Some("user".into()),
            )
            .await?;
        if !applied && s.status != Status::Cancelled {
            return Err(Error::Invalid("terminal execution cannot be cancelled"));
        }
        Ok(())
    }
    pub async fn resume(&self, run: &str) -> Result<()> {
        let _guard = self.journal.lock_run(run).await;
        let mut s = self.load(run).await?;
        if !self
            .transition_from(run, &mut s, &["paused"], Status::Running, None)
            .await?
        {
            return Err(Error::Invalid("only paused runs can resume"));
        }
        Ok(())
    }
    /// Trusted host budget amendment; increases are explicit durable facts, never a new run.
    pub async fn update_budget(&self, run: &str, budget: Budget) -> Result<()> {
        let _guard = self.journal.lock_run(run).await;
        let mut state = self.load(run).await?;
        if matches!(
            state.status,
            Status::Completed | Status::CompletedWithPending | Status::Cancelled | Status::Failed
        ) || budget.max_steps == 0
            || budget.max_tokens == 0
            || budget.max_wall_seconds == 0
            || budget.tool_timeout_seconds == 0
        {
            return Err(Error::Invalid("active run and positive budget required"));
        }
        if budget.max_cost_microusd.is_some() && !self.model.cost_is_known() {
            return Err(Error::Invalid("cost budget requires priced usage"));
        }
        let key = format!("budget:{}", state.last_sequence);
        self.event(
            run,
            &mut state,
            "budget_updated",
            key,
            json!({"budget":budget}),
        )
        .await
    }
    pub async fn directive(&self, run: &str, id: &str, text: &str) -> Result<()> {
        if id.is_empty() || text.len() > 64 * 1024 {
            return Err(Error::Invalid("directive ID/text"));
        }
        // Each fact is durable; an interrupted application is resumed from the received event.
        self.append_safe(
            run,
            "directive_received",
            &format!("directive:{id}:received"),
            json!({"id":id,"text":text}),
        )
        .await?;
        self.append_safe(
            run,
            "directive_applied",
            &format!("directive:{id}:applied"),
            json!({"id":id,"text":text}),
        )
        .await?;
        Ok(())
    }
    pub async fn record_check(
        &self,
        run: &str,
        id: &str,
        passed: bool,
        evidence: &str,
    ) -> Result<()> {
        if evidence.is_empty() {
            return Err(Error::Invalid("host evidence required"));
        }
        let s = self.load(run).await?;
        self.append_safe(
            run,
            "host_check",
            &format!("check:{id}:{}:{}", s.revision, s.last_sequence),
            json!({"id":id,"passed":passed,"evidence":evidence,"revision":s.revision}),
        )
        .await?;
        Ok(())
    }
    /// A trusted consumer supplies a reconciled outcome; it cannot request a blind retry.
    pub async fn resolve_uncertain(
        &self,
        run: &str,
        index: usize,
        result: ToolResult,
        evidence: &str,
    ) -> Result<()> {
        let _guard = self.journal.lock_run(run).await;
        let mut s = self.load(run).await?;
        if s.status != Status::Blocked || evidence.trim().is_empty() {
            return Err(Error::Invalid(
                "blocked run and explicit reconciliation evidence required",
            ));
        }
        let call = s
            .response
            .as_ref()
            .and_then(|r| r.message.tool_calls.get(index))
            .ok_or(Error::Invalid("pending call missing"))?
            .clone();
        if s.calls
            .get(&index)
            .is_none_or(|c| c.prepared.is_none() || c.result.is_some())
        {
            return Err(Error::Invalid("unresolved intention required"));
        }
        let key = tool_key(run, s.step, index);
        self.event(run,&mut s,"tool_result",format!("{key}:result"),json!({"index":index,"name":call.name,"result":result,"reconciliation_evidence":evidence})).await?;
        if !self
            .transition_from(run, &mut s, &["blocked"], Status::Running, None)
            .await?
        {
            return Err(Error::Invalid(
                "reconciled outcome recorded but control state prevents resuming",
            ));
        }
        Ok(())
    }
    async fn honor_approval_wait(&self, run: &str, state: &mut Snapshot) -> Result<bool> {
        if let Some(reason) = self.tools.approval_blocked(run).await {
            self.transition(run, state, Status::Blocked, Some(reason))
                .await?;
            return Ok(true);
        }
        if self.tools.pending_approval(run).await {
            self.transition(run, state, Status::Paused, Some("approval required".into()))
                .await?;
            return Ok(true);
        }
        Ok(false)
    }
    pub async fn run(&self, run: &str) -> Result<Snapshot> {
        let _guard = self.journal.lock_run(run).await;
        // Only intentions already present at admission need reconciliation.
        let initial = self.load(run).await?;
        if initial.gateway_identity != self.model.identity() {
            return Err(Error::Invalid("gateway profile differs from durable run"));
        }
        if initial.status == Status::Running {
            for input in &initial.spec.context.untrusted {
                self.tools
                    .observe_untrusted(run, &input.source)
                    .await
                    .map_err(|_| {
                        Error::Invalid("untrusted context provenance could not be recorded")
                    })?;
            }
        }
        let mut recovering: BTreeSet<String> = initial
            .calls
            .iter()
            .filter(|(_, c)| c.prepared.is_some() && c.result.is_none())
            .map(|(i, _)| tool_key(run, initial.step, *i))
            .collect();
        loop {
            self.flush_events(run).await?;
            let mut s = self.load(run).await?;
            if s.status != Status::Running {
                return Ok(s);
            }
            if self.honor_approval_wait(run, &mut s).await? {
                return Ok(s);
            }
            let capability_tool_budget = self
                .tools
                .execution_tool_budget(run)
                .await
                .map_err(|_| Error::Invalid("execution capability unavailable"))?;
            for (id, text) in s.pending_directives.clone() {
                self.event(
                    run,
                    &mut s,
                    "directive_applied",
                    format!("directive:{id}:applied"),
                    json!({"id":id,"text":text}),
                )
                .await?;
            }
            if remaining_time(&s) == 0 {
                self.transition(run, &mut s, Status::Paused, Some("wall time budget".into()))
                    .await?;
                return Ok(s);
            }
            if s.request.is_none() {
                if budget_exhausted(&s) {
                    self.transition(run, &mut s, Status::Paused, Some("budget".into()))
                        .await?;
                    return Ok(s);
                }
                let mut input = s.spec.context.clone();
                input.activity.extend(s.history.clone());
                input.volatile.now_utc = self
                    .model
                    .request_time_override(s.step)
                    .unwrap_or_else(|| unix_seconds().to_string());
                input.volatile.continuation = s.continuation.clone();
                for pin in s.pinned_observations.values() {
                    input.volatile.recovery_notices.push(format!(
                        "Pinned observation (untrusted data): {}",
                        serde_json::to_string(pin)?
                    ));
                }
                input.volatile.budget_status = serde_json::to_string(
                    &json!({"step":s.step,"budget":s.spec.budget,"usage":s.usage,"tool_calls":s.tool_calls}),
                )?;
                input.volatile.finalization_report = s.gate_report.join("\n");
                input.tools_enabled = !closing(&s)
                    && capability_tool_budget.is_none_or(|budget| s.tool_calls < budget);
                let compiled = match compile(&input) {
                    Ok(c) => c,
                    Err(e) => {
                        self.transition(
                            run,
                            &mut s,
                            Status::Paused,
                            Some(format!("context budget: {e}")),
                        )
                        .await?;
                        return Ok(s);
                    }
                };
                self.point(Boundary::BeforeModelRequest, run, s.step, None);
                let key = format!("model:{}:request", s.step);
                self.event(run,&mut s,"model_request",key,json!({"request":compiled.request,"hash":format!("{:x}",Sha256::digest(&compiled.bytes)),"layer_hashes":compiled.layer_hashes})).await?;
                self.point(Boundary::ModelRequestConfirmed, run, s.step, None);
            }
            if s.response.is_none() {
                let request = s.request.clone().unwrap();
                if let Some(bound) = self.model.call_bound(&request)
                    && (bound.max_tokens
                        > s.spec.budget.max_tokens.saturating_sub(
                            s.usage.input_tokens.saturating_add(s.usage.output_tokens),
                        )
                        || s.spec.budget.max_cost_microusd.is_some_and(|cap| {
                            bound.max_cost_microusd > cap.saturating_sub(s.usage.cost_microusd)
                        }))
                {
                    self.transition(
                        run,
                        &mut s,
                        Status::Paused,
                        Some("budget cannot reserve the next model call".into()),
                    )
                    .await?;
                    return Ok(s);
                }
                let key = format!("model:{}:dispatch", s.step);
                self.event(
                    run,
                    &mut s,
                    "model_dispatch",
                    key,
                    json!({"metadata":self.model.metadata()}),
                )
                .await?;
                let remaining = remaining_time(&s).max(1);
                let response = tokio::time::timeout(
                    Duration::from_secs(remaining),
                    AssertUnwindSafe(async {
                        if let Some(sink) = &self.stream_sink {
                            self.model
                                .complete_stream_observed(request, sink.clone())
                                .await
                        } else {
                            self.model.complete_observed(request).await
                        }
                    })
                    .catch_unwind(),
                )
                .await;
                let observed = match response {
                    Ok(Ok(Ok(observed))) => observed,
                    Ok(Ok(Err(error))) => {
                        let reason = error.to_string();
                        let failure_class =
                            serde_json::to_value(error.error.classification().class)
                                .unwrap_or(Value::Null);
                        let key = format!("model:{}:failure", s.step);
                        self.event(run,&mut s,"model_failure",key,json!({"metrics":error.metrics,"failure_class":failure_class,"error":reason})).await?;
                        return Err(Error::Model(reason));
                    }
                    Err(_) => {
                        let key = format!("model:{}:failure", s.step);
                        self.event(run,&mut s,"model_failure",key,json!({"failure_class":"agent_timeout","error":"wall time exhausted during model request"})).await?;
                        self.transition(
                            run,
                            &mut s,
                            Status::Paused,
                            Some("wall time budget".into()),
                        )
                        .await?;
                        return Ok(s);
                    }
                    Ok(Err(_)) => {
                        let key = format!("model:{}:failure", s.step);
                        self.event(
                            run,
                            &mut s,
                            "model_failure",
                            key,
                            json!({"failure_class":"harness_error","error":"gateway panicked"}),
                        )
                        .await?;
                        return Err(Error::Model(
                            "gateway panicked; no response recorded".into(),
                        ));
                    }
                };
                let response = observed.response;
                let model_metrics = observed.metrics;
                if response.message.role != "assistant"
                    || response.message.tool_calls.len() > 128
                    || response.message.tool_calls.iter().any(|c| c.id.is_empty())
                    || response
                        .message
                        .tool_calls
                        .iter()
                        .map(|c| &c.id)
                        .collect::<BTreeSet<_>>()
                        .len()
                        != response.message.tool_calls.len()
                {
                    return Err(Error::Invalid("invalid model tool call identifiers"));
                }
                for call in &response.message.tool_calls {
                    let original = json!(call.arguments);
                    let parsed: Option<Value> = serde_json::from_str(&call.arguments)
                        .or_else(|_| json5::from_str(&call.arguments))
                        .ok();
                    if self.sanitize(&original)? != original
                        || parsed.as_ref().is_some_and(|value| {
                            self.sanitize(value).is_ok_and(|safe| safe != *value)
                        })
                    {
                        let safe_response = ModelResponse {
                            message: Message::text(
                                "assistant",
                                "[tool request rejected: secret literal]",
                            ),
                            usage: response.usage.clone(),
                        };
                        let key = format!("model:{}:response", s.step);
                        self.event(
                            run,
                            &mut s,
                            "model_response",
                            key,
                            json!({"response":safe_response,"metrics":model_metrics}),
                        )
                        .await?;
                        self.transition(run, &mut s, Status::Blocked, Some("model requested a secret literal in tool arguments; use executor secret references".into())).await?;
                        return Ok(s);
                    }
                }
                self.point(Boundary::ModelReturned, run, s.step, None);
                let key = format!("model:{}:response", s.step);
                self.event(
                    run,
                    &mut s,
                    "model_response",
                    key,
                    json!({"response":response,"metrics":model_metrics}),
                )
                .await?;
                self.point(Boundary::ModelResponseConfirmed, run, s.step, None);
            }
            s = self.load(run).await?;
            if s.status != Status::Running {
                return Ok(s);
            }
            if self.honor_approval_wait(run, &mut s).await? {
                return Ok(s);
            }
            let response = s.response.clone().unwrap();
            if response.message.tool_calls.is_empty() {
                let pending = s.pending_criteria();
                let key = format!("gate:{}", s.step);
                self.event(
                    run,
                    &mut s,
                    "gate_report",
                    key,
                    json!({"pending":pending,"final_text":response.message.content}),
                )
                .await?;
                self.point(Boundary::GateReportConfirmed, run, s.step, None);
                if s.gate_report.is_empty() {
                    self.transition(run, &mut s, Status::Completed, None)
                        .await?;
                    return Ok(s);
                }
                if s.gate_attempts > 2 || closing(&s) {
                    let reason = s.gate_report.join("; ");
                    self.transition(run, &mut s, Status::CompletedWithPending, Some(reason))
                        .await?;
                    return Ok(s);
                }
                let key = format!("step:{}:completed", s.step);
                self.event(run, &mut s, "step_completed", key, json!({}))
                    .await?;
                self.checkpoint(run, &mut s).await?;
                continue;
            }
            if s.request.as_ref().is_some_and(|r| !r.tools_enabled) {
                self.transition(
                    run,
                    &mut s,
                    Status::CompletedWithPending,
                    Some("provider returned tools while tools were disabled".into()),
                )
                .await?;
                return Ok(s);
            }
            let mut planned = Vec::new();
            for (index, call) in response.message.tool_calls.iter().enumerate() {
                if s.calls.get(&index).is_some_and(|c| c.result.is_some()) {
                    continue;
                }
                let scheduling =
                    if let Some(prepared) = s.calls.get(&index).and_then(|c| c.prepared.as_ref()) {
                        ToolScheduling::from_prepared(prepared)
                    } else {
                        self.scheduling(run, call, s.spec.budget.tool_timeout_seconds)
                            .await
                    };
                planned.push((index, scheduling));
            }
            for batch in batches(planned) {
                // Acquire the shared/exclusive gate once per batch. Each call
                // takes only its own resources, so a blocked file cannot stall
                // independent calls and queued exclusive work cannot split readers.
                let _gate = batch_gate(batch.iter().any(|(_, plan)| plan.exclusive)).await;
                s = self.load(run).await?;
                if s.status != Status::Running {
                    return Ok(s);
                }
                let mut slots = s
                    .spec
                    .budget
                    .max_tool_calls
                    .min(capability_tool_budget.unwrap_or(u64::MAX))
                    .saturating_sub(s.tool_calls);
                let mut tool_limit = false;
                let mut actions = Vec::new();
                for (index, plan) in batch {
                    let prepared = s.calls.get(&index).and_then(|c| c.prepared.clone());
                    if prepared.is_none() {
                        if slots == 0 {
                            tool_limit = true;
                            break;
                        }
                        slots -= 1;
                    }
                    let key = tool_key(run, s.step, index);
                    actions.push(ToolJob {
                        index,
                        step: s.step,
                        recovery: recovering.remove(&key),
                        key,
                        plan,
                        prepared,
                        call: response.message.tool_calls[index].clone(),
                        timeout: s.spec.budget.tool_timeout_seconds,
                    });
                }
                let jobs = actions.into_iter().map(|job| self.execute_tool(run, job));
                let failures = join_all(jobs)
                    .await
                    .into_iter()
                    .flatten()
                    .collect::<Vec<_>>();
                if !failures.is_empty() {
                    s = self.load(run).await?;
                    // Any genuinely uncertain sibling dominates retryable failures.
                    let retryable = failures.iter().all(|failure| failure.retryable);
                    let reason = failures
                        .into_iter()
                        .map(|failure| failure.reason)
                        .collect::<Vec<_>>()
                        .join("; ");
                    self.transition(
                        run,
                        &mut s,
                        if retryable {
                            Status::Paused
                        } else {
                            Status::Blocked
                        },
                        Some(format!(
                            "{}: {reason}",
                            if retryable {
                                "retryable tool interruption"
                            } else {
                                "uncertain effect"
                            }
                        )),
                    )
                    .await?;
                    return Ok(s);
                }
                if self.honor_approval_wait(run, &mut s).await? {
                    return Ok(s);
                }
                s = self.load(run).await?;
                if let Some(reason) = s.calls.values().find_map(|call| {
                    let result = call.result.as_ref()?;
                    (call.prepared.as_ref()?.call.name == "status"
                        && result.success
                        && result.output["task_state"] == "blocked")
                        .then(|| result.output["reason"].as_str().map(str::to_owned))
                        .flatten()
                }) {
                    self.transition(run, &mut s, Status::Blocked, Some(reason))
                        .await?;
                    return Ok(s);
                }
                if tool_limit {
                    self.transition(run, &mut s, Status::Paused, Some("tool budget".into()))
                        .await?;
                    return Ok(s);
                }
            }
            s = self.load(run).await?;
            if s.status != Status::Running {
                return Ok(s);
            }
            let key = format!("step:{}:completed", s.step);
            self.event(run, &mut s, "step_completed", key, json!({}))
                .await?;
            self.checkpoint(run, &mut s).await?;
            if let Some(reason) = loop_reason(&s.history) {
                self.transition(run, &mut s, Status::Paused, Some(reason))
                    .await?;
                return Ok(s);
            }
        }
    }
    async fn execute_tool(&self, run: &str, job: ToolJob) -> Option<AttemptFailure> {
        let ToolJob {
            index,
            step,
            key,
            plan,
            prepared,
            call,
            recovery,
            timeout,
        } = job;
        let _locks = resource_locks(&plan).await;
        let tool_started = std::time::Instant::now();
        match self.journal.is_running(run).await {
            Ok(true) => {}
            Ok(false) => return None,
            Err(error) => {
                return Some(AttemptFailure {
                    retryable: true,
                    reason: format!("could not check tool admission: {error}"),
                });
            }
        }
        let prepared = if let Some(prepared) = prepared {
            prepared
        } else {
            let attempt = tokio::time::timeout(
                Duration::from_secs(timeout),
                AssertUnwindSafe(self.prepare(run, call.clone())).catch_unwind(),
            )
            .await;
            let attempt = match attempt {
                Ok(Ok(result)) => result,
                _ => Err(
                    "tool preparation timed out or panicked; requested effect was not admitted"
                        .into(),
                ),
            };
            let prepared = match attempt {
                Ok(prepared)
                    if prepared.call.id == call.id
                        && prepared.call.name == call.name
                        && covers(&plan, &prepared) =>
                {
                    prepared
                }
                other => {
                    if self.tools.pending_approval(run).await
                        || self.tools.approval_blocked(run).await.is_some()
                    {
                        return None;
                    }
                    let reason=match other{Err(error)=>error,Ok(_)=>"resource plan or call identity changed before admission; retry with current state".into()};
                    return self.append_safe(run,"tool_result",&format!("{key}:result"),json!({"index":index,"name":call.name,"result":ToolResult::error(reason),"duration_ms":tool_started.elapsed().as_millis().min(u64::MAX as u128) as u64})).await.err().map(|error|AttemptFailure{retryable:true,reason:format!("rejection could not be recorded: {error}")});
                }
            };
            let payload = json!({"index":index,"prepared":prepared});
            let payload = match self.sanitize(&payload) {
                Ok(safe) if safe == payload => safe,
                _ => {
                    return Some(AttemptFailure {
                        retryable: false,
                        reason: "secret literal in prepared operation; effect was not admitted"
                            .into(),
                    });
                }
            };
            self.point(Boundary::BeforeToolIntent, run, step, Some(&key));
            match self
                .journal
                .append_when_running(run, "tool_intent", &key, payload)
                .await
            {
                Ok(Some(_)) => {}
                Ok(None) => return None,
                Err(error) => {
                    return Some(AttemptFailure {
                        retryable: true,
                        reason: format!("intention could not be confirmed: {error}"),
                    });
                }
            }
            if let Err(error) = self.flush_events(run).await {
                return Some(AttemptFailure {
                    retryable: true,
                    reason: error.to_string(),
                });
            }
            self.point(Boundary::ToolIntentConfirmed, run, step, Some(&key));
            prepared
        };
        let outcome = if recovery {
            match tokio::time::timeout(
                Duration::from_secs(timeout),
                AssertUnwindSafe(self.reconcile(run, &key, &prepared)).catch_unwind(),
            )
            .await
            {
                Ok(Ok(outcome)) => outcome,
                _ => Reconcile::Uncertain("recovery probe timed out or panicked".into()),
            }
        } else {
            Reconcile::Retry
        };
        let result = match outcome {
            Reconcile::Done(result) => result,
            Reconcile::Uncertain(reason) => {
                return Some(AttemptFailure {
                    retryable: false,
                    reason,
                });
            }
            Reconcile::Retry => match tokio::time::timeout(
                Duration::from_secs(timeout),
                AssertUnwindSafe(self.execute(run, &key, &prepared)).catch_unwind(),
            )
            .await
            {
                Ok(Ok(result)) => result,
                _ if prepared.effect == Effect::Pure => {
                    ToolResult::error("read-only tool interrupted or timed out")
                }
                _ => {
                    return Some(AttemptFailure {
                        retryable: prepared.effect != Effect::NonIdempotent,
                        reason: "tool interrupted or timed out without a confirmed result".into(),
                    });
                }
            },
        };
        if self.tools.result_requires_approval(&result) {
            return None;
        }
        self.point(Boundary::ToolEffectReturned, run, step, Some(&key));
        if let Err(error) = self
            .append_safe(
                run,
                "tool_result",
                &format!("{key}:result"),
                json!({"index":index,"name":prepared.call.name,"result":result,"duration_ms":tool_started.elapsed().as_millis().min(u64::MAX as u128) as u64}),
            )
            .await
        {
            return Some(AttemptFailure {
                retryable: prepared.effect != Effect::NonIdempotent,
                reason: format!("result could not be confirmed: {error}"),
            });
        }
        self.point(Boundary::ToolResultConfirmed, run, step, Some(&key));
        None
    }
    async fn checkpoint(&self, run: &str, s: &mut Snapshot) -> Result<()> {
        if s.step > 0 && s.step.is_multiple_of(20) {
            let payload = serde_json::to_value(&*s)?;
            let key = format!("checkpoint:{}", s.step);
            self.event(run, s, "checkpoint", key, payload).await?;
            self.point(Boundary::CheckpointConfirmed, run, s.step, None);
        }
        Ok(())
    }
    async fn scheduling(&self, run: &str, call: &ToolCall, timeout: u64) -> ToolScheduling {
        if episode_utilities::is_builtin(&call.name)
            || matches!(
                call.name.as_str(),
                "leave_continuation_note" | "get_tool_output"
            )
        {
            return ToolScheduling {
                resources: Vec::new(),
                exclusive: false,
            };
        }
        match tokio::time::timeout(
            Duration::from_secs(timeout),
            AssertUnwindSafe(self.tools.scheduling(run, call)).catch_unwind(),
        )
        .await
        {
            Ok(Ok(Some(plan))) => plan,
            _ => ToolScheduling::isolated(),
        }
    }
    async fn prepare(
        &self,
        run: &str,
        call: ToolCall,
    ) -> std::result::Result<PreparedTool, String> {
        if episode_utilities::is_builtin(&call.name) {
            return episode_utilities::prepare(call);
        }
        if matches!(
            call.name.as_str(),
            "leave_continuation_note" | "get_tool_output"
        ) {
            let data: Value = serde_json::from_str(&call.arguments)
                .or_else(|_| json5::from_str(&call.arguments))
                .map_err(|e| format!("invalid tool JSON: {e}"))?;
            let object = data.as_object().ok_or("tool arguments must be an object")?;
            let allowed = if call.name == "leave_continuation_note" {
                vec!["note"]
            } else {
                vec!["hash", "offset", "limit"]
            };
            if object.keys().any(|k| !allowed.contains(&k.as_str())) {
                return Err("unknown tool argument".into());
            }
            if data.get("offset").is_some_and(|n| n.as_u64().is_none())
                || data
                    .get("limit")
                    .is_some_and(|n| n.as_u64().is_none_or(|n| !(1..=1048576).contains(&n)))
            {
                return Err("offset/limit must be bounded nonnegative integers".into());
            }
            if call.name == "leave_continuation_note"
                && data["note"].as_str().is_none_or(|s| s.len() > 8192)
            {
                return Err("note string required, max 8192 bytes".into());
            }
            if call.name == "get_tool_output"
                && data["hash"]
                    .as_str()
                    .is_none_or(|s| s.parse::<excalibur_store::BlobHash>().is_err())
            {
                return Err("valid blob hash required".into());
            }
            Ok(PreparedTool {
                call,
                effect: Effect::Pure,
                resources: Vec::new(),
                exclusive: false,
                data,
            })
        } else {
            self.tools.prepare(run, call).await
        }
    }
    async fn execute(&self, run: &str, key: &str, tool: &PreparedTool) -> ToolResult {
        let builtin = episode_utilities::is_builtin(&tool.call.name)
            || matches!(
                tool.call.name.as_str(),
                "leave_continuation_note" | "get_tool_output"
            );
        if builtin
            && self
                .tools
                .audit_builtin(run, &tool.call, None)
                .await
                .is_err()
        {
            return ToolResult::error("builtin audit admission failed");
        }
        let result = match tool.call.name.as_str() {
            "leave_continuation_note" => ToolResult::ok(json!({"note":tool.data["note"]})),
            "get_tool_output" => {
                let hash = tool.data["hash"].as_str().unwrap().parse().unwrap();
                use base64::Engine as _;
                let offset = tool.data["offset"].as_u64().unwrap_or(0);
                let limit = tool.data["limit"]
                    .as_u64()
                    .unwrap_or(8192)
                    .clamp(1, 1024 * 1024) as usize;
                match self
                    .journal
                    .database()
                    .read_blob_window_inspected(
                        hash,
                        offset,
                        limit,
                        self.tools.durable_blob_inspector(),
                    )
                    .await
                {
                    Ok(window) => ToolResult::ok(json!({
                        "text":std::str::from_utf8(&window.bytes).ok(),
                        "bytes_base64":base64::engine::general_purpose::STANDARD.encode(&window.bytes),
                        "offset":window.offset,"next_offset":window.offset+window.bytes.len() as u64,
                        "total_bytes":window.total_bytes
                    })),
                    Err(e) => ToolResult::error(e.to_string()),
                }
            }
            name if episode_utilities::is_builtin(name) => {
                episode_utilities::execute(&self.journal, self.tools.as_ref(), run, key, tool).await
            }
            _ => self.tools.execute(run, key, tool).await,
        };
        if builtin
            && self
                .tools
                .audit_builtin(run, &tool.call, Some(&result))
                .await
                .is_err()
        {
            return ToolResult::error("builtin result audit failed");
        }
        result
    }
    async fn reconcile(&self, run: &str, key: &str, tool: &PreparedTool) -> Reconcile {
        match tool.effect {
            Effect::Pure | Effect::Keyed => Reconcile::Retry,
            Effect::Idempotent | Effect::NonIdempotent => {
                self.tools.reconcile(run, key, tool).await
            }
        }
    }
}
pub fn builtin_tool_definitions() -> Vec<ToolDefinition> {
    builtin_definitions()
}
fn builtin_definitions() -> Vec<ToolDefinition> {
    let mut definitions = episode_utilities::definitions();
    definitions.extend(vec![
        ToolDefinition {
            name: "leave_continuation_note".into(),
            description: "Save or clear a brief note for subsequent model calls and recovery."
                .into(),
            input_schema: json!({"type":"object","required":["note"],"properties":{"note":{"type":"string","maxLength":8192}},"additionalProperties":false}),
        },
        ToolDefinition {
            name: "get_tool_output".into(),
            description: "Read a bounded byte window of a stored full tool output.".into(),
            input_schema: json!({"type":"object","required":["hash"],"properties":{"hash":{"type":"string"},"offset":{"type":"integer","minimum":0},"limit":{"type":"integer","minimum":1,"maximum":1048576}},"additionalProperties":false}),
        },
    ]);
    definitions
}
struct ToolJob {
    index: usize,
    step: u64,
    key: String,
    call: ToolCall,
    plan: ToolScheduling,
    prepared: Option<PreparedTool>,
    recovery: bool,
    timeout: u64,
}
struct AttemptFailure {
    retryable: bool,
    reason: String,
}
fn tool_key(run: &str, step: u64, index: usize) -> String {
    format!("{run}:step:{step}:tool:{index}")
}
fn loop_reason(history: &[Message]) -> Option<String> {
    let mut signature = None;
    let mut failed_steps = 0;
    let mut failed_results = Vec::new();
    for message in history.iter().rev() {
        if message.role == "system" {
            break;
        }
        if message.role == "tool" {
            let result: ToolResult = serde_json::from_str(&message.content).ok()?;
            let detail = result
                .output
                .get("error")
                .and_then(Value::as_str)
                .and_then(|text| serde_json::from_str::<Value>(text).ok());
            if failed_steps == 0
                && (result.output["escalate_to_parent"] == true
                    || detail
                        .as_ref()
                        .is_some_and(|v| v["escalate_to_parent"] == true))
            {
                return Some(
                    "file fingerprint conflicts require parent or host intervention".into(),
                );
            }
            failed_results.push(!result.success);
        } else if message.role == "assistant" && !message.tool_calls.is_empty() {
            if failed_results.len() != message.tool_calls.len() || failed_results.iter().any(|v| !v)
            {
                return None;
            }
            let current: Vec<_> = message
                .tool_calls
                .iter()
                .map(|call| (&call.name, &call.arguments))
                .collect();
            let current = serde_json::to_string(&current).ok()?;
            if signature.as_ref().is_some_and(|s| s != &current) {
                return None;
            }
            signature = Some(current);
            failed_results.clear();
            failed_steps += 1;
            if failed_steps >= 3 {
                return Some(
                    "three identical unsuccessful tool steps require host intervention".into(),
                );
            }
        }
    }
    None
}
fn remaining_time(s: &Snapshot) -> u64 {
    s.spec
        .budget
        .max_wall_seconds
        .saturating_sub(unix_seconds().saturating_sub(s.started_at).max(0) as u64)
}
fn budget_exhausted(s: &Snapshot) -> bool {
    s.step >= s.spec.budget.max_steps
        || s.usage.input_tokens.saturating_add(s.usage.output_tokens) >= s.spec.budget.max_tokens
        || s.spec
            .budget
            .max_cost_microusd
            .is_some_and(|max| s.usage.cost_microusd >= max)
        || remaining_time(s) == 0
}
fn closing(s: &Snapshot) -> bool {
    let b = &s.spec.budget;
    let used = s.usage.input_tokens.saturating_add(s.usage.output_tokens);
    s.tool_calls >= b.max_tool_calls
        || b.max_steps.saturating_sub(s.step) <= 1
        || b.max_steps.saturating_sub(s.step).saturating_mul(10) < b.max_steps
        || b.max_tokens.saturating_sub(used).saturating_mul(10) < b.max_tokens
        || b.max_cost_microusd
            .is_some_and(|max| max.saturating_sub(s.usage.cost_microusd).saturating_mul(10) < max)
        || remaining_time(s).saturating_mul(10) < b.max_wall_seconds
}
fn covers(plan: &ToolScheduling, tool: &PreparedTool) -> bool {
    plan.exclusive
        || (!tool.exclusive
            && tool.resources.iter().all(|actual| {
                plan.resources
                    .iter()
                    .any(|declared| declared.key == actual.key && (!actual.write || declared.write))
            }))
}
fn conflict(a: &ToolScheduling, b: &ToolScheduling) -> bool {
    a.exclusive
        || b.exclusive
        || a.resources.iter().any(|a| {
            b.resources
                .iter()
                .any(|b| a.key == b.key && (a.write || b.write))
        })
}
fn batches(actions: Vec<(usize, ToolScheduling)>) -> Vec<Vec<(usize, ToolScheduling)>> {
    let mut result = Vec::new();
    let mut current: Vec<(usize, ToolScheduling)> = Vec::new();
    for action in actions {
        if current.iter().any(|a| conflict(&a.1, &action.1)) {
            result.push(std::mem::take(&mut current));
        }
        let exclusive = action.1.exclusive;
        current.push(action);
        if exclusive {
            result.push(std::mem::take(&mut current));
        }
    }
    if !current.is_empty() {
        result.push(current);
    }
    result
}
type ResourceRegistry = Mutex<BTreeMap<String, Weak<tokio::sync::RwLock<()>>>>;
static RESOURCES: OnceLock<ResourceRegistry> = OnceLock::new();
static EXCLUSIVE: OnceLock<Arc<tokio::sync::RwLock<()>>> = OnceLock::new();
#[allow(dead_code)]
enum ResourceGuard {
    Read(tokio::sync::OwnedRwLockReadGuard<()>),
    Write(tokio::sync::OwnedRwLockWriteGuard<()>),
}
async fn batch_gate(exclusive: bool) -> ResourceGuard {
    let global = EXCLUSIVE
        .get_or_init(|| Arc::new(tokio::sync::RwLock::new(())))
        .clone();
    if exclusive {
        ResourceGuard::Write(global.write_owned().await)
    } else {
        ResourceGuard::Read(global.read_owned().await)
    }
}
async fn resource_locks(tool: &ToolScheduling) -> Vec<ResourceGuard> {
    let mut held = Vec::new();
    let mut keys = BTreeMap::new();
    for r in &tool.resources {
        keys.entry(r.key.clone())
            .and_modify(|write| *write |= r.write)
            .or_insert(r.write);
    }
    for (key, write) in keys {
        let lock = {
            let mut registry = RESOURCES.get_or_init(Default::default).lock().unwrap();
            registry.retain(|_, v| v.strong_count() > 0);
            if let Some(lock) = registry.get(&key).and_then(Weak::upgrade) {
                lock
            } else {
                let lock = Arc::new(tokio::sync::RwLock::new(()));
                registry.insert(key, Arc::downgrade(&lock));
                lock
            }
        };
        held.push(if write {
            ResourceGuard::Write(lock.write_owned().await)
        } else {
            ResourceGuard::Read(lock.read_owned().await)
        });
    }
    held
}
