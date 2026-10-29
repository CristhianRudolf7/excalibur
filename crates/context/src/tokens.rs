//! Local text tokenizers and durable, integer-only per-model calibration.
use excalibur_types::runtime::{Message, ModelRequest};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TokenEncoding {
    Cl100kBase,
    O200kBase,
    O200kHarmony,
    P50kBase,
    P50kEdit,
    R50kBase,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct TokenCounter {
    pub model: String,
    pub encoding: Option<TokenEncoding>,
    /// Base text estimate for providers without a locally known tokenizer.
    pub tokens_per_million_chars: u64,
    /// Request framing/provider correction, updated only from known actual input usage.
    pub request_ratio_ppm: u64,
    pub samples: u64,
}
impl Default for TokenCounter {
    fn default() -> Self {
        Self {
            model: "unknown".into(),
            encoding: None,
            tokens_per_million_chars: 333_334,
            request_ratio_ppm: 1_000_000,
            samples: 0,
        }
    }
}
impl TokenCounter {
    pub fn for_model(model: impl Into<String>) -> Self {
        let model = model.into();
        use tiktoken_rs::tokenizer::{Tokenizer, get_tokenizer};
        let encoding = match get_tokenizer(&model) {
            Some(Tokenizer::Cl100kBase) => Some(TokenEncoding::Cl100kBase),
            Some(Tokenizer::O200kBase) => Some(TokenEncoding::O200kBase),
            Some(Tokenizer::O200kHarmony) => Some(TokenEncoding::O200kHarmony),
            Some(Tokenizer::P50kBase) => Some(TokenEncoding::P50kBase),
            Some(Tokenizer::P50kEdit) => Some(TokenEncoding::P50kEdit),
            Some(Tokenizer::R50kBase | Tokenizer::Gpt2) => Some(TokenEncoding::R50kBase),
            _ => None,
        };
        Self {
            model,
            encoding,
            ..Self::default()
        }
    }
    pub fn count_text(&self, text: &str) -> usize {
        let tokenizer = match self.encoding {
            Some(TokenEncoding::Cl100kBase) => Some(tiktoken_rs::cl100k_base_singleton()),
            Some(TokenEncoding::O200kBase) => Some(tiktoken_rs::o200k_base_singleton()),
            Some(TokenEncoding::O200kHarmony) => Some(tiktoken_rs::o200k_harmony_singleton()),
            Some(TokenEncoding::P50kBase) => Some(tiktoken_rs::p50k_base_singleton()),
            Some(TokenEncoding::P50kEdit) => Some(tiktoken_rs::p50k_edit_singleton()),
            Some(TokenEncoding::R50kBase) => Some(tiktoken_rs::r50k_base_singleton()),
            None => None,
        };
        match tokenizer {
            Some(bpe) => bpe.encode_ordinary(text).len(),
            None => scaled(text.chars().count(), self.tokens_per_million_chars),
        }
    }
    pub fn estimate_text(&self, text: &str) -> usize {
        scaled(self.count_text(text), self.request_ratio_ppm)
    }
    pub fn count_messages(&self, messages: &[Message]) -> usize {
        messages
            .iter()
            .map(|m| self.estimate_text(&serde_json::to_string(m).expect("serializable message")))
            .sum()
    }
    pub fn base_request_tokens(&self, request: &ModelRequest) -> usize {
        if request.deferred_tools.is_empty() {
            return self
                .count_text(&serde_json::to_string(request).expect("serializable model request"));
        }
        // Native search transports the catalog to the provider, but only retrieved
        // definitions enter model context. This remains an estimate calibrated by usage.
        let mut value = serde_json::to_value(request).expect("serializable model request");
        value.as_object_mut().unwrap().remove("deferred_tools");
        value["deferred_search_descriptor"] = serde_json::json!({"type":"tool_search_tool_bm25_20251119","name":"tool_search_tool_bm25"});
        fn references(value: &serde_json::Value, names: &mut std::collections::BTreeSet<String>) {
            match value {
                serde_json::Value::Object(map) => {
                    if value["type"] == "tool_reference"
                        && let Some(name) = value["tool_name"].as_str()
                    {
                        names.insert(name.into());
                    }
                    for child in map.values() {
                        references(child, names)
                    }
                }
                serde_json::Value::Array(values) => {
                    for child in values {
                        references(child, names)
                    }
                }
                _ => {}
            }
        }
        let mut names = std::collections::BTreeSet::new();
        for message in &request.messages {
            if let Some(state) = &message.provider_state {
                references(state, &mut names)
            }
        }
        value["retrieved_deferred_definitions"] = serde_json::json!(
            request
                .deferred_tools
                .iter()
                .filter(|tool| names.contains(&tool.name))
                .collect::<Vec<_>>()
        );
        self.count_text(&serde_json::to_string(&value).expect("serializable request estimate"))
    }
    pub fn count_request(&self, request: &ModelRequest) -> usize {
        scaled(self.base_request_tokens(request), self.request_ratio_ppm)
    }
    pub fn with_margin(tokens: usize) -> usize {
        tokens.saturating_mul(110).div_ceil(100)
    }
    /// The caller must provide provider-reported input including cache reads/writes.
    /// Estimates, zero counts and empty requests cannot calibrate the estimator.
    pub fn observe(&mut self, request: &ModelRequest, actual_input: u64, estimated: bool) -> bool {
        let base = self.base_request_tokens(request);
        if estimated || actual_input == 0 || base == 0 {
            return false;
        }
        let observed =
            ((u128::from(actual_input) * 1_000_000) / (base as u128)).clamp(1, 100_000_000) as u64;
        self.request_ratio_ppm = if self.samples == 0 {
            observed
        } else {
            (self
                .request_ratio_ppm
                .saturating_mul(4)
                .saturating_add(observed))
                / 5
        };
        self.samples = self.samples.saturating_add(1);
        true
    }
}
fn scaled(n: usize, ppm: u64) -> usize {
    ((n as u128 * u128::from(ppm)).div_ceil(1_000_000)).min(usize::MAX as u128) as usize
}
