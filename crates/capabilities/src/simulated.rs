//! Deterministic simulated capability that respects the §14.1 contract
//! (§14.12: capabilities are tested against contract-respecting simulations).
//!
//! The simulation plays the remote side. Its state lives in its own JSON file,
//! separate from the harness database, so it survives a harness crash and a
//! test can count how many times each keyed effect was applied versus
//! received. It never touches the network.
use crate::{
    manifest::{CapabilityManifest, EffectClass, OperationSpec, ResourceLimits, Risk},
    provider::{
        CapabilityProvider, FailureClass, ProviderError, RemoteState, RemoteStatus, StartRequest,
    },
    tasks::TaskState,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    io::Write,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "step", rename_all = "snake_case")]
pub enum SimStep {
    Working,
    InputRequired {
        request: Value,
    },
    Completed {
        result: Value,
    },
    Failed {
        class: FailureClass,
        message: String,
    },
    Cancelled,
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CancelBehavior {
    Honor,
    /// The remote acknowledges the cancel but finishes its script anyway.
    Ignore,
    /// The remote keeps working for `n` more polls, then reports cancelled.
    HonorAfterPolls(u32),
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Script {
    /// Revealed one per poll; `working` steps advance, `input_required` waits
    /// for an answer and terminal steps stay.
    pub steps: Vec<SimStep>,
    pub cancel: CancelBehavior,
    pub poll_interval_ms: Option<u64>,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct SimTask {
    operation: String,
    script: Vec<SimStep>,
    cancel: Option<CancelBehavior>,
    poll_interval_ms: Option<u64>,
    cursor: usize,
    cancel_countdown: Option<u32>,
    cancelled: bool,
    inputs: Vec<Value>,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct SimState {
    next_id: u64,
    scripts: BTreeMap<String, Script>,
    tasks: BTreeMap<String, SimTask>,
    /// Idempotency key -> remote task it applied to.
    keys: BTreeMap<String, String>,
    receipts: BTreeMap<String, u32>,
    effects: BTreeMap<String, u32>,
    polls: u64,
    fail_next: u32,
    lose_next_responses: u32,
}
/// Called after an effect is durably applied and before the response is
/// returned, with the effect kind (`start`, `input`, `cancel`). Crash tests
/// use it to kill the harness at the most dangerous point.
pub type EffectHook = Arc<dyn Fn(&str) + Send + Sync>;

pub struct SimulatedCapability {
    path: PathBuf,
    state: Mutex<SimState>,
    hook: Option<EffectHook>,
}
fn transient(message: &str) -> ProviderError {
    ProviderError::Transient(message.to_string())
}
impl SimulatedCapability {
    /// Opens or creates the simulation state at `path`.
    pub fn open(path: impl AsRef<Path>) -> std::io::Result<Self> {
        let path = path.as_ref().to_path_buf();
        let state = match std::fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes).map_err(std::io::Error::other)?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => SimState::default(),
            Err(e) => return Err(e),
        };
        Ok(Self {
            path,
            state: Mutex::new(state),
            hook: None,
        })
    }
    pub fn with_effect_hook(mut self, hook: EffectHook) -> Self {
        self.hook = Some(hook);
        self
    }
    fn persist(&self, state: &SimState) -> std::io::Result<()> {
        let tmp = self.path.with_extension("tmp");
        let mut file = std::fs::File::create(&tmp)?;
        file.write_all(&serde_json::to_vec(state).map_err(std::io::Error::other)?)?;
        file.sync_all()?;
        std::fs::rename(&tmp, &self.path)
    }
    fn mutate<T>(&self, change: impl FnOnce(&mut SimState) -> T) -> T {
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        let value = change(&mut state);
        self.persist(&state).expect("persist simulated capability");
        value
    }
    fn read<T>(&self, view: impl FnOnce(&SimState) -> T) -> T {
        view(&self.state.lock().unwrap_or_else(|p| p.into_inner()))
    }
    pub fn define(&self, operation: &str, script: Script) {
        self.mutate(|s| {
            s.scripts.insert(operation.to_string(), script);
        });
    }
    /// The next `n` calls fail transiently before reaching the remote logic.
    pub fn fail_next(&self, n: u32) {
        self.mutate(|s| s.fail_next = n);
    }
    /// The next `n` effectful calls apply their effect and then lose the
    /// response, as a timeout after delivery would.
    pub fn lose_next_responses(&self, n: u32) {
        self.mutate(|s| s.lose_next_responses = n);
    }
    /// Times the effect identified by `key` was actually applied.
    pub fn effects(&self, key: &str) -> u32 {
        self.read(|s| s.effects.get(key).copied().unwrap_or(0))
    }
    /// Times a call carrying `key` was received, including deduplicated resends.
    pub fn receipts(&self, key: &str) -> u32 {
        self.read(|s| s.receipts.get(key).copied().unwrap_or(0))
    }
    pub fn total_effects(&self) -> u32 {
        self.read(|s| s.effects.values().sum())
    }
    pub fn started_tasks(&self) -> usize {
        self.read(|s| s.tasks.len())
    }
    pub fn polls(&self) -> u64 {
        self.read(|s| s.polls)
    }
    pub fn inputs(&self, remote_id: &str) -> Vec<Value> {
        self.read(|s| {
            s.tasks
                .get(remote_id)
                .map(|t| t.inputs.clone())
                .unwrap_or_default()
        })
    }
    fn status(remote_id: &str, task: &SimTask) -> RemoteStatus {
        let state = if task.cancelled {
            RemoteState::Cancelled
        } else {
            match &task.script[task.cursor] {
                SimStep::Working => RemoteState::Working,
                SimStep::InputRequired { request } => RemoteState::InputRequired {
                    request: request.clone(),
                },
                SimStep::Completed { result } => RemoteState::Completed {
                    result: result.clone(),
                },
                SimStep::Failed { class, message } => RemoteState::Failed {
                    class: *class,
                    message: message.clone(),
                },
                SimStep::Cancelled => RemoteState::Cancelled,
            }
        };
        RemoteStatus {
            remote_id: remote_id.to_string(),
            state,
            poll_interval_ms: task.poll_interval_ms,
        }
    }
    fn terminal(task: &SimTask) -> bool {
        task.cancelled
            || matches!(
                task.script[task.cursor],
                SimStep::Completed { .. } | SimStep::Failed { .. } | SimStep::Cancelled
            )
    }
    /// Shared keyed-effect path: counts the receipt, applies the effect only
    /// for a new key, persists, runs the hook and may drop the response.
    fn keyed(
        &self,
        key: &str,
        kind: &str,
        apply: impl FnOnce(&mut SimState) -> Result<String, ProviderError>,
    ) -> Result<RemoteStatus, ProviderError> {
        let (outcome, applied, lose) = self.mutate(|s| {
            if s.fail_next > 0 {
                s.fail_next -= 1;
                return (Err(transient("simulated outage")), false, false);
            }
            *s.receipts.entry(key.to_string()).or_default() += 1;
            let (remote, applied) = match s.keys.get(key) {
                Some(remote) => (Ok(remote.clone()), false),
                None => match apply(s) {
                    Ok(remote) => {
                        s.keys.insert(key.to_string(), remote.clone());
                        *s.effects.entry(key.to_string()).or_default() += 1;
                        (Ok(remote), true)
                    }
                    Err(e) => (Err(e), false),
                },
            };
            let lose = remote.is_ok() && s.lose_next_responses > 0;
            if lose {
                s.lose_next_responses -= 1;
            }
            let outcome = remote.map(|r| Self::status(&r, &s.tasks[&r]));
            (outcome, applied, lose)
        });
        if applied && let Some(hook) = &self.hook {
            hook(kind);
        }
        if lose {
            return Err(transient("response lost after delivery"));
        }
        outcome
    }
}
impl CapabilityProvider for SimulatedCapability {
    async fn start(&self, request: StartRequest) -> Result<RemoteStatus, ProviderError> {
        let key = request.idempotency_key.clone();
        self.keyed(&key, "start", |s| {
            let script = s
                .scripts
                .get(&request.operation)
                .cloned()
                .ok_or_else(|| ProviderError::Rejected("unknown operation".into()))?;
            if script.steps.is_empty() {
                return Err(ProviderError::Rejected("empty script".into()));
            }
            s.next_id += 1;
            let remote = format!("sim-task-{}", s.next_id);
            s.tasks.insert(
                remote.clone(),
                SimTask {
                    operation: request.operation.clone(),
                    script: script.steps,
                    cancel: Some(script.cancel),
                    poll_interval_ms: script.poll_interval_ms,
                    ..Default::default()
                },
            );
            Ok(remote)
        })
    }
    async fn poll(&self, remote_id: &str) -> Result<RemoteStatus, ProviderError> {
        self.mutate(|s| {
            if s.fail_next > 0 {
                s.fail_next -= 1;
                return Err(transient("simulated outage"));
            }
            s.polls += 1;
            let task = s
                .tasks
                .get_mut(remote_id)
                .ok_or(ProviderError::UnknownTask)?;
            if let Some(n) = task.cancel_countdown {
                if n == 0 {
                    task.cancelled = true;
                    task.cancel_countdown = None;
                } else {
                    task.cancel_countdown = Some(n - 1);
                }
            }
            let status = Self::status(remote_id, task);
            if !task.cancelled
                && matches!(task.script[task.cursor], SimStep::Working)
                && task.cursor + 1 < task.script.len()
            {
                task.cursor += 1;
            }
            Ok(status)
        })
    }
    async fn provide_input(
        &self,
        remote_id: &str,
        idempotency_key: &str,
        response: Value,
    ) -> Result<RemoteStatus, ProviderError> {
        self.keyed(idempotency_key, "input", |s| {
            let task = s
                .tasks
                .get_mut(remote_id)
                .ok_or(ProviderError::UnknownTask)?;
            if task.cancelled || !matches!(task.script[task.cursor], SimStep::InputRequired { .. })
            {
                return Err(ProviderError::Rejected("no input is expected".into()));
            }
            task.inputs.push(response);
            task.cursor = (task.cursor + 1).min(task.script.len() - 1);
            Ok(remote_id.to_string())
        })
    }
    async fn cancel(
        &self,
        remote_id: &str,
        idempotency_key: &str,
    ) -> Result<RemoteStatus, ProviderError> {
        self.keyed(idempotency_key, "cancel", |s| {
            let task = s
                .tasks
                .get_mut(remote_id)
                .ok_or(ProviderError::UnknownTask)?;
            if !Self::terminal(task) {
                match task.cancel.unwrap_or(CancelBehavior::Honor) {
                    CancelBehavior::Honor => task.cancelled = true,
                    CancelBehavior::Ignore => {}
                    CancelBehavior::HonorAfterPolls(n) => task.cancel_countdown = Some(n),
                }
            }
            Ok(remote_id.to_string())
        })
    }
}

/// Manifest for the simulation: `long_job` has external effects and honours
/// keys; `lookup` is read-only.
pub fn manifest(max_concurrency: u32) -> CapabilityManifest {
    let operation = |name: &str, effect, risk| OperationSpec {
        name: name.into(),
        input_schema: json!({"type": "object"}),
        output_schema: json!({"type": "object"}),
        effect,
        idempotent: true,
        risk,
        long_running: true,
    };
    CapabilityManifest {
        id: "sim.capability".into(),
        version: "1.0.0".into(),
        operations: vec![
            operation("long_job", EffectClass::External, Risk::Medium),
            operation("lookup", EffectClass::ReadOnly, Risk::Low),
        ],
        platforms: vec!["linux".into()],
        deployment_profiles: vec!["local".into()],
        resources: ResourceLimits {
            approx_ram_mb: 64,
            max_sessions: 4,
        },
        max_concurrency,
        states: vec![
            TaskState::Working,
            TaskState::InputRequired,
            TaskState::Completed,
            TaskState::Failed,
            TaskState::Cancelled,
        ],
    }
}
