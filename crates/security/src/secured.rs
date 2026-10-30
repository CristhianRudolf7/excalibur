//! Authorization decorator. Trusted process adapters still enforce the OS sandbox.
use crate::*;
use excalibur_types::runtime::*;
use std::{path::Path, sync::Arc};
struct SecretBlobInspector {
    stream: excalibur_vault::StreamRedactor,
    original: Sha256,
    sanitized: Sha256,
}
impl DurableByteInspector for SecretBlobInspector {
    fn inspect(&mut self, bytes: &[u8]) -> std::result::Result<(), String> {
        self.original.update(bytes);
        self.sanitized.update(self.stream.push(bytes));
        Ok(())
    }
    fn finish(&mut self) -> std::result::Result<(), String> {
        self.sanitized.update(self.stream.finish());
        if self.original.clone().finalize() != self.sanitized.clone().finalize() {
            return Err("blob contains secret material".into());
        }
        Ok(())
    }
}
#[derive(Clone)]
pub struct SecuredTools {
    delegate: Arc<dyn ToolRuntime>,
    security: Security,
}
impl SecuredTools {
    pub fn new(delegate: Arc<dyn ToolRuntime>, security: Security) -> Self {
        Self { delegate, security }
    }
    pub fn security(&self) -> &Security {
        &self.security
    }
    fn confined_reads(
        &self,
        run: &str,
        workspace: &Path,
    ) -> std::result::Result<Vec<Action>, String> {
        let Some(proof) = self.delegate.filesystem_confinement(run) else {
            return Ok(vec![Action::Read {
                path: workspace.to_path_buf(),
            }]);
        };
        if proof.workspace != workspace
            || proof.snapshot_owner != run
            || proof.read_roots.is_empty()
            || proof
                .read_roots
                .iter()
                .any(|root| !root.starts_with(workspace))
        {
            return Err("invalid host filesystem confinement attestation".into());
        }
        // Every attested root is still evaluated by the ordinary current token
        // and permission logic. No workspace-wide capability is manufactured.
        Ok(proof
            .read_roots
            .into_iter()
            .map(|path| Action::Read { path })
            .collect())
    }
    async fn actions(
        &self,
        run: &str,
        call: &ToolCall,
    ) -> std::result::Result<Vec<Action>, String> {
        let config = self
            .security
            .run_security(run)
            .await
            .map_err(|e| e.to_string())?;
        let args: Value = serde_json::from_str(&call.arguments)
            .or_else(|_| json5::from_str(&call.arguments))
            .map_err(|_| "invalid tool JSON")?;
        let path = |value: &str| -> std::result::Result<PathBuf, String> {
            canonical_target(&if Path::new(value).is_absolute() {
                PathBuf::from(value)
            } else {
                config.workspace.join(value)
            })
        };
        // Entry restoration publishes/replaces a directory entry, including a
        // symlink itself. Authorize the canonical parent rather than following
        // its current leaf to a different capability root.
        let entry_parent = |value: &str| -> std::result::Result<PathBuf, String> {
            let full = if Path::new(value).is_absolute() {
                PathBuf::from(value)
            } else {
                config.workspace.join(value)
            };
            let parent = full.parent().ok_or("entry parent missing")?;
            canonical_target(parent)
        };
        let required = |key: &str| -> std::result::Result<&str, String> {
            args.get(key)
                .and_then(Value::as_str)
                .ok_or_else(|| format!("{key}: string required"))
        };
        let mut actions = match call.name.as_str() {
            "read_file" => vec![Action::Read {
                path: path(required("path")?)?,
            }],
            "write_file" | "edit_file" => vec![Action::WriteExact {
                path: path(required("path")?)?,
                change: call.arguments.clone(),
            }],
            "apply_patch" => {
                let mut actions = Vec::new();
                for line in required("patch")?.lines() {
                    for prefix in [
                        "*** Add File: ",
                        "*** Delete File: ",
                        "*** Update File: ",
                        "*** Move to: ",
                    ] {
                        if let Some(target) = line.strip_prefix(prefix) {
                            actions.push(Action::WriteExact {
                                path: path(target)?,
                                change: call.arguments.clone(),
                            });
                        }
                    }
                }
                if actions.is_empty() {
                    return Err("patch has no file operations".into());
                }
                actions
            }
            "glob" | "grep" => {
                if self
                    .security
                    .protected
                    .iter()
                    .any(|path| path.starts_with(&config.workspace))
                {
                    return Err("search is unavailable across host-protected vault subtrees; use explicit authorized file reads".into());
                }
                self.confined_reads(run, &config.workspace)?
            }
            "file_history" => vec![Action::Read {
                path: entry_parent(required("path")?)?,
            }],
            "revert_edit" => vec![Action::WriteExact {
                path: entry_parent(required("path")?)?,
                change: call.arguments.clone(),
            }],
            "workspace_snapshot" if args["action"] == "restore" => {
                let targets = args["paths"]
                    .as_array()
                    .ok_or("snapshot restoration requires explicit target paths")?;
                if targets.is_empty() {
                    return Err("snapshot restoration requires target paths".into());
                }
                targets
                    .iter()
                    .map(|target| {
                        Ok(Action::WriteExact {
                            path: entry_parent(target.as_str().ok_or("invalid snapshot target")?)?,
                            change: call.arguments.clone(),
                        })
                    })
                    .collect::<std::result::Result<Vec<_>, String>>()?
            }
            "lsp" if args["action"] == "rename" => {
                let mut targets = std::collections::BTreeSet::from([required("path")?.to_owned()]);
                if let Some(paths) = args["paths"].as_array() {
                    for target in paths {
                        targets.insert(target.as_str().ok_or("invalid rename target")?.into());
                    }
                }
                targets
                    .into_iter()
                    .map(|target| {
                        Ok(Action::WriteExact {
                            path: path(&target)?,
                            change: call.arguments.clone(),
                        })
                    })
                    .collect::<std::result::Result<Vec<_>, String>>()?
            }
            "workspace_snapshot" => self.confined_reads(run, &config.workspace)?,
            "code_graph" | "lsp" => vec![Action::Read {
                path: config.workspace,
            }],
            "exec" => {
                if let Some(cwd) = args.get("cwd").and_then(Value::as_str)
                    && path(cwd)? != config.workspace
                {
                    return Err(
                        "cwd is fixed by the host; model-selected directory is not authorized"
                            .into(),
                    );
                }
                vec![Action::Exec {
                    command: required("cmd")?.into(),
                }]
            }
            "exec_io" => vec![Action::Exec {
                command: if matches!(args["action"].as_str(), Some("list" | "read" | "wait_for")) {
                    "true".into()
                } else {
                    // Terminal input can complete a previously split command,
                    // including key sequences; approve the exact JSON action.
                    format!("terminal-input:{}", call.arguments)
                },
            }],
            "settings" | "security_settings" | "configure_security" => {
                vec![Action::SecuritySettings {
                    diff: call.arguments.clone(),
                }]
            }
            "disable_sandbox" => vec![Action::DisableSandbox],
            "read_vault" => vec![Action::ReadVault],
            other => vec![Action::External {
                kind: format!("tool:{other}"),
                target: call.arguments.clone(),
            }],
        };
        fn references(value: &Value, out: &mut std::collections::BTreeSet<String>) {
            match value {
                Value::String(text) if text.starts_with("secret://") => {
                    out.insert(text.clone());
                }
                Value::Array(values) => {
                    for value in values {
                        references(value, out);
                    }
                }
                Value::Object(values) => {
                    for value in values.values() {
                        references(value, out);
                    }
                }
                _ => {}
            }
        }
        let mut refs = std::collections::BTreeSet::new();
        references(&args, &mut refs);
        actions.extend(
            refs.into_iter()
                .map(|reference| Action::UseSecret { reference }),
        );
        Ok(actions)
    }
    async fn process_boundary(
        &self,
        run: &str,
        call: &ToolCall,
        now: i64,
    ) -> std::result::Result<(), String> {
        if !matches!(call.name.as_str(), "exec" | "exec_io") {
            return Ok(());
        }
        let config = self
            .security
            .run_security(run)
            .await
            .map_err(|e| e.to_string())?;
        let token = self
            .security
            .effective_token(run, now)
            .await
            .map_err(|e| e.to_string())?;
        if !token.processes {
            return Err("process capability is absent".into());
        }
        // Eval uses the explicitly attested container network ceiling, validated
        // by the native adapter. Project-net retains the live host broker.
        let network_enabled = !matches!(token.network, Network::None)
            && token.sandbox_profile.as_deref() != Some("eval");
        self.delegate
            .configure_process_boundary(
                run,
                ProcessBoundary {
                    read_roots: token.roots.iter().map(|root| root.path.clone()).collect(),
                    write_roots: token
                        .roots
                        .iter()
                        .filter(|root| root.write)
                        .map(|root| root.path.clone())
                        .collect(),
                    network: match token.network {
                        Network::None => ProcessNetwork::None,
                        Network::Domains(domains) => {
                            ProcessNetwork::Domains(domains.into_iter().collect())
                        }
                        Network::Full => ProcessNetwork::Full,
                    },
                    profile: token
                        .sandbox_profile
                        .ok_or("host sandbox profile is absent")?,
                    workspace: config.workspace,
                    denied_paths: self
                        .security
                        .protected
                        .iter()
                        .chain(self.security.evaluations.iter())
                        .cloned()
                        .collect(),
                },
            )
            .await?;
        if network_enabled {
            let guard = self
                .security
                .network_authorizer(run.to_owned())
                .map_err(|error| error.to_string())?;
            self.delegate
                .configure_network_authorizer(run, guard)
                .await?;
        }
        Ok(())
    }
    fn unpack(tool: &PreparedTool) -> PreparedTool {
        let mut inner = tool.clone();
        inner.data = tool.data["_security_delegate"].clone();
        inner
    }
}
impl ToolRuntime for SecuredTools {
    fn overrides_builtin<'a>(&'a self, run: &'a str, name: &'a str) -> BoxFuture<'a, bool> {
        self.delegate.overrides_builtin(run, name)
    }

    fn deferred_definitions<'a>(
        &'a self,
        run: &'a str,
    ) -> excalibur_types::runtime::BoxFuture<
        'a,
        std::result::Result<Vec<excalibur_types::runtime::ToolDefinition>, String>,
    > {
        self.delegate.deferred_definitions(run)
    }
    fn live_status(&self, run: &str) -> Vec<serde_json::Value> {
        self.delegate.live_status(run)
    }
    fn execution_state<'a>(
        &'a self,
        run: &'a str,
        state: &'a str,
        reason: Option<&'a str>,
    ) -> excalibur_types::runtime::BoxFuture<'a, std::result::Result<(), String>> {
        self.delegate.execution_state(run, state, reason)
    }
    fn begin_step<'a>(
        &'a self,
        run: &'a str,
        step: u64,
    ) -> excalibur_types::runtime::BoxFuture<'a, std::result::Result<(), String>> {
        self.delegate.begin_step(run, step)
    }
    fn end_step<'a>(
        &'a self,
        run: &'a str,
        step: u64,
    ) -> excalibur_types::runtime::BoxFuture<'a, std::result::Result<(), String>> {
        self.delegate.end_step(run, step)
    }
    fn interrupt_execution<'a>(
        &'a self,
        run: &'a str,
        cancelled: bool,
        tool: Option<&'a excalibur_types::runtime::PreparedTool>,
    ) -> excalibur_types::runtime::BoxFuture<'a, std::result::Result<bool, String>> {
        self.delegate.interrupt_execution(run, cancelled, tool)
    }
    fn configure_execution_limits<'a>(
        &'a self,
        run: &'a str,
        limits: excalibur_types::runtime::ExecutionLimits,
    ) -> excalibur_types::runtime::BoxFuture<'a, std::result::Result<(), String>> {
        self.delegate.configure_execution_limits(run, limits)
    }
    fn execution_resources<'a>(
        &'a self,
        run: &'a str,
    ) -> excalibur_types::runtime::BoxFuture<
        'a,
        std::result::Result<Option<excalibur_types::runtime::ExecutionResources>, String>,
    > {
        self.delegate.execution_resources(run)
    }

    fn observe_untrusted<'a>(
        &'a self,
        run: &'a str,
        source: &'a str,
    ) -> BoxFuture<'a, std::result::Result<(), String>> {
        Box::pin(async move {
            self.security
                .mark_untrusted(run, source, excalibur_store::unix_seconds())
                .await
                .map_err(|error| error.to_string())
        })
    }
    fn execution_tool_budget<'a>(
        &'a self,
        run: &'a str,
    ) -> BoxFuture<'a, std::result::Result<Option<u64>, String>> {
        Box::pin(async move {
            self.security
                .effective_token(run, excalibur_store::unix_seconds())
                .await
                .map(|token| Some(token.budget))
                .map_err(|e| e.to_string())
        })
    }
    fn result_requires_approval(&self, result: &ToolResult) -> bool {
        result.output["_security_pause"] == true
    }
    fn durable_blob_inspector(&self) -> Option<Box<dyn DurableByteInspector>> {
        Some(Box::new(SecretBlobInspector {
            stream: self.security.redactor.stream(),
            original: Sha256::new(),
            sanitized: Sha256::new(),
        }))
    }
    fn audit_builtin<'a>(
        &'a self,
        run: &'a str,
        call: &'a ToolCall,
        result: Option<&'a ToolResult>,
    ) -> BoxFuture<'a, std::result::Result<(), String>> {
        Box::pin(async move {
            // Builtins remain read-only utilities; their output is external data.
            if result.is_some() {
                self.security
                    .mark_untrusted(run, "builtin_output", excalibur_store::unix_seconds())
                    .await
                    .map_err(|e| e.to_string())?;
            }
            self.security.audit(json!({"kind":if result.is_some(){"tool_result"}else{"tool_call"},"run":run,"call":call,"result":result,"at":excalibur_store::unix_seconds()})).await.map_err(|e|e.to_string())
        })
    }
    fn finalization_guard<'a>(
        &'a self,
        run: &'a str,
    ) -> BoxFuture<'a, std::result::Result<Box<dyn FinalizationGuard>, String>> {
        Box::pin(async move {
            self.security
                .finalization_guard(run, excalibur_store::unix_seconds())
                .await
                .map_err(|error| error.to_string())
        })
    }
    fn definitions(&self) -> Vec<ToolDefinition> {
        self.delegate.definitions()
    }
    fn scheduling<'a>(
        &'a self,
        run: &'a str,
        call: &'a ToolCall,
    ) -> BoxFuture<'a, Option<ToolScheduling>> {
        self.delegate.scheduling(run, call)
    }
    fn sanitize_durable(&self, value: &Value) -> std::result::Result<Value, String> {
        Ok(self.security.redactor.redact_json(value))
    }
    fn pending_approval<'a>(&'a self, run: &'a str) -> BoxFuture<'a, bool> {
        Box::pin(async move {
            self.security
                .pending_approval(run, excalibur_store::unix_seconds())
                .await
                .unwrap_or(true)
        })
    }
    fn approval_blocked<'a>(&'a self, run: &'a str) -> BoxFuture<'a, Option<String>> {
        Box::pin(async move {
            self.security
                .approval_blocked(run, excalibur_store::unix_seconds())
                .await
                .unwrap_or_else(|_| Some("security approval state unavailable".into()))
        })
    }
    fn prepare<'a>(
        &'a self,
        run: &'a str,
        call: ToolCall,
    ) -> BoxFuture<'a, std::result::Result<PreparedTool, String>> {
        Box::pin(async move {
            let now = excalibur_store::unix_seconds();
            if self
                .security
                .redactor
                .contains_secret(call.arguments.as_bytes())
            {
                return Err("secret literals are forbidden in tool arguments; use executor secret references".into());
            }
            let parsed: Value = serde_json::from_str(&call.arguments)
                .or_else(|_| json5::from_str(&call.arguments))
                .map_err(|_| "invalid tool JSON")?;
            if self.security.redactor.redact_json(&parsed) != parsed {
                return Err(
                    "secret literal in parsed tool arguments; use executor references".into(),
                );
            }
            let actions = self.actions(run, &call).await?;
            for action in &actions {
                match self
                    .security
                    .authorize(run, action, now)
                    .await
                    .map_err(|e| e.to_string())?
                {
                    Decision::Allow => {}
                    Decision::Ask {
                        approval_id,
                        reason,
                    } => return Err(format!("approval_required:{approval_id}:{reason}")),
                    Decision::Deny { reason } => return Err(format!("security_denied:{reason}")),
                }
            }
            self.process_boundary(run, &call, now).await?;
            let mut prepared = self.delegate.prepare(run, call).await?;
            if self
                .security
                .redactor
                .redact_json(&serde_json::to_value(&prepared).map_err(|e| e.to_string())?)
                != serde_json::to_value(&prepared).map_err(|e| e.to_string())?
            {
                return Err(
                    "prepared operation contains secret literals; refusing durable admission"
                        .into(),
                );
            }
            let ticket = self
                .security
                .admit(run, &prepared, &actions, now)
                .await
                .map_err(|e| e.to_string())?;
            prepared.data = json!({"_security_ticket":ticket,"_security_delegate":prepared.data});
            Ok(prepared)
        })
    }
    fn execute<'a>(
        &'a self,
        run: &'a str,
        key: &'a str,
        tool: &'a PreparedTool,
    ) -> BoxFuture<'a, ToolResult> {
        Box::pin(async move {
            let Some(ticket) = tool.data["_security_ticket"].as_str() else {
                return ToolResult::error("security admission ticket required");
            };
            let now = excalibur_store::unix_seconds();
            let actions = match self
                .security
                .validate_ticket(run, &Self::unpack(tool), ticket, now)
                .await
            {
                Ok(actions) => actions,
                Err(Error::ApprovalPending(id)) => {
                    return ToolResult::ok(json!({"_security_pause":true,"approval_id":id}));
                }
                Err(error) => return ToolResult::error(error.to_string()),
            };
            if let Err(error) = self.process_boundary(run, &tool.call, now).await {
                return ToolResult::error(error);
            }
            if let Err(error) = self
                .security
                .audit(json!({"kind":"tool_call","run":run,"call":tool.call,"at":now}))
                .await
            {
                return ToolResult::error(error.to_string());
            }
            // Conservatively retain sensitive/untrusted provenance even when a
            // command fails, times out, or crashes after producing partial data.
            if let Err(error) = self.security.record_action_result(run, &actions, now).await {
                return ToolResult::error(error.to_string());
            }
            if matches!(tool.call.name.as_str(), "grep" | "glob")
                && let Err(error) = self
                    .security
                    .state_fact(
                        run,
                        "tainted_secret",
                        json!({"kind":"search_may_read_sensitive_files","at":now}),
                    )
                    .await
            {
                return ToolResult::error(error.to_string());
            }
            match self.security.begin_dispatch(ticket).await {
                Ok(true) => {}
                Ok(false) => match self.security.dispatch_state(ticket).await {
                    Ok((_, Some(result))) => return result,
                    Ok(_) if tool.effect != Effect::NonIdempotent => {}
                    _ => {
                        return ToolResult::error(
                            "non-idempotent effect was already dispatched; reconciliation is required",
                        );
                    }
                },
                Err(error) => return ToolResult::error(error.to_string()),
            }
            let mut result = self.delegate.execute(run, key, &Self::unpack(tool)).await;
            if let Some(object) = result.output.as_object_mut() {
                object.remove("_security_pause");
            }
            result.output = self.security.redactor.redact_json(&result.output);
            if let Err(error) = self.security.finish_dispatch(ticket, &result).await {
                return ToolResult::error(format!(
                    "effect returned but durable result could not be confirmed: {error}"
                ));
            }

            if let Err(error)=self.security.audit(json!({"kind":"tool_result","run":run,"call_id":tool.call.id,"result":result,"at":now})).await{return ToolResult::error(error.to_string());}
            result
        })
    }
    fn reconcile<'a>(
        &'a self,
        run: &'a str,
        key: &'a str,
        tool: &'a PreparedTool,
    ) -> BoxFuture<'a, Reconcile> {
        Box::pin(async move {
            let Some(ticket) = tool.data["_security_ticket"].as_str() else {
                return Reconcile::Uncertain("security admission ticket missing".into());
            };
            let now = excalibur_store::unix_seconds();
            let actions = match self
                .security
                .validate_ticket(run, &Self::unpack(tool), ticket, now)
                .await
            {
                Ok(actions) => actions,
                Err(Error::ApprovalPending(id)) => {
                    return Reconcile::Done(ToolResult::ok(
                        json!({"_security_pause":true,"approval_id":id}),
                    ));
                }
                Err(error) => return Reconcile::Uncertain(error.to_string()),
            };
            match self.security.dispatch_state(ticket).await {
                Ok((_, Some(result))) => return Reconcile::Done(result),
                Ok((false, None)) => return Reconcile::Retry,
                Ok((true, None)) => {}
                Err(error) => return Reconcile::Uncertain(error.to_string()),
            }
            if let Err(error) = self.process_boundary(run, &tool.call, now).await {
                return Reconcile::Uncertain(error);
            }
            if let Err(error) = self.security.record_action_result(run, &actions, now).await {
                return Reconcile::Uncertain(error.to_string());
            }
            match self.delegate.reconcile(run, key, &Self::unpack(tool)).await {
                Reconcile::Done(mut result) => {
                    if let Some(object) = result.output.as_object_mut() {
                        object.remove("_security_pause");
                    }
                    result.output = self.security.redactor.redact_json(&result.output);
                    if let Err(error) = self.security.audit(json!({"kind":"tool_reconciled","run":run,"call_id":tool.call.id,"result":result,"at":now})).await {
                        return Reconcile::Uncertain(error.to_string());
                    }
                    if let Err(error) = self.security.finish_dispatch(ticket, &result).await {
                        return Reconcile::Uncertain(error.to_string());
                    }
                    Reconcile::Done(result)
                }
                other => other,
            }
        })
    }
}
