use crate::*;
use excalibur_engine::{
    CompletionGate, CompletionGateGuard, GateAssessment, GateInput, GateIssue, GateIssueKind,
};
use std::sync::Weak;
/// Couples child settlement to finalization. An optional task gate retains its
/// own evidence freshness guard under this tree-mutation guard.
#[derive(Default)]
pub struct OrchestrationGate {
    inner: Mutex<Weak<Inner>>,
    delegate: Option<Arc<dyn CompletionGate>>,
}
impl OrchestrationGate {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn with_delegate(mut self, delegate: Arc<dyn CompletionGate>) -> Self {
        self.delegate = Some(delegate);
        self
    }
    pub fn attach(&self, host: &Orchestrator) -> Result<()> {
        let mut old = self.inner.lock().unwrap();
        if let Some(known) = old.upgrade()
            && !Arc::ptr_eq(&known, &host.inner)
        {
            return Err(invalid("completion gate already bound"));
        }
        *old = Arc::downgrade(&host.inner);
        Ok(())
    }
}
struct Guard {
    report: GateAssessment,
    _tree: tokio::sync::OwnedMutexGuard<()>,
    _delegate: Option<Box<dyn CompletionGateGuard>>,
}
impl CompletionGateGuard for Guard {
    fn report(&self) -> GateAssessment {
        self.report.clone()
    }
}
impl CompletionGate for OrchestrationGate {
    fn inspect<'a>(
        &'a self,
        run: &'a str,
        input: &'a GateInput,
    ) -> BoxFuture<'a, std::result::Result<Box<dyn CompletionGateGuard>, String>> {
        Box::pin(async move {
            let inner = self
                .inner
                .lock()
                .unwrap()
                .upgrade()
                .ok_or("orchestration gate is not attached")?;
            let host = Orchestrator { inner };
            let delegate = if let Some(delegate) = &self.delegate {
                Some(delegate.inspect(run, input).await?)
            } else {
                None
            };
            let tree = host.inner.gate.clone().lock_owned().await;
            let agent = host.agent_for_run(run).await.map_err(|e| e.to_string())?;
            let mut report = GateAssessment::default();
            let state = host
                .inner
                .engine
                .load(run)
                .await
                .map_err(|e| e.to_string())?;
            for child in host
                .list_agents()
                .await
                .map_err(|e| e.to_string())?
                .into_iter()
                .filter(|a| a.parent_id.as_deref() == Some(&agent.id))
            {
                let Some(child_run) = child.run_id else {
                    continue;
                };
                let snapshot = host
                    .inner
                    .engine
                    .load(&child_run)
                    .await
                    .map_err(|e| e.to_string())?;
                let settled = state
                    .child_allocations
                    .get(&child.id)
                    .is_some_and(|a| a.settled.is_some());
                if !terminal(&snapshot.status) || !settled {
                    report.issues.push(GateIssue {
                        id: format!("child:{}", child.id),
                        kind: GateIssueKind::Child,
                        detail: format!(
                            "Child {} is {} and its budget is {}",
                            child.name,
                            label(&snapshot.status),
                            if settled { "settled" } else { "not settled" }
                        ),
                        blocked: false,
                    });
                }
            }
            if let Some(guard) = &delegate {
                let other = guard.report();
                report.issues.extend(other.issues);
                report.deliverables.extend(other.deliverables);
                report.evidence.extend(other.evidence);
                report.modified_files.extend(other.modified_files);
                report.assumptions.extend(other.assumptions);
                report.open_questions.extend(other.open_questions);
            }
            Ok(Box::new(Guard {
                report,
                _tree: tree,
                _delegate: delegate,
            }) as Box<dyn CompletionGateGuard>)
        })
    }
    fn admission<'a>(
        &'a self,
        run: &'a str,
    ) -> BoxFuture<'a, std::result::Result<excalibur_engine::AdmissionState, String>> {
        if let Some(delegate) = &self.delegate {
            delegate.admission(run)
        } else {
            Box::pin(async { Ok(excalibur_engine::AdmissionState::Ready) })
        }
    }
    fn interrupt<'a>(
        &'a self,
        run: &'a str,
        cancelled: bool,
    ) -> BoxFuture<'a, std::result::Result<(), String>> {
        if let Some(delegate) = &self.delegate {
            delegate.interrupt(run, cancelled)
        } else {
            Box::pin(async { Ok(()) })
        }
    }
    fn structured_context<'a>(
        &'a self,
        run: &'a str,
    ) -> BoxFuture<'a, std::result::Result<Option<excalibur_context::StructuredState>, String>>
    {
        if let Some(delegate) = &self.delegate {
            delegate.structured_context(run)
        } else {
            Box::pin(async { Ok(None) })
        }
    }
    fn context<'a>(&'a self, run: &'a str) -> BoxFuture<'a, std::result::Result<String, String>> {
        Box::pin(async move {
            let inner = self
                .inner
                .lock()
                .unwrap()
                .upgrade()
                .ok_or("orchestration gate is not attached")?;
            let host = Orchestrator { inner };
            let a = host.agent_for_run(run).await.map_err(|e| e.to_string())?;
            let s = host.status(&a.id).await.map_err(|e| e.to_string())?;
            let mut text=serde_json::to_string(&json!({"agent":a.id,"state":s.state,"jobs":host.list_agents().await.map_err(|e|e.to_string())?.into_iter().filter(|c|c.parent_id.as_deref()==Some(&a.id)).map(|c|json!({"id":c.id,"name":c.name,"run":c.run_id})).collect::<Vec<_>>(),"report_reminder":s.report_reminder,"pending_messages":s.pending_messages})).map_err(|e|e.to_string())?;
            if let Some(delegate) = &self.delegate {
                text.push('\n');
                text.push_str(&delegate.context(run).await?);
            }
            Ok(text)
        })
    }
}
