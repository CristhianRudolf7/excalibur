//! Durable authorization boundary for trusted host adapters without agent tool registration.
use crate::{Action, Decision, Error, Result, Security, digest};
use excalibur_store::ReadLane;
use excalibur_types::runtime::{PreparedTool, ToolResult};
impl Security {
    pub async fn host_effect_receipt(
        &self,
        run: &str,
        prepared: &PreparedTool,
        now: i64,
    ) -> Result<Option<ToolResult>> {
        let hash = digest(&serde_json::to_vec(prepared)?);
        let ticket = format!("permit_{}", digest(format!("{run}:{hash}").as_bytes()));
        let t = ticket.clone();
        let admitted = self
            .db
            .read(ReadLane::Interactive, move |c| {
                Ok(c.query_row(
                    "SELECT EXISTS(SELECT 1 FROM security_admissions WHERE ticket=?1)",
                    [t],
                    |r| r.get::<_, bool>(0),
                )?)
            })
            .await?;
        if !admitted {
            return Ok(None);
        }
        self.validate_ticket(run, prepared, &ticket, now).await?;
        match self.dispatch_state(&ticket).await? {
            (_, Some(result)) => Ok(Some(result)),
            (true, None) => Err(Error::Denied("host effect requires reconciliation".into())),
            _ => Ok(None),
        }
    }
    pub async fn execute_host_effect<F, Fut>(
        &self,
        run: &str,
        prepared: &PreparedTool,
        actions: &[Action],
        now: i64,
        operation: F,
    ) -> Result<ToolResult>
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = ToolResult>,
    {
        if actions.is_empty() {
            return Err(Error::Denied(
                "host effects require explicit authority".into(),
            ));
        }
        let call_hash = digest(&serde_json::to_vec(prepared)?);
        let ticket = format!("permit_{}", digest(format!("{run}:{call_hash}").as_bytes()));
        let t = ticket.clone();
        let admitted = self
            .db
            .read(ReadLane::Interactive, move |c| {
                Ok(c.query_row(
                    "SELECT EXISTS(SELECT 1 FROM security_admissions WHERE ticket=?1)",
                    [t],
                    |r| r.get::<_, bool>(0),
                )?)
            })
            .await?;
        if !admitted {
            for action in actions {
                match self.authorize(run, action, now).await? {
                    Decision::Allow => {}
                    Decision::Ask { approval_id, .. } => {
                        return Err(Error::ApprovalPending(approval_id));
                    }
                    Decision::Deny { reason } => return Err(Error::Denied(reason)),
                }
            }
            self.admit(run, prepared, actions, now).await?;
        }
        self.validate_ticket(run, prepared, &ticket, now).await?;
        if !self.begin_dispatch(&ticket).await? {
            return match self.dispatch_state(&ticket).await? {
                (_, Some(mut result)) => {
                    result.output = self.redactor.redact_json(&result.output);
                    Ok(result)
                }
                _ => Err(Error::Denied(
                    "host effect dispatched without receipt; reconciliation required".into(),
                )),
            };
        }
        let mut result = operation().await;
        result.output = self.redactor.redact_json(&result.output);
        self.finish_dispatch(&ticket, &result).await?;
        Ok(result)
    }
}
