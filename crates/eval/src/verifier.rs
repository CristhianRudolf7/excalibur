//! Independent verification uses a separate, read-only tool capability and journal.
use excalibur_models::ModelGateway;
use excalibur_security::{
    Autonomy, CapabilityToken, Network, Root, RunSecurity, SecuredTools, Security,
};
use excalibur_tasks::{
    TaskStore,
    verification::{CheckRunner, IndependentVerifier, VerificationInput, VerificationVerdict},
};
use excalibur_tools::{Access, LocalTools, ToolPolicy};
use excalibur_types::runtime::*;
use excalibur_vault::Redactor;
use serde_json::json;
use std::{path::PathBuf, sync::Arc};

pub struct GatewayVerifier {
    pub store: TaskStore,
    pub model: Arc<dyn ModelGateway>,
    pub workspace: PathBuf,
    pub sandbox: excalibur_sandbox::SandboxConfig,
    pub protected: Vec<PathBuf>,
    pub redactor: Redactor,
}
impl IndependentVerifier for GatewayVerifier {
    fn prepare<'a>(&'a self, root_run: &'a str) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async move {
            let journal = excalibur_journal::Journal::new(self.store.database().clone())
                .await
                .map_err(|e| e.to_string())?;
            let engine = excalibur_engine::Engine::new(
                journal.clone(),
                self.model.clone(),
                Arc::new(ReplayOnlyRuntime),
            );
            let parent = engine.load(root_run).await.map_err(|e| e.to_string())?;
            if !matches!(
                parent.status,
                excalibur_engine::Status::Running | excalibur_engine::Status::Verifying
            ) {
                return Err("verification parent is not active".into());
            }
            for (id, allocation) in parent
                .child_allocations
                .iter()
                .filter(|(id, a)| id.starts_with("verification:") && a.settled.is_none())
            {
                let child = engine
                    .load(&allocation.child_run)
                    .await
                    .map_err(|e| e.to_string())?;
                let owner = allocation.child_run.clone();
                let uncertain=self.store.database().read(excalibur_store::ReadLane::Interactive,move|c|Ok(c.query_row("SELECT EXISTS(SELECT 1 FROM task_check_attempts WHERE run_id=?1 AND state IN ('intent','expired') AND evidence_id IS NULL)",[owner],|r|r.get::<_,bool>(0))?)).await.map_err(|e|e.to_string())?;
                if uncertain {
                    return Err("interrupted independent check has uncertain effects; host reconciliation is required before another attempt".into());
                }
                if child.request.is_some() {
                    record_unknown_call(&journal, &allocation.child_run).await?;
                }
                journal
                    .set_state_if(
                        &allocation.child_run,
                        &[
                            "ready",
                            "running",
                            "verifying",
                            "paused",
                            "blocked",
                            "recovering",
                        ],
                        "cancelled",
                        Some("superseded interrupted verification attempt".into()),
                        "verifier:superseded",
                    )
                    .await
                    .map_err(|e| e.to_string())?;
                excalibur_engine::settle_child_budget(&journal, root_run, id)
                    .await
                    .map_err(|e| e.to_string())?;
            }
            Ok(())
        })
    }
    fn verify<'a>(
        &'a self,
        input: &'a VerificationInput,
    ) -> BoxFuture<'a, Result<VerificationVerdict, String>> {
        Box::pin(async move {
            let journal = excalibur_journal::Journal::new(self.store.database().clone())
                .await
                .map_err(|e| e.to_string())?;
            let mut sandbox = self.sandbox.clone();
            sandbox.write_roots.clear();
            sandbox.eval_network = false;
            sandbox.network = None;
            if sandbox.profile != excalibur_sandbox::SandboxProfile::Eval {
                sandbox.profile = excalibur_sandbox::SandboxProfile::ReadOnly;
            }
            let local = Arc::new(
                LocalTools::new(
                    self.store.database().clone(),
                    self.workspace.clone(),
                    ToolPolicy {
                        sandbox: Some(sandbox.clone()),
                        protected_paths: self.protected.clone(),
                        redactor: self.redactor.clone(),
                        read: Access::Allow,
                        write: Access::Deny,
                        exec: Access::Allow,
                        ..Default::default()
                    },
                )
                .await?,
            );
            let token = CapabilityToken {
                roots: vec![Root {
                    path: self.workspace.clone(),
                    write: false,
                }],
                network: Network::None,
                processes: true,
                sandbox_profile: Some(
                    if sandbox.profile == excalibur_sandbox::SandboxProfile::Eval {
                        "eval"
                    } else {
                        "read-only"
                    }
                    .into(),
                ),
                budget: input.budget.max_tool_calls.min(32),
                ..Default::default()
            };
            let security = Security::new(
                self.store.database().clone(),
                token.clone(),
                self.redactor.clone(),
            )
            .await
            .map_err(|e| e.to_string())?
            .with_noninteractive_policy()
            .with_verification_checks(
                input
                    .criteria
                    .iter()
                    .flat_map(|c| c.checks.iter())
                    .filter_map(|check| match &check.definition {
                        excalibur_tasks::CheckDefinition::Command { command, .. }
                        | excalibur_tasks::CheckDefinition::Test { command, .. }
                        | excalibur_tasks::CheckDefinition::Metric { command, .. } => {
                            Some(command.clone())
                        }
                        _ => None,
                    })
                    .collect(),
            )
            .map_err(|e| e.to_string())?;
            let tools: Arc<dyn ToolRuntime> =
                Arc::new(SecuredTools::new(local.clone(), security.clone()));
            let mut context = excalibur_context::ContextInput::new(
                "You are an independent verifier, with read-only task access and declared checks only.",
                input.literal_request.clone(),
                "UTC",
            );
            let skill = crate::skills::builtin_skills()
                .into_iter()
                .find(|skill| skill.name == "task-verification")
                .ok_or("built-in verification skill unavailable")?;
            context.layers[1] =
                json!({"role":"verifier","profile":"verifier@1","skill":skill}).to_string();
            context.tools = tools
                .definitions()
                .into_iter()
                .filter(|t| matches!(t.name.as_str(), "read_file" | "glob" | "grep"))
                .collect();
            let budget = excalibur_engine::Budget {
                max_steps: input.budget.max_steps.min(4),
                max_tokens: input
                    .budget
                    .input_tokens
                    .saturating_add(input.budget.output_tokens),
                max_cost_microusd: (input.budget.cost_microusd != u64::MAX)
                    .then_some(input.budget.cost_microusd),
                max_wall_seconds: input.budget.timeout_seconds,
                max_tool_calls: input.budget.max_tool_calls,
                tool_timeout_seconds: input.budget.timeout_seconds,
                max_processes: Some(1),
                max_terminal_sessions: None,
            };
            let engine =
                excalibur_engine::Engine::new(journal.clone(), self.model.clone(), tools.clone());
            let verifier_run = engine
                .create(
                    &format!("verification:{}:{}", input.job_id, input.round),
                    excalibur_engine::RunSpec {
                        context,
                        budget: budget.clone(),
                        criteria: vec![excalibur_engine::Criterion::FinalResponse { schema: None }],
                    },
                )
                .await
                .map_err(|e| e.to_string())?;
            let allocation_id = format!("verification:{}:{}", input.job_id, input.round);
            if let Err(error) = excalibur_engine::reserve_child_budget(
                &journal,
                &input.root_run_id,
                &allocation_id,
                &verifier_run,
                budget,
            )
            .await
            {
                journal
                    .set_state_if(
                        &verifier_run,
                        &["ready"],
                        "cancelled",
                        Some("verification allocation unavailable".into()),
                        "verifier:unallocated",
                    )
                    .await
                    .map_err(|e| e.to_string())?;
                local.terminals().shutdown().await;
                return Err(error.to_string());
            }
            let result=async {
                self.store.register_verifier_run(&input.task_id,&verifier_run,&input.root_run_id).await.map_err(|e|e.to_string())?;
                ensure_parent_active(&journal,&input.root_run_id).await?;
                if !journal.set_state_if(&verifier_run,&["ready"],"running",None,"verifier:started").await.map_err(|e|e.to_string())? {
                    return Err("verification admission was superseded by control".into());
                }
                security.register_run(&verifier_run,RunSecurity{parent:None,project:"verification".into(),workspace:self.workspace.clone(),token,autonomy:vec![Autonomy::Full],full_acknowledged:true,user_requested:true},excalibur_store::unix_seconds()).await.map_err(|e|e.to_string())?;
                tokio::select! {
                    result=tokio::time::timeout(std::time::Duration::from_secs(input.budget.timeout_seconds),Box::pin(self.verify_with_tools(input,&verifier_run,tools)))=>result.unwrap_or_else(|_|Err("independent verification exceeded its wall budget".into())),
                    result=wait_for_control(&journal,&input.root_run_id,&verifier_run)=>Err(result),
                }
            }.await;
            local.terminals().shutdown().await;
            if result.is_err() {
                // This verifier run is over; its evidence-less attempts would
                // otherwise block the gate even when a retry checks everything.
                self.store
                    .expire_abandoned_attempts(&verifier_run)
                    .await
                    .map_err(|e| e.to_string())?;
                record_unknown_call(&journal, &verifier_run).await?;
                let parent = journal
                    .state(&input.root_run_id)
                    .await
                    .map_err(|e| e.to_string())?
                    .0;
                let current = journal
                    .state(&verifier_run)
                    .await
                    .map_err(|e| e.to_string())?
                    .0;
                let target = match (parent.as_str(), current.as_str()) {
                    ("cancelled", _) => "cancelled",
                    ("paused" | "blocked", _) => "paused",
                    (_, "ready" | "created") => "cancelled",
                    _ => "failed",
                };
                if matches!(current.as_str(), "ready" | "running" | "verifying") {
                    let applied = journal
                        .set_state_if(
                            &verifier_run,
                            &[current.as_str()],
                            target,
                            Some(
                                "independent verification incomplete or controlled by parent"
                                    .into(),
                            ),
                            "verifier:stopped",
                        )
                        .await
                        .map_err(|e| e.to_string())?;
                    if !applied
                        && matches!(
                            journal
                                .state(&verifier_run)
                                .await
                                .map_err(|e| e.to_string())?
                                .0
                                .as_str(),
                            "ready" | "running" | "verifying"
                        )
                    {
                        return Err("verification stop could not be confirmed".into());
                    }
                }
            }
            let report = excalibur_engine::delivery_report_for(&journal, &verifier_run)
                .await
                .map_err(|e| e.to_string())?;
            journal
                .append(
                    &verifier_run,
                    "delivery_report",
                    &format!("verifier-report:{}", report.source_sequence),
                    self.redactor
                        .redact_json(&serde_json::to_value(&report).map_err(|e| e.to_string())?),
                )
                .await
                .map_err(|e| e.to_string())?;
            let state = journal
                .state(&verifier_run)
                .await
                .map_err(|e| e.to_string())?
                .0;
            if matches!(
                state.as_str(),
                "completed" | "completed_with_pending" | "failed" | "cancelled"
            ) {
                excalibur_engine::settle_child_budget(&journal, &input.root_run_id, &allocation_id)
                    .await
                    .map_err(|e| e.to_string())?;
            }
            result
        })
    }
}
impl GatewayVerifier {
    async fn verify_with_tools(
        &self,
        input: &VerificationInput,
        verifier_run: &str,
        tools: Arc<dyn ToolRuntime>,
    ) -> Result<VerificationVerdict, String> {
        let runner = CheckRunner::new(tools.clone(), self.workspace.clone())
            .with_tool_budget(input.budget.max_tool_calls)
            .with_exclusions(self.protected.clone());
        let journal = excalibur_journal::Journal::new(self.store.database().clone())
            .await
            .map_err(|e| e.to_string())?;
        let value = serde_json::to_value(input).map_err(|e| e.to_string())?;

        let mut own_evidence = Vec::new();
        for criterion in value["criteria"].as_array().into_iter().flatten() {
            for check in criterion["checks"].as_array().into_iter().flatten() {
                let id = check
                    .as_str()
                    .or_else(|| check["id"].as_str())
                    .ok_or("invalid verifier check")?;
                ensure_active(&journal, &input.root_run_id, verifier_run).await?;
                if runner.tool_calls() >= input.budget.max_tool_calls {
                    return Err("independent check tool budget exhausted".into());
                }
                let before_calls = runner.tool_calls();
                let result = runner
                    .run_keyed(
                        &self.store,
                        &input.task_id,
                        id,
                        verifier_run,
                        &format!("verification-check:{id}"),
                    )
                    .await;
                journal.append(verifier_run,"verification_usage",&format!("verification:{verifier_run}:check:{id}"),json!({"usage":Usage::default(),"model_calls":0,"tool_calls":runner.tool_calls().saturating_sub(before_calls),"step_count":0,"verifier_run_id":verifier_run})).await.map_err(|e|e.to_string())?;
                own_evidence.push(result.map_err(|e| e.to_string())?);
            }
        }

        let payload = tools.sanitize_durable(&json!({"request":value,"independent_evidence":own_evidence,"verifier_run_id":verifier_run}))?;
        let mut messages = vec![
            Message::text(
                "system",
                "You are the independent task verifier. Compare every part of the literal request with requirements and deliverables. Never use executor reasoning. Existing command/test/metric evidence cannot be replaced by judgment. Reproducible checks were rerun under your read-only capability; use their independent evidence IDs. You may read files, glob or grep; no writes, arbitrary commands or external tools. For verification_scope beginning stage:, assess only the supplied stage criteria; other task requirements may remain unfinished. For task scope, contrast the complete literal request. Return only a JSON VerificationVerdict: criteria[{criterion_id,passed,explanation,evidence_ids}], uncovered_requests[string], request_covered:boolean. Inspection judgments must be labeled. Missing or failed proof means passed=false.",
            ),
            Message::text("user", payload.to_string()),
        ];
        let skill = crate::skills::builtin_skills()
            .into_iter()
            .find(|skill| skill.name == "task-verification")
            .ok_or("built-in verification skill unavailable")?;
        messages[0]
            .content
            .push_str("\nFrozen role verifier; profile verifier@1; skill task-verification:\n");
        messages[0].content.push_str(&skill.instructions);
        let started = std::time::Instant::now();
        let mut usage = Usage::default();
        let mut tool_calls = runner.tool_calls();
        for step in 0..input.budget.max_steps.min(4) {
            if usage.input_tokens >= input.budget.input_tokens
                || usage.output_tokens >= input.budget.output_tokens
                || usage.cost_microusd >= input.budget.cost_microusd
                || started.elapsed().as_secs() >= input.budget.timeout_seconds
            {
                return Err("independent verification budget exhausted".into());
            }
            ensure_active(&journal, &input.root_run_id, verifier_run).await?;
            let request = ModelRequest {
                messages: messages.clone(),
                tools: tools
                    .definitions()
                    .into_iter()
                    .filter(|t| matches!(t.name.as_str(), "read_file" | "glob" | "grep"))
                    .collect(),
                tools_enabled: step < 3 && tool_calls < input.budget.max_tool_calls,
                reasoning_effort: None,
                output_schema: None,
                cache: None,
                deferred_tools: Vec::new(),
            };
            preflight(self.model.as_ref(), &request, &usage, &input.budget)?;
            ensure_active(&journal, &input.root_run_id, verifier_run).await?;
            journal
                .append(
                    verifier_run,
                    "verifier_model_request",
                    &format!("verifier-request:{step}"),
                    tools.sanitize_durable(
                        &serde_json::to_value(&request).map_err(|e| e.to_string())?,
                    )?,
                )
                .await
                .map_err(|e| e.to_string())?;
            let remaining = input
                .budget
                .timeout_seconds
                .saturating_sub(started.elapsed().as_secs());
            let observed = match tokio::time::timeout(
                std::time::Duration::from_secs(remaining),
                self.model.complete_observed(request.clone()),
            )
            .await
            {
                Err(_) => return Err("independent verifier timed out".into()),
                Ok(Err(failure)) => {
                    let receipt =
                        failure.metrics.usage_known || failure.metrics.cost_microusd.is_some();
                    let no_dispatch =
                        failure.metrics.provider_calls_known && failure.metrics.provider_calls == 0;
                    if receipt || no_dispatch {
                        let charged = Usage {
                            input_tokens: failure.metrics.usage.input_tokens,
                            output_tokens: failure.metrics.usage.output_tokens,
                            cost_microusd: failure.metrics.cost_microusd.unwrap_or(0),
                            estimated: !no_dispatch && !failure.metrics.usage_known,
                        };
                        journal.append(verifier_run,"verification_usage",&format!("verification:{verifier_run}:usage:{step}"),json!({"usage":charged,"model_calls":1,"tool_calls":0,"receipt_confirmed":true,"verifier_run_id":verifier_run,"metrics":failure.metrics})).await.map_err(|e|e.to_string())?;
                    }
                    let class = failure.error.classification();
                    journal.append(verifier_run,"verifier_model_failure",&format!("verifier-failure:{step}"),json!({"metrics":failure.metrics,"failure_class":class.class,"http_status":class.http_status,"receipt_confirmed":receipt,"dispatch_absent":no_dispatch,"error":"independent model request failed"})).await.map_err(|e|e.to_string())?;
                    return Err(format!(
                        "independent verifier model failed: {:?}",
                        class.class
                    ));
                }
                Ok(Ok(observed)) => observed,
            };
            let response = observed.response;
            usage.input_tokens = usage
                .input_tokens
                .saturating_add(response.usage.input_tokens);
            usage.output_tokens = usage
                .output_tokens
                .saturating_add(response.usage.output_tokens);
            usage.cost_microusd = usage
                .cost_microusd
                .saturating_add(response.usage.cost_microusd);
            journal.append(verifier_run,"verification_usage",&format!("verification:{verifier_run}:usage:{step}"),json!({"usage":response.usage,"model_calls":1,"tool_calls":0,"receipt_confirmed":true,"verifier_run_id":verifier_run,"metrics":observed.metrics})).await.map_err(|e|e.to_string())?;
            validate_response(&response, &request, tools.as_ref())?;
            if usage.input_tokens > input.budget.input_tokens
                || usage.output_tokens > input.budget.output_tokens
                || usage.cost_microusd > input.budget.cost_microusd
            {
                return Err("independent verifier receipt exceeds its allocation".into());
            }
            let response_value = tools
                .sanitize_durable(&serde_json::to_value(&response).map_err(|e| e.to_string())?)?;
            let response: ModelResponse = serde_json::from_value(response_value.clone())
                .map_err(|_| "independent verifier response sanitization failed".to_string())?;
            let response_event = journal
                .append(
                    verifier_run,
                    "verifier_model_response",
                    &format!("verifier-response:{step}"),
                    response_value,
                )
                .await
                .map_err(|e| e.to_string())?;
            if response.message.tool_calls.is_empty() {
                let mut value: serde_json::Value = serde_json::from_str(&response.message.content)
                    .map_err(|_| "independent verifier returned an invalid verdict".to_string())?;
                if !value.is_object() {
                    return Err("independent verdict must be an object".into());
                }
                value["verifier_run_id"] = json!(verifier_run);
                let mut verdict: VerificationVerdict = serde_json::from_value(value)
                    .map_err(|_| "independent verifier returned an invalid verdict".to_string())?;
                // This is the verifier's actual recorded judgment. The task gate
                // accepts it only for the explicitly tagged response inspection;
                // executable claims still require the verifier's own check IDs.
                for criterion in verdict.criteria.iter_mut().filter(|c| c.passed) {
                    criterion
                        .evidence_ids
                        .push(format!("journal:{verifier_run}:{response_event}"));
                }
                ensure_active(&journal, &input.root_run_id, verifier_run).await?;
                if !journal
                    .set_state_if(
                        verifier_run,
                        &["running"],
                        "verifying",
                        None,
                        "verifier:verifying",
                    )
                    .await
                    .map_err(|e| e.to_string())?
                {
                    return Err("verification finalization was superseded by control".into());
                }
                ensure_active(&journal, &input.root_run_id, verifier_run).await?;
                if !journal
                    .set_state_if(
                        verifier_run,
                        &["verifying"],
                        "completed",
                        None,
                        "verifier:completed",
                    )
                    .await
                    .map_err(|e| e.to_string())?
                {
                    return Err("verification completion was superseded by control".into());
                }
                return Ok(verdict);
            }
            messages.push(response.message.clone());
            for call in response.message.tool_calls {
                if tool_calls >= input.budget.max_tool_calls {
                    return Err("independent verification tool budget exhausted".into());
                }
                ensure_active(&journal, &input.root_run_id, verifier_run).await?;
                tool_calls += 1;
                let result = if matches!(call.name.as_str(), "read_file" | "glob" | "grep") {
                    match tools.prepare(verifier_run, call.clone()).await {
                        Ok(prepared) => {
                            tools
                                .execute(
                                    verifier_run,
                                    &format!("verify:{step}:{}", call.id),
                                    &prepared,
                                )
                                .await
                        }
                        Err(error) => ToolResult::error(error),
                    }
                } else {
                    ToolResult::error("independent verifier has no authority for this tool")
                };
                let event = journal
                    .append(
                        verifier_run,
                        "verifier_tool_result",
                        &format!("verifier-tool:{step}:{}", call.id),
                        tools.sanitize_durable(&json!({"call":call,"result":result}))?,
                    )
                    .await
                    .map_err(|e| e.to_string())?;
                journal.append(verifier_run,"verification_usage",&format!("verification:{verifier_run}:tool:{step}:{}",call.id),json!({"usage":Usage::default(),"model_calls":0,"tool_calls":1,"step_count":0,"verifier_run_id":verifier_run})).await.map_err(|e|e.to_string())?;
                messages.push(Message {role:"tool".into(), content:tools.sanitize_durable(&json!({"result":result,"independent_evidence_id":format!("journal:{verifier_run}:{}",event)}))?.to_string(), tool_calls:vec![], tool_call_id:Some(call.id),provider_state:None});
            }
        }
        Err("independent verification reached its bounded step limit".into())
    }
}

