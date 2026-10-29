use serde::{Deserialize, Serialize};
use serde_json::Value;
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum EventKind {
    RunStarted,
    StateChanged,
    ModelRequested,
    ModelDispatched,
    ModelCompleted,
    ToolStarted,
    ToolCompleted,
    Approval,
    Directive,
    Gate,
    MemoryWrite,
    Compaction,
    LoopDetected,
    Masking,
    Retry,
    Error,
    FileEdited,
    ResourceSample,
    VerifierResult,
    Checkpoint,
    Other,
}
impl EventKind {
    pub fn for_journal(kind: &str) -> Self {
        match kind {
            "run_started" => Self::RunStarted,
            "state_change" => Self::StateChanged,
            "model_request" => Self::ModelRequested,
            "model_dispatch" | "context_summary_started" => Self::ModelDispatched,
            "model_response" | "context_summary_completed" => Self::ModelCompleted,
            "model_failure" => Self::Error,
            "verifier_result" => Self::VerifierResult,
            "tool_intent" => Self::ToolStarted,
            "tool_result" => Self::ToolCompleted,
            "gate_report" => Self::Gate,
            "checkpoint" | "step_completed" => Self::Checkpoint,
            k if k.starts_with("approval") || k == "permission_requested" => Self::Approval,
            k if k.starts_with("directive") => Self::Directive,
            k if k.starts_with("memory_write") => Self::MemoryWrite,
            k if k.contains("compact") => Self::Compaction,
            k if k.contains("loop_detect") => Self::LoopDetected,
            k if k.contains("mask") => Self::Masking,
            k if k.contains("retry") => Self::Retry,
            k if k.contains("error") || k.ends_with("_failed") => Self::Error,
            "file_edit" | "file_edited" => Self::FileEdited,
            _ => Self::Other,
        }
    }
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct FileEditRecord {
    pub path: Option<String>,
    pub diff: String,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct EventEnvelope {
    pub schema_version: String,
    pub cursor: u64,
    pub run_id: String,
    pub sequence: u64,
    pub source: String,
    pub journal_kind: String,
    pub kind: EventKind,
    pub key: String,
    pub timestamp_unix_ms: i64,
    /// Backfilled records lack live event timing; consumers must not infer it.
    pub recovered: bool,
    pub payload: Value,
    pub file_edits: Vec<FileEditRecord>,
}
pub(crate) fn file_edits(payload: &Value) -> Vec<FileEditRecord> {
    fn visit(value: &Value, output: &mut Vec<FileEditRecord>) {
        match value {
            Value::Object(map) => {
                if let Some(diff) = map.get("diff").and_then(Value::as_str) {
                    output.push(FileEditRecord {
                        path: map.get("path").and_then(Value::as_str).map(str::to_owned),
                        diff: diff.into(),
                    });
                }
                for (key, value) in map {
                    if key != "diff" {
                        visit(value, output);
                    }
                }
            }
            Value::Array(items) => {
                for item in items {
                    visit(item, output);
                }
            }
            _ => {}
        }
    }
    let mut output = Vec::new();
    visit(payload, &mut output);
    output
}
