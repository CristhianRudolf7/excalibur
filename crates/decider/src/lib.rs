//! Bounded typed decisions with calibrated abstention.
//!
//! A decider answers `predicate`, `choice` and `score` questions about a state
//! with probabilities instead of prose. Implementations are interchangeable
//! (a hosted decision API, a cheap LLM with constrained JSON output, or a test
//! double). Callers route every answer through per-port thresholds calibrated
//! from Excalibur's own labeled ledger; the band between the thresholds defers
//! the decision to the agent's ordinary model flow. No port exists for the final
//! verdict on the correctness of work, so the decider can never issue it.
pub mod calibration;
pub mod ledger;
pub mod llm;
pub mod openai;

pub use calibration::{CalibrationReport, LabeledOutcome, ReliabilityBin, calibrate};
pub use ledger::{DecisionLedger, LabelSource, LedgerRecord};
pub use llm::LlmDecider;
pub use openai::OpenAiDecisions;

use excalibur_types::runtime::BoxFuture;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, RwLock},
    time::Duration,
};

pub const MAX_QUESTIONS: usize = 64;
pub const MAX_INPUT_BYTES: usize = 256 * 1024;
pub const MAX_INSTRUCTIONS_BYTES: usize = 8 * 1024;

#[derive(Debug, Clone, thiserror::Error, PartialEq)]
pub enum DeciderError {
    #[error("invalid decision request: {0}")]
    Invalid(String),
    #[error("decision backend returned an invalid answer: {0}")]
    InvalidAnswer(String),
    #[error("decision backend unavailable: {0}")]
    Unavailable(String),
    #[error("decision backend exceeded its deadline")]
    Timeout,
    #[error("the decider never issues the final verdict on the correctness of work")]
    FinalVerdictForbidden,
    #[error("decision ledger failed: {0}")]
    Ledger(String),
}
pub type Result<T> = std::result::Result<T, DeciderError>;

/// Closed set of harness integration points (architecture section 19.4).
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[serde(rename_all = "snake_case")]
pub enum DecisionPort {
    /// Request kind, scope, durable instructions and clarification need.
    Triage,
    /// Probability of the main interpretation before asking the user.
    AskOrAssume,
    /// Relevance of candidate skill groups and suggested skills.
    SkillRelevance,
    /// Reordering of deferred tool search candidates.
    ToolRerank,
    /// Memory admission in the ambiguous band.
    MemoryAdmission,
    /// Risk level of an unknown command.
    CommandRisk,
    /// Whether an execution is stuck in a loop.
    LoopStuck,
    /// Whether an event deserves an immediate notification.
    Notification,
}
impl DecisionPort {
    pub const ALL: [DecisionPort; 8] = [
        Self::Triage,
        Self::AskOrAssume,
        Self::SkillRelevance,
        Self::ToolRerank,
        Self::MemoryAdmission,
        Self::CommandRisk,
        Self::LoopStuck,
        Self::Notification,
    ];
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Triage => "triage",
            Self::AskOrAssume => "ask_or_assume",
            Self::SkillRelevance => "skill_relevance",
            Self::ToolRerank => "tool_rerank",
            Self::MemoryAdmission => "memory_admission",
            Self::CommandRisk => "command_risk",
            Self::LoopStuck => "loop_stuck",
            Self::Notification => "notification",
        }
    }
    /// Parse a configured port. Verdict-like purposes are rejected explicitly.
    pub fn parse(value: &str) -> Result<Self> {
        if let Some(port) = Self::ALL.into_iter().find(|p| p.as_str() == value) {
            return Ok(port);
        }
        let lowered = value.to_ascii_lowercase();
        if ["verdict", "correct", "finaliz", "completion", "acceptance"]
            .iter()
            .any(|word| lowered.contains(word))
        {
            return Err(DeciderError::FinalVerdictForbidden);
        }
        Err(DeciderError::Invalid(format!("unknown decision port {value}")))
    }
}

