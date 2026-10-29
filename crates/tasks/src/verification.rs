//! Declared checks, independent-verifier jobs and the task completion gate.
use crate::{Result, TaskStore, WorkLevel, evidence::*, fail, new_id};
use excalibur_store::{
    Priority, ReadLane,
    rusqlite::{self, OptionalExtension},
};
use excalibur_types::runtime::{
    BoxFuture, PreparedTool, Reconcile, ToolCall, ToolResult, ToolRuntime,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CheckDefinition {
    Command {
        command: String,
        #[serde(default = "default_timeout")]
        timeout_ms: u64,
        #[serde(default)]
        expected_exit: i32,
    },
    Test {
        command: String,
        #[serde(default = "default_timeout")]
        timeout_ms: u64,
        #[serde(default)]
        expected_exit: i32,
    },
    FileContains {
        path: String,
        needle: String,
    },
    Metric {
        command: String,
        pointer: String,
        min: Option<f64>,
        max: Option<f64>,
        #[serde(default = "default_timeout")]
        timeout_ms: u64,
    },
    /// A host-produced judgment; never executable and never suitable for code checks.
    Inspection,
}
fn default_timeout() -> u64 {
    120000
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct CheckBrief {
    pub id: String,
    pub definition: CheckDefinition,
    pub environment: Option<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct VerificationBudget {
    pub max_steps: u64,
    pub max_tool_calls: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cost_microusd: u64,
    pub timeout_seconds: u64,
}
impl Default for VerificationBudget {
    fn default() -> Self {
        Self {
            max_steps: 4,
            max_tool_calls: 16,
            input_tokens: 32000,
            output_tokens: 8000,
            cost_microusd: 1000000,
            timeout_seconds: 120,
        }
    }
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct VerificationInput {
    #[serde(default)]
    pub task_version: u64,
    #[serde(default)]
    pub applicable_instructions: Vec<ApplicableInstruction>,
    #[serde(default)]
    pub waived_criteria: std::collections::BTreeMap<String, String>,
    #[serde(default = "task_scope")]
    pub verification_scope: String,
    #[serde(default)]
    pub workspace_snapshots: std::collections::BTreeMap<String, WorkspaceSnapshot>,
    pub job_id: String,
    pub task_id: String,
    pub root_run_id: String,
    pub parent_run_id: String,
    pub round: u8,
    pub instruction_revision: u64,
    pub literal_request: String,
    pub requirements: Vec<RequirementBrief>,
    pub criteria: Vec<CriterionBrief>,
    pub evidence: Vec<EvidenceRecord>,
    pub diff: String,
    pub delivery_summary: String,
    pub budget: VerificationBudget,
    pub budget_remaining: excalibur_engine::Budget,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ApplicableInstruction {
    pub id: String,
    pub text: String,
    pub revision: u64,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct CriterionVerdict {
    pub criterion_id: String,
    pub passed: bool,
    pub explanation: String,
    pub evidence_ids: Vec<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct VerificationVerdict {
    #[serde(default)]
    pub verifier_run_id: String,
    pub criteria: Vec<CriterionVerdict>,
    pub uncovered_requests: Vec<String>,
    pub request_covered: bool,
}
fn task_scope() -> String {
    "task".into()
}
pub trait IndependentVerifier: Send + Sync {
    fn prepare<'a>(
        &'a self,
        _parent_run: &'a str,
    ) -> BoxFuture<'a, std::result::Result<(), String>> {
        Box::pin(async { Ok(()) })
    }
    fn verify<'a>(
        &'a self,
        input: &'a VerificationInput,
    ) -> BoxFuture<'a, std::result::Result<VerificationVerdict, String>>;
}
#[derive(Clone)]
pub struct CheckRunner {
    pub(crate) runtime: Arc<dyn ToolRuntime>,
    pub(crate) workspace: PathBuf,
    projection_paths: Vec<PathBuf>,
    excluded_paths: Vec<PathBuf>,
    calls: Arc<AtomicU64>,
    tool_budget: Option<u64>,
}
impl CheckRunner {
    pub fn new(runtime: Arc<dyn ToolRuntime>, workspace: PathBuf) -> Self {
        Self {
            runtime,
            workspace,
            projection_paths: vec![],
            excluded_paths: vec![],
            calls: Arc::new(AtomicU64::new(0)),
            tool_budget: None,
        }
    }
    pub fn with_projection(mut self, path: PathBuf) -> Self {
        self.projection_paths.push(path);
        self
    }
    pub fn with_exclusions(mut self, paths: Vec<PathBuf>) -> Self {
        self.excluded_paths = paths;
        self
    }
    pub fn with_tool_budget(mut self, limit: u64) -> Self {
        self.tool_budget = Some(limit);
        self
    }
    pub fn tool_calls(&self) -> u64 {
        self.calls.load(Ordering::Relaxed)
    }
    pub async fn snapshot(
        &self,
        patterns: Vec<String>,
        revision: u64,
    ) -> Result<WorkspaceSnapshot> {
        WorkspaceSnapshot::capture(
            self.workspace.clone(),
            patterns,
            revision,
            self.projection_paths.clone(),
            self.excluded_paths.clone(),
        )
        .await
    }
    pub async fn run(
        &self,
        store: &TaskStore,
        task_id: &str,
        check_id: &str,
        run: &str,
    ) -> Result<EvidenceRecord> {
        self.run_keyed(store, task_id, check_id, run, &new_id("check-op"))
            .await
    }
    pub async fn run_keyed(
        &self,
        store: &TaskStore,
        task_id: &str,
        check_id: &str,
        run: &str,
        operation_key: &str,
    ) -> Result<EvidenceRecord> {
        let task = store
            .task_for_run(run)
            .await?
            .ok_or_else(|| fail("check run must be bound to task before execution"))?;
        if task.id != task_id {
            return Err(fail("check run belongs to another task"));
        }
        let (_, mut criteria) = store.requirements_and_criteria(task_id).await?;
        for criterion in store.project_check_catalog(task_id).await? {
            if !criteria.iter().any(|c| c.id == criterion.id) {
                criteria.push(criterion)
            }
        }
        let (criterion, check) = criteria
            .iter()
            .find_map(|cr| {
                cr.checks
                    .iter()
                    .find(|ck| ck.id == check_id)
                    .map(|ck| (cr.clone(), ck.clone()))
            })
            .ok_or_else(|| fail("declared check not in task"))?;
        if matches!(check.definition, CheckDefinition::Inspection) {
            return Err(fail("inspection is not an executable check"));
        }
        if operation_key.is_empty() || operation_key.len() > 1024 {
            return Err(fail("invalid check operation key"));
        }
        // The same operation cannot run twice, including concurrent callers.
        let admission_lease = store.mutation_lock.write().await;
        let key = format!("{run}:{operation_key}");
        let lookup = key.clone();
        let known=store.db.read(ReadLane::Interactive,move|c|Ok(c.query_row("SELECT task_id,check_id,run_id,prepared,snapshot,evidence_id,producer_snapshot FROM task_check_attempts WHERE operation_key=?1",[lookup],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,String>(2)?,r.get::<_,String>(3)?,r.get::<_,String>(4)?,r.get::<_,Option<String>>(5)?,r.get::<_,String>(6)?))).optional()?)).await?;
        if let Some((task, check, owner, _, _, Some(id), _)) = &known {
            if task != task_id || check != check_id || owner != run {
                return Err(fail("check operation key reused"));
            }
            return store
                .evidence(id)
                .await?
                .ok_or_else(|| fail("check evidence receipt missing"));
        }
        if matches!(
            check.definition,
            CheckDefinition::Command { .. }
                | CheckDefinition::Test { .. }
                | CheckDefinition::Metric { .. }
        ) {
            store.effect_admission(run).await?;
        }
        if self
            .tool_budget
            .is_some_and(|limit| self.tool_calls() >= limit)
        {
            return Err(fail(
                "declared check tool budget exhausted before admission",
            ));
        }
        let calls_at_start = self.tool_calls();
        let id = check.id.clone();
        let check_version = store
            .db
            .read(ReadLane::Interactive, move |c| {
                Ok(
                    c.query_row("SELECT version FROM checks WHERE id=?1", [id], |r| {
                        r.get::<_, u64>(0)
                    })?,
                )
            })
            .await?;
        let producer = if let Some(attempt) = &known {
            serde_json::from_str::<Value>(&attempt.6)?
        } else {
            json!({"check":check,"check_version":check_version,"criterion":{"id":criterion.id,"method":criterion.method,"expected":criterion.expected},"runtime":{"os":std::env::consts::OS,"architecture":std::env::consts::ARCH,"workspace":self.workspace}})
        };
        let check: CheckBrief = serde_json::from_value(producer["check"].clone())?;
        let revision = store.criterion_revision(&criterion.id).await?;
        let before = self
            .snapshot(criterion.impact_area.clone(), revision)
            .await?;
        let call = match &check.definition {
            CheckDefinition::FileContains { path, .. } => ToolCall {
                id: new_id("check-read"),
                name: "read_file".into(),
                arguments: json!({"path":path}).to_string(),
            },
            CheckDefinition::Command {
                command,
                timeout_ms,
                ..
            }
            | CheckDefinition::Test {
                command,
                timeout_ms,
                ..
            }
            | CheckDefinition::Metric {
                command,
                timeout_ms,
                ..
            } => {
                if command.is_empty() || !(250..=300000).contains(timeout_ms) {
                    return Err(fail("check command requires timeout within 250..300000 ms"));
                }
                ToolCall{id:new_id("check-exec"),name:"exec".into(),arguments:json!({"cmd":command,"cwd":self.workspace,"yield_ms":(*timeout_ms).min(30000),"timeout_ms":timeout_ms}).to_string()}
            }
            CheckDefinition::Inspection => unreachable!(),
        };
        let recovery = known.is_some();
        let (prepared, snapshot) = if let Some((task, check, owner, prepared, snapshot, _, _)) =
            known
        {
            if task != task_id || check != check_id || owner != run {
                return Err(fail("check operation key reused"));
            }
            (
                serde_json::from_str::<PreparedTool>(&prepared)?,
                serde_json::from_str::<WorkspaceSnapshot>(&snapshot)?,
            )
        } else {
            let prepared = self.runtime.prepare(run, call).await.map_err(fail)?;
            let sanitized = self
                .runtime
                .sanitize_durable(&serde_json::to_value(&prepared)?)
                .map_err(fail)?;
            if sanitized != serde_json::to_value(&prepared)? {
                return Err(fail("declared check contains protected material"));
            }
            let (op, task, check, owner, prep, snap, trace) = (
                key.clone(),
                task_id.to_owned(),
                check_id.to_owned(),
                run.to_owned(),
                serde_json::to_string(&prepared)?,
                serde_json::to_string(&before)?,
                producer.to_string(),
            );
            store.db.write(Priority::Journal,move|c|{c.execute("INSERT INTO task_check_attempts(operation_key,task_id,check_id,run_id,prepared,snapshot,producer_snapshot) VALUES(?1,?2,?3,?4,?5,?6,?7)",rusqlite::params![op,task,check,owner,prep,snap,trace])?;Ok(())}).await?;
            (prepared, before)
        };
        let timeout = match check.definition {
            CheckDefinition::Command { timeout_ms, .. }
            | CheckDefinition::Test { timeout_ms, .. }
            | CheckDefinition::Metric { timeout_ms, .. } => timeout_ms,
            _ => 30000,
        };
        let deadline = tokio::time::Instant::now() + Duration::from_millis(timeout);
        drop(admission_lease);
        let mut result = if recovery {
            match self.runtime.reconcile(run, &key, &prepared).await {
                Reconcile::Done(result) => result,
                Reconcile::Retry
                    if prepared.effect != excalibur_types::runtime::Effect::NonIdempotent =>
                {
                    self.invoke_monitored(store, run, &key, &prepared, deadline)
                        .await?
                }
                Reconcile::Retry | Reconcile::Uncertain(_) => {
                    return Err(fail(
                        "check outcome is uncertain after interruption; explicit new attempt required, command was not repeated",
                    ));
                }
            }
        } else {
            self.invoke_monitored(store, run, &key, &prepared, deadline)
                .await?
        };
        let mut tool_calls = 1u64;
        while result.output["exited"] == false && result.output["session_id"].is_string() {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                return Err(fail(
                    "declared check timed out; its receipt remains uncertain",
                ));
            }
            let wait=ToolCall{id:new_id("check-wait"),name:"exec_io".into(),arguments:json!({"action":"wait_for","session_id":result.output["session_id"],"timeout_ms":remaining.as_millis().min(300000)as u64}).to_string()};
            let wait = self.runtime.prepare(run, wait).await.map_err(fail)?;
            result = self
                .invoke_monitored(
                    store,
                    run,
                    &format!("{key}:wait:{tool_calls}"),
                    &wait,
                    deadline,
                )
                .await?;
            tool_calls += 1;
            if tool_calls > 32 {
                return Err(fail("declared check exceeded bounded session waits"));
            }
        }
        let _publication_lease = store.mutation_lock.write().await;
        let (passed, explanation) = self.evaluate(store, &check.definition, &result).await?;
        let current_revision = store.criterion_revision(&criterion.id).await?;
        let id = check.id.clone();
        let current_version = store
            .db
            .read(ReadLane::Interactive, move |c| {
                Ok(
                    c.query_row("SELECT version FROM checks WHERE id=?1", [id], |r| {
                        r.get::<_, u64>(0)
                    })?,
                )
            })
            .await?;
        let fresh = snapshot.fresh(current_revision).await?
            && producer["check_version"].as_u64() == Some(current_version);
        let result_value = self
            .runtime
            .sanitize_durable(&serde_json::to_value(&result)?)
            .map_err(fail)?;
        let evidence = store
            .store_evidence(
                EvidenceRecord {
                    id: new_id("evidence"),
                    task_id: task_id.into(),
                    criterion_id: criterion.id,
                    check_id: check_id.into(),
                    run_id: run.into(),
                    result: if !fresh {
                        EvidenceResult::Error
                    } else if passed {
                        EvidenceResult::Pass
                    } else {
                        EvidenceResult::Fail
                    },
                    kind: EvidenceKind::Execution,
                    output_blob: None,
                    recorded_at: excalibur_store::unix_seconds().max(0) as u64,
                    snapshot,
                    tool_call_id: Some(prepared.call.id),
                    trusted: true,
                    stale: !fresh,
                    explanation: if fresh {
                        explanation
                    } else {
                        "affected files changed while check was running".into()
                    },
                    producer_snapshot: producer,
                    tool_calls: self.tool_calls().saturating_sub(calls_at_start).max(1),
                },
                result_value,
            )
            .await?;
        let (op, id) = (key, evidence.id.clone());
        store.db.write(Priority::Journal,move|c|{c.execute("UPDATE task_check_attempts SET evidence_id=?1,state='completed' WHERE operation_key=?2",rusqlite::params![id,op])?;Ok(())}).await?;
        drop(_publication_lease);
        if evidence.result == EvidenceResult::Fail {
            let id = check_id.to_owned();
            let task = task_id.to_owned();
            let version = evidence.producer_snapshot["check_version"]
                .as_u64()
                .unwrap_or(0);
            let recent=store.db.read(ReadLane::Interactive,move|c|{let mut q=c.prepare("SELECT result FROM evidence WHERE check_id=?1 AND evidence_kind='execution' AND json_extract(metadata,'$.task_id')=?2 AND json_extract(metadata,'$.producer_snapshot.check_version')=?3 ORDER BY rowid DESC LIMIT 4")?;Ok(q.query_map(rusqlite::params![id,task,version],|r|r.get::<_,String>(0))?.collect::<std::result::Result<Vec<_>,_>>()?)}).await?;
            if recent.len() >= 3
                && recent.iter().take(3).all(|r| r == "fail")
                && recent.get(3).is_none_or(|r| r != "fail")
            {
                store
                    .request_replan(
                        task_id,
                        "declared check failed after three actual correction attempts",
                        vec![check_id.into()],
                    )
                    .await?;
            }
        }
        Ok(evidence)
    }
    async fn invoke_monitored(
        &self,
        store: &TaskStore,
        run: &str,
        key: &str,
        prepared: &PreparedTool,
        deadline: tokio::time::Instant,
    ) -> Result<ToolResult> {
        let journal = excalibur_journal::Journal::new(store.db.clone()).await?;
        let (state, _) = journal.state(run).await?;
        if !matches!(state.as_str(), "running" | "verifying") {
            return Err(fail(
                "check execution is not admitted in the current run state",
            ));
        }
        let execution = self.invoke(run, key, prepared);
        tokio::pin!(execution);
        loop {
            tokio::select! {result=&mut execution=>return Ok(result),_=tokio::time::sleep_until(deadline)=>{self.runtime.interrupt_execution(run,false,Some(prepared)).await.map_err(fail)?;return Err(fail("declared check timed out; outcome remains uncertain"))},_=tokio::time::sleep(Duration::from_millis(25))=>{let(state,_)=journal.state(run).await?;if !matches!(state.as_str(),"running"|"verifying"){self.runtime.interrupt_execution(run,state=="cancelled",Some(prepared)).await.map_err(fail)?;return Err(fail("check interrupted by run control; outcome remains uncertain"))}}}
        }
    }
    async fn invoke(&self, run: &str, key: &str, prepared: &PreparedTool) -> ToolResult {
        let mut used = self.calls.load(Ordering::Relaxed);
        loop {
            if self.tool_budget.is_some_and(|limit| used >= limit) {
                return ToolResult::error("declared check tool budget exhausted");
            }
            let Some(next) = used.checked_add(1) else {
                return ToolResult::error("declared check tool counter exhausted");
            };
            match self
                .calls
                .compare_exchange_weak(used, next, Ordering::AcqRel, Ordering::Relaxed)
            {
                Ok(_) => break,
                Err(actual) => used = actual,
            }
        }
        self.runtime.execute(run, key, prepared).await
    }
    async fn evaluate(
        &self,
        store: &TaskStore,
        definition: &CheckDefinition,
        result: &ToolResult,
    ) -> Result<(bool, String)> {
        if !result.success && result.output["exited"] != true {
            return Ok((
                false,
                "tool boundary rejected or failed the declared check".into(),
            ));
        }
        match definition {
            CheckDefinition::Command { expected_exit, .. }
            | CheckDefinition::Test { expected_exit, .. } => {
                let passed = result.output["exited"] == true
                    && result.output["exit_code"].as_i64() == Some(i64::from(*expected_exit))
                    && result.output["timed_out"] != true
                    && result.output["error"].is_null();
                Ok((
                    passed,
                    format!(
                        "actual exit {:?}; expected {expected_exit}",
                        result.output["exit_code"]
                    ),
                ))
            }
            CheckDefinition::FileContains { needle, .. } => {
                let fp = result.output["fp"]
                    .as_str()
                    .ok_or_else(|| fail("file check did not return verified fingerprint"))?;
                let bytes = store.db.read_blob(fp.parse()?, 64 * 1024 * 1024).await?;
                let text =
                    std::str::from_utf8(&bytes).map_err(|_| fail("file check expected UTF-8"))?;
                Ok((
                    text.contains(needle),
                    "declared file-content predicate evaluated on verified complete content".into(),
                ))
            }
            CheckDefinition::Metric {
                pointer, min, max, ..
            } => {
                if result.output["exited"] != true || result.output["exit_code"] != 0 {
                    return Ok((false, "metric command did not exit successfully".into()));
                }
                let output = if let Some(hash) = &result.full_output_blob {
                    String::from_utf8(store.db.read_blob(hash.parse()?, 16 * 1024 * 1024).await?)
                        .map_err(|_| fail("metric output must be UTF-8 JSON"))?
                } else {
                    result.output["output"].as_str().unwrap_or("").to_owned()
                };
                let value: Value = serde_json::from_str(&output)?;
                let number = value
                    .pointer(pointer)
                    .and_then(Value::as_f64)
                    .filter(|n| n.is_finite())
                    .ok_or_else(|| fail("metric pointer is not a finite number"))?;
                Ok((
                    min.is_none_or(|n| number >= n) && max.is_none_or(|n| number <= n),
                    format!("metric {pointer}={number}; bounds {min:?}..{max:?}"),
                ))
            }
            CheckDefinition::Inspection => Err(fail("inspection cannot execute")),
        }
    }
}

#[derive(Clone)]
pub struct TaskCompletionGate {
    store: TaskStore,
    runner: CheckRunner,
    verifier: Option<Arc<dyn IndependentVerifier>>,
    checklist: Option<crate::checklist::ChecklistProjection>,
}
impl TaskCompletionGate {
    pub fn new(store: TaskStore, runner: CheckRunner) -> Self {
        Self {
            store,
            runner,
            verifier: None,
            checklist: None,
        }
    }
    pub fn with_verifier(mut self, verifier: Arc<dyn IndependentVerifier>) -> Self {
        self.verifier = Some(verifier);
        self
    }
    pub fn with_checklist(mut self, runtime: Arc<dyn ToolRuntime>, path: PathBuf) -> Self {
        self.checklist = Some(crate::checklist::ChecklistProjection::new(runtime, path));
        self
    }
    async fn evaluate_gate(
        &self,
        run: &str,
        input: &excalibur_engine::GateInput,
    ) -> Result<Box<dyn excalibur_engine::CompletionGateGuard>> {
        use excalibur_engine::{DeliveryReference, GateAssessment, GateIssueKind as Kind};
        let mut report = GateAssessment::default();
        let Some(task) = self.store.task_for_run(run).await? else {
            issue(
                &mut report,
                "task",
                Kind::Criterion,
                "execution is not bound to a durable task",
                true,
            );
            return Ok(Box::new(TaskGateGuard {
                report,
                _lease: self.store.mutation_lock.clone().read_owned().await,
            }));
        };
        for value in task.request_card["assumptions"]
            .as_array()
            .into_iter()
            .flatten()
        {
            if let Some(text) = value.as_str().or_else(|| value["text"].as_str()) {
                report.assumptions.push(text.into());
            }
        }
        for (index, value) in task.request_card["open_questions"]
            .as_array()
            .into_iter()
            .flatten()
            .enumerate()
        {
            let text = value
                .as_str()
                .or_else(|| value["question"].as_str())
                .or_else(|| value["text"].as_str())
                .unwrap_or("unspecified question");
            let answered = value["answer"]
                .as_str()
                .is_some_and(|s| !s.trim().is_empty());
            let default = value.get("default").filter(|v| !v.is_null());
            if !answered {
                if let Some(default) = default {
                    report
                        .assumptions
                        .push(format!("{text}: assumed {default}"));
                } else {
                    report.open_questions.push(text.into());
                    if value["blocking"] == true {
                        issue(
                            &mut report,
                            &format!("question-{index}"),
                            Kind::OriginalRequest,
                            text,
                            true,
                        );
                    }
                }
            }
        }
        let (requirements, criteria) = self.store.requirements_and_criteria(&task.id).await?;
        if requirements.is_empty() || criteria.is_empty() {
            issue(
                &mut report,
                "requirements",
                Kind::Criterion,
                "task needs at least one requirement with acceptance criteria",
                true,
            )
        }
        for req in &requirements {
            if !criteria.iter().any(|c| c.requirement_id == req.id) {
                issue(
                    &mut report,
                    &req.id,
                    Kind::Criterion,
                    "requirement has no acceptance criteria",
                    false,
                )
            }
        }
        if let Err(error) = self.store.effect_admission(run).await {
            issue(
                &mut report,
                "plan-approval",
                Kind::Criterion,
                &error.to_string(),
                true,
            );
            return Ok(Box::new(TaskGateGuard {
                report,
                _lease: self.store.mutation_lock.clone().read_owned().await,
            }));
        }
        let mut effective_budget = input.budget_remaining.clone();
        if let Some(verifier) = &self.verifier
            && let Err(error) = verifier.prepare(run).await
        {
            issue(
                &mut report,
                "independent-verification",
                Kind::IndependentVerification,
                &error,
                true,
            )
        }
        let journal = excalibur_journal::Journal::new(self.store.db.clone()).await?;
        let started = journal.entries(run, 0, 1).await?;
        if started
            .first()
            .is_some_and(|e| !e.payload["spec"].is_null())
        {
            effective_budget = excalibur_engine::remaining_budget_for(&journal, run)
                .await
                .map_err(|e| fail(e.to_string()))?;
        }
        let project_checks = self.store.project_check_catalog(&task.id).await?;
        let mut check_catalog = criteria.clone();
        for criterion in &project_checks {
            if !check_catalog.iter().any(|c| c.id == criterion.id) {
                check_catalog.push(criterion.clone());
            }
        }
        let mut exemptions = std::collections::BTreeMap::new();
        for criterion in &check_catalog {
            let revision = self.store.criterion_revision(&criterion.id).await?;
            let assessment = self.store.assess_criterion(criterion, revision).await?;
            if assessment.state == CriterionState::NotApplicable {
                exemptions.insert(criterion.id.clone(), assessment.reason);
            }
        }
        let mut runner = self.runner.clone().with_tool_budget(
            self.runner
                .tool_calls()
                .saturating_add(effective_budget.max_tool_calls),
        );
        runner.workspace = runner
            .workspace
            .canonicalize()
            .map_err(|e| fail(e.to_string()))?;
        if let Some(projection) = &self.checklist {
            match projection.absolute_path(run).await {
                Ok(path) if path.starts_with(&runner.workspace) => {
                    runner.projection_paths.push(path)
                }
                Ok(_) => {}
                Err(e) => issue(
                    &mut report,
                    "checklist",
                    Kind::ProjectCheck,
                    &e.to_string(),
                    true,
                ),
            }
        }
        // Reuse fresh passing execution evidence; all remaining declared checks run
        // through the host's secure ToolRuntime, never shell shortcuts.
        for criterion in &check_catalog {
            if exemptions.contains_key(&criterion.id) {
                continue;
            }
            for check in &criterion.checks {
                if matches!(check.definition, CheckDefinition::Inspection) {
                    continue;
                }
                let old = self
                    .store
                    .task_evidence(&task.id)
                    .await?
                    .into_iter()
                    .rev()
                    .find(|e| e.check_id == check.id && e.kind == EvidenceKind::Execution);
                let revision = self.store.criterion_revision(&criterion.id).await?;
                let fresh = if let Some(e) = old {
                    e.trusted
                        && e.snapshot.workspace == runner.workspace
                        && !e.stale
                        && e.result == EvidenceResult::Pass
                        && e.snapshot.fresh(revision).await?
                } else {
                    false
                };
                if !fresh {
                    let before_calls = runner.tool_calls();
                    match runner
                        .run_keyed(
                            &self.store,
                            &task.id,
                            &check.id,
                            run,
                            &format!("final-check:{}:{}", input.step, check.id),
                        )
                        .await
                    {
                        Ok(e) => {
                            let journal =
                                excalibur_journal::Journal::new(self.store.db.clone()).await?;
                            journal.append(run,"verification_usage",&format!("final-check-usage:{}:{}",input.step,check.id),json!({"usage":excalibur_types::runtime::Usage::default(),"model_calls":0,"tool_calls":e.tool_calls,"step_count":0,"source":"declared_final_check"})).await?;
                        }
                        Err(e) => {
                            let delta = runner.tool_calls().saturating_sub(before_calls);
                            if delta > 0 {
                                let journal =
                                    excalibur_journal::Journal::new(self.store.db.clone()).await?;
                                journal.append(run,"verification_usage",&format!("final-check-usage:{}:{}",input.step,check.id),json!({"usage":excalibur_types::runtime::Usage::default(),"model_calls":0,"tool_calls":delta,"step_count":0,"source":"declared_final_check"})).await?;
                            }
                            issue(
                                &mut report,
                                &check.id,
                                Kind::ProjectCheck,
                                &e.to_string(),
                                true,
                            )
                        }
                    }
                }
            }
        }
        for criterion in &criteria {
            if exemptions.contains_key(&criterion.id)
                || criterion.method != CheckMethod::Inspection
                || !self.implicit(&criterion.id).await?
                || !input.local_pending.is_empty()
            {
                continue;
            }
            if !self.confirmed_response(run, &input.final_text).await? {
                issue(
                    &mut report,
                    &criterion.id,
                    Kind::Criterion,
                    "implicit response lacks a matching durable model response",
                    true,
                );
                continue;
            }
            let revision = self.store.criterion_revision(&criterion.id).await?;
            let prior = self
                .store
                .task_evidence(&task.id)
                .await?
                .into_iter()
                .rev()
                .find(|e| {
                    e.criterion_id == criterion.id && e.result == EvidenceResult::Pass && !e.stale
                });
            if let Some(e) = prior
                && e.snapshot.fresh(revision).await?
            {
                continue;
            }
            let snapshot = runner
                .snapshot(criterion.impact_area.clone(), revision)
                .await?;
            self.store.record_judgment(&task.id,&criterion.id,run,snapshot,!input.final_text.trim().is_empty(),"inspection of the durable final response; independent review remains required at higher work levels",false).await?;
        }
        let mandatory = task.level == WorkLevel::N3
            || (task.level == WorkLevel::N2 && requirements.len() >= 5)
            || task.metadata["independent_verification"].as_bool() == Some(true)
            || task.metadata["user_requested_verification"].as_bool() == Some(true)
            || (task.level != WorkLevel::N1
                && criteria.iter().any(|c| c.method == CheckMethod::Inspection));
        // Execute the independent reviewer even when an inspection has no executor
        // judgment; its own observation may supply the missing inspection proof.
        let executable_ready = criteria
            .iter()
            .filter(|c| {
                matches!(
                    c.method,
                    CheckMethod::Command | CheckMethod::Test | CheckMethod::Metric
                )
            })
            .map(|c| c.id.clone())
            .collect::<Vec<_>>();
        let mut executable_ok = true;
        for criterion in criteria.iter().filter(|c| executable_ready.contains(&c.id)) {
            let revision = self.store.criterion_revision(&criterion.id).await?;
            if self
                .store
                .assess_criterion(criterion, revision)
                .await?
                .state
                != CriterionState::Satisfied
            {
                executable_ok = false;
            }
        }
        if mandatory && executable_ok {
            match &self.verifier {
                Some(verifier) => {
                    match self
                        .independent(
                            run,
                            input,
                            &task,
                            &requirements,
                            &criteria,
                            &runner,
                            verifier.as_ref(),
                            "task",
                        )
                        .await
                    {
                        Ok(verdict) => {
                            if task.level == WorkLevel::N3
                                && verdict.request_covered
                                && verdict.uncovered_requests.is_empty()
                                && verdict.criteria.iter().all(|v| v.passed)
                                && let Err(error) =
                                    self.close_verified_milestones(&task, &verdict).await
                            {
                                issue(
                                    &mut report,
                                    "milestones",
                                    Kind::IndependentVerification,
                                    &error.to_string(),
                                    false,
                                )
                            }
                            for omission in &verdict.uncovered_requests {
                                self.record_omission(&task, omission).await?;
                                issue(
                                    &mut report,
                                    "original-request",
                                    Kind::OriginalRequest,
                                    omission,
                                    false,
                                )
                            }
                            if !verdict.request_covered {
                                issue(
                                    &mut report,
                                    "original-request",
                                    Kind::OriginalRequest,
                                    "independent review found uncovered parts of the literal request",
                                    false,
                                )
                            }
                            for cr in &criteria {
                                if cr.method == CheckMethod::UserConfirmation
                                    || exemptions.contains_key(&cr.id)
                                {
                                    continue;
                                }
                                match verdict.criteria.iter().find(|v| v.criterion_id == cr.id) {
                                    Some(v) if v.passed => {}
                                    Some(v) => issue(
                                        &mut report,
                                        &cr.id,
                                        Kind::IndependentVerification,
                                        &v.explanation,
                                        false,
                                    ),
                                    None => issue(
                                        &mut report,
                                        &cr.id,
                                        Kind::IndependentVerification,
                                        "independent review omitted the criterion",
                                        false,
                                    ),
                                }
                            }
                        }
                        Err(e) => issue(
                            &mut report,
                            "independent-verification",
                            Kind::IndependentVerification,
                            &e.to_string(),
                            e.to_string().contains("two rounds"),
                        ),
                    }
                }
                None => issue(
                    &mut report,
                    "independent-verification",
                    Kind::IndependentVerification,
                    "required independent verifier is not configured",
                    true,
                ),
            }
        }
        let mut entries = Vec::new();
        for cr in &criteria {
            let revision = self.store.criterion_revision(&cr.id).await?;
            let assessment = self.store.assess_criterion(cr, revision).await?;
            if !matches!(
                assessment.state,
                CriterionState::Satisfied | CriterionState::NotApplicable
            ) {
                issue(
                    &mut report,
                    &cr.id,
                    if cr.method == CheckMethod::UserConfirmation {
                        Kind::UserConfirmation
                    } else {
                        Kind::Criterion
                    },
                    &assessment.reason,
                    false,
                );
            }
            for check in cr.checks.iter().filter(|c| {
                !matches!(c.definition, CheckDefinition::Inspection)
                    && !exemptions.contains_key(&cr.id)
            }) {
                let evidence = self
                    .store
                    .task_evidence(&task.id)
                    .await?
                    .into_iter()
                    .rev()
                    .find(|e| e.check_id == check.id && e.kind == EvidenceKind::Execution);
                let fresh = if let Some(e) = &evidence {
                    e.trusted && !e.stale && e.snapshot.fresh(revision).await?
                } else {
                    false
                };
                entries.push(crate::checklist::ChecklistEntry {
                    check_id: check.id.clone(),
                    label: check.id.clone(),
                    passed: evidence
                        .as_ref()
                        .is_some_and(|e| e.result == EvidenceResult::Pass),
                    fresh,
                    recorded_at: evidence.as_ref().map(|e| e.recorded_at),
                    commit: evidence.as_ref().and_then(|e| e.snapshot.commit.clone()),
                    evidence_id: evidence.as_ref().map(|e| e.id.clone()),
                });
            }
            if let Some(e) = &assessment.evidence {
                report.evidence.push(DeliveryReference {
                    reference: e.id.clone(),
                    description: format!("{}: {}", cr.id, assessment.reason),
                });
            }
            self.persist_assessment(&assessment, revision).await?;
        }
        for criterion in &project_checks {
            if criteria.iter().any(|c| c.id == criterion.id)
                || exemptions.contains_key(&criterion.id)
            {
                continue;
            }
            let revision = self.store.criterion_revision(&criterion.id).await?;
            let a = self.store.assess_criterion(criterion, revision).await?;
            if a.state != CriterionState::Satisfied {
                issue(
                    &mut report,
                    &criterion.id,
                    Kind::ProjectCheck,
                    &a.reason,
                    false,
                )
            }
            for check in &criterion.checks {
                let e = self
                    .store
                    .task_evidence(&task.id)
                    .await?
                    .into_iter()
                    .rev()
                    .find(|e| e.check_id == check.id && e.kind == EvidenceKind::Execution);
                let fresh = if let Some(e) = &e {
                    e.snapshot.workspace == runner.workspace
                        && e.trusted
                        && !e.stale
                        && e.snapshot.fresh(revision).await?
                } else {
                    false
                };
                entries.push(crate::checklist::ChecklistEntry {
                    check_id: check.id.clone(),
                    label: check.id.clone(),
                    passed: e.as_ref().is_some_and(|e| e.result == EvidenceResult::Pass),
                    fresh,
                    recorded_at: e.as_ref().map(|e| e.recorded_at),
                    commit: e.as_ref().and_then(|e| e.snapshot.commit.clone()),
                    evidence_id: e.as_ref().map(|e| e.id.clone()),
                });
                if !fresh || e.as_ref().is_none_or(|e| e.result != EvidenceResult::Pass) {
                    issue(
                        &mut report,
                        &check.id,
                        Kind::ProjectCheck,
                        "project check lacks fresh passing execution in the current workspace",
                        false,
                    )
                }
            }
        }
        if let Some(projection) = &self.checklist
            && let Err(e) = projection.update(&self.store, run, &entries).await
        {
            issue(
                &mut report,
                "checklist",
                Kind::ProjectCheck,
                &e.to_string(),
                true,
            )
        }
        // Keep this lease through the engine's terminal compare-and-set. Recheck
        // snapshots/revisions after checks, independent work and projection writes.
        let lease = self.store.mutation_lock.clone().read_owned().await;
        let current_task = self.store.task(&task.scope_id, &task.id).await?;
        if current_task.version != task.version || current_task.request_card != task.request_card {
            issue(
                &mut report,
                "task-specification",
                Kind::OriginalRequest,
                "accepted task specification changed during verification",
                false,
            )
        }
        let (now_requirements, now_criteria) =
            self.store.requirements_and_criteria(&task.id).await?;
        // Internal synthetic inspection checks are added by the host and are not
        // requirement edits. Compare all user-facing acceptance fields instead.
        if serde_json::to_value(&requirements)? != serde_json::to_value(&now_requirements)?
            || criterion_signature(&criteria) != criterion_signature(&now_criteria)
        {
            issue(
                &mut report,
                "requirements",
                Kind::Criterion,
                "requirements changed during verification",
                false,
            )
        }
        for cr in &now_criteria {
            let revision = self.store.criterion_revision(&cr.id).await?;
            if revision > input.instruction_revision {
                issue(
                    &mut report,
                    &cr.id,
                    Kind::InstructionRevision,
                    "applicable instructions are newer than the final response",
                    false,
                )
            }
            let assessment = self.store.assess_criterion(cr, revision).await?;
            if !matches!(
                assessment.state,
                CriterionState::Satisfied | CriterionState::NotApplicable
            ) && !report.issues.iter().any(|i| i.id == cr.id)
            {
                issue(
                    &mut report,
                    &cr.id,
                    if cr.method == CheckMethod::UserConfirmation {
                        Kind::UserConfirmation
                    } else {
                        Kind::Criterion
                    },
                    &assessment.reason,
                    false,
                )
            }
        }
        for criterion in &project_checks {
            if exemptions.contains_key(&criterion.id) {
                continue;
            }
            let revision = self.store.criterion_revision(&criterion.id).await?;
            let a = self.store.assess_criterion(criterion, revision).await?;
            if a.state != CriterionState::Satisfied
                || a.evidence
                    .as_ref()
                    .is_some_and(|e| e.snapshot.workspace != runner.workspace)
            {
                issue(
                    &mut report,
                    &criterion.id,
                    Kind::ProjectCheck,
                    "registered project check is stale, failed, or belongs to another workspace",
                    false,
                )
            }
        }
        let run_id = run.to_owned();
        let children=self.store.db.read(ReadLane::Interactive,move|c|{let mut s=c.prepare("WITH RECURSIVE descendants(id) AS(SELECT run_id FROM task_run_links WHERE parent_run_id=?1 UNION SELECT l.run_id FROM task_run_links l JOIN descendants d ON l.parent_run_id=d.id) SELECT r.id,r.state FROM runs r JOIN descendants d ON d.id=r.id WHERE r.state NOT IN('completed','completed_with_pending','cancelled','failed')")?;Ok(s.query_map([run_id],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?)))?.collect::<std::result::Result<Vec<_>,_>>()?)}).await?;
        for (id, state) in children {
            issue(
                &mut report,
                &id,
                Kind::Child,
                &format!("child execution remains {state}"),
                false,
            )
        }
        if let Some(resources) = runner
            .runtime
            .execution_resources(run)
            .await
            .map_err(fail)?
            && resources.processes > 0
        {
            issue(
                &mut report,
                "active-processes",
                Kind::Child,
                "managed processes remain active; finalization must wait",
                false,
            )
        }
        let owner = run.to_owned();
        let pending=self.store.db.read(ReadLane::Interactive,move|c|{let mut q=c.prepare("WITH RECURSIVE tree(id) AS(SELECT ?1 UNION SELECT l.run_id FROM task_run_links l JOIN tree t ON l.parent_run_id=t.id) SELECT a.check_id FROM task_check_attempts a JOIN tree t ON t.id=a.run_id WHERE a.state='intent'")?;Ok(q.query_map([owner],|r|r.get::<_,String>(0))?.collect::<std::result::Result<Vec<_>,_>>()?)}).await?;
        for id in pending {
            issue(
                &mut report,
                &id,
                Kind::ProjectCheck,
                "an admitted check has no confirmed outcome; it cannot be repeated implicitly",
                true,
            )
        }
        let owner = run.to_owned();
        report.modified_files=self.store.db.read(ReadLane::Interactive,move|c|{let mut s=c.prepare("SELECT DISTINCT path FROM file_versions WHERE run_id=?1 ORDER BY path LIMIT 1000")?;Ok(s.query_map([owner],|r|r.get::<_,String>(0))?.collect::<std::result::Result<Vec<_>,_>>()?)}).await?;
        let owner = run.to_owned();
        report.deliverables=self.store.db.read(ReadLane::Interactive,move|c|{let mut s=c.prepare("SELECT v.path FROM file_versions v JOIN(SELECT path,max(rowid) latest FROM file_versions WHERE run_id=?1 GROUP BY path) last ON last.latest=v.rowid WHERE v.postimage_blob IS NOT NULL ORDER BY v.path LIMIT 1000")?;Ok(s.query_map([owner],|r|Ok(DeliveryReference{reference:r.get(0)?,description:"versioned file output".into()}))?.collect::<std::result::Result<Vec<_>,_>>()?)}).await?;
        report
            .modified_files
            .retain(|p| std::path::Path::new(p).starts_with(&runner.workspace));
        report
            .deliverables
            .retain(|d| std::path::Path::new(&d.reference).starts_with(&runner.workspace));
        Ok(Box::new(TaskGateGuard {
            report,
            _lease: lease,
        }))
    }
    async fn implicit(&self, id: &str) -> Result<bool> {
        let id = id.to_owned();
        Ok(self.store.db.read(ReadLane::Interactive,move|c|Ok(c.query_row("SELECT COALESCE(json_extract(metadata,'$.implicit_final_response'),0) FROM criteria WHERE id=?1",[id],|r|r.get::<_,bool>(0))?)).await?)
    }
    async fn confirmed_response(&self, run: &str, text: &str) -> Result<bool> {
        let (run, text) = (run.to_owned(), text.to_owned());
        Ok(self.store.db.read(ReadLane::Interactive,move|c|{let payload:Option<String>=c.query_row("SELECT payload FROM journal WHERE run_id=?1 AND kind='model_response' AND state='confirmed' ORDER BY sequence DESC LIMIT 1",[run],|r|r.get(0)).optional()?;Ok(payload.and_then(|s|serde_json::from_str::<Value>(&s).ok()).is_some_and(|p|p["response"]["message"]["content"].as_str()==Some(&text)&&p["response"]["message"]["tool_calls"].as_array().is_none_or(Vec::is_empty)))}).await?)
    }
    async fn persist_assessment(&self, a: &CriterionAssessment, revision: u64) -> Result<()> {
        let a = a.clone();
        self.store.db.write(Priority::Normal,move|c|{let state=serde_json::to_value(a.state).unwrap().as_str().unwrap().to_owned();c.execute("INSERT INTO task_criterion_state(criterion_id,state,reason,latest_evidence_id,instruction_revision) VALUES(?1,?2,?3,?4,?5) ON CONFLICT(criterion_id) DO UPDATE SET state=excluded.state,reason=excluded.reason,latest_evidence_id=excluded.latest_evidence_id,instruction_revision=excluded.instruction_revision",rusqlite::params![a.criterion_id,state,a.reason,a.evidence.map(|e|e.id),revision])?;Ok(())}).await?;
        Ok(())
    }
    #[allow(clippy::too_many_arguments)]
    async fn independent(
        &self,
        run: &str,
        gate: &excalibur_engine::GateInput,
        task: &crate::TaskRecord,
        requirements: &[RequirementBrief],
        criteria: &[CriterionBrief],
        runner: &CheckRunner,
        verifier: &dyn IndependentVerifier,
        scope: &str,
    ) -> Result<VerificationVerdict> {
        verifier.prepare(run).await.map_err(fail)?;
        let literal = literal_request(&task.request_card)?;
        let owner = run.to_owned();
        let mut applicable=self.store.db.read(ReadLane::Interactive,move|c|{let mut q=c.prepare("SELECT json_extract(payload,'$.id'),json_extract(payload,'$.text') FROM journal WHERE run_id=?1 AND kind='directive_applied' AND state='confirmed' ORDER BY sequence")?;let rows=q.query_map([owner],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?)))?.collect::<std::result::Result<Vec<_>,_>>()?;Ok(rows.into_iter().enumerate().map(|(i,(id,text))|ApplicableInstruction{id,text,revision:i as u64+1}).collect::<Vec<_>>())}).await?;
        if applicable
            .last()
            .is_some_and(|i| i.revision > gate.instruction_revision)
        {
            return Err(fail(
                "new applicable instructions are not covered by the final response",
            ));
        }
        for (index, value) in task.request_card["pinned_instructions"]
            .as_array()
            .into_iter()
            .flatten()
            .enumerate()
        {
            if let Some(text) = value
                .as_str()
                .or_else(|| value["text"].as_str())
                .or_else(|| value["literal_text"].as_str())
            {
                applicable.push(ApplicableInstruction {
                    id: value["id"]
                        .as_str()
                        .map(str::to_owned)
                        .unwrap_or_else(|| format!("pinned:{index}")),
                    text: text.into(),
                    revision: value["revision"].as_u64().unwrap_or(0),
                });
            }
        }
        let evidence = self.store.task_evidence(&task.id).await?;
        let mut waived = std::collections::BTreeMap::new();
        for criterion in criteria {
            let revision = self.store.criterion_revision(&criterion.id).await?;
            let a = self.store.assess_criterion(criterion, revision).await?;
            if a.state == CriterionState::NotApplicable {
                waived.insert(criterion.id.clone(), a.reason);
            }
        }
        let mut impacts = std::collections::BTreeMap::new();
        for criterion in criteria {
            if waived.contains_key(&criterion.id) {
                continue;
            }
            impacts.insert(
                criterion.id.clone(),
                runner
                    .snapshot(criterion.impact_area.clone(), gate.instruction_revision)
                    .await?,
            );
        }
        let signature = crate::digest(&serde_json::to_vec(
            &json!({"run":run,"task_version":task.version,"instructions":applicable,"literal":literal,"scope":scope,"waived":waived,"instruction_revision":gate.instruction_revision,"requirements":requirements,"criteria":criterion_signature(criteria),"impacts":impacts,"summary":gate.final_text}),
        )?);
        let job_base = format!("verification-{signature}");
        let pattern = format!("{job_base}-%");
        let existing=self.store.db.read(ReadLane::Interactive,move|c|{let mut s=c.prepare("SELECT verdict,input FROM task_verifier_jobs WHERE id LIKE ?1 AND state='completed' ORDER BY id DESC")?;Ok(s.query_map([pattern],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?)))?.collect::<std::result::Result<Vec<_>,_>>()?)}).await?;
        for (text, source) in existing {
            let verdict: VerificationVerdict = serde_json::from_str(&text)?;
            if verdict.request_covered
                && verdict.uncovered_requests.is_empty()
                && verdict.criteria.iter().all(|v| v.passed)
            {
                let source: VerificationInput = serde_json::from_str(&source)?;
                if self
                    .validate_verdict(&source, &verdict, runner)
                    .await
                    .is_ok()
                {
                    return Ok(verdict);
                }
            }
        }
        let (root, ids, round_scope) = (
            run.to_owned(),
            criteria
                .iter()
                .filter(|c| {
                    c.method != CheckMethod::UserConfirmation && !waived.contains_key(&c.id)
                })
                .map(|c| c.id.clone())
                .collect::<Vec<_>>(),
            scope.to_owned(),
        );
        let round=self.store.db.read(ReadLane::Interactive,move|c|{let mut round=1u8;for id in &ids{let n:u8=c.query_row("SELECT rounds FROM task_verifier_rounds WHERE root_run_id=?1 AND criterion_id=?2 AND verification_scope=?3",rusqlite::params![root,id,round_scope],|r|r.get(0)).optional()?.unwrap_or(0);if n>=2{return Err(excalibur_store::Error::Invalid("independent verifier exhausted two rounds for a criterion"))}round=round.max(n+1);}Ok(round)}).await?;
        let job_id = format!("{job_base}-{}", new_id("attempt"));
        let journal = excalibur_journal::Journal::new(self.store.db.clone()).await?;
        let events = journal.entries(run, 0, 1).await?;
        let refreshed = if events.first().is_some_and(|e| !e.payload["spec"].is_null()) {
            Some(
                excalibur_engine::remaining_budget_for(&journal, run)
                    .await
                    .map_err(|e| fail(e.to_string()))?,
            )
        } else {
            None
        };
        let remaining = refreshed.as_ref().unwrap_or(&gate.budget_remaining);
        let tokens = remaining.max_tokens.min(40000);
        let budget = VerificationBudget {
            max_steps: remaining.max_steps.min(4),
            max_tool_calls: remaining.max_tool_calls.min(16),
            input_tokens: tokens.saturating_mul(3) / 4,
            output_tokens: tokens / 4,
            cost_microusd: remaining
                .max_cost_microusd
                .map(|n| n.min(1000000))
                .unwrap_or(u64::MAX),
            timeout_seconds: remaining.max_wall_seconds.min(120),
        };
        let input = VerificationInput {
            task_version: task.version,
            applicable_instructions: applicable,
            waived_criteria: waived.clone(),
            workspace_snapshots: impacts,
            verification_scope: scope.into(),
            job_id: job_id.clone(),
            task_id: task.id.clone(),
            root_run_id: run.into(),
            parent_run_id: run.into(),
            round,
            instruction_revision: gate.instruction_revision,
            literal_request: literal,
            requirements: requirements.to_vec(),
            criteria: criteria
                .iter()
                .filter(|c| {
                    c.method != CheckMethod::UserConfirmation && !waived.contains_key(&c.id)
                })
                .map(|c| {
                    let mut c = c.clone();
                    c.checks
                        .retain(|c| !matches!(c.definition, CheckDefinition::Inspection));
                    c
                })
                .collect(),
            evidence,
            diff: self.delivery_diff(run).await?,
            delivery_summary: gate.final_text.clone(),
            budget,
            budget_remaining: remaining.clone(),
        };
        let input: VerificationInput = serde_json::from_value(
            self.store
                .redactor
                .redact_json(&serde_json::to_value(input)?),
        )?;
        let (id, task_id, root, text) = (
            job_id.clone(),
            task.id.clone(),
            run.to_owned(),
            serde_json::to_string(&input)?,
        );
        self.store.db.write(Priority::Journal,move|c|{c.execute("INSERT INTO task_verifier_jobs(id,task_id,root_run_id,round,instruction_revision,input,state) VALUES(?1,?2,?3,?4,?5,?6,'running')",rusqlite::params![id,task_id,root,round,gate_revision(&text),text])?;Ok(())}).await?;
        let verdict = match verifier.verify(&input).await {
            Ok(v) => serde_json::from_value::<VerificationVerdict>(
                self.store.redactor.redact_json(&serde_json::to_value(v)?),
            )?,
            Err(error) => {
                self.fail_verifier_attempt(&job_id, &error).await?;
                return Err(fail(error));
            }
        };
        if let Err(error) = self.validate_verdict(&input, &verdict, runner).await {
            self.fail_verifier_attempt(&job_id, &error.to_string())
                .await?;
            self.record_disagreement(
                &input,
                &input
                    .criteria
                    .iter()
                    .map(|c| c.id.clone())
                    .collect::<Vec<_>>(),
            )
            .await?;
            return Err(error);
        }
        let failed = if !verdict.request_covered || !verdict.uncovered_requests.is_empty() {
            input
                .criteria
                .iter()
                .map(|c| c.id.clone())
                .collect::<Vec<_>>()
        } else {
            verdict
                .criteria
                .iter()
                .filter(|v| !v.passed)
                .map(|v| v.criterion_id.clone())
                .collect()
        };
        if !failed.is_empty() {
            self.record_disagreement(&input, &failed).await?;
        }
        let (id, owner, text) = (
            job_id,
            verdict.verifier_run_id.clone(),
            serde_json::to_string(&verdict)?,
        );
        self.store.db.write(Priority::Journal,move|c|{c.execute("UPDATE task_verifier_jobs SET verifier_run_id=?1,verdict=?2,state='completed',error=NULL WHERE id=?3",rusqlite::params![owner,text,id])?;Ok(())}).await?;
        Ok(verdict)
    }
    async fn fail_verifier_attempt(&self, id: &str, error: &str) -> Result<()> {
        let (id, error) = (id.to_owned(), self.store.redactor.redact_text(error));
        self.store
            .db
            .write(Priority::Journal, move |c| {
                c.execute(
                    "UPDATE task_verifier_jobs SET state='failed',error=?1 WHERE id=?2",
                    rusqlite::params![error, id],
                )?;
                Ok(())
            })
            .await?;
        Ok(())
    }
    async fn record_disagreement(
        &self,
        input: &VerificationInput,
        criteria: &[String],
    ) -> Result<()> {
        let (root, scope, criteria) = (
            input.root_run_id.clone(),
            input.verification_scope.clone(),
            criteria.to_vec(),
        );
        self.store.db.write(Priority::Journal,move|c|{for id in criteria{c.execute("INSERT INTO task_verifier_rounds(root_run_id,criterion_id,rounds,verification_scope) VALUES(?1,?2,1,?3) ON CONFLICT(root_run_id,criterion_id,verification_scope) DO UPDATE SET rounds=min(2,rounds+1)",rusqlite::params![root,id,scope])?;}Ok(())}).await?;
        Ok(())
    }
    async fn validate_verdict(
        &self,
        input: &VerificationInput,
        verdict: &VerificationVerdict,
        _runner: &CheckRunner,
    ) -> Result<()> {
        let ids = verdict
            .criteria
            .iter()
            .map(|v| v.criterion_id.as_str())
            .collect::<std::collections::BTreeSet<_>>();
        if ids.len() != verdict.criteria.len()
            || ids.len() != input.criteria.len()
            || ids
                .iter()
                .any(|id| !input.criteria.iter().any(|c| c.id == *id))
        {
            return Err(fail(
                "independent verdict criterion set differs from the accepted input",
            ));
        }
        for snapshot in input.workspace_snapshots.values() {
            if !snapshot.fresh(input.instruction_revision).await? {
                return Err(fail(
                    "affected content changed during independent verification",
                ));
            }
        }
        if verdict.verifier_run_id.is_empty() || verdict.verifier_run_id == input.root_run_id {
            return Err(fail(
                "independent verdict must identify a distinct durable execution",
            ));
        }
        let binding = self
            .store
            .task_for_run(&verdict.verifier_run_id)
            .await?
            .ok_or_else(|| fail("verifier run is not bound to task"))?;
        if binding.id != input.task_id {
            return Err(fail("verifier proof belongs to another task"));
        }
        let owner = verdict.verifier_run_id.clone();
        let parent = self
            .store
            .db
            .read(ReadLane::Interactive, move |c| {
                Ok(c.query_row(
                    "SELECT parent_run_id FROM task_run_links WHERE run_id=?1",
                    [owner],
                    |r| r.get::<_, Option<String>>(0),
                )?)
            })
            .await?;
        if parent.as_deref() != Some(&input.root_run_id) {
            return Err(fail("verifier proof has a different parent execution"));
        }

        for criterion in &input.criteria {
            let Some(v) = verdict
                .criteria
                .iter()
                .find(|v| v.criterion_id == criterion.id)
            else {
                return Err(fail("independent verdict omitted a criterion"));
            };
            if !v.passed {
                continue;
            }
            if matches!(
                criterion.method,
                CheckMethod::Command | CheckMethod::Test | CheckMethod::Metric
            ) || !criterion.checks.is_empty()
            {
                for check in &criterion.checks {
                    let mut proven = false;
                    for id in &v.evidence_ids {
                        if let Some(e) = self.store.evidence(id).await?
                            && e.run_id == verdict.verifier_run_id
                            && e.check_id == check.id
                            && e.kind == EvidenceKind::Execution
                            && e.trusted
                            && !e.stale
                            && e.result == EvidenceResult::Pass
                            && e.snapshot.fresh(input.instruction_revision).await?
                        {
                            proven = true;
                            break;
                        }
                    }
                    if !proven {
                        return Err(fail(
                            "independent code verdict lacks its own fresh executed proof",
                        ));
                    }
                }
            } else {
                let implicit = self.implicit(&criterion.id).await?;
                let mut own = false;
                for id in &v.evidence_ids {
                    if let Some(seq) = id
                        .strip_prefix(&format!("journal:{}:", verdict.verifier_run_id))
                        .and_then(|s| s.parse::<u64>().ok())
                    {
                        let (run, seq) = (verdict.verifier_run_id.clone(), seq);
                        let row=self.store.db.read(ReadLane::Interactive,move|c|Ok(c.query_row("SELECT kind,payload FROM journal WHERE run_id=?1 AND sequence=?2 AND state='confirmed'",rusqlite::params![run,seq],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?))).optional()?)).await?;
                        if let Some((kind, payload)) = row {
                            let p: Value = serde_json::from_str(&payload)?;
                            if kind == "verifier_tool_result"
                                && p["result"]["success"] == true
                                && matches!(
                                    p["call"]["name"].as_str(),
                                    Some("read_file" | "glob" | "grep")
                                )
                            {
                                own = true
                            }
                            if implicit && kind == "verifier_model_response" {
                                own = true
                            }
                        }
                    }
                }
                if !own {
                    return Err(fail(
                        "independent inspection lacks its own durable observation",
                    ));
                }
                let snapshot = input
                    .workspace_snapshots
                    .get(&criterion.id)
                    .cloned()
                    .ok_or_else(|| fail("independent input lacks criterion snapshot"))?;
                let mut e = self
                    .store
                    .record_judgment(
                        &input.task_id,
                        &criterion.id,
                        &verdict.verifier_run_id,
                        snapshot,
                        true,
                        &v.explanation,
                        false,
                    )
                    .await?;
                e.kind = EvidenceKind::IndependentVerification;
                let (id, text) = (e.id.clone(), serde_json::to_string(&e)?);
                self.store.db.write(Priority::Journal,move|c|{c.execute("UPDATE evidence SET evidence_kind='independent_verification',metadata=?1 WHERE id=?2",rusqlite::params![text,id])?;Ok(())}).await?;
            }
        }
        Ok(())
    }
    async fn close_verified_milestones(
        &self,
        task: &crate::TaskRecord,
        verdict: &VerificationVerdict,
    ) -> Result<()> {
        let (root, owner) = (
            task.root_run_id
                .clone()
                .ok_or_else(|| fail("milestone verification needs task root"))?,
            verdict.verifier_run_id.clone(),
        );
        let job=self.store.db.read(ReadLane::Interactive,move|c|Ok(c.query_row("SELECT id FROM task_verifier_jobs WHERE root_run_id=?1 AND verifier_run_id=?2 AND state='completed' ORDER BY id DESC LIMIT 1",rusqlite::params![root,owner],|r|r.get::<_,String>(0))?)).await?;
        let (scope, taskid, principal) = (
            task.scope_id.clone(),
            task.id.clone(),
            self.store.policy.principal_id.clone(),
        );
        let rows=self.store.db.read(ReadLane::Interactive,move|c|{let mut q=c.prepare("SELECT DISTINCT m.id,m.version,cl.verifier_job_id,cl.version FROM milestones m LEFT JOIN task_milestone_closures cl ON cl.milestone_id=m.id WHERE m.scope_id=?1 AND m.principal_id IS ?2 AND m.state NOT IN('archived','cancelled') AND EXISTS(SELECT 1 FROM json_each(m.criteria) refs JOIN criteria cr ON cr.id=refs.value JOIN task_requirements tr ON tr.requirement_id=cr.requirement_id WHERE tr.task_id=?3)")?;Ok(q.query_map(rusqlite::params![scope,principal,taskid],|r|Ok((r.get::<_,String>(0)?,r.get::<_,u64>(1)?,r.get::<_,Option<String>>(2)?,r.get::<_,Option<u64>>(3)?)))?.collect::<std::result::Result<Vec<_>,_>>()?)}).await?;
        for (id, version, old, closed_version) in rows {
            if old.as_deref() == Some(&job) && closed_version == Some(version) {
                continue;
            }
            self.store
                .close_milestone(
                    &task.scope_id,
                    &id,
                    version,
                    task.root_run_id.as_deref().unwrap(),
                    &job,
                )
                .await?;
        }
        Ok(())
    }
    async fn record_omission(&self, task: &crate::TaskRecord, text: &str) -> Result<()> {
        let id = format!(
            "omission-{}",
            &crate::digest(format!("{}:{text}", task.id).as_bytes())[..24]
        );
        let criterion = format!("{id}-criterion");
        let mut data = json!({"text":text,"state":"accepted","kind":"functional","metadata":{"origin":"independent_request_contrast"}});
        if let Some(project) = &task.project_id {
            data["project_id"] = json!(project);
        }
        self.store.batch(&task.scope_id,&format!("record-{id}"),json!({"ops":[{"op":"create_requirement","id":id,"data":data},{"op":"create_criterion","id":criterion,"data":{"requirement_id":id,"statement":text,"method":"inspection","impact_area":["**"]}},{"op":"link_requirement","task":task.id,"requirement":id}]})).await?;
        Ok(())
    }
    async fn delivery_diff(&self, run: &str) -> Result<String> {
        let run = run.to_owned();
        let values=self.store.db.read(ReadLane::Interactive,move|c|{let mut s=c.prepare("SELECT payload FROM journal WHERE run_id=?1 AND kind='tool_result' AND state='confirmed' ORDER BY sequence")?;Ok(s.query_map([run],|r|r.get::<_,String>(0))?.collect::<std::result::Result<Vec<_>,_>>()?)}).await?;
        let mut out = String::new();
        fn visit(value: &Value, out: &mut String) {
            if let Some(diff) = value["diff"].as_str()
                && out.len() + diff.len() <= 65536
            {
                out.push_str(diff);
                out.push('\n')
            }
            match value {
                Value::Object(o) => {
                    for v in o.values() {
                        visit(v, out)
                    }
                }
                Value::Array(a) => {
                    for v in a {
                        visit(v, out)
                    }
                }
                _ => {}
            }
        }
        for value in values {
            if let Ok(v) = serde_json::from_str::<Value>(&value) {
                visit(&v, &mut out)
            }
        }
        Ok(out)
    }
}
fn literal_request(card: &Value) -> Result<String> {
    let value = card
        .get("original")
        .or_else(|| card.get("literal_request"))
        .ok_or_else(|| fail("literal original request is missing"))?;
    if let Some(text) = value.as_str() {
        return Ok(text.into());
    }
    let sources = match value {
        Value::Array(a) => a.clone(),
        Value::Object(_) => vec![value.clone()],
        _ => {
            return Err(fail(
                "literal original request must be text or source messages",
            ));
        }
    };
    let mut output = Vec::new();
    for source in sources {
        let text = source["text"]
            .as_str()
            .ok_or_else(|| fail("original source message lacks literal text"))?;
        output.push(
            json!({"msg_id":source.get("msg_id").cloned().unwrap_or(Value::Null),"text":text}),
        );
    }
    if output.is_empty() {
        return Err(fail("literal original request has no source messages"));
    }
    Ok(serde_json::to_string(&output)?)
}
fn gate_revision(text: &str) -> u64 {
    serde_json::from_str::<Value>(text)
        .ok()
        .and_then(|v| v["instruction_revision"].as_u64())
        .unwrap_or(0)
}
fn criterion_signature(criteria: &[CriterionBrief]) -> Value {
    json!(criteria.iter().map(|c|json!({"id":c.id,"requirement":c.requirement_id,"statement":c.statement,"method":c.method,"expected":c.expected,"impact":c.impact_area,"checks":c.checks.iter().filter(|c|!matches!(c.definition,CheckDefinition::Inspection)).collect::<Vec<_>>()})).collect::<Vec<_>>())
}
fn issue(
    report: &mut excalibur_engine::GateAssessment,
    id: &str,
    kind: excalibur_engine::GateIssueKind,
    detail: &str,
    blocked: bool,
) {
    report.issues.push(excalibur_engine::GateIssue {
        id: id.into(),
        kind,
        detail: detail.into(),
        blocked,
    });
}
struct TaskGateGuard {
    report: excalibur_engine::GateAssessment,
    _lease: tokio::sync::OwnedRwLockReadGuard<()>,
}
impl excalibur_engine::CompletionGateGuard for TaskGateGuard {
    fn report(&self) -> excalibur_engine::GateAssessment {
        self.report.clone()
    }
}
impl excalibur_engine::CompletionGate for TaskCompletionGate {
    fn interrupt<'a>(
        &'a self,
        run: &'a str,
        cancelled: bool,
    ) -> BoxFuture<'a, std::result::Result<(), String>> {
        Box::pin(async move {
            let Some(task) = self
                .store
                .task_for_run(run)
                .await
                .map_err(|e| e.to_string())?
            else {
                return Ok(());
            };
            let owner = run.to_owned();
            let sequence=self.store.db.read(ReadLane::Interactive,move|c|Ok(c.query_row("SELECT COALESCE(max(sequence),0) FROM journal WHERE run_id=?1 AND kind='state_change'",[owner],|r|r.get::<_,u64>(0))?)).await.map_err(|e|e.to_string())?;
            let receipt = if cancelled {
                self.store
                    .cancel_subtree(
                        &task.id,
                        &format!("gate-interrupt:{run}:{sequence}:cancel"),
                        true,
                    )
                    .await
            } else {
                self.store
                    .pause_subtree(&task.id, &format!("gate-interrupt:{run}:{sequence}:pause"))
                    .await
            }
            .map_err(|e| e.to_string())?;
            for child in receipt.runs {
                self.runner
                    .runtime
                    .interrupt_execution(&child, cancelled, None)
                    .await?;
            }
            Ok(())
        })
    }

    fn admission<'a>(
        &'a self,
        run: &'a str,
    ) -> BoxFuture<'a, std::result::Result<excalibur_engine::AdmissionState, String>> {
        Box::pin(async move {
            self.store
                .admission_state(run)
                .await
                .map_err(|e| e.to_string())
        })
    }

    fn inspect<'a>(
        &'a self,
        run: &'a str,
        input: &'a excalibur_engine::GateInput,
    ) -> BoxFuture<'a, std::result::Result<Box<dyn excalibur_engine::CompletionGateGuard>, String>>
    {
        Box::pin(async move {
            self.evaluate_gate(run, input)
                .await
                .map_err(|e| e.to_string())
        })
    }
    fn context<'a>(&'a self, run: &'a str) -> BoxFuture<'a, std::result::Result<String, String>> {
        Box::pin(async move {
            Ok(self
                .store
                .compact_micro_plan(run)
                .await
                .map_err(|e| e.to_string())?
                .unwrap_or_default())
        })
    }
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct StageVerification {
    pub job_id: String,
    pub verdict: VerificationVerdict,
}
impl TaskCompletionGate {
    /// Host stage verification. Only the selected, task-owned criterion set is
    /// reviewed; final request contrast remains a separate full-task operation.
    pub async fn verify_task_stage(
        &self,
        run: &str,
        input: &excalibur_engine::GateInput,
        criterion_ids: &[String],
    ) -> Result<StageVerification> {
        let task = self
            .store
            .task_for_run(run)
            .await?
            .ok_or_else(|| fail("stage execution is not task bound"))?;
        self.store.effect_admission(run).await?;
        let (requirements, all) = self.store.requirements_and_criteria(&task.id).await?;
        if criterion_ids.is_empty()
            || criterion_ids
                .iter()
                .any(|id| !all.iter().any(|c| c.id == *id))
        {
            return Err(fail("stage requires existing task criteria"));
        }
        let criteria = all
            .into_iter()
            .filter(|c| criterion_ids.contains(&c.id))
            .collect::<Vec<_>>();
        let requirements = requirements
            .into_iter()
            .filter(|r| criteria.iter().any(|c| c.requirement_id == r.id))
            .collect::<Vec<_>>();
        let mut runner = self.runner.clone();
        if let Some(projection) = &self.checklist {
            let path = projection.absolute_path(run).await?;
            if path.starts_with(&runner.workspace) {
                runner.projection_paths.push(path);
            }
        }
        let verifier = self
            .verifier
            .as_ref()
            .ok_or_else(|| fail("stage requires an independent verifier"))?;
        let mut ids = criterion_ids.to_vec();
        ids.sort();
        ids.dedup();
        let scope = format!("stage:{}", crate::digest(&serde_json::to_vec(&ids)?));
        let verdict = self
            .independent(
                run,
                input,
                &task,
                &requirements,
                &criteria,
                &runner,
                verifier.as_ref(),
                &scope,
            )
            .await?;
        for criterion in criteria {
            let revision = self.store.criterion_revision(&criterion.id).await?;
            let assessment = self.store.assess_criterion(&criterion, revision).await?;
            self.store
                .persist_criterion_assessment(assessment, revision)
                .await?;
        }
        let owner = verdict.verifier_run_id.clone();
        let job_id=self.store.db.read(ReadLane::Interactive,move|c|Ok(c.query_row("SELECT id FROM task_verifier_jobs WHERE verifier_run_id=?1 AND state='completed' ORDER BY id DESC LIMIT 1",[owner],|r|r.get(0))?)).await?;
        Ok(StageVerification { job_id, verdict })
    }
}
