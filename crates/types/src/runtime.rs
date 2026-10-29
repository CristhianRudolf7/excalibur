//! Provider-neutral durable execution contracts. Host adapters own permissions and sandboxes.
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{future::Future, pin::Pin};
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: String,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Message {
    pub role: String,
    pub content: String,
    #[serde(default)]
    pub tool_calls: Vec<ToolCall>,
    #[serde(default)]
    pub tool_call_id: Option<String>,
}
impl Message {
    pub fn text(role: &str, content: impl Into<String>) -> Self {
        Self {
            role: role.into(),
            content: content.into(),
            tool_calls: Vec::new(),
            tool_call_id: None,
        }
    }
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ModelRequest {
    pub messages: Vec<Message>,
    pub tools: Vec<ToolDefinition>,
    pub tools_enabled: bool,
    pub reasoning_effort: Option<String>,
    pub output_schema: Option<Value>,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cost_microusd: u64,
    /// True when token counts include a conservative local estimate rather than
    /// complete provider-reported usage. Old durable records default to false.
    #[serde(default)]
    pub estimated: bool,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ModelResponse {
    pub message: Message,
    pub usage: Usage,
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Effect {
    Pure,
    Idempotent,
    Keyed,
    NonIdempotent,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Resource {
    pub key: String,
    pub write: bool,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct PreparedTool {
    pub call: ToolCall,
    pub effect: Effect,
    pub resources: Vec<Resource>,
    pub exclusive: bool,
    /// Persisted preconditions/postconditions, including preimage blob references where needed.
    pub data: Value,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ToolResult {
    pub success: bool,
    pub output: Value,
    #[serde(default)]
    pub full_output_blob: Option<String>,
}
impl ToolResult {
    pub fn ok(output: Value) -> Self {
        Self {
            success: true,
            output,
            full_output_blob: None,
        }
    }
    pub fn error(message: impl Into<String>) -> Self {
        Self {
            success: false,
            output: serde_json::json!({"error":message.into()}),
            full_output_blob: None,
        }
    }
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub enum Reconcile {
    Done(ToolResult),
    Retry,
    Uncertain(String),
}
/// Preparation may persist preimages, but must not perform the requested external effect.
/// Execution is called only after a confirmed durable intention; recovery never guesses.
pub trait ToolRuntime: Send + Sync {
    fn definitions(&self) -> Vec<ToolDefinition>;
    fn prepare<'a>(
        &'a self,
        run: &'a str,
        call: ToolCall,
    ) -> BoxFuture<'a, Result<PreparedTool, String>>;
    fn execute<'a>(
        &'a self,
        run: &'a str,
        key: &'a str,
        tool: &'a PreparedTool,
    ) -> BoxFuture<'a, ToolResult>;
    fn reconcile<'a>(
        &'a self,
        run: &'a str,
        key: &'a str,
        tool: &'a PreparedTool,
    ) -> BoxFuture<'a, Reconcile>;
}