/// State given to the decider: literal text or structured JSON.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum DecisionInput {
    Text(String),
    Json(Value),
}
impl DecisionInput {
    pub fn render(&self) -> String {
        match self {
            Self::Text(text) => text.clone(),
            Self::Json(value) => value.to_string(),
        }
    }
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ChoiceOption {
    pub value: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ScoreLevel {
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}
/// Typed question. `Predicate` is the probability that a statement about the
/// input is true (the architecture's `noul`).
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Question {
    Predicate {
        name: String,
        instructions: String,
    },
    Choice {
        name: String,
        instructions: String,
        choices: Vec<ChoiceOption>,
    },
    Score {
        name: String,
        instructions: String,
        /// Ordered from lowest to highest.
        levels: Vec<ScoreLevel>,
    },
}
impl Question {
    pub fn predicate(name: &str, instructions: &str) -> Self {
        Self::Predicate {
            name: name.into(),
            instructions: instructions.into(),
        }
    }
    pub fn choice(name: &str, instructions: &str, values: &[&str]) -> Self {
        Self::Choice {
            name: name.into(),
            instructions: instructions.into(),
            choices: values
                .iter()
                .map(|v| ChoiceOption {
                    value: (*v).into(),
                    description: None,
                })
                .collect(),
        }
    }
    pub fn score(name: &str, instructions: &str, labels: &[&str]) -> Self {
        Self::Score {
            name: name.into(),
            instructions: instructions.into(),
            levels: labels
                .iter()
                .map(|l| ScoreLevel {
                    label: (*l).into(),
                    description: None,
                })
                .collect(),
        }
    }
    pub fn name(&self) -> &str {
        match self {
            Self::Predicate { name, .. } | Self::Choice { name, .. } | Self::Score { name, .. } => {
                name
            }
        }
    }
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Predicate { .. } => "predicate",
            Self::Choice { .. } => "choice",
            Self::Score { .. } => "score",
        }
    }
    fn instructions(&self) -> &str {
        match self {
            Self::Predicate { instructions, .. }
            | Self::Choice { instructions, .. }
            | Self::Score { instructions, .. } => instructions,
        }
    }
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct DecisionRequest {
    pub port: DecisionPort,
    pub input: DecisionInput,
    pub questions: Vec<Question>,
}
impl DecisionRequest {
    pub fn new(port: DecisionPort, input: DecisionInput, questions: Vec<Question>) -> Self {
        Self {
            port,
            input,
            questions,
        }
    }
    pub fn validate(&self) -> Result<()> {
        let invalid = |m: &str| Err(DeciderError::Invalid(m.into()));
        if self.questions.is_empty() || self.questions.len() > MAX_QUESTIONS {
            return invalid("1..64 questions are required");
        }
        let rendered = self.input.render();
        if rendered.trim().is_empty() || rendered.len() > MAX_INPUT_BYTES {
            return invalid("input must contain 1..256 KiB");
        }
        let mut names = BTreeSet::new();
        for question in &self.questions {
            let name = question.name();
            if name.is_empty()
                || name.len() > 64
                || !name
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
                || !names.insert(name)
            {
                return invalid("question names must be unique [A-Za-z0-9_-]{1,64}");
            }
            let instructions = question.instructions();
            if instructions.trim().is_empty() || instructions.len() > MAX_INSTRUCTIONS_BYTES {
                return invalid("question instructions must contain 1..8 KiB");
            }
            match question {
                Question::Predicate { .. } => {}
                Question::Choice { choices, .. } => {
                    let values = choices.iter().map(|c| &c.value).collect::<BTreeSet<_>>();
                    if !(2..=255).contains(&choices.len())
                        || values.len() != choices.len()
                        || choices.iter().any(|c| c.value.is_empty() || c.value.len() > 256)
                    {
                        return invalid("choice questions need 2..255 unique nonempty values");
                    }
                }
                Question::Score { levels, .. } => {
                    let labels = levels.iter().map(|l| &l.label).collect::<BTreeSet<_>>();
                    if !(2..=10).contains(&levels.len())
                        || labels.len() != levels.len()
                        || levels.iter().any(|l| l.label.is_empty() || l.label.len() > 256)
                    {
                        return invalid("score questions need 2..10 unique ordered levels");
                    }
                }
            }
        }
        Ok(())
    }
    fn subset(&self, names: &BTreeSet<String>) -> Self {
        Self {
            port: self.port,
            input: self.input.clone(),
            questions: self
                .questions
                .iter()
                .filter(|q| names.contains(q.name()))
                .cloned()
                .collect(),
        }
    }
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct OptionProbability {
    pub value: String,
    pub probability: f64,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct LevelProbability {
    pub label: String,
    /// Zero-based ordinal position of the level.
    pub value: usize,
    pub probability: f64,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Answer {
    Predicate {
        name: String,
        probability: f64,
    },
    Choice {
        name: String,
        choice: String,
        confidence: f64,
        probabilities: Vec<OptionProbability>,
    },
    Score {
        name: String,
        /// Expected ordinal value over the level distribution.
        score: f64,
        /// Most probable level.
        level: usize,
        confidence: f64,
        probabilities: Vec<LevelProbability>,
    },
    Refusal {
        name: String,
    },
}
impl Answer {
    pub fn name(&self) -> &str {
        match self {
            Self::Predicate { name, .. }
            | Self::Choice { name, .. }
            | Self::Score { name, .. }
            | Self::Refusal { name } => name,
        }
    }
    /// The probability compared against thresholds: P(true) for predicates and
    /// the confidence of the selected option or level otherwise.
    pub fn probability(&self) -> Option<f64> {
        match self {
            Self::Predicate { probability, .. } => Some(*probability),
            Self::Choice { confidence, .. } | Self::Score { confidence, .. } => Some(*confidence),
            Self::Refusal { .. } => None,
        }
    }
    fn validate(&self, question: &Question) -> Result<()> {
        let bad = |m: String| Err(DeciderError::InvalidAnswer(m));
        let unit = |p: f64| p.is_finite() && (0.0..=1.0).contains(&p);
        if self.name() != question.name() {
            return bad(format!("answer order mismatch at {}", question.name()));
        }
        let sums = |values: Vec<f64>| {
            values.is_empty()
                || (values.iter().all(|p| unit(*p)) && (values.iter().sum::<f64>() - 1.0).abs() <= 0.02)
        };
        match (self, question) {
            (Self::Refusal { .. }, _) => Ok(()),
            (Self::Predicate { probability, .. }, Question::Predicate { .. }) if unit(*probability) => {
                Ok(())
            }
            (
                Self::Choice {
                    choice,
                    confidence,
                    probabilities,
                    ..
                },
                Question::Choice { choices, .. },
            ) if unit(*confidence)
                && choices.iter().any(|c| &c.value == choice)
                && probabilities
                    .iter()
                    .all(|p| choices.iter().any(|c| c.value == p.value))
                && sums(probabilities.iter().map(|p| p.probability).collect()) =>
            {
                Ok(())
            }
            (
                Self::Score {
                    score,
                    level,
                    confidence,
                    probabilities,
                    ..
                },
                Question::Score { levels, .. },
            ) if unit(*confidence)
                && *level < levels.len()
                && score.is_finite()
                && (0.0..=(levels.len() - 1) as f64 + 1e-9).contains(score)
                && probabilities.iter().all(|p| {
                    p.value < levels.len() && levels[p.value].label == p.label
                })
                && sums(probabilities.iter().map(|p| p.probability).collect()) =>
            {
                Ok(())
            }
            _ => bad(format!("answer for {} has an invalid type or range", question.name())),
        }
    }
}
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct DecisionUsage {
    pub input_tokens: u64,
    pub cached_tokens: u64,
    pub output_tokens: u64,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Decision {
    pub answers: Vec<Answer>,
    /// Backend identity of the answers (for example `openai-decisions` or `llm`).
    pub backend: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default)]
    pub usage: DecisionUsage,
    /// Questions answered by a fallback backend after the primary failed or refused.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fallback_questions: Vec<String>,
}
impl Decision {
    /// Answers must keep the request order and match each question's type.
    pub fn validate_for(&self, request: &DecisionRequest) -> Result<()> {
        if self.answers.len() != request.questions.len() {
            return Err(DeciderError::InvalidAnswer(
                "answer count differs from question count".into(),
            ));
        }
        for (answer, question) in self.answers.iter().zip(&request.questions) {
            answer.validate(question)?;
        }
        Ok(())
    }
    pub fn answer(&self, name: &str) -> Option<&Answer> {
        self.answers.iter().find(|a| a.name() == name)
    }
}

/// Interchangeable decision backend.
pub trait Decider: Send + Sync {
    fn backend(&self) -> &str;
    fn decide<'a>(&'a self, request: &'a DecisionRequest) -> BoxFuture<'a, Result<Decision>>;
}

/// Abstention thresholds. `None` disables that action (always defer).
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq)]
pub struct Thresholds {
    pub tau_low: Option<f64>,
    pub tau_high: Option<f64>,
    /// False while the thresholds are conservative defaults without own labels.
    pub calibrated: bool,
}
impl Thresholds {
    /// Conservative defaults used before Excalibur has its own labeled records.
    pub const UNCALIBRATED: Thresholds = Thresholds {
        tau_low: Some(0.05),
        tau_high: Some(0.95),
        calibrated: false,
    };
    pub fn validate(&self) -> Result<()> {
        let unit = |p: Option<f64>| p.is_none_or(|p| p.is_finite() && (0.0..=1.0).contains(&p));
        if !unit(self.tau_low)
            || !unit(self.tau_high)
            || matches!((self.tau_low, self.tau_high), (Some(l), Some(h)) if l >= h)
        {
            return Err(DeciderError::Invalid(
                "thresholds must be within [0, 1] with tau_low < tau_high".into(),
            ));
        }
        Ok(())
    }
    pub fn verdict(&self, probability: Option<f64>) -> Verdict {
        let Some(p) = probability else {
            return Verdict::Defer;
        };
        if self.tau_high.is_some_and(|h| p >= h) {
            Verdict::Act
        } else if self.tau_low.is_some_and(|l| p <= l) {
            Verdict::Alternative
        } else {
            Verdict::Defer
        }
    }
}
/// What the caller does with one answer.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    /// p >= tau_high: act on the answer.
    Act,
    /// tau_low < p < tau_high (or a refusal): the agent's normal model flow decides.
    Defer,
    /// p <= tau_low: take the alternative.
    Alternative,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct RoutedAnswer {
    pub answer: Answer,
    pub verdict: Verdict,
    pub thresholds: Thresholds,
    /// Ledger identifier used to attach the eventual label.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub record_id: Option<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct RoutedDecision {
    pub port: DecisionPort,
    pub backend: String,
    pub answers: Vec<RoutedAnswer>,
    pub fallback_questions: Vec<String>,
}

/// Primary backend with a bounded deadline and an LLM (or other) fallback for
/// failures, invalid answers and per-question refusals.
pub struct FallbackDecider {
    primary: Arc<dyn Decider>,
    fallback: Arc<dyn Decider>,
    deadline: Duration,
    identity: String,
}
impl FallbackDecider {
    pub fn new(primary: Arc<dyn Decider>, fallback: Arc<dyn Decider>, deadline: Duration) -> Self {
        let identity = format!("{}+{}", primary.backend(), fallback.backend());
        Self {
            primary,
            fallback,
            deadline,
            identity,
        }
    }
    async fn bounded(&self, decider: &dyn Decider, request: &DecisionRequest) -> Result<Decision> {
        let decision = tokio::time::timeout(self.deadline, decider.decide(request))
            .await
            .map_err(|_| DeciderError::Timeout)??;
        decision.validate_for(request)?;
        Ok(decision)
    }
}
impl Decider for FallbackDecider {
    fn backend(&self) -> &str {
        &self.identity
    }
    fn decide<'a>(&'a self, request: &'a DecisionRequest) -> BoxFuture<'a, Result<Decision>> {
        Box::pin(async move {
            request.validate()?;
            let primary = match self.bounded(self.primary.as_ref(), request).await {
                Ok(decision) => decision,
                Err(DeciderError::FinalVerdictForbidden) => {
                    return Err(DeciderError::FinalVerdictForbidden);
                }
                Err(_) => {
                    let mut decision = self.bounded(self.fallback.as_ref(), request).await?;
                    decision.fallback_questions = request
                        .questions
                        .iter()
                        .map(|q| q.name().to_string())
                        .collect();
                    return Ok(decision);
                }
            };
            let refused = primary
                .answers
                .iter()
                .filter(|a| matches!(a, Answer::Refusal { .. }))
                .map(|a| a.name().to_string())
                .collect::<BTreeSet<_>>();
            if refused.is_empty() {
                return Ok(primary);
            }
            let subset = request.subset(&refused);
            let Ok(retry) = self.bounded(self.fallback.as_ref(), &subset).await else {
                return Ok(primary);
            };
            let mut replacements = retry
                .answers
                .into_iter()
                .map(|a| (a.name().to_string(), a))
                .collect::<BTreeMap<_, _>>();
            let mut merged = primary;
            for answer in &mut merged.answers {
                if let Some(replacement) = replacements.remove(answer.name()) {
                    *answer = replacement;
                }
            }
            merged.fallback_questions = refused.into_iter().collect();
            merged.usage.input_tokens += retry.usage.input_tokens;
            merged.usage.cached_tokens += retry.usage.cached_tokens;
            merged.usage.output_tokens += retry.usage.output_tokens;
            Ok(merged)
        })
    }
}

/// Decider plus thresholds and the own labeled ledger used to calibrate them.
pub struct DecisionService {
    decider: Arc<dyn Decider>,
    ledger: Option<DecisionLedger>,
    thresholds: RwLock<BTreeMap<(DecisionPort, String), Thresholds>>,
}
impl DecisionService {
    pub fn new(decider: Arc<dyn Decider>) -> Self {
        Self {
            decider,
            ledger: None,
            thresholds: RwLock::new(BTreeMap::new()),
        }
    }
    /// Attach the ledger and install every threshold previously calibrated from it.
    pub async fn with_ledger(mut self, ledger: DecisionLedger) -> Result<Self> {
        let stored = ledger.thresholds().await?;
        *self.thresholds.write().unwrap_or_else(|e| e.into_inner()) = stored;
        self.ledger = Some(ledger);
        Ok(self)
    }
    pub fn ledger(&self) -> Option<&DecisionLedger> {
        self.ledger.as_ref()
    }
    pub fn thresholds(&self, port: DecisionPort, question: &str) -> Thresholds {
        self.thresholds
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(&(port, question.to_string()))
            .copied()
            .unwrap_or(Thresholds::UNCALIBRATED)
    }
    pub async fn decide(&self, request: &DecisionRequest) -> Result<RoutedDecision> {
        request.validate()?;
        let decision = self.decider.decide(request).await?;
        decision.validate_for(request)?;
        let input_hash = ledger::digest(request.input.render().as_bytes());
        let mut answers = Vec::new();
        for answer in decision.answers {
            let thresholds = self.thresholds(request.port, answer.name());
            let verdict = thresholds.verdict(answer.probability());
            let record_id = match (&self.ledger, answer.probability()) {
                (Some(ledger), Some(probability)) => Some(
                    ledger
                        .record(ledger::NewRecord {
                            port: request.port,
                            question: answer.name().to_string(),
                            kind: answer_kind(&answer).into(),
                            backend: if decision.fallback_questions.iter().any(|q| q == answer.name()) {
                                format!("{}:fallback", decision.backend)
                            } else {
                                decision.backend.clone()
                            },
                            probability,
                            verdict,
                            selected: selected(&answer),
                            input_hash: input_hash.clone(),
                        })
                        .await?,
                ),
                _ => None,
            };
            answers.push(RoutedAnswer {
                answer,
                verdict,
                thresholds,
                record_id,
            });
        }
        Ok(RoutedDecision {
            port: request.port,
            backend: decision.backend,
            answers,
            fallback_questions: decision.fallback_questions,
        })
    }
    /// Calibrate one (port, question) from labeled ledger records, persist and install it.
    pub async fn calibrate(
        &self,
        port: DecisionPort,
        question: &str,
        target_precision: f64,
        min_support: usize,
    ) -> Result<CalibrationReport> {
        let ledger = self
            .ledger
            .as_ref()
            .ok_or_else(|| DeciderError::Ledger("no ledger attached".into()))?;
        let samples = ledger.labeled(port, question).await?;
        let report = calibrate(&samples, target_precision, min_support)?;
        ledger.store_thresholds(port, question, &report).await?;
        self.thresholds
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .insert((port, question.to_string()), report.thresholds);
        Ok(report)
    }
}
fn answer_kind(answer: &Answer) -> &'static str {
    match answer {
        Answer::Predicate { .. } => "predicate",
        Answer::Choice { .. } => "choice",
        Answer::Score { .. } => "score",
        Answer::Refusal { .. } => "refusal",
    }
}
fn selected(answer: &Answer) -> Option<String> {
    match answer {
        Answer::Choice { choice, .. } => Some(choice.clone()),
        Answer::Score { level, .. } => Some(level.to_string()),
        _ => None,
    }
}
/// Normalize a distribution, choose its mode and return (mode, confidence, distribution).
pub(crate) fn normalized(values: &[(String, f64)]) -> Option<(usize, f64, Vec<f64>)> {
    if values.is_empty() || values.iter().any(|(_, p)| !p.is_finite() || *p < 0.0) {
        return None;
    }
    let total = values.iter().map(|(_, p)| p).sum::<f64>();
    if total <= 0.0 {
        return None;
    }
    let distribution = values.iter().map(|(_, p)| p / total).collect::<Vec<_>>();
    let (index, confidence) = distribution
        .iter()
        .copied()
        .enumerate()
        .fold((0, f64::MIN), |best, (i, p)| if p > best.1 { (i, p) } else { best });
    Some((index, confidence, distribution))
}
