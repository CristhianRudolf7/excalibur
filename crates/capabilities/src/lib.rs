//! External capabilities as black boxes (architecture §14.1).
//!
//! This shared-core library provides the capability manifest contract, the
//! durable long-operation task lifecycle (`working`, `input_required`,
//! `completed`, `failed`, `cancelled`), keyed effects, cooperative cancellation
//! that is re-polled, restart-safe polling with backoff driven by an injected
//! clock, the §14.11 outbound disclosure filter and a deterministic simulated
//! provider for tests. It performs no network I/O itself and is not wired into
//! the daemon; MCP/ACP transports and concrete capabilities remain pending.
pub mod disclosure;
pub mod manifest;
pub mod provider;
pub mod simulated;
pub mod tasks;

pub use manifest::{CapabilityManifest, EffectClass, OperationSpec, ResourceLimits, Risk};
pub use provider::{
    CapabilityProvider, FailureClass, ProviderError, RemoteState, RemoteStatus, StartRequest,
};
pub use tasks::{
    CapabilityTask, CapabilityTasks, CancelState, Clock, ExternalOutput, ManualClock, PollPolicy,
    Submission, SystemClock, TaskEvent, TaskFailure, TaskState,
};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("storage: {0}")]
    Store(#[from] excalibur_store::Error),
    #[error("invalid capability input: {0}")]
    Invalid(&'static str),
    #[error("conflict: {0}")]
    Conflict(&'static str),
    #[error("capability task not found")]
    NotFound,
}
pub type Result<T> = std::result::Result<T, Error>;
