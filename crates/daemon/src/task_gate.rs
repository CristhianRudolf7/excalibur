//! Per-run verification always uses the workspace selected by the trusted host.
use crate::worker_tools::WorkerTools;
use excalibur_engine::{AdmissionState, CompletionGate, CompletionGateGuard, GateInput};
use excalibur_models::ModelGateway;
use excalibur_tasks::{CheckRunner, TaskCompletionGate, TaskStore};
use excalibur_types::runtime::{BoxFuture, ToolRuntime};
use excalibur_vault::Redactor;
use std::sync::Arc;
pub struct WorkerTaskGate {
    pub store: TaskStore,
    pub runtime: Arc<dyn ToolRuntime>,
    pub workers: Arc<WorkerTools>,
    pub model: Arc<dyn ModelGateway>,
    pub redactor: Redactor,
}
impl WorkerTaskGate {
    async fn gate(&self, run: &str, verify: bool) -> Result<TaskCompletionGate, String> {
        let workspace = self.workers.workspace_for(run)?;
        let gate = TaskCompletionGate::new(
            self.store.clone(),
            CheckRunner::new(self.runtime.clone(), workspace.clone()),
        );
        if !verify {
            return Ok(gate);
        }
        let tools = self.workers.verifier_tools(run).await?;
        let verifier = excalibur_eval::verifier::BoundGatewayVerifier {
            verifier: excalibur_eval::verifier::GatewayVerifier {
                store: self.store.clone(),
                model: self.model.clone(),
                sandbox: excalibur_tools::terminal::SandboxConfig::read_only(&workspace),
                workspace,
                protected: vec![
                    self.store.database().path().to_path_buf(),
                    self.store.database().blobs().root().to_path_buf(),
                ],
                redactor: self.redactor.clone(),
            },
            tools,
        };
        Ok(gate.with_verifier(Arc::new(verifier)))
    }
}
impl CompletionGate for WorkerTaskGate {
    fn inspect<'a>(
        &'a self,
        run: &'a str,
        input: &'a GateInput,
    ) -> BoxFuture<'a, Result<Box<dyn CompletionGateGuard>, String>> {
        Box::pin(async move { self.gate(run, true).await?.inspect(run, input).await })
    }
    fn admission<'a>(&'a self, run: &'a str) -> BoxFuture<'a, Result<AdmissionState, String>> {
        Box::pin(async move { self.gate(run, false).await?.admission(run).await })
    }
    fn interrupt<'a>(&'a self, run: &'a str, cancelled: bool) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async move { self.gate(run, false).await?.interrupt(run, cancelled).await })
    }
    fn context<'a>(&'a self, run: &'a str) -> BoxFuture<'a, Result<String, String>> {
        Box::pin(async move { self.gate(run, false).await?.context(run).await })
    }
    fn structured_context<'a>(
        &'a self,
        run: &'a str,
    ) -> BoxFuture<'a, Result<Option<excalibur_context::StructuredState>, String>> {
        Box::pin(async move { self.gate(run, false).await?.structured_context(run).await })
    }
}