async fn wait_for_control(
    journal: &excalibur_journal::Journal,
    parent: &str,
    child: &str,
) -> String {
    loop {
        for run in [parent, child] {
            match journal.state(run).await {
                Ok((state, _)) if matches!(state.as_str(), "ready" | "running" | "verifying") => {}
                Ok((state, _)) => {
                    return format!("independent verification interrupted by {state}");
                }
                Err(_) => return "independent verification control unavailable".into(),
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
}

struct ReplayOnlyRuntime;
impl ToolRuntime for ReplayOnlyRuntime {
    fn definitions(&self) -> Vec<ToolDefinition> {
        vec![]
    }
    fn prepare<'a>(
        &'a self,
        _: &'a str,
        _: ToolCall,
    ) -> BoxFuture<'a, Result<PreparedTool, String>> {
        Box::pin(async { Err("replay-only runtime".into()) })
    }
    fn execute<'a>(
        &'a self,
        _: &'a str,
        _: &'a str,
        _: &'a PreparedTool,
    ) -> BoxFuture<'a, ToolResult> {
        Box::pin(async { ToolResult::error("replay-only runtime") })
    }
    fn reconcile<'a>(
        &'a self,
        _: &'a str,
        _: &'a str,
        _: &'a PreparedTool,
    ) -> BoxFuture<'a, Reconcile> {
        Box::pin(async { Reconcile::Uncertain("replay-only runtime".into()) })
    }
}

