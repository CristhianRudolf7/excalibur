//! Adapter for the OpenAI Decisions API (`POST /v1/decisions`).
//!
//! The request carries only `model`, `input`, `questions` and optional prompt
//! cache settings: the endpoint accepts no reasoning or sampling parameters.
use crate::{
    Answer, Decider, DeciderError, Decision, DecisionInput, DecisionRequest, DecisionUsage,
    LevelProbability, OptionProbability, Question, Result,
};
use excalibur_providers::{Error as ProviderError, KnownFailure, ProviderClient};
use excalibur_types::runtime::BoxFuture;
use serde_json::{Value, json};

pub struct OpenAiDecisions {
    client: ProviderClient,
    fallback_client: Option<ProviderClient>,
    model: String,
    prompt_cache_key: Option<String>,
}
impl OpenAiDecisions {
    /// `model` comes from the host role configuration (`decider` role).
    pub fn new(client: ProviderClient, model: impl Into<String>) -> Result<Self> {
        let model = model.into();
        if model.trim().is_empty() || model.len() > 128 {
            return Err(DeciderError::Invalid("decision model is required".into()));
        }
        Ok(Self {
            client,
            fallback_client: None,
            model,
            prompt_cache_key: None,
        })
    }
    /// Secondary credential used only after a classified quota failure on the primary.
    pub fn with_quota_fallback(mut self, client: ProviderClient) -> Self {
        self.fallback_client = Some(client);
        self
    }
    pub fn with_prompt_cache_key(mut self, key: impl Into<String>) -> Result<Self> {
        let key = key.into();
        if key.is_empty() || key.len() > 64 {
            return Err(DeciderError::Invalid(
                "prompt cache key must contain 1..64 characters".into(),
            ));
        }
        self.prompt_cache_key = Some(key);
        Ok(self)
    }
    pub fn body(&self, request: &DecisionRequest) -> Value {
        let input = match &request.input {
            DecisionInput::Text(text) => json!(text),
            DecisionInput::Json(data) => {
                json!([{"role":"user","content":[{"type":"input_json","data":data}]}])
            }
        };
        let questions = request
            .questions
            .iter()
            .map(|q| match q {
                Question::Predicate { name, instructions } => {
                    json!({"type":"predicate","name":name,"instructions":instructions})
                }
                Question::Choice {
                    name,
                    instructions,
                    choices,
                } => json!({"type":"choice","name":name,"instructions":instructions,"choices":choices}),
                Question::Score {
                    name,
                    instructions,
                    levels,
                } => json!({"type":"score","name":name,"instructions":instructions,"levels":levels}),
            })
            .collect::<Vec<_>>();
        let mut body = json!({"model":self.model,"input":input,"questions":questions});
        if let Some(key) = &self.prompt_cache_key {
            body["prompt_cache_key"] = json!(key);
        }
        body
    }
}
fn number(value: &Value, field: &str) -> Result<f64> {
    value[field]
        .as_f64()
        .ok_or_else(|| DeciderError::InvalidAnswer(format!("missing numeric {field}")))
}
fn text(value: &Value) -> Option<String> {
    match value {
        Value::String(s) => Some(s.clone()),
        Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}
/// Parse a Decisions API response body against the request it answers.
pub fn parse_response(request: &DecisionRequest, body: &Value) -> Result<Decision> {
    let answers = body["answers"]
        .as_array()
        .ok_or_else(|| DeciderError::InvalidAnswer("missing answers".into()))?;
    if answers.len() != request.questions.len() {
        return Err(DeciderError::InvalidAnswer(
            "answer count differs from question count".into(),
        ));
    }
    let mut parsed = Vec::new();
    for (raw, question) in answers.iter().zip(&request.questions) {
        let name = question.name().to_string();
        if raw["name"].as_str().is_some_and(|n| n != name) {
            return Err(DeciderError::InvalidAnswer("answer order mismatch".into()));
        }
        let answer = match (raw["type"].as_str(), question) {
            (Some("refusal"), _) => Answer::Refusal { name },
            (Some("predicate"), Question::Predicate { .. }) => Answer::Predicate {
                name,
                probability: number(raw, "probability")?,
            },
            (Some("choice"), Question::Choice { .. }) => Answer::Choice {
                name,
                choice: text(&raw["choice"])
                    .ok_or_else(|| DeciderError::InvalidAnswer("missing choice".into()))?,
                confidence: number(raw, "confidence")?,
                probabilities: raw["probabilities"]
                    .as_array()
                    .map(|items| {
                        items
                            .iter()
                            .map(|p| {
                                Ok(OptionProbability {
                                    value: text(&p["value"]).ok_or_else(|| {
                                        DeciderError::InvalidAnswer("invalid choice value".into())
                                    })?,
                                    probability: number(p, "probability")?,
                                })
                            })
                            .collect::<Result<Vec<_>>>()
                    })
                    .transpose()?
                    .unwrap_or_default(),
            },
            (Some("score"), Question::Score { levels, .. }) => {
                let probabilities = raw["probabilities"]
                    .as_array()
                    .map(|items| {
                        items
                            .iter()
                            .map(|p| {
                                let value = p["value"].as_u64().ok_or_else(|| {
                                    DeciderError::InvalidAnswer("invalid level value".into())
                                })? as usize;
                                Ok(LevelProbability {
                                    label: p["label"]
                                        .as_str()
                                        .map(String::from)
                                        .or_else(|| levels.get(value).map(|l| l.label.clone()))
                                        .unwrap_or_default(),
                                    value,
                                    probability: number(p, "probability")?,
                                })
                            })
                            .collect::<Result<Vec<_>>>()
                    })
                    .transpose()?
                    .unwrap_or_default();
                let score = number(raw, "score")?;
                let level = probabilities
                    .iter()
                    .max_by(|a, b| a.probability.total_cmp(&b.probability))
                    .map(|p| p.value)
                    .unwrap_or_else(|| score.round().max(0.0) as usize);
                Answer::Score {
                    name,
                    score,
                    level,
                    confidence: number(raw, "confidence")?,
                    probabilities,
                }
            }
            _ => {
                return Err(DeciderError::InvalidAnswer(format!(
                    "unexpected answer type for {}",
                    question.name()
                )));
            }
        };
        parsed.push(answer);
    }
    let usage = &body["usage"];
    let decision = Decision {
        answer_origins: Default::default(),
        answers: parsed,
        backend: "openai-decisions".into(),
        model: body["model"].as_str().map(String::from),
        usage: DecisionUsage {
            input_tokens: usage["input_tokens"].as_u64().unwrap_or(0),
            cached_tokens: usage["input_tokens_details"]["cached_tokens"]
                .as_u64()
                .unwrap_or(0),
            output_tokens: usage["output_tokens"].as_u64().unwrap_or(0),
        },
        fallback_questions: Vec::new(),
    };
    decision.validate_for(request)?;
    Ok(decision)
}
impl Decider for OpenAiDecisions {
    fn backend(&self) -> &str {
        "openai-decisions"
    }
    fn decide<'a>(&'a self, request: &'a DecisionRequest) -> BoxFuture<'a, Result<Decision>> {
        Box::pin(async move {
            request.validate()?;
            let body = self.body(request);
            let response = match self.client.openai_decision(body.clone()).await {
                Err(ProviderError::ClassifiedHttp {
                    kind: KnownFailure::Quota,
                    ..
                }) if self.fallback_client.is_some() => {
                    self.fallback_client
                        .as_ref()
                        .unwrap()
                        .openai_decision(body)
                        .await
                }
                other => other,
            }
            .map_err(|e| DeciderError::Unavailable(e.to_string()))?;
            let mut decision = parse_response(request, &response)?;
            if decision.model.is_none() {
                decision.model = Some(self.model.clone());
            }
            decision.validate_for(request)?;
            Ok(decision)
        })
    }
}
