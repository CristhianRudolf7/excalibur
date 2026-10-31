//! Versioned workflow definitions: a validated acyclic graph of typed nodes.
use crate::retry::RetryPolicy;
use excalibur_types::runtime::Effect;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};

pub const MAX_NODES: usize = 256;
const MAX_DEFINITION_BYTES: usize = 256 * 1024;

/// One immutable published version. Instances are pinned to `(id, version, hash)`.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct WorkflowDefinition {
    pub id: String,
    pub version: u32,
    pub start: String,
    pub nodes: Vec<Node>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Node {
    pub id: String,
    #[serde(flatten)]
    pub kind: NodeKind,
}

/// The node kinds implemented by this crate. Agent steps, sandboxed scripts,
/// event waits, parallel branches, bounded loops and subflows (21.4) are pending.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum NodeKind {
    /// Calls a host tool through the [`crate::Effector`] with a per-node idempotency key.
    Tool {
        tool: String,
        #[serde(default)]
        input: Value,
        effect: Effect,
        /// Undo step run in reverse order when the instance aborts (saga).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        compensation: Option<Compensation>,
        /// Declared `dry_run` behaviour: the simulated output used by dry runs.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        dry_run: Option<Value>,
        #[serde(default)]
        retry: RetryPolicy,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        next: Option<String>,
    },
    /// Compares the value at a JSON pointer into `{"input":…, "outputs":{node:…}}`.
    Condition {
        pointer: String,
        equals: Value,
        then: String,
        otherwise: String,
    },
    /// Waits durably for a user decision. Rejection aborts and compensates.
    Approval {
        prompt: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        next: Option<String>,
    },
    /// Waits durably until the injected clock reaches the recorded wake time.
    Wait {
        seconds: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        next: Option<String>,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Compensation {
    pub tool: String,
    #[serde(default)]
    pub input: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dry_run: Option<Value>,
}

fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-' || b == b'.')
}

impl NodeKind {
    pub fn successors(&self) -> Vec<&str> {
        match self {
            NodeKind::Tool { next, .. }
            | NodeKind::Approval { next, .. }
            | NodeKind::Wait { next, .. } => next.iter().map(String::as_str).collect(),
            NodeKind::Condition {
                then, otherwise, ..
            } => vec![then.as_str(), otherwise.as_str()],
        }
    }
    pub fn type_name(&self) -> &'static str {
        match self {
            NodeKind::Tool { .. } => "tool",
            NodeKind::Condition { .. } => "condition",
            NodeKind::Approval { .. } => "approval",
            NodeKind::Wait { .. } => "wait",
        }
    }
}

impl WorkflowDefinition {
    pub fn node(&self, id: &str) -> Option<&Node> {
        self.nodes.iter().find(|n| n.id == id)
    }

    /// Canonical serialized form and its SHA-256, used to pin instances.
    pub fn canonical(&self) -> Result<(String, String), String> {
        let text = serde_json::to_string(self).map_err(|e| e.to_string())?;
        let hash = format!("{:x}", Sha256::digest(text.as_bytes()));
        Ok((text, hash))
    }

