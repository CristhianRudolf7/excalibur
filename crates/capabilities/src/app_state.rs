//! Contextual-application state deltas (§14.8).
//!
//! Each application with its own interface is a capability with an
//! `app_state(instance)` provider. The host polls the provider, keeps the
//! last snapshot per `app:<id>` conversation scope and includes only the
//! delta in L3 when the state changes. Manual user edits and agent edits
//! converge through the same revision counter; the application's own rules
//! are exposed as acceptance checks that the harness must respect before
//! marking work complete.

use crate::{Error, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

const MAX_INSTANCE: usize = 128;
const MAX_JSON: usize = 1 << 20;

/// Compact snapshot of one application instance at one revision.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct AppSnapshot {
    pub instance: String,
    pub rev: u64,
    pub state: Value,
}

impl AppSnapshot {
    /// Builds the first snapshot for an instance. `state` must be a JSON
    /// object within the shared 1 MiB bound.
    pub fn initial(instance: &str, state: Value) -> Result<Self> {
        check_instance(instance)?;
        check_state(&state)?;
        Ok(Self {
            instance: instance.to_string(),
            rev: 0,
            state,
        })
    }

    /// Returns the successor snapshot after either a manual user change or
    /// an agent operation. Both converge through `rev + 1`; the caller
    /// records which side made the change in its own audit log.
    pub fn next(&self, state: Value) -> Result<Self> {
        check_state(&state)?;
        let rev = self
            .rev
            .checked_add(1)
            .ok_or(Error::Invalid("app revision overflow"))?;
        Ok(Self {
            instance: self.instance.clone(),
            rev,
            state,
        })
    }
}

/// Top-level difference between two snapshots of the same instance.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct AppDelta {
    pub instance: String,
    pub from_rev: u64,
    pub to_rev: u64,
    /// New or changed top-level fields, by key.
    pub changed: BTreeMap<String, Value>,
    /// Top-level keys present before and absent now.
    pub removed: Vec<String>,
}

impl AppDelta {
    /// True when the two snapshots hold identical top-level content.
    pub fn is_empty(&self) -> bool {
        self.changed.is_empty() && self.removed.is_empty()
    }

    /// Compact L3 summary: instance, revisions and the sorted changed and
    /// removed key names. Values stay in [`AppDelta::changed`] for hosts
    /// that need them; this summary never carries document bodies beyond
    /// the keys the caller already holds in the snapshots.
    pub fn summary(&self) -> Value {
        serde_json::json!({
            "instance": self.instance,
            "from_rev": self.from_rev,
            "to_rev": self.to_rev,
            "changed_keys": self.changed.keys().cloned().collect::<Vec<_>>(),
            "removed_keys": self.removed,
        })
    }
}

/// Computes the L3 delta between two snapshots. Requires the same instance
/// and a forward revision; anything else is a caller error.
pub fn delta(before: &AppSnapshot, after: &AppSnapshot) -> Result<AppDelta> {
    if before.instance != after.instance {
        return Err(Error::Invalid("app delta spans instances"));
    }
    if after.rev < before.rev {
        return Err(Error::Invalid("app delta moves backwards"));
    }
    let before_map = as_object(&before.state)?;
    let after_map = as_object(&after.state)?;
    let mut changed = BTreeMap::new();
    for (key, value) in &after_map {
        if before_map.get(key) != Some(value) {
            changed.insert(key.clone(), value.clone());
        }
    }
    let mut removed = Vec::new();
    for key in before_map.keys() {
        if !after_map.contains_key(key) {
            removed.push(key.clone());
        }
    }
    Ok(AppDelta {
        instance: after.instance.clone(),
        from_rev: before.rev,
        to_rev: after.rev,
        changed,
        removed,
    })
}

/// One application rule exposed as an acceptance check (for example the
/// classroom quiz rule: every answer must be correct). The application owns
/// the evaluation; the harness only enforces the verdict.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct AcceptanceCheck {
    pub id: String,
    pub description: String,
    pub required: bool,
    pub passed: bool,
}

impl AcceptanceCheck {
    pub fn new(id: &str, description: &str, required: bool, passed: bool) -> Result<Self> {
        if id.is_empty() || id.len() > MAX_INSTANCE || description.len() > 1024 {
            return Err(Error::Invalid("bounded acceptance check identity"));
        }
        Ok(Self {
            id: id.to_string(),
            description: description.to_string(),
            required,
            passed,
        })
    }
}

/// True when every required check passes. Optional checks never block.
pub fn gate_allows(checks: &[AcceptanceCheck]) -> bool {
    gate_blockers(checks).is_empty()
}

/// Ids of the required checks that currently fail, in caller order.
pub fn gate_blockers(checks: &[AcceptanceCheck]) -> Vec<String> {
    checks
        .iter()
        .filter(|c| c.required && !c.passed)
        .map(|c| c.id.clone())
        .collect()
}

/// Sorted union of required check ids; useful for stable audit lines.
pub fn required_ids(checks: &[AcceptanceCheck]) -> Vec<String> {
    let mut ids: BTreeSet<String> = BTreeSet::new();
    for check in checks.iter().filter(|c| c.required) {
        ids.insert(check.id.clone());
    }
    ids.into_iter().collect()
}

fn check_instance(instance: &str) -> Result<()> {
    if instance.is_empty() || instance.len() > MAX_INSTANCE {
        return Err(Error::Invalid("bounded application instance"));
    }
    Ok(())
}

fn check_state(state: &Value) -> Result<()> {
    if !state.is_object() {
        return Err(Error::Invalid("application state is a JSON object"));
    }
    let len = serde_json::to_vec(state)
        .map(|v| v.len())
        .unwrap_or(MAX_JSON + 1);
    if len > MAX_JSON {
        return Err(Error::Invalid("application state exceeds 1 MiB"));
    }
    Ok(())
}

fn as_object(state: &Value) -> Result<BTreeMap<String, Value>> {
    match state {
        Value::Object(map) => Ok(map.iter().map(|(k, v)| (k.clone(), v.clone())).collect()),
        _ => Err(Error::Invalid("application state is a JSON object")),
    }
}
