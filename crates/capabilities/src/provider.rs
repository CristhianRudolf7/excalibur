//! Transport-neutral port to a capability. An MCP task client, a native
//! capability or the deterministic simulation implement the same contract.
use serde::{Deserialize, Serialize};
use std::future::Future;

/// Error classification used by §17.7 retry policy.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum FailureClass {
    Transient,
    Input,
    Permanent,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum RemoteState {
    Working,
    InputRequired {
        request: serde_json::Value,
    },
    Completed {
        result: serde_json::Value,
    },
    Failed {
        class: FailureClass,
        message: String,
    },
    Cancelled,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct RemoteStatus {
    pub remote_id: String,
    pub state: RemoteState,
    /// Suggested interval before the next poll; clamped by the harness policy.
    pub poll_interval_ms: Option<u64>,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct StartRequest {
    pub capability: String,
    pub operation: String,
    pub input: serde_json::Value,
    pub idempotency_key: String,
}
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ProviderError {
    /// The call may or may not have reached the remote (timeouts, resets,
    /// overload). Keyed effects are resent with the same key.
    #[error("transient capability error: {0}")]
    Transient(String),
    /// The remote rejected the request without applying it.
    #[error("capability rejected the request: {0}")]
    Rejected(String),
    /// The remote no longer knows this task identifier.
    #[error("capability task is unknown to the remote")]
    UnknownTask,
}

/// Every call that can produce an effect carries an idempotency key. The
/// harness may resend the same key after a crash; implementations must apply
/// the effect at most once per key and return the same task identifier.
pub trait CapabilityProvider: Send + Sync {
    fn start(
        &self,
        request: StartRequest,
    ) -> impl Future<Output = Result<RemoteStatus, ProviderError>> + Send;
    fn poll(
        &self,
        remote_id: &str,
    ) -> impl Future<Output = Result<RemoteStatus, ProviderError>> + Send;
    fn provide_input(
        &self,
        remote_id: &str,
        idempotency_key: &str,
        response: serde_json::Value,
    ) -> impl Future<Output = Result<RemoteStatus, ProviderError>> + Send;
    /// Cooperative: the returned status may still be `working` or even
    /// `completed`; the harness keeps polling for the final state.
    fn cancel(
        &self,
        remote_id: &str,
        idempotency_key: &str,
    ) -> impl Future<Output = Result<RemoteStatus, ProviderError>> + Send;
}
