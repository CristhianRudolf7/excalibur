#![allow(dead_code)]
use excalibur_store::{Database, DatabaseKind};
use excalibur_workflow::{
    Clock, Compensation, Effect, EffectRequest, Effector, ManualClock, Node, NodeKind, RetryPolicy,
    StepFailure, WorkflowDefinition, Workflows,
};
use futures::future::BoxFuture;
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, VecDeque},
    path::Path,
    sync::{Arc, Mutex},
};

pub const SCOPE: &str = "principal:fixture";

pub fn tool(id: &str, effect: Effect, next: Option<&str>, compensate: bool) -> Node {
    Node {
        id: id.into(),
        kind: NodeKind::Tool {
            tool: format!("tool_{id}"),
            input: json!({"node": id}),
            effect,
            compensation: compensate.then(|| Compensation {
                tool: format!("undo_{id}"),
                input: json!({"undo": id}),
                dry_run: Some(json!({"simulated_undo": id})),
            }),
            dry_run: Some(json!({"simulated": id})),
            retry: RetryPolicy::default(),
            next: next.map(Into::into),
        },
    }
}
pub fn approval(id: &str, next: Option<&str>) -> Node {
    Node {
        id: id.into(),
        kind: NodeKind::Approval {
            prompt: format!("approve {id}?"),
            next: next.map(Into::into),
        },
    }
}
pub fn definition(id: &str, version: u32, nodes: Vec<Node>) -> WorkflowDefinition {
    WorkflowDefinition {
        id: id.into(),
        version,
        start: nodes[0].id.clone(),
        nodes,
    }
}

pub async fn open(path: &Path, clock: Arc<dyn Clock>) -> (Database, Workflows) {
    let db = Database::open(path, DatabaseKind::Main, Default::default()).unwrap();
    let workflows = Workflows::open(db.clone(), SCOPE.into(), clock)
        .await
        .unwrap();
    (db, workflows)
}
pub fn clock(at: i64) -> Arc<ManualClock> {
    Arc::new(ManualClock::new(at))
}

/// In-memory effector: records every call, deduplicates applied effects by key
/// (keyed semantics) and replays scripted failures per node.
#[derive(Default)]
pub struct Recorder {
    pub calls: Mutex<Vec<EffectRequest>>,
    pub applied: Mutex<Vec<(String, String)>>,
    pub script: Mutex<BTreeMap<String, VecDeque<Result<Value, StepFailure>>>>,
}
impl Recorder {
    pub fn fail(&self, node: &str, results: Vec<Result<Value, StepFailure>>) {
        self.script
            .lock()
            .unwrap()
            .insert(node.into(), results.into());
    }
    pub fn calls(&self) -> Vec<EffectRequest> {
        self.calls.lock().unwrap().clone()
    }
    pub fn applied_tools(&self) -> Vec<String> {
        self.applied
            .lock()
            .unwrap()
            .iter()
            .map(|(_, t)| t.clone())
            .collect()
    }
}
impl Effector for Recorder {
    fn execute<'a>(
        &'a self,
        request: &'a EffectRequest,
    ) -> BoxFuture<'a, Result<Value, StepFailure>> {
        Box::pin(async move {
            self.calls.lock().unwrap().push(request.clone());
            let script_key = if request.compensation {
                format!("{}:compensate", request.node_id)
            } else {
                request.node_id.clone()
            };
            if let Some(next) = self
                .script
                .lock()
                .unwrap()
                .get_mut(&script_key)
                .and_then(VecDeque::pop_front)
            {
                next?;
            }
            let mut applied = self.applied.lock().unwrap();
            if !applied.iter().any(|(k, _)| k == &request.idempotency_key) {
                applied.push((request.idempotency_key.clone(), request.tool.clone()));
            }
            Ok(json!({"applied": request.tool}))
        })
    }
}
