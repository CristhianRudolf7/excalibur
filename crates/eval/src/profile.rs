//! Versioned evaluation policy. Availability is derived from actual adapters;
//! policy permission never fabricates an implementation.
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ProfileOverrides {
    pub language: Option<String>,
    pub model_roles: BTreeMap<String, String>,
    pub task_skills_dir: Option<PathBuf>,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct FrozenProfile {
    pub version: String,
    pub language: String,
    pub timezone: String,
    pub max_active_children: usize,
    pub overrides: Option<ProfileOverrides>,
    pub policy: Value,
}
pub fn hash<T: Serialize>(value: &T) -> String {
    format!(
        "{:x}",
        Sha256::digest(serde_json::to_vec(value).expect("serializable profile"))
    )
}
impl FrozenProfile {
    pub fn evaluation(cpu: usize, overrides: Option<ProfileOverrides>) -> Result<Self, String> {
        if cpu == 0 {
            return Err("task CPU count must be positive".into());
        }
        if let Some(o) = &overrides {
            if o.language
                .as_ref()
                .is_some_and(|s| s.trim().is_empty() || s.len() > 32)
            {
                return Err("invalid declared language override".into());
            }
            if o.model_roles.iter().any(|(k, v)| {
                !matches!(k.as_str(), "coordinator" | "worker" | "verifier") || v.trim().is_empty()
            }) {
                return Err("invalid model role override".into());
            }
            if o.task_skills_dir
                .as_ref()
                .is_some_and(|p| p != Path::new("/runtime/skills"))
            {
                return Err(
                    "task skill override must use the reserved /runtime/skills mount".into(),
                );
            }
        }
        Ok(Self {
            version: "eval@1".into(),
            language: overrides
                .as_ref()
                .and_then(|o| o.language.clone())
                .unwrap_or_else(|| "en".into()),
            timezone: "UTC".into(),
            max_active_children: cpu.min(2),
            overrides,
            policy: json!({
                "task_planning":"episode_only", "projects":"deny", "children":"bounded_episode_only", "file_terminal_tools":"enabled", "coding_and_media_adapters":"deferred_if_registered", "web":"task_network_policy_and_benchmark_source_block", "persistent_memory":"deny", "user_identity":"deny", "conversation_and_approvals":"deny", "agenda_and_workflows":"deny", "personal_external_capabilities":"deny", "skills":"frozen_builtin_and_explicit_task_mount_only", "model_roles":"single_model_unless_declared", "decisor_and_expert":"deny", "skill_learning_and_autopoiesis":"deny", "local_inference":"deny", "telemetry_atif_metrics":"enabled", "result_upload":"deny"
            }),
        })
    }
    pub fn validate_stores(
        &self,
        memory: Option<&Path>,
        skills: Option<&Path>,
        credentials: Option<&Path>,
    ) -> Result<(), String> {
        if memory.is_some() || credentials.is_some() {
            return Err("eval forbids external memory and credential stores; inject the model credential through the host environment".into());
        }
        if let Some(skills) = skills
            && self
                .overrides
                .as_ref()
                .and_then(|o| o.task_skills_dir.as_deref())
                != Some(skills)
        {
            return Err("task skill store requires a declared profile override".into());
        }
        Ok(())
    }
    pub fn allows_tool(&self, name: &str) -> bool {
        matches!(
            name,
            "read_file"
                | "write_file"
                | "edit_file"
                | "apply_patch"
                | "exec"
                | "exec_io"
                | "task_db"
                | "history_search"
                | "get_tool_output"
                | "pin_observation"
                | "leave_continuation_note"
                | "ack_directive"
                | "status"
                | "clock"
                | "search_skills"
                | "load_skill"
                | "search_tools"
                | "call_tool"
                | "subagent"
                | "glob"
                | "grep"
                | "lsp"
                | "code_graph"
                | "file_history"
                | "revert_edit"
                | "workspace_snapshot"
                | "worktree"
                | "integration_queue"
                | "document_read"
                | "pdf_ops"
                | "file_convert"
                | "diagram_render"
                | "chart_render"
                | "vision_extract"
                | "web_search"
                | "web_read"
                | "source_search"
        )
    }
}
/// Shared benchmark-origin guard for registered web adapters. Process egress is
/// controlled separately by the externally declared container network policy.
pub fn benchmark_source_blocked(url: &str) -> bool {
    let value = url.to_ascii_lowercase();
    [
        "tbench.ai",
        "harborframework.com",
        "harbor-framework/harbor",
        "harbor-framework/terminal-bench",
        "stanfordnlp/terminal-bench",
        "laude-institute/terminal-bench",
        "terminal-bench/terminal-bench",
        "harbor hub",
        "terminal-bench-2-1",
    ]
    .iter()
    .any(|needle| value.contains(needle))
}
