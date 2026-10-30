use excalibur_engine::{Budget, RunSpec, Status};
use excalibur_security::CapabilityToken;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{collections::BTreeSet, path::PathBuf};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Limits {
    pub running: usize,
    pub children_per_parent: usize,
    pub ram_bytes: u64,
    pub estimated_run_bytes: u64,
}
impl Default for Limits {
    fn default() -> Self {
        Self {
            running: 8,
            children_per_parent: 4,
            ram_bytes: 4 * 1024 * 1024 * 1024,
            estimated_run_bytes: 64 * 1024 * 1024,
        }
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Agent {
    pub id: String,
    pub parent_id: Option<String>,
    pub run_id: Option<String>,
    pub depth: u8,
    pub name: String,
    pub profile: String,
    #[serde(default)]
    pub profile_id: Option<String>,
    pub scope: String,
    pub task: String,
    pub origin_conversation: String,
    pub token: CapabilityToken,
    pub budget: Budget,
    pub delegation: Option<Delegation>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Delegation {
    pub operation: String,
    pub name: String,
    pub profile: String,
    pub scope: String,
    pub task: String,
    pub origin_conversation: String,
    pub goal: String,
    pub deliverable: String,
    #[serde(default)]
    pub inputs: Vec<String>,
    #[serde(default)]
    pub pinned_instructions: Vec<String>,
    #[serde(default)]
    pub suggested_skills: Vec<String>,
    #[serde(default)]
    pub fork_context: bool,
    pub token: CapabilityToken,
    pub spec: RunSpec,
    #[serde(default)]
    pub priority: u8,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum Control {
    Pause,
    Resume,
    Interrupt,
    Cancel { confirmed: bool },
    Redirect { text: String },
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ControlReport {
    pub operation: String,
    pub affected: Vec<String>,
    pub unchanged: Vec<String>,
    pub pending: Vec<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AgentStatus {
    pub agent: Agent,
    pub state: String,
    pub reason: Option<String>,
    pub queue_position: Option<usize>,
    pub step: u64,
    pub model_in_flight: bool,
    pub tools: Vec<Value>,
    pub last_progress: Option<Value>,
    pub last_report: Option<Value>,
    pub plan: Value,
    pub usage: Value,
    pub pending_messages: usize,
    pub report_reminder: bool,
}
pub(crate) fn label(s: &Status) -> &'static str {
    match s {
        Status::Created => "created",
        Status::Ready => "ready",
        Status::Running => "running",
        Status::Waiting => "waiting",
        Status::Verifying => "verifying",
        Status::Recovering => "recovering",
        Status::Paused => "paused",
        Status::Blocked => "blocked",
        Status::Completed => "completed",
        Status::CompletedWithPending => "completed_with_pending",
        Status::Cancelled => "cancelled",
        Status::Failed => "failed",
    }
}
pub(crate) fn terminal(s: &Status) -> bool {
    matches!(
        s,
        Status::Completed | Status::CompletedWithPending | Status::Cancelled | Status::Failed
    )
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Project {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub aliases: Vec<String>,
    #[serde(default)]
    pub description: String,
    pub roots: Vec<PathBuf>,
    pub status: String,
    pub vcs: Option<String>,
    #[serde(default)]
    pub settings: Value,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ProjectResolution {
    pub chosen: Option<String>,
    pub candidates: Vec<String>,
    pub canonical_path: PathBuf,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AgentMessage {
    pub id: String,
    pub client_id: String,
    pub sender: String,
    pub recipient: String,
    pub kind: String,
    pub payload: Value,
    pub priority: String,
    pub created: i64,
    pub step: u64,
    pub state: String,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct DirectiveImpact {
    pub scopes: BTreeSet<String>,
    pub tasks: BTreeSet<String>,
    pub resources: BTreeSet<String>,
    pub stop: bool,
    pub prohibits_current_action: bool,
    pub instruction_revision: u64,
    pub text: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct NotificationPolicy {
    pub preferred_channel: Option<String>,
    pub progress: Option<bool>,
    pub quiet_start_minute: Option<u16>,
    pub quiet_end_minute: Option<u16>,
    pub utc_offset_minutes: i32,
    pub urgent_during_quiet: bool,
}
impl Default for NotificationPolicy {
    fn default() -> Self {
        Self {
            preferred_channel: None,
            progress: None,
            quiet_start_minute: None,
            quiet_end_minute: None,
            utc_offset_minutes: 0,
            urgent_during_quiet: true,
        }
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Notification {
    pub id: String,
    pub agent_id: String,
    pub conversation: String,
    pub class: String,
    pub content: String,
    pub created: i64,
    pub acknowledged: bool,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Delivery {
    pub notification: Notification,
    pub channel: String,
    pub cross_conversation: Option<String>,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ResourceAction {
    pub admit: bool,
    pub unload_idle: bool,
    pub pause: Vec<String>,
    pub resume: Vec<String>,
    pub last_resort: Option<String>,
}
