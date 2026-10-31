//! Capability manifest (§14.1): identity, operations with schemas, effect
//! class, idempotency and risk, platforms, resources, concurrency and the
//! states the capability can report.
use crate::{Error, Result, tasks::TaskState};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum EffectClass {
    /// Reads only; a duplicate call has no external consequence.
    ReadOnly,
    /// Changes local or remote state owned by the user.
    Write,
    /// Acts outside the user's systems (messages, purchases, publications).
    External,
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum Risk {
    Low,
    Medium,
    High,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct OperationSpec {
    pub name: String,
    pub input_schema: serde_json::Value,
    pub output_schema: serde_json::Value,
    pub effect: EffectClass,
    /// Whether the remote honours an idempotency key for this operation. The
    /// harness always sends one; an operation with effects that does not honour
    /// keys is rejected because a resend after a crash could duplicate it.
    pub idempotent: bool,
    pub risk: Risk,
    /// Whether the operation is long-running and reports task states.
    pub long_running: bool,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ResourceLimits {
    pub approx_ram_mb: u32,
    pub max_sessions: u32,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct CapabilityManifest {
    pub id: String,
    pub version: String,
    pub operations: Vec<OperationSpec>,
    pub platforms: Vec<String>,
    pub deployment_profiles: Vec<String>,
    pub resources: ResourceLimits,
    /// Maximum concurrently started, nonterminal tasks for this capability.
    pub max_concurrency: u32,
    /// States the capability may report. `working` and a terminal state are
    /// required; a reported state outside this list is a protocol violation.
    pub states: Vec<TaskState>,
}

fn identifier(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b':'))
}
impl CapabilityManifest {
    pub fn validate(&self) -> Result<()> {
        if !identifier(&self.id) || !identifier(&self.version) {
            return Err(Error::Invalid("capability identity and version"));
        }
        if self.operations.is_empty() || self.operations.len() > 512 {
            return Err(Error::Invalid("capability needs 1..512 operations"));
        }
        let mut names = BTreeSet::new();
        for operation in &self.operations {
            if !identifier(&operation.name) || !names.insert(operation.name.as_str()) {
                return Err(Error::Invalid("unique bounded operation names"));
            }
            if !operation.input_schema.is_object() || !operation.output_schema.is_object() {
                return Err(Error::Invalid("operation schemas must be JSON objects"));
            }
            if operation.effect != EffectClass::ReadOnly && !operation.idempotent {
                return Err(Error::Invalid(
                    "operations with effects must honour idempotency keys",
                ));
            }
        }
        if self.platforms.is_empty()
            || self.platforms.iter().any(|p| !identifier(p))
            || self.deployment_profiles.iter().any(|p| !identifier(p))
        {
            return Err(Error::Invalid("platforms and deployment profiles"));
        }
        if self.max_concurrency == 0 || self.max_concurrency > 1024 {
            return Err(Error::Invalid("max_concurrency must be 1..1024"));
        }
        if self.resources.max_sessions == 0 {
            return Err(Error::Invalid("max_sessions must be positive"));
        }
        let states: BTreeSet<_> = self.states.iter().collect();
        if states.len() != self.states.len()
            || !states.contains(&TaskState::Working)
            || !states.iter().any(|s| s.is_terminal())
        {
            return Err(Error::Invalid(
                "states must be unique and include working and a terminal state",
            ));
        }
        let encoded = serde_json::to_vec(self).map_err(|_| Error::Invalid("manifest encoding"))?;
        if encoded.len() > 1 << 20 {
            return Err(Error::Invalid("manifest exceeds 1 MiB"));
        }
        Ok(())
    }
    pub fn operation(&self, name: &str) -> Option<&OperationSpec> {
        self.operations.iter().find(|o| o.name == name)
    }
    pub fn reports(&self, state: TaskState) -> bool {
        self.states.contains(&state)
    }
}