    /// Structural validation: bounded IDs, resolvable references, every node
    /// reachable from `start`, no cycles (bounded loops are pending) and saga
    /// compensations only on effecting tools.
    pub fn validate(&self) -> Result<(), String> {
        if !valid_id(&self.id) {
            return Err("workflow id must be 1..64 of [A-Za-z0-9_.-]".into());
        }
        if self.version == 0 {
            return Err("workflow version must be positive".into());
        }
        if self.nodes.is_empty() || self.nodes.len() > MAX_NODES {
            return Err(format!("a workflow needs 1..{MAX_NODES} nodes"));
        }
        let mut by_id = BTreeMap::new();
        for node in &self.nodes {
            if !valid_id(&node.id) {
                return Err(format!("invalid node id {:?}", node.id));
            }
            if by_id.insert(node.id.as_str(), node).is_some() {
                return Err(format!("duplicate node id {}", node.id));
            }
            match &node.kind {
                NodeKind::Tool {
                    tool,
                    effect,
                    compensation,
                    retry,
                    ..
                } => {
                    if tool.is_empty() || tool.len() > 128 {
                        return Err(format!("node {} needs a tool name", node.id));
                    }
                    if compensation.is_some() && *effect == Effect::Pure {
                        return Err(format!(
                            "node {} is pure and cannot declare a compensation",
                            node.id
                        ));
                    }
                    if let Some(c) = compensation
                        && (c.tool.is_empty() || c.tool.len() > 128)
                    {
                        return Err(format!("node {} compensation needs a tool", node.id));
                    }
                    retry.validate()?;
                }
                NodeKind::Condition { pointer, .. } => {
                    if !(pointer.starts_with("/input") || pointer.starts_with("/outputs/")) {
                        return Err(format!(
                            "condition {} must point into /input or /outputs/",
                            node.id
                        ));
                    }
                }
                NodeKind::Approval { prompt, .. } => {
                    if prompt.is_empty() || prompt.len() > 4096 {
                        return Err(format!("approval {} needs a bounded prompt", node.id));
                    }
                }
                NodeKind::Wait { seconds, .. } => {
                    if *seconds > 366 * 24 * 3600 {
                        return Err(format!("wait {} exceeds one year", node.id));
                    }
                }
            }
        }
        for node in &self.nodes {
            for next in node.kind.successors() {
                if !by_id.contains_key(next) {
                    return Err(format!("node {} references unknown node {next}", node.id));
                }
            }
        }
        if !by_id.contains_key(self.start.as_str()) {
            return Err("start node does not exist".into());
        }
        // Depth-first search from start: reachability and cycle detection.
        let mut state: BTreeMap<&str, u8> = BTreeMap::new();
        let mut stack = vec![(self.start.as_str(), 0usize)];
        state.insert(self.start.as_str(), 1);
        while let Some((id, index)) = stack.pop() {
            let successors = by_id[id].kind.successors();
            if let Some(next) = successors.get(index) {
                stack.push((id, index + 1));
                match state.get(next) {
                    Some(1) => return Err(format!("cycle through node {next}")),
                    Some(_) => {}
                    None => {
                        state.insert(next, 1);
                        stack.push((next, 0));
                    }
                }
            } else {
                state.insert(id, 2);
            }
        }
        let unreachable: BTreeSet<_> = by_id.keys().filter(|k| !state.contains_key(*k)).collect();
        if !unreachable.is_empty() {
            return Err(format!("unreachable nodes: {unreachable:?}"));
        }
        let (text, _) = self.canonical()?;
        if text.len() > MAX_DEFINITION_BYTES {
            return Err("definition exceeds 256 KiB".into());
        }
        Ok(())
    }

    /// Nodes-and-connections JSON for an n8n-style drawing.
    pub fn graph(&self) -> Value {
        let nodes: Vec<Value> = self
            .nodes
            .iter()
            .map(|n| {
                let mut v = serde_json::to_value(n).unwrap_or(Value::Null);
                if let Some(o) = v.as_object_mut() {
                    o.remove("next");
                    o.remove("then");
                    o.remove("otherwise");
                }
                v
            })
            .collect();
        let mut connections = Vec::new();
        for n in &self.nodes {
            match &n.kind {
                NodeKind::Condition {
                    then, otherwise, ..
                } => {
                    connections.push(json!({"from": n.id, "to": then, "when": true}));
                    connections.push(json!({"from": n.id, "to": otherwise, "when": false}));
                }
                kind => {
                    for to in kind.successors() {
                        connections.push(json!({"from": n.id, "to": to}));
                    }
                }
            }
        }
        json!({"id": self.id, "version": self.version, "start": self.start,
               "nodes": nodes, "connections": connections})
    }

    /// One-line-per-node textual flow (`id [type] -> succ, ...`, `(end)` when
    /// terminal). Small simulation aid; the JSON `graph` feeds drawings.
    pub fn describe(&self) -> String {
        let mut out = format!(
            "workflow {} v{} start {}\n",
            self.id, self.version, self.start
        );
        for node in &self.nodes {
            let succ = node.kind.successors();
            let targets = if succ.is_empty() {
                "(end)".to_string()
            } else {
                succ.join(", ")
            };
            out.push_str(&format!(
                "{} [{}] -> {}\n",
                node.id,
                node.kind.type_name(),
                targets
            ));
        }
        out
    }
}