async fn ensure_active(
    journal: &excalibur_journal::Journal,
    parent: &str,
    child: &str,
) -> Result<(), String> {
    for run in [parent, child] {
        let (state, _) = journal
            .state(run)
            .await
            .map_err(|_| "independent verification control unavailable".to_string())?;
        if !matches!(state.as_str(), "running" | "verifying") {
            return Err(format!("independent verification stopped by {state}"));
        }
    }
    Ok(())
}
async fn ensure_parent_active(
    journal: &excalibur_journal::Journal,
    parent: &str,
) -> Result<(), String> {
    if !matches!(
        journal
            .state(parent)
            .await
            .map_err(|e| e.to_string())?
            .0
            .as_str(),
        "running" | "verifying"
    ) {
        return Err("independent verification parent is not active".into());
    }
    Ok(())
}
fn preflight(
    model: &dyn ModelGateway,
    request: &ModelRequest,
    usage: &Usage,
    budget: &excalibur_tasks::verification::VerificationBudget,
) -> Result<(), String> {
    if let Some(bound) = model.call_bound(request)
        && (bound.max_tokens
            > budget
                .input_tokens
                .saturating_sub(usage.input_tokens)
                .saturating_add(budget.output_tokens.saturating_sub(usage.output_tokens))
            || bound.max_cost_microusd > budget.cost_microusd.saturating_sub(usage.cost_microusd))
    {
        return Err("independent verification request exceeds remaining budget".into());
    }
    Ok(())
}
fn validate_response(
    response: &ModelResponse,
    request: &ModelRequest,
    tools: &dyn ToolRuntime,
) -> Result<(), String> {
    let calls = &response.message.tool_calls;
    if response.message.role != "assistant"
        || calls.len() > 8
        || calls.iter().any(|call| call.id.is_empty())
        || calls
            .iter()
            .map(|call| &call.id)
            .collect::<std::collections::BTreeSet<_>>()
            .len()
            != calls.len()
    {
        return Err("independent verifier returned an invalid tool batch".into());
    }
    if !request.tools_enabled && !calls.is_empty() {
        return Err("independent verifier requested disabled tools".into());
    }
    for call in calls {
        let original = serde_json::to_value(call).map_err(|_| "invalid verifier tool call")?;
        let parsed: serde_json::Value = serde_json::from_str(&call.arguments)
            .map_err(|_| "invalid verifier tool argument JSON")?;
        if tools.sanitize_durable(&original)? != original
            || tools.sanitize_durable(&parsed)? != parsed
        {
            return Err("independent verifier tool arguments contain a secret literal".into());
        }
    }
    Ok(())
}
async fn record_unknown_call(
    journal: &excalibur_journal::Journal,
    run: &str,
) -> Result<(), String> {
    let events = journal.replay(run, 0).await.map_err(|e| e.to_string())?;
    let request = events
        .iter()
        .rev()
        .find(|event| event.kind == "verifier_model_request");
    let Some(request) = request else {
        return Ok(());
    };
    let confirmed = events
        .iter()
        .filter(|event| event.sequence > request.sequence)
        .any(|event| {
            event.kind == "verifier_model_response"
                || (event.kind == "verification_usage"
                    && event.payload["receipt_confirmed"] == true)
                || (event.kind == "verifier_model_failure"
                    && (event.payload["receipt_confirmed"] == true
                        || event.payload["dispatch_absent"] == true))
        });
    if confirmed {
        return Ok(());
    }
    journal.append(run,"verification_usage",&format!("verification:unknown-call:{}",request.sequence),json!({"usage":Usage{estimated:true,..Default::default()},"model_calls":1,"tool_calls":0,"step_count":1,"receipt_confirmed":false,"verifier_run_id":run})).await.map_err(|e|e.to_string())?;
    Ok(())
}

