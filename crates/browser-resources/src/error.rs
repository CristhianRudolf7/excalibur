//! Error type for the standalone browser resource foundation.

use std::fmt;

/// Errors returned by the resource foundation.
///
/// The wire codes (`capacity_exceeded`, `listener_cannot_suspend`) match the
/// architecture so the main lane can reuse them without renaming.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResourceError {
    /// Caller supplied an invalid value (empty origin, unknown profile...).
    InvalidInput(String),
    /// A budget check refused admission; carries the human-readable breakdown.
    BudgetExceeded(String),
    /// No room after the eviction order; carries the memory breakdown.
    CapacityExceeded(String),
    /// `browser_suspend_session` on a listening session without approval.
    ListenerCannotSuspend,
    /// A disk-growing operation would breach the 5 GiB free reserve.
    DiskReserve(String),
}

impl fmt::Display for ResourceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidInput(detail) => write!(f, "invalid_input: {detail}"),
            Self::BudgetExceeded(detail) => write!(f, "budget_exceeded: {detail}"),
            Self::CapacityExceeded(detail) => write!(f, "capacity_exceeded: {detail}"),
            Self::ListenerCannotSuspend => write!(f, "listener_cannot_suspend"),
            Self::DiskReserve(detail) => write!(f, "disk_reserve: {detail}"),
        }
    }
}

impl std::error::Error for ResourceError {}

/// Local result alias.
pub type Result<T> = std::result::Result<T, ResourceError>;
