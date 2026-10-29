//! Bound host executor for secret references. No value is returned from the vault
//! before authorization, a durable dispatch claim, taint and reference-only audit.
use crate::{Action, Decision, Error, Result, Security, digest};
use excalibur_store::ReadLane;
use excalibur_types::runtime::{Effect, PreparedTool, ToolCall, ToolResult};
use excalibur_vault::{SecretRef, Vault};
use serde_json::json;
use std::sync::Arc;

/// Trusted backend boundary, never an agent-facing raw-secret tool. The operation
/// injects the borrowed value into its environment/header/form and performs the
/// effect inside the closure. It must not log or independently publish raw bytes.
/// Returned output is redacted before durable completion and caller delivery.
#[derive(Clone)]
pub struct SecretExecutor {
    security: Security,
    vault: Arc<Vault>,
}
impl std::fmt::Debug for SecretExecutor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SecretExecutor([REDACTED])")
    }
}
impl SecretExecutor {
    pub fn new(security: Security, vault: Arc<Vault>) -> Result<Self> {
        if !security.redactor().same_registry(&vault.redactor()) {
            return Err(Error::Denied(
                "security and vault must share the live redaction registry".into(),
            ));
        }
        Ok(Self { security, vault })
    }

    /// `operation_id` is a stable host-generated effect key. Retries with the
    /// same run/key/reference use the durable sanitized result. A claimed effect
    /// without a receipt is uncertain and is never automatically repeated.
    /// `now` is supplied by the authenticated host, in Unix seconds.
    pub async fn execute<F>(
        &self,
        run: &str,
        operation_id: &str,
        reference: &SecretRef,
        now: i64,
        operation: F,
    ) -> Result<ToolResult>
    where
        F: FnOnce(&[u8]) -> ToolResult + Send + 'static,
    {
        if operation_id.is_empty() || operation_id.len() > 256 {
            return Err(Error::Denied(
                "a bounded host operation ID is required".into(),
            ));
        }
        let prepared = PreparedTool {
            call: ToolCall {
                id: format!("secret-effect:{operation_id}"),
                name: "__host_secret_executor".into(),
                arguments: json!({"operation_id":operation_id,"reference":reference}).to_string(),
            },
            effect: Effect::NonIdempotent,
            resources: Vec::new(),
            exclusive: true,
            data: json!({"reference":reference}),
        };
        // Use the same full-plan identity as Security::admit, allowing a retry to
        // validate its own consumed Once approval without requesting another one.
        let call_hash = digest(&serde_json::to_vec(&prepared)?);
        let ticket = format!("permit_{}", digest(format!("{run}:{call_hash}").as_bytes()));
        let action = Action::UseSecret {
            reference: reference.as_str().to_owned(),
        };
        let ticket_copy = ticket.clone();
        let exists = self
            .security
            .database()
            .read(ReadLane::Interactive, move |c| {
                Ok(c.query_row(
                    "SELECT EXISTS(SELECT 1 FROM security_admissions WHERE ticket=?1)",
                    [ticket_copy],
                    |r| r.get::<_, bool>(0),
                )?)
            })
            .await?;
        if !exists {
            match self.security.authorize(run, &action, now).await? {
                Decision::Allow => {}
                Decision::Ask { approval_id, .. } => {
                    return Err(Error::ApprovalPending(approval_id));
                }
                Decision::Deny { reason } => return Err(Error::Denied(reason)),
            }
            self.security.admit(run, &prepared, &[action], now).await?;
        }
        self.security
            .validate_ticket(run, &prepared, &ticket, now)
            .await?;
        if !self.security.begin_dispatch(&ticket).await? {
            return match self.security.dispatch_state(&ticket).await? {
                (_, Some(mut result)) => {
                    result.output = self.security.redactor().redact_json(&result.output);
                    Ok(result)
                }
                _ => Err(Error::Denied(
                    "secret operation is already dispatched; reconciliation is required".into(),
                )),
            };
        }
        // Both facts are durably committed BEFORE any plaintext enters the
        // operation. Failure here never falls through into secret resolution.
        self.security
            .record_secret_use(run, reference.as_str(), now)
            .await?;
        self.security.audit(json!({"kind":"secret_executor_dispatch","run":run,"operation_id":operation_id,"reference":reference,"at":now})).await?;
        let vault = self.vault.clone();
        let reference = reference.clone();
        let mut result =
            match tokio::task::spawn_blocking(move || vault.with_secret(&reference, operation))
                .await
            {
                Ok(Ok(result)) => result,
                Ok(Err(_)) => ToolResult::error("secret resolution failed"),
                // Do not display JoinError/panic payloads, which may contain values.
                Err(_) => {
                    return Err(Error::Denied(
                        "secret executor interrupted; reconciliation is required".into(),
                    ));
                }
            };
        if result.full_output_blob.is_some() {
            result = ToolResult::error(
                "secret executor requires inline output; unchecked artifact references are rejected",
            );
        }
        result.output = self.security.redactor().redact_json(&result.output);
        self.security
            .mark_untrusted(run, "secret_executor_output", now)
            .await?;
        self.security.finish_dispatch(&ticket, &result).await?;
        Ok(result)
    }
}
