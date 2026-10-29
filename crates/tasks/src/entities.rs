use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeSet;
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum WorkLevel {
    N1,
    N2,
    N3,
}
impl WorkLevel {
    pub fn label(self) -> &'static str {
        match self {
            Self::N1 => "N1",
            Self::N2 => "N2",
            Self::N3 => "N3",
        }
    }
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct TaskRecord {
    pub id: String,
    pub project_id: Option<String>,
    pub scope_id: String,
    pub code: String,
    pub title: String,
    pub detail: String,
    pub level: WorkLevel,
    pub root_run_id: Option<String>,
    pub derived_state: String,
    pub version: u64,
    pub request_card: Value,
    pub budget: Value,
    pub origin_conversation_id: Option<String>,
    pub notification_policy: Option<String>,
    pub macro_item_id: Option<String>,
    pub forced_level: bool,
    pub estimate_seconds: Option<u64>,
    pub metadata: Value,
}
#[derive(Clone, Debug)]
pub struct TaskPolicy {
    pub allow_projects: bool,
    pub allowed_scopes: Option<BTreeSet<String>>,
    pub principal_id: Option<String>,
    pub autonomy: u8,
    pub trusted_host: bool,
}
impl Default for TaskPolicy {
    fn default() -> Self {
        Self {
            allow_projects: true,
            allowed_scopes: None,
            principal_id: None,
            autonomy: 2,
            trusted_host: false,
        }
    }
}
impl TaskPolicy {
    pub fn episode(scope: impl Into<String>) -> Self {
        Self {
            allow_projects: false,
            allowed_scopes: Some([scope.into()].into_iter().collect()),
            autonomy: 3,
            ..Default::default()
        }
    }
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct PromotionInput {
    pub touched_project: Option<String>,
    pub steps: u64,
    pub estimated_seconds: Option<u64>,
    pub milestone_count: u64,
    pub user_requests_initiative: bool,
    pub forced_level: Option<WorkLevel>,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ControlReceipt {
    pub id: String,
    pub kind: String,
    pub task_id: String,
    pub runs: Vec<String>,
    pub confirmation_required: bool,
    #[serde(default)]
    pub deferred_runs: Vec<String>,
    pub applied: bool,
}
