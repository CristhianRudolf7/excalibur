//! Deterministic context assembly. Selection, retrieval and compaction are host inputs.
use excalibur_types::runtime::{Message, ModelRequest, ToolDefinition};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct VolatileContext {
    /// Explicit host time, never obtained by the compiler.
    pub now_utc: String,
    pub continuation: String,
    pub budget_status: String,
    pub pending_directives: Vec<String>,
    pub finalization_report: String,
    pub recovery_notices: Vec<String>,
    pub master_jobs: String,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ContextInput {
    /// Frozen L0 core, L1 identity/catalog, L2 stable scope and L3 original task.
    pub layers: [String; 4],
    pub tools: Vec<ToolDefinition>,
    /// Ordered append-only history including versioned context updates.
    pub activity: Vec<Message>,
    pub volatile: VolatileContext,
    pub token_budgets: [usize; 6],
    pub suggested_skills: Vec<String>,
    pub tools_enabled: bool,
    pub reasoning_effort: Option<String>,
    pub output_schema: Option<serde_json::Value>,
}
impl Default for ContextInput {
    fn default() -> Self {
        Self::new("", "", "")
    }
}
impl ContextInput {
    pub fn new(
        core: impl Into<String>,
        task: impl Into<String>,
        now_utc: impl Into<String>,
    ) -> Self {
        Self {
            layers: [core.into(), String::new(), String::new(), task.into()],
            tools: Vec::new(),
            activity: Vec::new(),
            volatile: VolatileContext {
                now_utc: now_utc.into(),
                ..Default::default()
            },
            token_budgets: [4096, 4096, 8192, 8192, 16384, 4096],
            suggested_skills: Vec::new(),
            tools_enabled: true,
            reasoning_effort: None,
            output_schema: None,
        }
    }
    /// Frozen layers are not rerendered: a changed requirement/instruction is a new L4 message.
    pub fn append_delta(&mut self, version: u64, text: impl AsRef<str>) {
        self.activity.push(Message::text(
            "system",
            format!("Context update v{version}: {}", text.as_ref()),
        ));
    }
    pub fn append_message(&mut self, message: Message) {
        self.activity.push(message);
    }
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct CompiledContext {
    pub request: ModelRequest,
    pub bytes: Vec<u8>,
    pub layer_hashes: [String; 6],
    pub estimated_tokens: [usize; 6],
}
#[derive(Debug, thiserror::Error)]
pub enum ContextError {
    #[error("context JSON serialization failed")]
    Serialization(#[from] serde_json::Error),
    #[error("layer L{layer} estimate {estimated} exceeds budget {budget}")]
    Budget {
        layer: usize,
        estimated: usize,
        budget: usize,
    },
    #[error("duplicate or empty tool name")]
    ToolNames,
    #[error("an explicit UTC time is required")]
    MissingTime,
}
/// Conservative character-based estimate for deterministic budgeting, not provider token accounting.
pub fn estimate_tokens(text: &str) -> usize {
    text.chars().count().div_ceil(3)
}
pub fn compile(input: &ContextInput) -> Result<CompiledContext, ContextError> {
    if input.volatile.now_utc.trim().is_empty() {
        return Err(ContextError::MissingTime);
    }
    let mut tools = input.tools.clone();
    tools.sort_by(|a, b| a.name.cmp(&b.name));
    if tools.iter().any(|t| t.name.is_empty()) || tools.windows(2).any(|t| t[0].name == t[1].name) {
        return Err(ContextError::ToolNames);
    }
    let mut layer_text: [String; 6] = std::array::from_fn(|_| String::new());
    layer_text[..4].clone_from_slice(&input.layers);
    layer_text[0].push_str("\nCore tool definitions: ");
    layer_text[0].push_str(&serde_json::to_string(&tools)?);
    if !input.suggested_skills.is_empty() {
        layer_text[3].push_str("\nSuggested skills: ");
        layer_text[3].push_str(&serde_json::to_string(
            &input.suggested_skills.iter().take(3).collect::<Vec<_>>(),
        )?);
    }
    layer_text[4] = serde_json::to_string(&input.activity)?;
    layer_text[5] = serde_json::to_string(&input.volatile)?;
    let estimated_tokens = std::array::from_fn(|i| estimate_tokens(&layer_text[i]));
    for (layer, estimated) in estimated_tokens.iter().copied().enumerate() {
        if estimated > input.token_budgets[layer] {
            return Err(ContextError::Budget {
                layer,
                estimated,
                budget: input.token_budgets[layer],
            });
        }
    }
    let layer_hashes =
        std::array::from_fn(|i| format!("{:x}", Sha256::digest(layer_text[i].as_bytes())));
    let mut messages: Vec<Message> = (0..4)
        .map(|i| {
            Message::text(
                "system",
                format!(
                    "[L{i}]\n{}",
                    if i == 0 {
                        &input.layers[0]
                    } else {
                        &layer_text[i]
                    }
                ),
            )
        })
        .collect();
    messages.extend(input.activity.clone());
    messages.push(Message::text("system", format!("[L5]\n{}", layer_text[5])));
    let request = ModelRequest {
        messages,
        tools,
        tools_enabled: input.tools_enabled,
        reasoning_effort: input.reasoning_effort.clone(),
        output_schema: input.output_schema.clone(),
    };
    let bytes = serde_json::to_vec(&request)?;
    Ok(CompiledContext {
        request,
        bytes,
        layer_hashes,
        estimated_tokens,
    })
}
/// Verify the stable prefix did not drift within a session.
pub fn stable_prefix_changed(previous: &CompiledContext, current: &CompiledContext) -> bool {
    previous.layer_hashes[..3] != current.layer_hashes[..3]
}