#[cfg(test)]
mod accounting_tests {
    use super::*;
    use excalibur_models::{
        FailureClass, ModelCallBound, ModelCallError, ModelCallMetrics, ModelError, ModelFailure,
        ObservedResponse, UsageBreakdown,
    };
    use std::sync::{
        Mutex,
        atomic::{AtomicUsize, Ordering},
    };
    enum Mode {
        Preflight,
        Overflow,
        Failure,
        TooMany,
        Duplicate,
        Final,
    }
    struct Model {
        mode: Mode,
        calls: AtomicUsize,
        requests: Mutex<Vec<ModelRequest>>,
    }
    impl ModelGateway for Model {
        fn call_bound(&self, _: &ModelRequest) -> Option<ModelCallBound> {
            Some(ModelCallBound {
                max_tokens: if matches!(self.mode, Mode::Preflight) {
                    200
                } else {
                    20
                },
                max_cost_microusd: 0,
            })
        }
        fn complete<'a>(
            &'a self,
            _: ModelRequest,
        ) -> BoxFuture<'a, Result<ModelResponse, ModelError>> {
            Box::pin(async { unreachable!("observed fixture only") })
        }
        fn complete_observed<'a>(
            &'a self,
            request: ModelRequest,
        ) -> BoxFuture<'a, Result<ObservedResponse, ModelCallError>> {
            Box::pin(async move {
                self.calls.fetch_add(1, Ordering::SeqCst);
                self.requests.lock().unwrap().push(request.clone());
                let mut metrics = ModelCallMetrics::request(&request, self.metadata());
                metrics.provider_calls = 1;
                metrics.provider_calls_known = true;
                metrics.usage_known = true;
                if matches!(self.mode, Mode::Failure) {
                    metrics.usage = UsageBreakdown {
                        input_tokens: 5,
                        ..Default::default()
                    };
                    metrics.cost_microusd = Some(7);
                    return Err(ModelCallError::new(
                        ModelError::ProviderFailure(ModelFailure {
                            class: FailureClass::Quota,
                            http_status: Some(429),
                        }),
                        metrics,
                    ));
                }
                let mut message = Message::text(
                    "assistant",
                    json!({"criteria":[],"uncovered_requests":[],"request_covered":true})
                        .to_string(),
                );
                if matches!(self.mode, Mode::TooMany | Mode::Duplicate) {
                    message.tool_calls = (0..if matches!(self.mode, Mode::TooMany) {
                        9
                    } else {
                        2
                    })
                        .map(|n| ToolCall {
                            id: if matches!(self.mode, Mode::Duplicate) {
                                "same".into()
                            } else {
                                format!("call-{n}")
                            },
                            name: "read_file".into(),
                            arguments: "{}".into(),
                        })
                        .collect();
                }
                let usage = Usage {
                    input_tokens: if matches!(self.mode, Mode::Overflow) {
                        30
                    } else {
                        2
                    },
                    output_tokens: 1,
                    ..Default::default()
                };
                metrics.usage = UsageBreakdown::from_usage(&usage);
                metrics.cost_microusd = Some(0);
                Ok(ObservedResponse {
                    response: ModelResponse { message, usage },
                    metrics,
                })
            })
        }
    }
    struct Tools(AtomicUsize);
    impl ToolRuntime for Tools {
        fn definitions(&self) -> Vec<ToolDefinition> {
            vec![ToolDefinition {
                name: "read_file".into(),
                description: "Synthetic read".into(),
                input_schema: json!({"type":"object"}),
            }]
        }
        fn prepare<'a>(
            &'a self,
            _: &'a str,
            call: ToolCall,
        ) -> BoxFuture<'a, Result<PreparedTool, String>> {
            Box::pin(async move {
                Ok(PreparedTool {
                    call,
                    effect: Effect::Pure,
                    resources: vec![],
                    exclusive: false,
                    data: serde_json::Value::Null,
                })
            })
        }
        fn execute<'a>(
            &'a self,
            _: &'a str,
            _: &'a str,
            _: &'a PreparedTool,
        ) -> BoxFuture<'a, ToolResult> {
            Box::pin(async {
                self.0.fetch_add(1, Ordering::SeqCst);
                ToolResult::ok(json!({"synthetic":true}))
            })
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
    async fn setup(
        mode: Mode,
    ) -> (
        tempfile::TempDir,
        excalibur_store::Database,
        GatewayVerifier,
        VerificationInput,
        String,
        Arc<Model>,
        Arc<Tools>,
    ) {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("workspace");
        std::fs::create_dir(&root).unwrap();
        let db = excalibur_store::Database::open(
            temp.path().join("main.db"),
            excalibur_store::DatabaseKind::Main,
            excalibur_store::StorageConfig::default(),
        )
        .unwrap();
        let store = TaskStore::open(db.clone()).await.unwrap();
        let journal = excalibur_journal::Journal::new(db.clone()).await.unwrap();
        let model = Arc::new(Model {
            mode,
            calls: AtomicUsize::new(0),
            requests: Mutex::new(vec![]),
        });
        let tools = Arc::new(Tools(AtomicUsize::new(0)));
        let engine = excalibur_engine::Engine::new(journal.clone(), model.clone(), tools.clone());
        let spec = excalibur_engine::RunSpec {
            context: excalibur_context::ContextInput::new(
                "Synthetic rules",
                "Synthetic task",
                "fixed",
            ),
            budget: excalibur_engine::Budget::default(),
            criteria: vec![excalibur_engine::Criterion::FinalResponse { schema: None }],
        };
        let parent = engine.create("parent", spec.clone()).await.unwrap();
        let child = engine.create("child", spec).await.unwrap();
        for run in [&parent, &child] {
            journal
                .set_state_if(run, &["ready"], "running", None, "fixture:admit")
                .await
                .unwrap();
        }
        let input = VerificationInput {
            task_version: 0,
            applicable_instructions: vec![],
            waived_criteria: Default::default(),
            verification_scope: "task".into(),
            workspace_snapshots: Default::default(),
            job_id: "synthetic-job".into(),
            task_id: "missing-synthetic-task".into(),
            root_run_id: parent.clone(),
            parent_run_id: parent,
            round: 1,
            instruction_revision: 0,
            literal_request: "Synthetic request".into(),
            requirements: vec![],
            criteria: vec![],
            evidence: vec![],
            diff: String::new(),
            delivery_summary: "Synthetic delivery".into(),
            budget: excalibur_tasks::verification::VerificationBudget {
                max_steps: 2,
                max_tool_calls: 5,
                input_tokens: 10,
                output_tokens: 10,
                cost_microusd: u64::MAX,
                timeout_seconds: 10,
            },
            budget_remaining: excalibur_engine::Budget::default(),
        };
        let driver = GatewayVerifier {
            store,
            model: model.clone(),
            workspace: root.clone(),
            sandbox: excalibur_sandbox::SandboxConfig::project(root),
            protected: vec![],
            redactor: Redactor::default(),
        };
        (temp, db, driver, input, child, model, tools)
    }
    #[tokio::test]
    async fn preflight_rejection_has_no_dispatched_request_or_fabricated_unknown_usage() {
        let (_temp, db, driver, input, child, model, tools) = setup(Mode::Preflight).await;
        assert!(
            driver
                .verify_with_tools(&input, &child, tools)
                .await
                .is_err()
        );
        assert_eq!(model.calls.load(Ordering::SeqCst), 0);
        let journal = excalibur_journal::Journal::new(db.clone()).await.unwrap();
        record_unknown_call(&journal, &child).await.unwrap();
        let events = journal.replay(&child, 0).await.unwrap();
        assert!(!events.iter().any(|event| matches!(
            event.kind.as_str(),
            "verifier_model_request" | "verification_usage"
        )));
        db.shutdown().await.unwrap();
    }
    #[tokio::test]
    async fn actual_overrun_is_recorded_before_stopping_and_invalid_batches_do_not_execute() {
        for mode in [Mode::Overflow, Mode::TooMany, Mode::Duplicate] {
            let (_temp, db, driver, input, child, model, tools) = setup(mode).await;
            assert!(
                driver
                    .verify_with_tools(&input, &child, tools.clone())
                    .await
                    .is_err()
            );
            assert_eq!(model.calls.load(Ordering::SeqCst), 1);
            assert_eq!(tools.0.load(Ordering::SeqCst), 0);
            let journal = excalibur_journal::Journal::new(db.clone()).await.unwrap();
            record_unknown_call(&journal, &child).await.unwrap();
            let state = excalibur_engine::Engine::new(journal.clone(), model, tools)
                .load(&child)
                .await
                .unwrap();
            assert!(state.request.is_none());
            assert_eq!(state.model_calls, 1);
            assert!(state.usage.input_tokens >= 2);
            assert!(
                !journal
                    .replay(&child, 0)
                    .await
                    .unwrap()
                    .iter()
                    .any(|event| event.key.starts_with("verification:unknown-call"))
            );
            db.shutdown().await.unwrap();
        }
    }
    #[tokio::test]
    async fn failure_receipt_keeps_classification_and_replaying_cleanup_never_double_charges() {
        let (_temp, db, driver, input, child, model, tools) = setup(Mode::Failure).await;
        assert!(
            driver
                .verify_with_tools(&input, &child, tools.clone())
                .await
                .unwrap_err()
                .contains("Quota")
        );
        let journal = excalibur_journal::Journal::new(db.clone()).await.unwrap();
        record_unknown_call(&journal, &child).await.unwrap();
        record_unknown_call(&journal, &child).await.unwrap();
        let state = excalibur_engine::Engine::new(journal.clone(), model, tools)
            .load(&child)
            .await
            .unwrap();
        assert!(state.request.is_none());
        assert_eq!(state.usage.input_tokens, 5);
        assert_eq!(state.usage.cost_microusd, 7);
        assert_eq!(state.model_calls, 1);
        assert!(
            journal
                .replay(&child, 0)
                .await
                .unwrap()
                .iter()
                .any(|event| event.kind == "verifier_model_failure"
                    && event.payload["failure_class"] == "quota")
        );
        db.shutdown().await.unwrap();
    }
    #[tokio::test]
    async fn post_reservation_setup_failure_cancels_and_settles_the_unstarted_child() {
        let (_temp, db, driver, input, _, model, _) = setup(Mode::Final).await;
        assert!(driver.verify(&input).await.is_err());
        assert_eq!(model.calls.load(Ordering::SeqCst), 0);
        let journal = excalibur_journal::Journal::new(db.clone()).await.unwrap();
        let engine =
            excalibur_engine::Engine::new(journal.clone(), model, Arc::new(ReplayOnlyRuntime));
        let parent = engine.load(&input.root_run_id).await.unwrap();
        assert_eq!(parent.child_allocations.len(), 1);
        let allocation = parent.child_allocations.values().next().unwrap();
        assert_eq!(allocation.settled.as_ref().unwrap().tokens, 0);
        assert_eq!(
            journal.state(&allocation.child_run).await.unwrap().0,
            "cancelled"
        );
        assert!(
            engine
                .load(&allocation.child_run)
                .await
                .unwrap()
                .delivery_report
                .is_some()
        );
        db.shutdown().await.unwrap();
    }
    #[tokio::test]
    async fn direct_verifier_request_uses_the_actual_frozen_verification_skill() {
        let (_temp, db, driver, input, child, model, tools) = setup(Mode::Final).await;
        assert!(
            driver
                .verify_with_tools(&input, &child, tools)
                .await
                .is_ok()
        );
        let skill = crate::skills::builtin_skills()
            .into_iter()
            .find(|skill| skill.name == "task-verification")
            .unwrap();
        assert!(
            model.requests.lock().unwrap()[0].messages[0]
                .content
                .contains(&skill.instructions)
        );
        db.shutdown().await.unwrap();
    }
}
