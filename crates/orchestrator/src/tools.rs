//! Shared model-callable coordination adapter; no protocol/CLI implementation.
use crate::*;
use excalibur_types::runtime::*;
use std::sync::Weak;

pub struct OrchestratorTools {
    delegate: Arc<dyn ToolRuntime>,
    orchestrator: Mutex<Weak<Inner>>,
}
impl OrchestratorTools {
    pub fn new(delegate: Arc<dyn ToolRuntime>) -> Self {
        Self {
            delegate,
            orchestrator: Mutex::new(Weak::new()),
        }
    }
    pub fn attach(&self, orchestrator: &Orchestrator) -> Result<()> {
        let mut current = self.orchestrator.lock().unwrap();
        if let Some(old) = current.upgrade()
            && !Arc::ptr_eq(&old, &orchestrator.inner)
        {
            return Err(invalid("coordination runtime already bound"));
        }
        *current = Arc::downgrade(&orchestrator.inner);
        Ok(())
    }
    fn host(&self) -> std::result::Result<Orchestrator, String> {
        self.orchestrator
            .lock()
            .unwrap()
            .upgrade()
            .map(|inner| Orchestrator { inner })
            .ok_or_else(|| "orchestrator is not attached".into())
    }
    pub async fn definitions_for_run(
        &self,
        run: &str,
    ) -> std::result::Result<Vec<ToolDefinition>, String> {
        let host = self.host()?;
        let agent = host.agent_for_run(run).await.map_err(|e| e.to_string())?;
        let mut defs = self.delegate.definitions();
        defs.retain(|d| profile_allows(&agent.profile, &d.name));
        defs.extend(coordination_definitions(agent.depth < 2));
        Ok(defs)
    }
    async fn execute_coordination(
        &self,
        run: &str,
        key: &str,
        tool: &PreparedTool,
    ) -> Result<Value> {
        let host = self.host().map_err(Error::Authority)?;
        let agent = host.agent_for_run(run).await?;
        let args: Value = serde_json::from_str(&tool.call.arguments)?;
        match tool.call.name.as_str() {
            "status" => {
                let target = args["agent"].as_str().unwrap_or(&agent.id);
                let other = host.agent(target).await?;
                if other.id != agent.id && other.parent_id.as_deref() != Some(&agent.id) {
                    return Err(invalid("status target is outside agent branch"));
                }
                Ok(serde_json::to_value(host.status(target).await?)?)
            }
            "subagent" => {
                if agent.depth >= 2 {
                    return Err(invalid("workers cannot create or control subagents"));
                }
                match args["action"].as_str().unwrap_or("") {
                    "wait" => {
                        host.request_wait_for_children(&agent.id).await?;
                        Ok(json!({"waiting_for_children":true}))
                    }
                    "spawn" => {
                        let operation = format!("{}:{}", run, digest(key.as_bytes()));
                        let d = delegation_from_call(&host, &agent, &args, operation).await?;
                        Ok(serde_json::to_value(host.spawn(&agent.id, d).await?)?)
                    }
                    "pause" | "resume" | "interrupt" | "cancel" | "redirect" => {
                        let target = args["agent"]
                            .as_str()
                            .ok_or_else(|| invalid("target agent required"))?;
                        let a = host.agent(target).await?;
                        if a.parent_id.as_deref() != Some(&agent.id) {
                            return Err(invalid("parent controls only its own children"));
                        }
                        let action = match args["action"].as_str().unwrap_or("") {
                            "pause" => Control::Pause,
                            "resume" => Control::Resume,
                            "interrupt" => Control::Interrupt,
                            "cancel" => Control::Cancel {
                                confirmed: args["confirmed"].as_bool().unwrap_or(false),
                            },
                            _ => Control::Redirect {
                                text: args["text"].as_str().unwrap_or("").into(),
                            },
                        };
                        Ok(serde_json::to_value(
                            host.control_keyed(&format!("{run}:{key}"), target, action)
                                .await?,
                        )?)
                    }
                    _ => Err(invalid("unknown subagent action")),
                }
            }
            "agent_message" => {
                let action = args["action"].as_str().unwrap_or("");
                match action {
                    "mailbox" => Ok(serde_json::to_value(host.mailbox(&agent.id).await?)?),
                    "ack" => {
                        host.ack_message(
                            &agent.id,
                            args["id"]
                                .as_str()
                                .ok_or_else(|| invalid("message id required"))?,
                        )
                        .await?;
                        Ok(json!({"acknowledged":true}))
                    }
                    "report" | "question" | "blocked" => {
                        let parent = agent
                            .parent_id
                            .as_deref()
                            .ok_or_else(|| invalid("master has no parent"))?;
                        Ok(serde_json::to_value(
                            host.send_message(
                                &format!("{run}:{key}"),
                                &agent.id,
                                parent,
                                action,
                                {
                                    let mut payload = args["payload"].clone();
                                    if action == "question"
                                        && (agent.token.processes
                                            || !agent.token.external_actions.is_empty()
                                            || agent.token.roots.iter().any(|root| root.write))
                                    {
                                        payload["host_requires_blocking"] = json!(true);
                                    }
                                    payload
                                },
                                "normal",
                            )
                            .await?,
                        )?)
                    }
                    "answer" | "directive" => {
                        let target = args["agent"]
                            .as_str()
                            .ok_or_else(|| invalid("target required"))?;
                        Ok(serde_json::to_value(
                            host.send_message(
                                &format!("{run}:{key}"),
                                &agent.id,
                                target,
                                action,
                                args["payload"].clone(),
                                args["priority"].as_str().unwrap_or("normal"),
                            )
                            .await?,
                        )?)
                    }
                    _ => Err(invalid("unknown message action")),
                }
            }
            _ => Err(invalid("unknown coordination tool")),
        }
    }
}
async fn delegation_from_call(
    host: &Orchestrator,
    parent: &Agent,
    args: &Value,
    operation: String,
) -> Result<Delegation> {
    let input = &args["delegation"];
    if input.get("spec").is_some() {
        let mut d: Delegation = serde_json::from_value(input.clone())?;
        d.operation = operation;
        return Ok(d);
    }
    let required = |key: &str| {
        input[key]
            .as_str()
            .filter(|s| !s.trim().is_empty())
            .map(str::to_owned)
            .ok_or_else(|| invalid(&format!("delegation.{key} required")))
    };
    let op = operation.clone();
    let known = host
        .inner
        .db
        .read(ReadLane::Interactive, move |c| {
            Ok(c.query_row(
                "SELECT body FROM orchestration_spawn WHERE operation=?1",
                [op],
                |r| r.get::<_, String>(0),
            )
            .optional()?)
        })
        .await?;
    if let Some(known) = known {
        let d: Delegation = serde_json::from_str(&known)?;
        let list = |key: &str| -> Result<Vec<String>> {
            input
                .get(key)
                .map(|v| serde_json::from_value(v.clone()).map_err(Error::from))
                .transpose()
                .map(Option::unwrap_or_default)
        };
        if d.name != required("name")?
            || d.profile != required("profile")?
            || d.goal != required("goal")?
            || d.deliverable != required("deliverable")?
            || d.scope != input["scope"].as_str().unwrap_or(&parent.scope)
            || d.inputs != list("inputs")?
            || d.pinned_instructions != list("pinned_instructions")?
            || d.suggested_skills != list("suggested_skills")?
            || d.fork_context != input["fork_context"].as_bool().unwrap_or(false)
        {
            return Err(invalid("delegation key reused with different tool input"));
        }
        return Ok(d);
    }
    let run = parent
        .run_id
        .as_ref()
        .ok_or_else(|| invalid("master uses its intake delegation adapter"))?;
    let state = host.inner.engine.load(run).await?;
    let remaining =
        excalibur_engine::remaining_budget_for(host.inner.engine.journal(), run).await?;
    let mut budget = remaining.clone();
    budget.max_steps = 60.min(remaining.max_steps / 2);
    budget.max_tokens = 30_000.min(remaining.max_tokens / 2);
    budget.max_wall_seconds = 600.min(remaining.max_wall_seconds / 2);
    budget.max_tool_calls = 100.min(remaining.max_tool_calls / 2);
    budget.max_cost_microusd = remaining.max_cost_microusd.map(|v| v / 2);
    budget.max_processes = remaining.max_processes.map(|v| v.min(1));
    budget.max_terminal_sessions = remaining.max_terminal_sessions.map(|v| v.min(2));
    if budget.max_steps == 0 || budget.max_tokens == 0 || budget.max_wall_seconds == 0 {
        return Err(invalid("parent has no unallocated child budget"));
    }
    let profile = required("profile")?;
    let effective_profile = if crate::profiles::builtin_profile(&profile) {
        profile.clone()
    } else {
        host.custom_profile(&profile).await?.base_profile
    };
    let scope = input["scope"].as_str().unwrap_or(&parent.scope).to_owned();
    let mut token = parent.token.clone();
    token.data_scopes.retain(|s| s == "general" || s == &scope);
    token.budget = token.budget.min(budget.max_tool_calls);
    if matches!(
        effective_profile.as_str(),
        "planner" | "explorer" | "verifier" | "reviewer" | "researcher" | "security-auditor"
    ) {
        for root in &mut token.roots {
            root.write = false;
        }
        if effective_profile != "verifier" {
            token.processes = false;
        }
    }
    let goal = required("goal")?;
    let priority = parent.delegation.as_ref().map(|d| d.priority).unwrap_or(1);
    // The child's L0 core is sized by the same native path as its parent, but
    // against the child's own run budget and the host/model input limits. The
    // parent's lane marker is removed here; spawn appends the child's own,
    // which is exactly the suffix counted below.
    let (mut context, _) = host.inner.engine.native_context(
        &crate::without_lane_markers(&state.spec.context.layers[0]),
        &goal,
        &format!("UTC Unix seconds {}", now()),
        &budget,
        &crate::lane_marker(priority.max(1)),
    )?;
    // Parent profile identity is removed by spawn, which owns that block.
    context.layers[1] = state.spec.context.layers[1].clone();
    context.layers[2] = state.spec.context.layers[2].clone();
    let bytes = Sha256::digest(operation.as_bytes());
    let mut idbytes = [0u8; 16];
    idbytes.copy_from_slice(&bytes[..16]);
    let task = excalibur_types::TaskId::from_ulid(u128::from_be_bytes(idbytes).into()).to_string();
    let list = |key: &str| -> Result<Vec<String>> {
        input
            .get(key)
            .map(|v| serde_json::from_value(v.clone()).map_err(Error::from))
            .transpose()
            .map(Option::unwrap_or_default)
    };
    Ok(Delegation {
        operation,
        name: required("name")?,
        profile,
        scope,
        task,
        origin_conversation: parent.origin_conversation.clone(),
        goal,
        deliverable: required("deliverable")?,
        inputs: list("inputs")?,
        pinned_instructions: list("pinned_instructions")?,
        suggested_skills: list("suggested_skills")?,
        fork_context: input["fork_context"].as_bool().unwrap_or(false),
        token,
        spec: excalibur_engine::RunSpec {
            context,
            budget,
            criteria: vec![excalibur_engine::Criterion::FinalResponse { schema: None }],
        },
        priority,
    })
}
fn coordination_definitions(parent: bool) -> Vec<ToolDefinition> {
    let mut defs=vec![ToolDefinition{name:"status".into(),description:"Read deterministic status for yourself or a direct child, without a model call.".into(),input_schema:json!({"type":"object","properties":{"agent":{"type":"string"}},"additionalProperties":false})},ToolDefinition{name:"agent_message".into(),description:"Read/ack your durable mailbox, report or ask your parent, and direct/answer your own children. Reports never substitute verification evidence.".into(),input_schema:json!({"type":"object","properties":{"action":{"enum":["mailbox","ack","report","question","blocked","answer","directive"]},"agent":{"type":"string"},"id":{"type":"string"},"payload":{},"priority":{"enum":["normal","interrupt"]}},"required":["action"],"additionalProperties":false})}];
    if parent {
        defs.push(ToolDefinition{name:"subagent".into(),description:"Delegate independent work with a structured contract and a capability/budget subset, or control your children. Level-two workers cannot delegate.".into(),input_schema:json!({"type":"object","properties":{"action":{"enum":["spawn","pause","resume","interrupt","cancel","redirect","wait"]},"agent":{"type":"string"},"delegation":{"type":"object","properties":{"name":{"type":"string"},"profile":{"type":"string","description":"Built-in role or host-registered custom profile ID"},"goal":{"type":"string"},"deliverable":{"type":"string"},"scope":{"type":"string"},"inputs":{"type":"array","items":{"type":"string"}},"pinned_instructions":{"type":"array","items":{"type":"string"}},"suggested_skills":{"type":"array","items":{"type":"string"}},"fork_context":{"type":"boolean"}},"required":["name","profile","goal","deliverable"]},"text":{"type":"string"},"confirmed":{"type":"boolean"}},"required":["action"],"additionalProperties":false})});
    }
    defs
}
fn is_coordination(name: &str) -> bool {
    matches!(name, "subagent" | "status" | "agent_message")
}
fn profile_allows(profile: &str, name: &str) -> bool {
    if matches!(
        profile,
        "planner" | "explorer" | "reviewer" | "researcher" | "security-auditor"
    ) {
        !matches!(
            name,
            "exec"
                | "exec_io"
                | "write_file"
                | "edit_file"
                | "apply_patch"
                | "revert_edit"
                | "workspace_snapshot"
                | "worktree"
                | "integration_queue"
        )
    } else if profile == "verifier" {
        !matches!(
            name,
            "write_file"
                | "edit_file"
                | "apply_patch"
                | "revert_edit"
                | "workspace_snapshot"
                | "worktree"
                | "integration_queue"
        )
    } else {
        true
    }
}
impl ToolRuntime for OrchestratorTools {
    fn overrides_builtin<'a>(&'a self, run: &'a str, name: &'a str) -> BoxFuture<'a, bool> {
        self.delegate.overrides_builtin(run, name)
    }

    fn run_definitions<'a>(
        &'a self,
        run: &'a str,
    ) -> BoxFuture<'a, std::result::Result<Option<Vec<ToolDefinition>>, String>> {
        Box::pin(async move { self.definitions_for_run(run).await.map(Some) })
    }
    fn definitions(&self) -> Vec<ToolDefinition> {
        let mut defs = self.delegate.definitions();
        defs.retain(|d| !is_coordination(&d.name));
        defs.extend(coordination_definitions(true));
        defs
    }
    fn prepare<'a>(
        &'a self,
        run: &'a str,
        call: ToolCall,
    ) -> BoxFuture<'a, std::result::Result<PreparedTool, String>> {
        Box::pin(async move {
            let host = self.host()?;
            let a = host.agent_for_run(run).await.map_err(|e| e.to_string())?;
            if !profile_allows(&a.profile, &call.name) {
                return Err("tool is excluded by role profile".into());
            }
            if !is_coordination(&call.name) {
                return self.delegate.prepare(run, call).await;
            }
            if call.name == "subagent" && a.depth >= 2 {
                return Err("level-two workers have no subagent tool".into());
            }
            let _: Value = serde_json::from_str(&call.arguments).map_err(|e| e.to_string())?;
            Ok(PreparedTool {
                call,
                effect: Effect::Keyed,
                resources: vec![Resource {
                    key: format!("orchestrator:{}", a.id),
                    write: true,
                }],
                exclusive: true,
                data: json!({"agent":a.id}),
            })
        })
    }
    fn execute<'a>(
        &'a self,
        run: &'a str,
        key: &'a str,
        tool: &'a PreparedTool,
    ) -> BoxFuture<'a, ToolResult> {
        Box::pin(async move {
            if !is_coordination(&tool.call.name) {
                return self.delegate.execute(run, key, tool).await;
            }
            match self.execute_coordination(run, key, tool).await {
                Ok(v) => ToolResult::ok(v),
                Err(e) => ToolResult::error(e.to_string()),
            }
        })
    }
    fn reconcile<'a>(
        &'a self,
        run: &'a str,
        key: &'a str,
        tool: &'a PreparedTool,
    ) -> BoxFuture<'a, Reconcile> {
        Box::pin(async move {
            if !is_coordination(&tool.call.name) {
                return self.delegate.reconcile(run, key, tool).await;
            }
            Reconcile::Done(self.execute(run, key, tool).await)
        })
    }

    fn execution_state<'a>(
        &'a self,
        run: &'a str,
        state: &'a str,
        reason: Option<&'a str>,
    ) -> BoxFuture<'a, std::result::Result<(), String>> {
        self.delegate.execution_state(run, state, reason)
    }
    fn begin_step<'a>(
        &'a self,
        run: &'a str,
        step: u64,
    ) -> BoxFuture<'a, std::result::Result<(), String>> {
        Box::pin(async move {
            self.delegate.begin_step(run, step).await?;
            let host = self.host()?;
            let agent = host.agent_for_run(run).await.map_err(|e| e.to_string())?;
            host.observe_plan_progress(&agent, step)
                .await
                .map_err(|e| e.to_string())?;
            host.deliver_mailbox_at_boundary(&agent.id)
                .await
                .map_err(|e| e.to_string())?;
            if !host
                .apply_agent_control(&agent.id)
                .await
                .map_err(|e| e.to_string())?
            {
                host.apply_child_wait(&agent.id)
                    .await
                    .map_err(|e| e.to_string())?;
            }
            let status = host.status(&agent.id).await.map_err(|e| e.to_string())?;
            if status.report_reminder {
                let last = status
                    .last_report
                    .as_ref()
                    .and_then(|value| value["id"].as_str())
                    .unwrap_or("initial");
                host.engine().receive_agent_message(run,&format!("report-reminder:{run}:{last}:{}",step/15),"report","orchestrator",json!({"reminder":"Update your plan or send a brief progress report; retain deterministic evidence of completed work."})).await.map_err(|e|e.to_string())?;
            }
            Ok(())
        })
    }
    fn end_step<'a>(
        &'a self,
        run: &'a str,
        step: u64,
    ) -> BoxFuture<'a, std::result::Result<(), String>> {
        self.delegate.end_step(run, step)
    }
    fn interrupt_execution<'a>(
        &'a self,
        run: &'a str,
        cancelled: bool,
        tool: Option<&'a PreparedTool>,
    ) -> BoxFuture<'a, std::result::Result<bool, String>> {
        self.delegate.interrupt_execution(run, cancelled, tool)
    }
    fn configure_execution_limits<'a>(
        &'a self,
        run: &'a str,
        limits: ExecutionLimits,
    ) -> BoxFuture<'a, std::result::Result<(), String>> {
        self.delegate.configure_execution_limits(run, limits)
    }
    fn execution_resources<'a>(
        &'a self,
        run: &'a str,
    ) -> BoxFuture<'a, std::result::Result<Option<ExecutionResources>, String>> {
        self.delegate.execution_resources(run)
    }
    fn sanitize_durable(&self, value: &Value) -> std::result::Result<Value, String> {
        self.delegate.sanitize_durable(value)
    }
    fn pending_approval<'a>(&'a self, run: &'a str) -> BoxFuture<'a, bool> {
        self.delegate.pending_approval(run)
    }
    fn approval_blocked<'a>(&'a self, run: &'a str) -> BoxFuture<'a, Option<String>> {
        self.delegate.approval_blocked(run)
    }
    fn observe_untrusted<'a>(
        &'a self,
        run: &'a str,
        source: &'a str,
    ) -> BoxFuture<'a, std::result::Result<(), String>> {
        self.delegate.observe_untrusted(run, source)
    }
    fn configure_process_boundary<'a>(
        &'a self,
        run: &'a str,
        boundary: ProcessBoundary,
    ) -> BoxFuture<'a, std::result::Result<(), String>> {
        self.delegate.configure_process_boundary(run, boundary)
    }
    fn execution_tool_budget<'a>(
        &'a self,
        run: &'a str,
    ) -> BoxFuture<'a, std::result::Result<Option<u64>, String>> {
        self.delegate.execution_tool_budget(run)
    }
    fn result_requires_approval(&self, result: &ToolResult) -> bool {
        self.delegate.result_requires_approval(result)
    }
    fn durable_blob_inspector(&self) -> Option<Box<dyn DurableByteInspector>> {
        self.delegate.durable_blob_inspector()
    }
    fn audit_builtin<'a>(
        &'a self,
        run: &'a str,
        call: &'a ToolCall,
        result: Option<&'a ToolResult>,
    ) -> BoxFuture<'a, std::result::Result<(), String>> {
        self.delegate.audit_builtin(run, call, result)
    }
    fn configure_network_authorizer<'a>(
        &'a self,
        run: &'a str,
        authorizer: Arc<dyn NetworkAuthorizer>,
    ) -> BoxFuture<'a, std::result::Result<(), String>> {
        self.delegate.configure_network_authorizer(run, authorizer)
    }
    fn finalization_guard<'a>(
        &'a self,
        run: &'a str,
    ) -> BoxFuture<'a, std::result::Result<Box<dyn FinalizationGuard>, String>> {
        self.delegate.finalization_guard(run)
    }
    fn deferred_definitions<'a>(
        &'a self,
        run: &'a str,
    ) -> BoxFuture<'a, std::result::Result<Vec<ToolDefinition>, String>> {
        self.delegate.deferred_definitions(run)
    }
    fn live_status(&self, run: &str) -> Vec<Value> {
        self.delegate.live_status(run)
    }
    fn scheduling<'a>(
        &'a self,
        run: &'a str,
        call: &'a ToolCall,
    ) -> BoxFuture<'a, Option<ToolScheduling>> {
        if is_coordination(&call.name) {
            Box::pin(async { Some(ToolScheduling::isolated()) })
        } else {
            self.delegate.scheduling(run, call)
        }
    }
}
