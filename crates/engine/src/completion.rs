//! Host-extensible completion checks and durable delivery records.
use crate::{Budget, Result as EngineResult, Snapshot, Status, Usage};
use excalibur_journal::Journal;
use excalibur_types::runtime::BoxFuture;
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WaitReason {
    Tool,
    User,
    Approval,
    Children,
    Agenda,
    Capability,
    Lease,
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WakeEvent {
    ToolResult,
    UserReply,
    ApprovalDecision,
    ChildSignal,
    Directive,
    Timer,
    CapabilityUpdate,
    LeaseReleased,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum GateIssueKind {
    Criterion,
    ProjectCheck,
    Child,
    InstructionRevision,
    OriginalRequest,
    IndependentVerification,
    UserConfirmation,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct GateIssue {
    pub id: String,
    pub kind: GateIssueKind,
    pub detail: String,
    /// A host decision is required; further model attempts cannot resolve it.
    #[serde(default)]
    pub blocked: bool,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct DeliveryReference {
    pub reference: String,
    pub description: String,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct GateAssessment {
    pub issues: Vec<GateIssue>,
    pub deliverables: Vec<DeliveryReference>,
    pub evidence: Vec<DeliveryReference>,
    pub modified_files: Vec<String>,
    pub assumptions: Vec<String>,
    pub open_questions: Vec<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct GateInput {
    pub step: u64,
    pub instruction_revision: u64,
    pub final_text: String,
    pub local_pending: Vec<String>,
    pub budget_remaining: Budget,
}
/// A task adapter retains its evidence/revision read lock until this guard drops.
/// The engine holds it through the final journal compare-and-set transaction.
pub trait CompletionGateGuard: Send {
    fn report(&self) -> GateAssessment;
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AdmissionState {
    Ready,
    Paused(String),
    Blocked(String),
    Cancelled(String),
}
pub trait CompletionGate: Send + Sync {
    /// Called only after dropping an interrupted inspection future.
    fn interrupt<'a>(
        &'a self,
        _run: &'a str,
        _cancelled: bool,
    ) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async { Ok(()) })
    }
    fn admission<'a>(&'a self, _run: &'a str) -> BoxFuture<'a, Result<AdmissionState, String>> {
        Box::pin(async { Ok(AdmissionState::Ready) })
    }
    fn inspect<'a>(
        &'a self,
        run: &'a str,
        input: &'a GateInput,
    ) -> BoxFuture<'a, Result<Box<dyn CompletionGateGuard>, String>>;
    /// Return a bounded current micro-plan projection, never instructions from tools.
    fn context<'a>(&'a self, _run: &'a str) -> BoxFuture<'a, Result<String, String>> {
        Box::pin(async { Ok(String::new()) })
    }
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct DeliveryMetrics {
    pub usage: Usage,
    pub elapsed_seconds: u64,
    pub model_calls: u64,
    pub tool_calls: u64,
    #[serde(default)]
    pub child_consumption: std::collections::BTreeMap<String, crate::AllocatedUsage>,
    #[serde(default)]
    pub child_reservations: std::collections::BTreeMap<String, Budget>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DeliveryReport {
    #[serde(default)]
    pub source_sequence: u64,
    #[serde(default)]
    pub urgent: bool,
    pub status: Status,
    pub reason: Option<String>,
    pub summary: String,
    pub deliverables: Vec<DeliveryReference>,
    pub evidence: Vec<DeliveryReference>,
    pub modified_files: Vec<String>,
    pub assumptions: Vec<String>,
    pub open_questions: Vec<String>,
    pub pending: Vec<String>,
    pub metrics: DeliveryMetrics,
}
/// Project an already-confirmed stop without invoking models or tools. A host
/// must apply its shared redactor before persisting or exporting this value.
pub async fn delivery_report_for(journal: &Journal, run: &str) -> EngineResult<DeliveryReport> {
    let state = crate::allocations::snapshot(journal, run).await?;
    build_report(journal, run, &state).await
}
pub async fn delivery_report_at(
    journal: &Journal,
    run: &str,
    source_sequence: u64,
) -> EngineResult<DeliveryReport> {
    let first = journal
        .entries(run, 0, 1)
        .await?
        .pop()
        .ok_or(crate::Error::Invalid("run missing"))?;
    if source_sequence < first.sequence {
        return Err(crate::Error::Invalid("invalid delivery report revision"));
    }
    let mut state = Snapshot::started(&first.payload)?;
    let mut cursor = first.sequence;
    while cursor < source_sequence {
        let before = cursor;
        let page = journal.entries(run, cursor, 256).await?;
        for event in page
            .into_iter()
            .take_while(|event| event.sequence <= source_sequence)
        {
            cursor = event.sequence;
            state.apply(&event)?;
        }
        if cursor == before {
            return Err(crate::Error::Invalid(
                "delivery report revision is not confirmed",
            ));
        }
    }
    build_report(journal, run, &state).await
}
pub(crate) async fn build_report(
    journal: &Journal,
    run: &str,
    state: &Snapshot,
) -> EngineResult<DeliveryReport> {
    let mut evidence = state.gate_assessment.evidence.clone();
    for (id, passed) in &state.host_checks {
        evidence.push(DeliveryReference {
            reference: format!("journal:{run}:host_check:{id}"),
            description: format!("Host check {id}: {}", if *passed { "pass" } else { "fail" }),
        });
    }
    let mut modified = state.gate_assessment.modified_files.clone();
    let mut confirmed = std::collections::BTreeSet::new();
    for message in &state.history {
        if message.role == "tool"
            && let Some(id) = &message.tool_call_id
        {
            confirmed.insert(id.clone());
        }
    }
    if let Some(response) = &state.response {
        for (index, call) in response.message.tool_calls.iter().enumerate() {
            if state
                .calls
                .get(&index)
                .is_some_and(|call| call.result.is_some())
            {
                confirmed.insert(call.id.clone());
            }
        }
    }
    let(paths,stopped_at)=journal.database().read(excalibur_store::ReadLane::Interactive,{
        let run=run.to_owned();let source=state.last_sequence;move|c|{
            let mut stmt=c.prepare("SELECT DISTINCT path,tool_call_id FROM file_versions WHERE run_id=?1 ORDER BY path")?;
            let rows=stmt.query_map([&run],|r|Ok((r.get::<_,String>(0)?,r.get::<_,Option<String>>(1)?)))?.collect::<std::result::Result<Vec<_>,_>>()?;
            let paths=rows.into_iter().filter(|(_,call)|call.as_ref().is_some_and(|id|confirmed.contains(id))).map(|(path,_)|path).collect::<Vec<_>>();
            let stopped_at:i64=c.query_row("SELECT created_at FROM journal WHERE run_id=?1 AND sequence=?2",excalibur_store::rusqlite::params![run,source],|r|r.get(0))?;
            Ok((paths,stopped_at))
        }
    }).await.map_err(excalibur_journal::Error::from)?;
    modified.extend(paths);
    modified.sort();
    modified.dedup();
    let mut pending = if state.gate_report.is_empty() {
        state.pending_criteria()
    } else {
        state.gate_report.clone()
    };
    for call in state.calls.values() {
        if let Some(prepared) = &call.prepared
            && call.result.is_none()
        {
            pending.push(format!(
                "Unconfirmed tool intention {} ({}) requires reconciliation",
                prepared.call.name, prepared.call.id
            ));
        }
    }
    Ok(DeliveryReport {
        source_sequence: state.last_sequence,
        urgent: state.status == Status::Paused
            && state
                .reason
                .as_ref()
                .is_some_and(|reason| reason.contains("budget") || reason.contains("wall time")),
        status: state.status.clone(),
        reason: state.reason.clone(),
        summary: state
            .final_text
            .clone()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| {
                format!(
                    "Execution stopped as {} after {} steps",
                    state.status.label(),
                    state.step
                )
            }),
        deliverables: state.gate_assessment.deliverables.clone(),
        evidence,
        modified_files: modified,
        assumptions: state.gate_assessment.assumptions.clone(),
        open_questions: state.gate_assessment.open_questions.clone(),
        pending,
        metrics: DeliveryMetrics {
            usage: state.usage.clone(),
            elapsed_seconds: stopped_at.saturating_sub(state.started_at).max(0) as u64,
            model_calls: state.model_calls,
            tool_calls: state.tool_calls,
            child_consumption: state
                .child_allocations
                .iter()
                .filter_map(|(id, allocation)| {
                    allocation.settled.clone().map(|usage| (id.clone(), usage))
                })
                .collect(),
            child_reservations: state
                .child_allocations
                .iter()
                .filter(|(_, allocation)| allocation.settled.is_none())
                .map(|(id, allocation)| (id.clone(), allocation.budget.clone()))
                .collect(),
        },
    })
}
