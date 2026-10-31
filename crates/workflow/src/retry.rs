//! Failure classes and per-class retry policy (architecture 17.7 applied per node, 21.5).
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Failure class reported by the effector for one node attempt.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum FailureClass {
    /// Provider 5xx/429, network down, capacity timeout: exponential backoff with jitter.
    Transient,
    /// Invalid arguments, missing resource: no blind retry, returned as the result.
    Input,
    /// Failing tests, unmet criterion: no automatic retry; bounded corrections then replanning.
    Logical,
    /// Revoked credential, permission denied: pause and notify the user.
    Permanent,
}
impl FailureClass {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Transient => "transient",
            Self::Input => "input",
            Self::Logical => "logical",
            Self::Permanent => "permanent",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "transient" => Self::Transient,
            "input" => Self::Input,
            "logical" => Self::Logical,
            "permanent" => Self::Permanent,
            _ => return None,
        })
    }
}

/// Per-node policy. Defaults follow 17.7: 30 s, 2 min, 10 min, 1 h; a `Retry-After`
/// hint lengthens a wait up to `max_retry_after`; three logical corrections.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields, default)]
pub struct RetryPolicy {
    /// One delay per automatic transient retry, in seconds.
    pub transient_delays: Vec<u64>,
    /// Upper bound applied to a provider `Retry-After` hint, in seconds.
    pub max_retry_after: u64,
    /// Positive jitter as a fraction (per mille) of each base delay.
    pub jitter_per_mille: u64,
    /// Logical failures of one node after which replanning is required (8.5).
    pub logical_attempts: u32,
}
impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            transient_delays: vec![30, 120, 600, 3600],
            max_retry_after: 3600,
            jitter_per_mille: 100,
            logical_attempts: 3,
        }
    }
}
impl RetryPolicy {
    pub fn validate(&self) -> Result<(), String> {
        if self.transient_delays.len() > 16
            || self.transient_delays.iter().any(|d| *d > 7 * 24 * 3600)
            || self.max_retry_after > 7 * 24 * 3600
            || self.jitter_per_mille > 1000
            || self.logical_attempts > 16
        {
            return Err("retry policy exceeds its bounds".into());
        }
        Ok(())
    }

    /// Delay before transient retry number `retry` (1-based), or `None` when exhausted.
    /// Jitter is deterministic from the idempotency key so replays compute the same wait.
    pub fn transient_delay(&self, key: &str, retry: u32, retry_after: Option<u64>) -> Option<u64> {
        let base = *self.transient_delays.get(retry.checked_sub(1)? as usize)?;
        let span = base.saturating_mul(self.jitter_per_mille) / 1000;
        let jitter = if span == 0 {
            0
        } else {
            let digest = Sha256::digest(format!("{key}\u{0}{retry}").as_bytes());
            u64::from_le_bytes(digest[..8].try_into().unwrap_or_default()) % (span + 1)
        };
        let hinted = retry_after.unwrap_or(0).min(self.max_retry_after);
        Some((base + jitter).max(hinted))
    }
}
