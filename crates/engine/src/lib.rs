//! A durable single-run step engine. Applications embed it; no CLI or HTTP server.
mod allocations;
mod completion;
mod context_runtime;
mod episode_utilities;
mod loops;
pub use allocations::{
    AllocatedUsage, ChildBudgetAllocation, remaining_budget, remaining_budget_for,
    reserve_child_budget, settle_child_budget,
};
pub use completion::*;
use excalibur_context::ContextInput;
use excalibur_journal::{Event, Journal};
use excalibur_models::ModelGateway;
use excalibur_store::unix_seconds;
use excalibur_types::runtime::*;
use futures::{FutureExt, future::join_all};
pub use loops::{LoopDetection, LoopRule, LoopState};
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_processes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_terminal_sessions: Option<u64>,
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
            max_processes: None,
            max_terminal_sessions: None,
        }
    }
}
impl Budget {
    pub fn fits_within(&self, cap: &Self) -> bool {
        self.max_steps <= cap.max_steps
            && self.max_tokens <= cap.max_tokens
            && self.max_wall_seconds <= cap.max_wall_seconds
            && self.max_tool_calls <= cap.max_tool_calls
            && self.tool_timeout_seconds <= cap.tool_timeout_seconds
            && cap
                .max_cost_microusd
                .is_none_or(|max| self.max_cost_microusd.is_some_and(|cost| cost <= max))
            && cap
                .max_processes
                .is_none_or(|max| self.max_processes.is_some_and(|count| count <= max))
            && cap
                .max_terminal_sessions
                .is_none_or(|max| self.max_terminal_sessions.is_some_and(|count| count <= max))
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
    Created,
    Ready,
    Running,
    Waiting,
    Verifying,
    Recovering,
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
            Self::Created => "created",
            Self::Ready => "ready",
            Self::Running => "running",
            Self::Waiting => "waiting",
            Self::Verifying => "verifying",
            Self::Recovering => "recovering",
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
    #[serde(default)]
    pub gate_assessment: GateAssessment,
    #[serde(default)]
    pub delivery_report: Option<DeliveryReport>,
    #[serde(default)]
    pub model_calls: u64,
    #[serde(default)]
    pub last_gate_step: Option<u64>,
    #[serde(default)]
    pub loops: LoopState,
    #[serde(default)]
    pub child_allocations: BTreeMap<String, ChildBudgetAllocation>,
    #[serde(default)]
    pub last_loop_step: Option<u64>,
    #[serde(default)]
    pub confirmed_model_failure: Option<Value>,
    #[serde(default)]
    pub request_revision: u64,
    #[serde(default)]
    pub context_session: Option<Box<excalibur_context::ContextSession>>,
    #[serde(default)]
    pub context_policy: Option<excalibur_context::SessionPolicy>,
    #[serde(default)]
    pub context_history_cursor: usize,
    #[serde(default)]
    pub context_instruction_digest: Option<String>,
    #[serde(default)]
    pub context_summaries: BTreeMap<String, excalibur_context::SummaryOutcome>,
    #[serde(default)]
    pub pending_context_summary: Option<Value>,
}
impl Snapshot {
    fn started(seed: &Value) -> Result<Self> {
        Ok(Self {
            spec: serde_json::from_value(seed["spec"].clone())?,
            status: if seed["initial_state"] == "created" {
                Status::Created
            } else {
                Status::Running
            },
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
            gate_assessment: GateAssessment::default(),
            delivery_report: None,
            model_calls: 0,
            last_gate_step: None,
            loops: LoopState::default(),
            child_allocations: BTreeMap::new(),
            last_loop_step: None,
            confirmed_model_failure: None,
            request_revision: 0,
            context_session: None,
            context_policy: seed
                .get("context_policy")
                .cloned()
                .map(serde_json::from_value)
                .transpose()?,
            context_history_cursor: 0,
            context_instruction_digest: None,
            context_summaries: BTreeMap::new(),
            pending_context_summary: None,
        })
    }
    fn apply(&mut self, event: &Event) -> Result<()> {
        let p = &event.payload;
        match event.kind.as_str() {
            "run_started" => {}
            "model_request" => {
                self.confirmed_model_failure = None;
                self.request = Some(serde_json::from_value(p["request"].clone())?);
                self.request_revision = p["revision"].as_u64().unwrap_or(self.revision);
            }
            "model_response" => {
                let response: ModelResponse = serde_json::from_value(p["response"].clone())?;
                if let (Some(session), Some(request)) = (&mut self.context_session, &self.request) {
                    session.observe_usage(
                        request,
                        response.usage.input_tokens,
                        response.usage.estimated,
                    );
                }
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
                    self.gate_assessment.evidence.clear();
                    self.gate_report.clear();
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
                        provider_state: None,
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
                self.gate_report = serde_json::from_value(p["pending"].clone())?;
                if self.last_gate_step != Some(self.step) && !self.gate_report.is_empty() {
                    self.gate_attempts += 1;
                }
                self.last_gate_step = Some(self.step);
                self.final_text = p["final_text"].as_str().map(str::to_owned);
                if let Some(assessment) = p.get("assessment") {
                    self.gate_assessment = serde_json::from_value(assessment.clone())?;
                }
            }
            "delivery_report" => {
                let report: DeliveryReport = serde_json::from_value(p.clone())?;
                if report.status == self.status && report.reason == self.reason {
                    self.delivery_report = Some(report);
                }
            }
            "loop_observation" => {
                self.loops = serde_json::from_value(p["state"].clone())?;
                self.last_loop_step = p["step"].as_u64();
            }
            "loop_detection" => {}
            "child_budget_reserved" => {
                let id = p["allocation_id"]
                    .as_str()
                    .ok_or(Error::Invalid("allocation ID missing"))?;
                self.child_allocations
                    .insert(id.into(), serde_json::from_value(p["allocation"].clone())?);
            }
            "child_budget_settled" => {
                let id = p["allocation_id"]
                    .as_str()
                    .ok_or(Error::Invalid("allocation ID missing"))?;
                self.child_allocations
                    .get_mut(id)
                    .ok_or(Error::Invalid("settled allocation missing"))?
                    .settled = Some(serde_json::from_value(p["usage"].clone())?);
            }
            "model_interrupted" => {
                self.confirmed_model_failure = None;
                let usage: Usage = serde_json::from_value(p["estimated_usage"].clone())?;
                self.usage.input_tokens =
                    self.usage.input_tokens.saturating_add(usage.input_tokens);
                self.usage.output_tokens =
                    self.usage.output_tokens.saturating_add(usage.output_tokens);
                self.usage.cost_microusd =
                    self.usage.cost_microusd.saturating_add(usage.cost_microusd);
                self.usage.estimated |= usage.estimated;
                self.request = None;
                self.response = None;
                self.calls.clear();
                self.step += 1;
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
                self.gate_attempts = 0;
                self.loops = LoopState::default();
                self.last_loop_step = None;
                self.history.push(Message::text(
                    "system",
                    format!("Context update v{}: {text}", self.revision),
                ));
                self.host_checks.clear();
                self.gate_assessment.evidence.clear();
                self.gate_report.clear();
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
            "model_dispatch" => self.model_calls += 1,
            "verifier_model_request" => self.request = Some(serde_json::from_value(p.clone())?),
            "verifier_model_response" => {
                let response: ModelResponse = serde_json::from_value(p.clone())?;
                self.final_text = Some(response.message.content);
                self.request = None;
            }
            "verifier_tool_result" => {}
            "verifier_model_failure" => {
                if p["receipt_confirmed"] == true
                    || (p["dispatch_absent"] == true
                        && p.pointer("/metrics/provider_calls_known") == Some(&Value::Bool(true))
                        && p.pointer("/metrics/provider_calls").and_then(Value::as_u64) == Some(0))
                {
                    self.request = None;
                }
            }
            "verification_interruption_failed" => {
                self.gate_assessment.open_questions.push("A verifier interruption could not be confirmed; inspect child state before resuming".into());
                self.gate_report
                    .push("Verifier interruption requires host reconciliation".into());
            }
            "verification_usage" => {
                if p["receipt_confirmed"] == true {
                    self.request = None;
                }
                let usage: Usage = serde_json::from_value(p["usage"].clone())?;
                self.usage.input_tokens =
                    self.usage.input_tokens.saturating_add(usage.input_tokens);
                self.usage.output_tokens =
                    self.usage.output_tokens.saturating_add(usage.output_tokens);
                self.usage.cost_microusd =
                    self.usage.cost_microusd.saturating_add(usage.cost_microusd);
                self.usage.estimated |= usage.estimated;
                self.model_calls = self
                    .model_calls
                    .saturating_add(p["model_calls"].as_u64().unwrap_or(0));
                self.tool_calls = self
                    .tool_calls
                    .saturating_add(p["tool_calls"].as_u64().unwrap_or(0));
                self.step = self.step.saturating_add(
                    p["step_count"]
                        .as_u64()
                        .unwrap_or_else(|| p["model_calls"].as_u64().unwrap_or(0)),
                );
            }
            "model_failure" => self.confirmed_model_failure = Some(p.clone()),
            "context_prepared" => {
                self.context_session = Some(serde_json::from_value(p["session"].clone())?);
                self.context_instruction_digest =
                    p["instruction_digest"].as_str().map(str::to_owned);
                self.context_history_cursor = p["history_cursor"]
                    .as_u64()
                    .ok_or(Error::Invalid("context history cursor missing"))?
                    as usize;
            }
            "context_summary_started" => self.pending_context_summary = Some(p.clone()),
            "context_summary_completed" => {
                let id = p["id"]
                    .as_str()
                    .ok_or(Error::Invalid("summary identity missing"))?;
                self.context_summaries
                    .insert(id.into(), serde_json::from_value(p["outcome"].clone())?);
                self.pending_context_summary = None;
                let usage: Usage = serde_json::from_value(p["usage"].clone())?;
                self.usage.input_tokens =
                    self.usage.input_tokens.saturating_add(usage.input_tokens);
                self.usage.output_tokens =
                    self.usage.output_tokens.saturating_add(usage.output_tokens);
                self.usage.cost_microusd =
                    self.usage.cost_microusd.saturating_add(usage.cost_microusd);
                self.usage.estimated |= usage.estimated;
                self.model_calls = self
                    .model_calls
                    .saturating_add(p["model_calls"].as_u64().unwrap_or(0));
            }
            "checkpoint" | "wake_event" | "context_observation_archived" => {}
            _ => return Err(Error::Invalid("unknown durable event")),
        }
        self.last_sequence = event.sequence;
        Ok(())
    }
    pub fn pending_criteria(&self) -> Vec<String> {
        let mut pending: Vec<String> = self
            .spec
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
            .collect();
        if self.response.is_some() && self.request_revision != self.revision {
            pending.push("Final response precedes the latest instruction revision".into());
        }
        pending
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
    ContextPreparedConfirmed,
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
    completion_gate: Option<Arc<dyn CompletionGate>>,
    context_policy: excalibur_context::SessionPolicy,
    context_extractor: Option<Arc<dyn ModelGateway>>,
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
            completion_gate: None,
            context_policy: excalibur_context::SessionPolicy::default(),
            context_extractor: None,
        }
    }
    pub fn with_completion_gate(mut self, gate: Arc<dyn CompletionGate>) -> Self {
        self.completion_gate = Some(gate);
        self
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
    pub fn create<'a>(
        &'a self,
        operation: &'a str,
        spec: RunSpec,
    ) -> BoxFuture<'a, Result<String>> {
        Box::pin(self.create_inner(operation, spec))
    }
    async fn create_inner(&self, operation: &str, mut spec: RunSpec) -> Result<String> {
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
        excalibur_context::ContextSession::new(
            spec.context.clone(),
            self.effective_context_policy()?,
        )
        .map_err(|_| Error::Invalid("invalid initial context"))?;
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
            let original_policy: Option<excalibur_context::SessionPolicy> = seed
                .payload
                .get("context_policy")
                .cloned()
                .map(serde_json::from_value)
                .transpose()?;
            let effective_policy = self.effective_context_policy()?;
            if original != spec
                || state.gateway_identity != self.model.identity()
                || original_policy.is_some_and(|policy| policy != effective_policy)
            {
                return Err(Error::Invalid(
                    "operation reused with different run specification",
                ));
            }
            self.flush_events(&run).await?;
            return Ok(run);
        }
        let seed = json!({"spec":spec,"started_at":unix_seconds(),"gateway_identity":self.model.identity(),"model_dispatch_protocol":1,"initial_state":"created","context_policy":self.effective_context_policy()?});
        if self.sanitize(&seed)? != seed {
            return Err(Error::Invalid("secret literal in durable run identity"));
        }
        let run = self.journal.create_run(operation, seed).await?;
        let mut state = self.load(&run).await?;
        self.transition_from(&run, &mut state, &["created"], Status::Ready, None)
            .await?;
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
        self.transition_from(run, state, &["running", "verifying"], status, reason)
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
        if matches!(status, Status::Completed | Status::CompletedWithPending)
            && (!state.pending_directives.is_empty() || state.request_revision != state.revision)
        {
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
        let applied = if matches!(status, Status::Completed | Status::CompletedWithPending) {
            self.journal
                .set_state_if_revision(
                    run,
                    expected,
                    state.last_sequence,
                    status.label(),
                    reason,
                    &key,
                )
                .await?
        } else {
            self.journal
                .set_state_if(run, expected, status.label(), reason, &key)
                .await?
        };
        self.flush_events(run).await?;
        *state = self.load(run).await?;
        if applied
            && matches!(
                state.status,
                Status::Paused
                    | Status::Blocked
                    | Status::Waiting
                    | Status::Completed
                    | Status::CompletedWithPending
                    | Status::Cancelled
                    | Status::Failed
            )
        {
            self.emit_delivery_report(run, state).await?;
        }
        Ok(applied)
    }
    async fn emit_delivery_report(&self, run: &str, state: &mut Snapshot) -> Result<()> {
        let report = completion::build_report(&self.journal, run, state).await?;
        let key = format!("delivery:{}", state.last_sequence);
        self.event(
            run,
            state,
            "delivery_report",
            key,
            serde_json::to_value(report)?,
        )
        .await
    }
    /// Host finalization of an unrecoverable execution error; never overwrites
    /// user controls or an already completed run.
    pub async fn fail(&self, run: &str, reason: &str) -> Result<()> {
        let mut state = self.load(run).await?;
        if state.status == Status::Ready {
            self.transition_from(run, &mut state, &["ready"], Status::Running, None)
                .await?;
        }
        self.transition_from(
            run,
            &mut state,
            &["ready", "running", "waiting", "verifying", "recovering"],
            Status::Failed,
            Some(reason.into()),
        )
        .await?;
        Ok(())
    }
    pub async fn block(&self, run: &str, reason: &str) -> Result<()> {
        let mut state = self.load(run).await?;
        if state.status == Status::Ready {
            self.transition_from(run, &mut state, &["ready"], Status::Running, None)
                .await?;
        }
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
                &[
                    "ready",
                    "running",
                    "waiting",
                    "verifying",
                    "recovering",
                    "paused",
                    "blocked",
                ],
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
                &[
                    "created",
                    "ready",
                    "running",
                    "waiting",
                    "verifying",
                    "recovering",
                    "paused",
                    "blocked",
                ],
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
            .transition_from(run, &mut s, &["paused"], Status::Ready, None)
            .await?
        {
            return Err(Error::Invalid("only paused runs can resume"));
        }
        Ok(())
    }
    /// Host-declared wait. Calling run again does not wake it or spend model tokens.
    pub async fn wait(&self, run: &str, reason: WaitReason) -> Result<()> {
        let mut state = self.load(run).await?;
        let reason = serde_json::to_value(reason)?.as_str().unwrap().to_owned();
        if !self
            .transition_from(run, &mut state, &["running"], Status::Waiting, Some(reason))
            .await?
        {
            return Err(Error::Invalid("only a running execution can wait"));
        }
        Ok(())
    }
    /// A durable external event resumes a wait, without reopening user-paused runs.
    pub async fn wake(&self, run: &str, event: WakeEvent, evidence: &str) -> Result<()> {
        if evidence.trim().is_empty() {
            return Err(Error::Invalid("wake evidence required"));
        }
        let mut state = self.load(run).await?;
        if state.status != Status::Waiting {
            return Err(Error::Invalid("only waiting executions can wake"));
        }
        let key = format!("wake:{}", state.last_sequence);
        self.event(
            run,
            &mut state,
            "wake_event",
            key,
            json!({"event":event,"evidence":evidence}),
        )
        .await?;
        if !self
            .transition_from(run, &mut state, &["waiting"], Status::Running, None)
            .await?
        {
            return Err(Error::Invalid("control state prevented waking"));
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
        let cap=self.journal.database().read(excalibur_store::ReadLane::Interactive,{
            let run=run.to_owned();move|c|{use excalibur_store::rusqlite::OptionalExtension;Ok(c.query_row("SELECT json_extract(payload,'$.allocation.budget') FROM journal WHERE kind='child_budget_reserved' AND json_extract(payload,'$.allocation.child_run')=?1 LIMIT 1",[run],|r|r.get::<_,String>(0)).optional()?)}
        }).await.map_err(excalibur_journal::Error::from)?;
        if cap.as_ref().is_some_and(|json| {
            !serde_json::from_str::<Budget>(json).is_ok_and(|cap| budget.fits_within(&cap))
        }) {
            return Err(Error::Invalid(
                "child budget cannot exceed its parent allocation",
            ));
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
        let mut state = self.load(run).await?;
        if state.status == Status::Waiting {
            self.wake(run, WakeEvent::Directive, id).await?;
        } else if state.status == Status::Blocked {
            self.transition_from(run, &mut state, &["blocked"], Status::Ready, None)
                .await?;
        }
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
            .transition_from(run, &mut s, &["blocked"], Status::Ready, None)
            .await?
        {
            return Err(Error::Invalid(
                "reconciled outcome recorded but control state prevents resuming",
            ));
        }
        Ok(())
    }
    /// Explicit host recovery of a confirmed model failure whose stop transition
    /// was interrupted. Account from a receipt or a conservative reservation;
    /// unknown charge is never silently converted into a measured zero.
    pub async fn resolve_model_failure(
        &self,
        run: &str,
        accounted_usage: Usage,
        evidence: &str,
    ) -> Result<()> {
        let _guard = self.journal.lock_run(run).await;
        let mut state = self.load(run).await?;
        if !matches!(state.status, Status::Paused | Status::Blocked)
            || state.confirmed_model_failure.is_none()
            || state.request.is_none()
            || state.response.is_some()
            || !state.calls.is_empty()
            || evidence.trim().is_empty()
        {
            return Err(Error::Invalid(
                "explicit evidence and a paused confirmed model failure required",
            ));
        }
        let key = format!("model:{}:interrupted", state.step);
        self.event(
            run,
            &mut state,
            "model_interrupted",
            key,
            json!({"estimated_usage":accounted_usage,"reconciliation_evidence":evidence}),
        )
        .await?;
        self.transition_from(run, &mut state, &["paused", "blocked"], Status::Ready, None)
            .await?;
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
    async fn honor_admission(&self, run: &str, state: &mut Snapshot) -> Result<bool> {
        let Some(gate) = &self.completion_gate else {
            return Ok(false);
        };
        let admission = gate.admission(run).await.unwrap_or_else(|_| {
            AdmissionState::Blocked("execution admission authority unavailable".into())
        });
        let (status, reason) = match admission {
            AdmissionState::Ready => return Ok(false),
            AdmissionState::Paused(reason) => (Status::Paused, reason),
            AdmissionState::Blocked(reason) => (Status::Blocked, reason),
            AdmissionState::Cancelled(reason) => (Status::Cancelled, reason),
        };
        if state.status == Status::Ready && status == Status::Blocked {
            self.transition_from(run, state, &["ready"], Status::Running, None)
                .await?;
        }
        self.transition_from(
            run,
            state,
            &["ready", "running", "verifying", "recovering"],
            status,
            Some(reason),
        )
        .await?;
        Ok(true)
    }
    pub fn run<'a>(&'a self, run: &'a str) -> BoxFuture<'a, Result<Snapshot>> {
        Box::pin(self.run_inner(run))
    }
    async fn run_inner(&self, run: &str) -> Result<Snapshot> {
        match Box::pin(self.drive(run)).await {
            Ok(mut state) => {
                if matches!(
                    state.status,
                    Status::Paused
                        | Status::Blocked
                        | Status::Waiting
                        | Status::Completed
                        | Status::CompletedWithPending
                        | Status::Failed
                        | Status::Cancelled
                ) && state.delivery_report.as_ref().is_none_or(|report| {
                    report.source_sequence.saturating_add(1) < state.last_sequence
                        || report.status != state.status
                        || report.reason != state.reason
                        || report.metrics.usage != state.usage
                        || report.metrics.tool_calls != state.tool_calls
                        || report.metrics.model_calls != state.model_calls
                }) {
                    self.emit_delivery_report(run, &mut state).await?;
                }
                Ok(state)
            }
            Err(error) => {
                if matches!(error, Error::Model(_)) {
                    self.fail(
                        run,
                        "model execution failed; inspect the classified model_failure event",
                    )
                    .await?;
                }
                Err(error)
            }
        }
    }
    async fn drive(&self, run: &str) -> Result<Snapshot> {
        let _guard = self.journal.lock_run(run).await;
        // Only intentions already present at admission need reconciliation.
        let mut initial = self.load(run).await?;
        if initial.gateway_identity != self.model.identity() {
            return Err(Error::Invalid("gateway profile differs from durable run"));
        }
        if initial.status == Status::Created {
            self.transition_from(run, &mut initial, &["created"], Status::Ready, None)
                .await?;
        }
        if self.completion_gate.is_none()
            && matches!(
                initial.status,
                Status::Ready | Status::Running | Status::Verifying | Status::Recovering
            )
        {
            let bound=self.journal.database().read(excalibur_store::ReadLane::Interactive,{
                let run=run.to_owned();move|c|{
                    let exists:bool=c.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='task_run_links')",[],|r|r.get(0))?;
                    if !exists{return Ok(false);}
                    Ok(c.query_row("SELECT EXISTS(SELECT 1 FROM task_run_links WHERE run_id=?1)",[run],|r|r.get::<_,bool>(0))?)
                }
            }).await.map_err(excalibur_journal::Error::from)?;
            if bound {
                if initial.status == Status::Ready {
                    self.transition_from(run, &mut initial, &["ready"], Status::Running, None)
                        .await?;
                }
                self.transition_from(
                    run,
                    &mut initial,
                    &["running", "verifying", "recovering"],
                    Status::Blocked,
                    Some(
                        "durable task completion adapter is required for this bound execution"
                            .into(),
                    ),
                )
                .await?;
                return Ok(initial);
            }
        }
        if matches!(
            initial.status,
            Status::Ready | Status::Running | Status::Verifying | Status::Recovering
        ) && self.honor_admission(run, &mut initial).await?
        {
            return Ok(initial);
        }
        if matches!(initial.status, Status::Running | Status::Verifying) {
            self.transition_from(
                run,
                &mut initial,
                &["running", "verifying"],
                Status::Recovering,
                Some("restart reconciliation".into()),
            )
            .await?;
        }
        if initial.status == Status::Recovering {
            self.transition_from(run, &mut initial, &["recovering"], Status::Ready, None)
                .await?;
        }
        if initial.status == Status::Ready {
            self.transition_from(run, &mut initial, &["ready"], Status::Running, None)
                .await?;
        }
        if initial.status == Status::Running {
            if initial.confirmed_model_failure.is_some()
                && initial.request.is_some()
                && initial.response.is_none()
            {
                self.transition(run,&mut initial,Status::Paused,Some("recovery: a model failure was already confirmed; provider calls will not repeat without an explicit host resolution".into())).await?;
                return Ok(initial);
            }
            let available = remaining_budget(&initial);
            let limits = ExecutionLimits {
                max_processes: available.max_processes,
                max_terminal_sessions: available.max_terminal_sessions,
            };
            if self
                .tools
                .configure_execution_limits(run, limits)
                .await
                .is_err()
            {
                self.transition(
                    run,
                    &mut initial,
                    Status::Paused,
                    Some("budget resource limits cannot be enforced by this adapter".into()),
                )
                .await?;
                return Ok(initial);
            }
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
        'execution: loop {
            self.flush_events(run).await?;
            let mut s = self.load(run).await?;
            if s.status != Status::Running {
                return Ok(s);
            }
            if self.honor_approval_wait(run, &mut s).await? {
                return Ok(s);
            }
            if self.honor_admission(run, &mut s).await? {
                return Ok(s);
            }
            if self.resource_limit_reached(run, &mut s).await? {
                return Ok(s);
            }
            let capability_tool_budget = self
                .tools
                .execution_tool_budget(run)
                .await
                .map_err(|_| Error::Invalid("execution capability unavailable"))?;
            self.refresh_context_instructions(run, &s).await?;
            s = self.load(run).await?;
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
            // A durable projection may enqueue another directive while the
            // previous application is being projected. Observe that fact before
            // compiling or reusing an earlier response.
            s = self.load(run).await?;
            if s.status != Status::Running {
                return Ok(s);
            }
            if !s.pending_directives.is_empty() {
                continue;
            }
            if s.request_revision != s.revision
                && let Some(response) = s.response.clone()
            {
                for (index, call) in response.message.tool_calls.iter().enumerate() {
                    if !s
                        .calls
                        .get(&index)
                        .is_some_and(|state| state.prepared.is_some() || state.result.is_some())
                    {
                        let key = format!("{}:result", tool_key(run, s.step, index));
                        self.event(run,&mut s,"tool_result",key,json!({"index":index,"name":call.name,"result":ToolResult::error("Tool was not executed: its response predates the current instruction revision")})).await?;
                    }
                }
            }
            if s.request_revision != s.revision
                && s.response.as_ref().is_some_and(|response| {
                    response
                        .message
                        .tool_calls
                        .iter()
                        .enumerate()
                        .all(|(index, _)| {
                            s.calls
                                .get(&index)
                                .is_some_and(|call| call.result.is_some())
                        })
                })
            {
                let key = format!("step:{}:completed", s.step);
                let revision = s.revision;
                self.event(
                    run,
                    &mut s,
                    "step_completed",
                    key,
                    json!({"superseded_by_instruction_revision":revision}),
                )
                .await?;
                self.checkpoint(run, &mut s).await?;
                continue;
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
                if let Some(gate) = &self.completion_gate {
                    input.volatile.current_plan = gate
                        .context(run)
                        .await
                        .map_err(|_| Error::Invalid("current task plan is unavailable"))?;
                }
                input.activity.extend(s.history.clone());
                input.volatile.now_utc = self
                    .model
                    .request_time_override(s.step)
                    .unwrap_or_else(|| unix_seconds().to_string());
                input.volatile.continuation = s.continuation.clone();
                if !s.loops.note.is_empty() {
                    input.volatile.recovery_notices.push(s.loops.note.clone());
                }
                input.volatile.budget_status = serde_json::to_string(
                    &json!({"step":s.step,"budget":s.spec.budget,"usage":s.usage,"tool_calls":s.tool_calls}),
                )?;
                if !s.child_allocations.is_empty() {
                    let mut budget_status: Value =
                        serde_json::from_str(&input.volatile.budget_status)?;
                    budget_status["child_allocations"] =
                        serde_json::to_value(&s.child_allocations)?;
                    input.volatile.budget_status = serde_json::to_string(&budget_status)?;
                }
                input.volatile.finalization_report = s.gate_report.join("\n");
                input.tools_enabled = !closing(&s)
                    && capability_tool_budget.is_none_or(|budget| s.tool_calls < budget);
                let compiled = match Box::pin(self.prepare_context(run, &mut s, input)).await {
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
                let compiled_revision = s.revision;
                s = self.load(run).await?;
                if s.status != Status::Running {
                    return Ok(s);
                }
                if s.revision != compiled_revision || !s.pending_directives.is_empty() {
                    continue;
                }
                if budget_exhausted(&s) {
                    self.transition(run, &mut s, Status::Paused, Some("budget".into()))
                        .await?;
                    return Ok(s);
                }
                self.point(Boundary::BeforeModelRequest, run, s.step, None);
                let key = format!("model:{}:request", s.step);
                let revision = s.revision;
                self.event(run,&mut s,"model_request",key,json!({"request":compiled.request,"hash":format!("{:x}",Sha256::digest(&compiled.bytes)),"layer_hashes":compiled.layer_hashes,"revision":revision})).await?;
                self.point(Boundary::ModelRequestConfirmed, run, s.step, None);
            }
            if s.response.is_none() {
                let request = s.request.clone().unwrap();
                let available = remaining_budget(&s);
                if let Some(bound) = self.model.call_bound(&request)
                    && (bound.max_tokens > available.max_tokens
                        || available
                            .max_cost_microusd
                            .is_some_and(|cap| bound.max_cost_microusd > cap))
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
                let payload = self.sanitize(&json!({"metadata":self.model.metadata()}))?;
                let Some(sequence) = self
                    .journal
                    .append_when_running(run, "model_dispatch", &key, payload.clone())
                    .await?
                else {
                    return self.load(run).await;
                };
                self.flush_events(run).await?;
                if sequence > s.last_sequence {
                    s.apply(&Event {
                        sequence,
                        kind: "model_dispatch".into(),
                        key,
                        payload,
                    })?;
                }
                let remaining = remaining_time(&s).max(1);
                let stream_repeats = Arc::new(Mutex::new(loops::StreamRepeats::default()));
                let repeated = Arc::new(tokio::sync::Notify::new());
                let tracked_sink = self.stream_sink.as_ref().map(|sink| {
                    let sink = sink.clone();
                    let tracker = stream_repeats.clone();
                    let repeated = repeated.clone();
                    Arc::new(move |delta: excalibur_models::StreamDelta| {
                        if let excalibur_models::StreamDelta::Text { delta: text, .. } = &delta
                            && tracker.lock().unwrap().push(text)
                        {
                            repeated.notify_one();
                            return;
                        }
                        sink(delta);
                    }) as excalibur_models::StreamSink
                });
                let mut interruption_metrics =
                    excalibur_models::ModelCallMetrics::request(&request, self.model.metadata());
                interruption_metrics.usage.estimated = true;
                let control_metrics = interruption_metrics.clone();
                let control_interrupted = Arc::new(std::sync::atomic::AtomicBool::new(false));
                let interrupted_flag = control_interrupted.clone();
                let interruption_bound = self.model.call_bound(&request);
                let reserved_tokens = interruption_bound
                    .as_ref()
                    .map(|bound| bound.max_tokens)
                    .unwrap_or(available.max_tokens);
                let reserved_cost = interruption_bound
                    .as_ref()
                    .map(|bound| bound.max_cost_microusd)
                    .unwrap_or(available.max_cost_microusd.unwrap_or(0));
                let estimated_input = serde_json::to_vec(&request)?.len() as u64 + 100;
                let response = tokio::time::timeout(
                    Duration::from_secs(remaining),
                    AssertUnwindSafe(async {
                        let call=async{if let Some(sink) = tracked_sink {
                            tokio::select!{
                                biased;
                                _=repeated.notified()=>Err(excalibur_models::ModelCallError::new(excalibur_models::ModelError::Consumer,interruption_metrics)),
                                outcome=self.model.complete_stream_observed(request,sink)=>outcome,
                            }
                        } else {
                            self.model.complete_observed(request).await
                        }};
                        tokio::select!{
                            outcome=call=>outcome,
                            _=self.wait_for_control(run,None)=>{
                                interrupted_flag.store(true,std::sync::atomic::Ordering::SeqCst);
                                Err(excalibur_models::ModelCallError::new(excalibur_models::ModelError::Consumer,control_metrics))
                            },
                        }
                    })
                    .catch_unwind(),
                )
                .await;
                if control_interrupted.load(std::sync::atomic::Ordering::SeqCst) {
                    let key = format!("model:{}:failure", s.step);
                    self.event(run,&mut s,"model_failure",key,json!({"failure_class":"consumer","error":"local model request cancelled by durable control; provider receipt and charge are unknown"})).await?;
                    let key = format!("model:{}:interrupted", s.step);
                    self.event(run,&mut s,"model_interrupted",key,json!({"accounting_kind":"conservative_reservation","estimated_usage":Usage{input_tokens:estimated_input.min(reserved_tokens),output_tokens:reserved_tokens.saturating_sub(estimated_input),cost_microusd:reserved_cost,estimated:true}})).await?;
                    return self.load(run).await;
                }
                if stream_repeats.lock().unwrap().detected {
                    let (metrics, usage, cost_known) = match &response {
                        Ok(Ok(Ok(observed))) => (
                            Some(observed.metrics.clone()),
                            observed.response.usage.clone(),
                            observed.metrics.cost_microusd.is_some(),
                        ),
                        Ok(Ok(Err(failure))) => (
                            Some(*failure.metrics.clone()),
                            Usage {
                                input_tokens: estimated_input.min(reserved_tokens),
                                output_tokens: reserved_tokens.saturating_sub(estimated_input),
                                cost_microusd: reserved_cost,
                                estimated: true,
                            },
                            false,
                        ),
                        _ => (
                            None,
                            Usage {
                                input_tokens: estimated_input.min(reserved_tokens),
                                output_tokens: reserved_tokens.saturating_sub(estimated_input),
                                cost_microusd: reserved_cost,
                                estimated: true,
                            },
                            false,
                        ),
                    };
                    let key = format!("model:{}:failure", s.step);
                    self.event(run,&mut s,"model_failure",key,json!({"failure_class":"validation","metrics":metrics,"error":"stream repetition R5 stopped; absent provider counters and charge remain unknown"})).await?;
                    let detection = s.loops.detect(LoopRule::R5);
                    self.record_loop_detection(run, &mut s, Some(detection.clone()))
                        .await?;
                    let key = format!("model:{}:interrupted", s.step);
                    self.event(
                        run,
                        &mut s,
                        "model_interrupted",
                        key,
                        json!({"estimated_usage":usage}),
                    )
                    .await?;
                    if detection.level >= 3 {
                        self.transition(run, &mut s, Status::Blocked, Some(detection.note))
                            .await?;
                        return Ok(s);
                    }
                    if s.spec.budget.max_cost_microusd.is_some() && !cost_known {
                        self.transition(
                            run,
                            &mut s,
                            Status::Paused,
                            Some("budget: interrupted model charge is unknown".into()),
                        )
                        .await?;
                        return Ok(s);
                    }
                    continue;
                }
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
            if self.refresh_context_instructions(run, &s).await? {
                continue;
            }
            let response = s.response.clone().unwrap();
            if response.message.tool_calls.is_empty() {
                self.transition(run, &mut s, Status::Verifying, None)
                    .await?;
                if s.status != Status::Verifying {
                    return Ok(s);
                }
                let input = GateInput {
                    step: s.step,
                    instruction_revision: s.revision,
                    final_text: response.message.content.clone(),
                    local_pending: s.pending_criteria(),
                    budget_remaining: remaining_budget(&s),
                };
                let external_guard = if let Some(gate) = &self.completion_gate {
                    let (inspected, timed_out) = tokio::select! {
                        outcome=gate.inspect(run,&input)=>(Some(outcome),false),
                        _=self.wait_for_phase_control(run,None,"verifying")=>(None,false),
                        _=tokio::time::sleep(Duration::from_secs(remaining_time(&s)))=>(None,true),
                    };
                    if timed_out {
                        self.transition(
                            run,
                            &mut s,
                            Status::Paused,
                            Some("wall time budget exhausted during verification".into()),
                        )
                        .await?;
                    }
                    let Some(inspected) = inspected else {
                        let cancelled = self.journal.state(run).await?.0 == "cancelled";
                        if gate.interrupt(run, cancelled).await.is_err() {
                            let key =
                                format!("verification-interrupt:{}:{}", s.step, s.last_sequence);
                            self.event(
                                run,
                                &mut s,
                                "verification_interruption_failed",
                                key,
                                json!({"warning":"verifier child interruption was not confirmed"}),
                            )
                            .await?;
                        }
                        return self.load(run).await;
                    };
                    match inspected {
                        Ok(guard) => Some(guard),
                        Err(_) => {
                            self.transition(
                                run,
                                &mut s,
                                Status::Blocked,
                                Some(
                                    "completion gate unavailable; verification cannot be claimed"
                                        .into(),
                                ),
                            )
                            .await?;
                            return Ok(s);
                        }
                    }
                } else {
                    None
                };
                s = self.load(run).await?;
                if s.status != Status::Verifying {
                    return Ok(s);
                }
                let (instructions, instruction_guard) =
                    self.journal.context_instruction_guard(run).await?;
                let changed = s.context_instruction_digest.as_ref().map_or(
                    !instructions.stable.is_empty() || !instructions.task.is_empty(),
                    |old| old != &instructions.revision,
                );
                if changed {
                    drop(instruction_guard);
                    drop(external_guard);
                    self.refresh_context_instructions(run, &s).await?;
                    self.transition(run, &mut s, Status::Running, None).await?;
                    continue 'execution;
                }
                // Keep instruction mutation ordered with the terminal journal CAS.
                let _instruction_guard = instruction_guard;
                let assessment = external_guard
                    .as_ref()
                    .map(|guard| guard.report())
                    .unwrap_or_default();
                let mut pending = input.local_pending;
                pending.extend(
                    assessment
                        .issues
                        .iter()
                        .map(|issue| format!("{}: {}", issue.id, issue.detail)),
                );
                let confirmations_only = !assessment.issues.is_empty()
                    && assessment
                        .issues
                        .iter()
                        .all(|issue| issue.kind == GateIssueKind::UserConfirmation)
                    && pending.len() == assessment.issues.len();
                let blocked = assessment.issues.iter().any(|issue| issue.blocked);
                let key = format!("gate:{}:{}", s.step, s.last_sequence);
                self.event(
                    run,
                    &mut s,
                    "gate_report",
                    key,
                    json!({"pending":pending,"final_text":response.message.content,"assessment":assessment}),
                )
                .await?;
                self.point(Boundary::GateReportConfirmed, run, s.step, None);
                if s.gate_report.is_empty() {
                    self.transition(run, &mut s, Status::Completed, None)
                        .await?;
                    if s.status == Status::Verifying {
                        drop(external_guard);
                        self.transition_from(run, &mut s, &["verifying"], Status::Running, None)
                            .await?;
                        continue;
                    }
                    return Ok(s);
                }
                if s.gate_attempts >= 2 || closing(&s) || confirmations_only {
                    let reason = if confirmations_only {
                        format!("pending your confirmation: {}", s.gate_report.join("; "))
                    } else {
                        s.gate_report.join("; ")
                    };
                    self.transition(
                        run,
                        &mut s,
                        if blocked {
                            Status::Blocked
                        } else {
                            Status::CompletedWithPending
                        },
                        Some(reason),
                    )
                    .await?;
                    if s.status == Status::Verifying {
                        drop(external_guard);
                        self.transition_from(run, &mut s, &["verifying"], Status::Running, None)
                            .await?;
                        continue;
                    }
                    return Ok(s);
                }
                drop(external_guard);
                self.transition_from(run, &mut s, &["verifying"], Status::Running, None)
                    .await?;
                if s.status != Status::Running {
                    return Ok(s);
                }
                if self.honor_admission(run, &mut s).await? {
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
                if self.honor_admission(run, &mut s).await? {
                    return Ok(s);
                }
                if self.refresh_context_instructions(run, &s).await?
                    || !s.pending_directives.is_empty()
                {
                    continue 'execution;
                }
                let mut slots = remaining_budget(&s).max_tool_calls.min(
                    capability_tool_budget
                        .unwrap_or(u64::MAX)
                        .saturating_sub(s.tool_calls),
                );
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
            let loop_calls = response
                .message
                .tool_calls
                .iter()
                .enumerate()
                .filter_map(|(index, call)| {
                    s.calls
                        .get(&index)
                        .and_then(|c| c.result.clone())
                        .map(|result| (call.clone(), result))
                })
                .collect::<Vec<_>>();
            let detection = self.observe_loops(run, &mut s, &loop_calls).await?;
            let key = format!("step:{}:completed", s.step);
            self.event(run, &mut s, "step_completed", key, json!({}))
                .await?;
            self.checkpoint(run, &mut s).await?;
            if let Some(detection) = detection.filter(|d| d.level >= 3) {
                self.transition(run, &mut s, Status::Blocked, Some(detection.note))
                    .await?;
                return Ok(s);
            }
        }
    }
    async fn observe_loops(
        &self,
        run: &str,
        state: &mut Snapshot,
        calls: &[(ToolCall, ToolResult)],
    ) -> Result<Option<LoopDetection>> {
        if state.last_loop_step == Some(state.step) {
            return Ok(state.loops.last_detection.clone());
        }
        let versions=self.journal.database().read(excalibur_store::ReadLane::Interactive,{
            let run=run.to_owned();move|c|{let mut stmt=c.prepare("SELECT DISTINCT path,postimage_blob FROM file_versions WHERE run_id=?1 ORDER BY path,postimage_blob")?;Ok(stmt.query_map([run],|r|Ok((r.get::<_,String>(0)?,r.get::<_,Option<String>>(1)?)))?.collect::<std::result::Result<Vec<_>,_>>()?)}
        }).await.map_err(excalibur_journal::Error::from)?;
        let plan = if let Some(gate) = &self.completion_gate {
            gate.context(run)
                .await
                .map_err(|_| Error::Invalid("task progress unavailable"))?
        } else {
            String::new()
        };
        let progress = json!({"files":versions,"revision":state.revision,"evidence":state.host_checks,"plan":plan});
        let detection = state.loops.observe(calls, &progress);
        self.record_loop_detection(run, state, detection.clone())
            .await?;
        Ok(detection)
    }
    async fn wait_for_control(&self, run: &str, tool: Option<&PreparedTool>) -> bool {
        self.wait_for_phase_control(run, tool, "running").await
    }
    async fn wait_for_phase_control(
        &self,
        run: &str,
        tool: Option<&PreparedTool>,
        phase: &str,
    ) -> bool {
        loop {
            tokio::time::sleep(Duration::from_millis(10)).await;
            match self.journal.state(run).await {
                Ok((state, _)) if state == phase => {}
                Ok((state, _)) => {
                    return self
                        .tools
                        .interrupt_execution(run, state == "cancelled", tool)
                        .await
                        .unwrap_or(false);
                }
                Err(_) => {
                    return self
                        .tools
                        .interrupt_execution(run, false, tool)
                        .await
                        .unwrap_or(false);
                }
            }
        }
    }
    async fn record_loop_detection(
        &self,
        run: &str,
        state: &mut Snapshot,
        detection: Option<LoopDetection>,
    ) -> Result<()> {
        let key = format!("loops:{}:{}", state.step, state.last_sequence);
        self.event(
            run,
            state,
            "loop_observation",
            key,
            json!({"state":state.loops,"step":state.step}),
        )
        .await?;
        if let Some(detection) = detection {
            let key = format!("loop:{}:{}", state.step, state.last_sequence);
            self.event(
                run,
                state,
                "loop_detection",
                key,
                serde_json::to_value(detection)?,
            )
            .await?;
        }
        Ok(())
    }
    async fn resource_limit_reached(&self, run: &str, state: &mut Snapshot) -> Result<bool> {
        let limits = remaining_budget(state);
        if limits.max_processes.is_none() && limits.max_terminal_sessions.is_none() {
            return Ok(false);
        }
        if self
            .tools
            .configure_execution_limits(
                run,
                ExecutionLimits {
                    max_processes: limits.max_processes,
                    max_terminal_sessions: limits.max_terminal_sessions,
                },
            )
            .await
            .is_err()
        {
            self.transition(
                run,
                state,
                Status::Paused,
                Some("budget resource limit cannot be enforced".into()),
            )
            .await?;
            return Ok(true);
        }
        let resources = self
            .tools
            .execution_resources(run)
            .await
            .map_err(|_| Error::Invalid("execution resource counts unavailable"))?;
        let reason = match resources {
            None => Some("budget resource counts cannot be verified"),
            Some(r) if limits.max_processes.is_some_and(|max| r.processes >= max) => {
                Some("budget: managed process limit")
            }
            Some(r)
                if limits
                    .max_terminal_sessions
                    .is_some_and(|max| r.terminal_sessions >= max) =>
            {
                Some("budget: terminal session limit")
            }
            _ => None,
        };
        if let Some(reason) = reason {
            self.transition(run, state, Status::Paused, Some(reason.into()))
                .await?;
            Ok(true)
        } else {
            Ok(false)
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
                AssertUnwindSafe(async {
                    let execution=self.execute(run,&key,&prepared);
                    tokio::pin!(execution);
                    tokio::select! {
                        result=&mut execution=>Some(result),
                        interrupted=self.wait_for_control(run,Some(&prepared))=>{
                            if !interrupted&&prepared.effect!=Effect::Pure{Some(execution.await)}else{None}
                        },
                    }
                })
                .catch_unwind(),
            )
            .await
            {
                Ok(Ok(Some(result))) => result,
                Ok(Ok(None)) => return None,
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
fn remaining_time(s: &Snapshot) -> u64 {
    remaining_budget(s).max_wall_seconds
}
fn budget_exhausted(s: &Snapshot) -> bool {
    let remaining = remaining_budget(s);
    remaining.max_steps == 0
        || remaining.max_tokens == 0
        || remaining.max_cost_microusd == Some(0)
        || remaining.max_wall_seconds == 0
}
fn closing(s: &Snapshot) -> bool {
    let b = &s.spec.budget;
    let remaining = remaining_budget(s);
    let steps_with_current = remaining
        .max_steps
        .saturating_add(u64::from(s.request.is_some()));
    remaining.max_tool_calls == 0
        || steps_with_current <= 1
        || steps_with_current.saturating_mul(10) < b.max_steps
        || remaining.max_tokens.saturating_mul(10) < b.max_tokens
        || remaining
            .max_cost_microusd
            .zip(b.max_cost_microusd)
            .is_some_and(|(left, total)| left.saturating_mul(10) < total)
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
