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
    /// Opaque response state retransmitted only by its originating provider adapter.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_state: Option<Value>,
}
impl Message {
    pub fn text(role: &str, content: impl Into<String>) -> Self {
        Self {
            role: role.into(),
            content: content.into(),
            tool_calls: Vec::new(),
            tool_call_id: None,
            provider_state: None,
        }
    }
}
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct ModelRequest {
    pub messages: Vec<Message>,
    pub tools: Vec<ToolDefinition>,
    pub tools_enabled: bool,
    pub reasoning_effort: Option<String>,
    pub output_schema: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache: Option<PromptCache>,
    /// Provider-native deferred catalog; excluded from the eager core prefix.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub deferred_tools: Vec<ToolDefinition>,
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CacheTtl {
    FiveMinutes,
    OneHour,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct CacheBreakpoint {
    pub message_index: usize,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct PromptCache {
    /// The host hashes principal, role and scope; no raw personal identity.
    pub namespace_key: String,
    pub ttl: CacheTtl,
    pub breakpoints: Vec<CacheBreakpoint>,
    pub stable_prefix_hash: String,
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
/// Side-effect-free scheduling metadata. Resource discovery must not read file
/// contents, create preimages or bind an operation to a mutable content snapshot.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ToolScheduling {
    pub resources: Vec<Resource>,
    pub exclusive: bool,
}
impl ToolScheduling {
    /// Adapters without independent metadata are scheduled alone for safety.
    pub fn isolated() -> Self {
        Self {
            resources: Vec::new(),
            exclusive: true,
        }
    }
    pub fn from_prepared(tool: &PreparedTool) -> Self {
        Self {
            resources: tool.resources.clone(),
            exclusive: tool.exclusive,
        }
    }
}
/// Inspects the full uncompressed blob during the same streaming hash verification.
/// No byte window may be returned until finish accepts the complete content.
pub trait DurableByteInspector: Send {
    fn inspect(&mut self, bytes: &[u8]) -> Result<(), String>;
    fn finish(&mut self) -> Result<(), String>;
}
/// Live egress gate, called by a trusted host broker before each new TCP
/// connection. Implementations must fail closed when authorization is unavailable.
pub trait NetworkAdmission: Send + Sync {
    /// Recheck the same admitted attempt immediately before the pinned TCP
    /// connect, after DNS. This must not consume a second Once approval.
    fn revalidate(&self) -> Result<(), String>;
}
struct UnrestrictedNetworkAdmission;
impl NetworkAdmission for UnrestrictedNetworkAdmission {
    fn revalidate(&self) -> Result<(), String> {
        Ok(())
    }
}
pub trait NetworkAuthorizer: Send + Sync {
    fn authorize(&self, domain: &str, port: u16) -> Result<(), String>;
    fn authorize_connection(
        &self,
        domain: &str,
        port: u16,
    ) -> Result<Box<dyn NetworkAdmission>, String> {
        self.authorize(domain, port)?;
        Ok(Box::new(UnrestrictedNetworkAdmission))
    }

    /// Metadata-only denial audit; never receives URI paths, headers or bodies.
    fn rejected(&self, _domain: &str, _port: u16, _reason: &str) -> Result<(), String> {
        Ok(())
    }
}
/// Kernel/process adapter input, derived only from the host execution capability.
/// Adapters must reject an unsupported restriction instead of broadening it.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum ProcessNetwork {
    None,
    Domains(Vec<String>),
    Full,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProcessBoundary {
    pub read_roots: Vec<std::path::PathBuf>,
    pub write_roots: Vec<std::path::PathBuf>,
    pub network: ProcessNetwork,
    pub profile: String,
    pub workspace: std::path::PathBuf,
    pub denied_paths: Vec<std::path::PathBuf>,
}
/// Security facts observed while holding an approval/finalization ordering guard.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FinalizationState {
    Ready,
    Pending,
    Blocked(String),
}
/// Retain this guard until the terminal journal transition has committed.
pub trait FinalizationGuard: Send {
    fn state(&self) -> FinalizationState;
}
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ExecutionLimits {
    pub max_processes: Option<u64>,
    pub max_terminal_sessions: Option<u64>,
}
/// Managed command groups and terminal records; this is not an OS descendant count.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ExecutionResources {
    pub processes: u64,
    pub terminal_sessions: u64,
}
struct ReadyFinalizationGuard;
impl FinalizationGuard for ReadyFinalizationGuard {
    fn state(&self) -> FinalizationState {
        FinalizationState::Ready
    }
}
/// Preparation may persist preimages, but must not perform the requested external effect.
/// Execution is called only after a confirmed durable intention; recovery never guesses.
/// Trusted host runtime evidence; never deserialized from a model tool call.
#[derive(Clone, Debug)]
pub struct FilesystemConfinement {
    pub workspace: std::path::PathBuf,
    pub read_roots: Vec<std::path::PathBuf>,
    pub snapshot_owner: String,
}
/// Host-owned frozen session context and append-only updates. No tool arguments
/// can provide this authority or mutate an existing stable prefix.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct RunContext {
    pub identity: String,
    pub suggested_skills: Vec<String>,
    pub updates: Vec<(String, String)>,
}
pub trait ToolRuntime: Send + Sync {
    /// A trusted scoped adapter can own a same-named engine utility. The host
    /// binding must keep this routing stable for the run, including restart.
    fn overrides_builtin<'a>(&'a self, _run: &'a str, _name: &'a str) -> BoxFuture<'a, bool> {
        Box::pin(async { false })
    }
    fn session_context<'a>(&'a self, _run: &'a str) -> BoxFuture<'a, Result<RunContext, String>> {
        Box::pin(async { Ok(RunContext::default()) })
    }
    /// A positively confined filesystem and run-owned snapshot namespace.
    /// Generic runtimes deliberately provide no attestation.
    fn filesystem_confinement(&self, _run: &str) -> Option<FilesystemConfinement> {
        None
    }

    /// Authoritative lifecycle update. Adapters release leases on human waits and terminal states.
    fn execution_state<'a>(
        &'a self,
        _run: &'a str,
        _state: &'a str,
        _reason: Option<&'a str>,
    ) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async { Ok(()) })
    }
    /// Durable, idempotent step boundaries for command shadow snapshots.
    fn begin_step<'a>(&'a self, _run: &'a str, _step: u64) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async { Ok(()) })
    }
    fn end_step<'a>(&'a self, _run: &'a str, _step: u64) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async { Ok(()) })
    }
    /// Interrupt managed run processes after an authoritative pause/cancel.
    /// Effect reconciliation still decides whether an interrupted operation may retry.
    fn interrupt_execution<'a>(
        &'a self,
        _run: &'a str,
        _cancelled: bool,
        _tool: Option<&'a PreparedTool>,
    ) -> BoxFuture<'a, Result<bool, String>> {
        Box::pin(async { Ok(false) })
    }
    fn configure_execution_limits<'a>(
        &'a self,
        _run: &'a str,
        limits: ExecutionLimits,
    ) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async move {
            if limits.max_processes.is_some() || limits.max_terminal_sessions.is_some() {
                Err("tool adapter cannot enforce execution resource limits".into())
            } else {
                Ok(())
            }
        })
    }
    fn execution_resources<'a>(
        &'a self,
        _run: &'a str,
    ) -> BoxFuture<'a, Result<Option<ExecutionResources>, String>> {
        Box::pin(async { Ok(None) })
    }
    /// Sanitize data before any durable model/context or execution event is recorded.
    fn sanitize_durable(&self, value: &Value) -> Result<Value, String> {
        Ok(value.clone())
    }
    /// Approval waits do not consume a tool result or advance the model step.
    fn pending_approval<'a>(&'a self, _run: &'a str) -> BoxFuture<'a, bool> {
        Box::pin(async { false })
    }
    fn approval_blocked<'a>(&'a self, _run: &'a str) -> BoxFuture<'a, Option<String>> {
        Box::pin(async { None })
    }
    /// The engine forwards explicitly untrusted context before any model/tool step.
    fn observe_untrusted<'a>(
        &'a self,
        _run: &'a str,
        _source: &'a str,
    ) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async { Ok(()) })
    }
    /// Install an effective per-run process boundary; unsupported adapters fail closed.
    fn configure_process_boundary<'a>(
        &'a self,
        _run: &'a str,
        _boundary: ProcessBoundary,
    ) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async { Err("process adapter cannot enforce execution capabilities".into()) })
    }
    /// Optional host capability limit, measured in admitted tool calls per run.
    fn execution_tool_budget<'a>(
        &'a self,
        _run: &'a str,
    ) -> BoxFuture<'a, Result<Option<u64>, String>> {
        Box::pin(async { Ok(None) })
    }
    /// A trusted decorator can defer an admitted effect for a new approval.
    /// The returned result is not final and must not be persisted as tool_result.
    fn result_requires_approval(&self, _result: &ToolResult) -> bool {
        false
    }
    /// Optional output inspector, recreated for every verified blob read.
    fn durable_blob_inspector(&self) -> Option<Box<dyn DurableByteInspector>> {
        None
    }
    /// Audit engine-owned pure utilities through the same host security boundary.
    fn audit_builtin<'a>(
        &'a self,
        _run: &'a str,
        _call: &'a ToolCall,
        _result: Option<&'a ToolResult>,
    ) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async { Ok(()) })
    }
    fn configure_network_authorizer<'a>(
        &'a self,
        _run: &'a str,
        _authorizer: std::sync::Arc<dyn NetworkAuthorizer>,
    ) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async { Err("process adapter cannot enforce live network authorization".into()) })
    }
    fn finalization_guard<'a>(
        &'a self,
        _run: &'a str,
    ) -> BoxFuture<'a, Result<Box<dyn FinalizationGuard>, String>> {
        Box::pin(async { Ok(Box::new(ReadyFinalizationGuard) as Box<dyn FinalizationGuard>) })
    }
    /// Domain schemas kept outside the compiled core prefix for native deferred adapters.
    fn deferred_definitions<'a>(
        &'a self,
        _run: &'a str,
    ) -> BoxFuture<'a, Result<Vec<ToolDefinition>, String>> {
        Box::pin(async { Ok(Vec::new()) })
    }
    fn live_status(&self, _run: &str) -> Vec<serde_json::Value> {
        Vec::new()
    }
    /// Optional immutable per-run role catalog, fixed before the first context
    /// session is compiled. Execution authorization remains independently required.
    fn run_definitions<'a>(
        &'a self,
        _run: &'a str,
    ) -> BoxFuture<'a, Result<Option<Vec<ToolDefinition>>, String>> {
        Box::pin(async { Ok(None) })
    }
    fn definitions(&self) -> Vec<ToolDefinition>;
    /// Return canonical resource keys without preparing content-dependent work.
    /// None conservatively schedules this call alone; errors are reported by prepare.
    fn scheduling<'a>(
        &'a self,
        _run: &'a str,
        _call: &'a ToolCall,
    ) -> BoxFuture<'a, Option<ToolScheduling>> {
        Box::pin(async { None })
    }

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
