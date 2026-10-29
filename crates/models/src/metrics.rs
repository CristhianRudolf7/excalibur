//! Per-call accounting. Missing provider counters remain unknown; no prices are bundled.
use crate::{ModelError, ModelRole};
use excalibur_types::runtime::{ModelRequest, ModelResponse, Usage};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct UsageBreakdown {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: Option<u64>,
    pub cache_write_tokens: Option<u64>,
    pub cache_write_by_ttl: BTreeMap<String, u64>,
    pub reasoning_tokens: Option<u64>,
    pub estimated: bool,
}
impl UsageBreakdown {
    pub fn from_usage(usage: &Usage) -> Self {
        Self {
            input_tokens: usage.input_tokens,
            output_tokens: usage.output_tokens,
            estimated: usage.estimated,
            ..Default::default()
        }
    }
    pub fn zero() -> Self {
        Self {
            cache_read_tokens: Some(0),
            cache_write_tokens: Some(0),
            reasoning_tokens: Some(0),
            ..Default::default()
        }
    }
    pub fn validate(&self) -> Result<(), ModelError> {
        if self
            .cache_read_tokens
            .unwrap_or(0)
            .checked_add(self.cache_write_tokens.unwrap_or(0))
            .is_none_or(|tokens| tokens > self.input_tokens)
            || self.reasoning_tokens.unwrap_or(0) > self.output_tokens
            || self
                .cache_write_by_ttl
                .values()
                .try_fold(0_u64, |n, value| n.checked_add(*value))
                .is_none_or(|n| n > self.cache_write_tokens.unwrap_or(0))
        {
            return Err(ModelError::Accounting("inconsistent token subsets".into()));
        }
        Ok(())
    }
    pub fn add(&mut self, other: &Self) {
        self.input_tokens = self.input_tokens.saturating_add(other.input_tokens);
        self.output_tokens = self.output_tokens.saturating_add(other.output_tokens);
        self.cache_read_tokens = optional_add(self.cache_read_tokens, other.cache_read_tokens);
        self.cache_write_tokens = optional_add(self.cache_write_tokens, other.cache_write_tokens);
        self.reasoning_tokens = optional_add(self.reasoning_tokens, other.reasoning_tokens);
        for (key, value) in &other.cache_write_by_ttl {
            let total = self.cache_write_by_ttl.entry(key.clone()).or_default();
            *total = total.saturating_add(*value);
        }
        self.estimated |= other.estimated;
    }
}
fn optional_add(a: Option<u64>, b: Option<u64>) -> Option<u64> {
    a.zip(b).map(|(a, b)| a.saturating_add(b))
}
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct GatewayMetadata {
    pub provider: Option<String>,
    pub model: Option<String>,
    pub role: Option<ModelRole>,
    pub configuration_hash: String,
    pub price_table_version: Option<String>,
    pub price_table_hash: Option<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum FailureClass {
    Authentication,
    Permission,
    RateLimit,
    Quota,
    ContextWindowExceeded,
    Overloaded,
    Timeout,
    Server,
    InvalidRequest,
    Transport,
    InvalidResponse,
    Truncated,
    Validation,
    Consumer,
    ReplayDivergence,
    Budget,
    Accounting,
    Unknown,
    AgentTimeout,
    HarnessError,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ModelFailure {
    pub class: FailureClass,
    pub http_status: Option<u16>,
}
impl ModelFailure {
    pub fn http(status: u16) -> Self {
        Self {
            class: match status {
                401 => FailureClass::Authentication,
                403 => FailureClass::Permission,
                408 | 504 => FailureClass::Timeout,
                429 => FailureClass::RateLimit,
                500..=599 => FailureClass::Server,
                400..=499 => FailureClass::InvalidRequest,
                _ => FailureClass::Unknown,
            },
            http_status: Some(status),
        }
    }
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ModelAttemptMetrics {
    pub index: u8,
    pub duration_ms: u64,
    pub usage: UsageBreakdown,
    pub cost_microusd: Option<u64>,
    pub error: Option<ModelFailure>,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ModelCallMetrics {
    pub metadata: GatewayMetadata,
    pub request_hash: String,
    pub duration_ms: u64,
    pub provider_calls: u64,
    #[serde(default)]
    pub provider_calls_known: bool,
    /// True for provider-reported input/output totals; optional subcounts may still be absent.
    #[serde(default)]
    pub usage_known: bool,
    pub replayed: bool,
    pub usage: UsageBreakdown,
    pub cost_microusd: Option<u64>,
    pub attempts: Vec<ModelAttemptMetrics>,
    pub recorded_usage: Option<UsageBreakdown>,
    pub recorded_cost_microusd: Option<u64>,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ObservedResponse {
    pub response: ModelResponse,
    pub metrics: ModelCallMetrics,
}
#[derive(Clone, Debug, thiserror::Error)]
#[error("{error}")]
pub struct ModelCallError {
    pub error: ModelError,
    pub metrics: Box<ModelCallMetrics>,
}
impl ModelCallError {
    pub fn new(error: ModelError, metrics: ModelCallMetrics) -> Self {
        Self {
            error,
            metrics: Box::new(metrics),
        }
    }
}
impl ModelCallMetrics {
    pub fn request(request: &ModelRequest, metadata: GatewayMetadata) -> Self {
        Self {
            request_hash: request_hash(request),
            metadata,
            ..Default::default()
        }
    }
}
pub fn request_hash(request: &ModelRequest) -> String {
    hash(&serde_json::to_vec(request).expect("request serializable"))
}
pub(crate) fn hash(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
/// Rates are integer micro-USD PER MILLION tokens. A separate reasoning rate
/// replaces that subset of the output rate, never adds the same tokens twice.
/// None cache rates means the host declares no separate cache billing for that model.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ModelPrice {
    pub provider: String,
    pub model: String,
    pub input_microusd_per_million: u64,
    pub cache_read_microusd_per_million: Option<u64>,
    pub cache_write_microusd_per_million_by_ttl: BTreeMap<String, u64>,
    pub output_microusd_per_million: u64,
    pub reasoning_microusd_per_million: Option<u64>,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct PriceTable {
    pub version: String,
    pub entries: Vec<ModelPrice>,
}
impl PriceTable {
    pub fn validate(&self) -> Result<(), ModelError> {
        let mut keys = BTreeSet::new();
        if self.version.trim().is_empty() || self.entries.is_empty() {
            return Err(ModelError::Accounting(
                "versioned nonempty price table required".into(),
            ));
        }
        for entry in &self.entries {
            if entry.provider.trim().is_empty()
                || entry.model.trim().is_empty()
                || !keys.insert((&entry.provider, &entry.model))
            {
                return Err(ModelError::Accounting(
                    "unique provider/model price entries required".into(),
                ));
            }
        }
        Ok(())
    }
    pub fn content_hash(&self) -> Result<String, ModelError> {
        self.validate()?;
        let mut canonical = self.clone();
        canonical
            .entries
            .sort_by(|a, b| (&a.provider, &a.model).cmp(&(&b.provider, &b.model)));
        Ok(hash(
            &serde_json::to_vec(&canonical).expect("price serializable"),
        ))
    }
    pub fn price(
        &self,
        provider: &str,
        model: &str,
        usage: &UsageBreakdown,
    ) -> Result<Option<u64>, ModelError> {
        self.validate()?;
        usage.validate()?;
        let entry = self.entry(provider, model)?;
        if usage.estimated {
            return Ok(None);
        }
        let mut input = usage.input_tokens;
        let mut numerator = 0_u128;
        if let Some(rate) = entry.cache_read_microusd_per_million {
            let Some(tokens) = usage.cache_read_tokens else {
                return Ok(None);
            };
            input -= tokens;
            numerator = numerator.saturating_add(u128::from(tokens) * u128::from(rate));
        }
        if !entry.cache_write_microusd_per_million_by_ttl.is_empty() {
            let Some(tokens) = usage.cache_write_tokens else {
                return Ok(None);
            };
            if tokens > 0 {
                let by_ttl = if usage.cache_write_by_ttl.is_empty() {
                    BTreeMap::from([("unspecified".into(), tokens)])
                } else {
                    usage.cache_write_by_ttl.clone()
                };
                if by_ttl.values().sum::<u64>() != tokens {
                    return Ok(None);
                }
                for (ttl, tokens) in by_ttl {
                    let Some(rate) = entry.cache_write_microusd_per_million_by_ttl.get(&ttl) else {
                        return Ok(None);
                    };
                    numerator = numerator.saturating_add(u128::from(tokens) * u128::from(*rate));
                }
                input -= tokens;
            }
        }
        numerator = numerator
            .saturating_add(u128::from(input) * u128::from(entry.input_microusd_per_million));
        let mut output = usage.output_tokens;
        if let Some(rate) = entry.reasoning_microusd_per_million {
            let Some(tokens) = usage.reasoning_tokens else {
                return Ok(None);
            };
            output -= tokens;
            numerator = numerator.saturating_add(u128::from(tokens) * u128::from(rate));
        }
        numerator = numerator
            .saturating_add(u128::from(output) * u128::from(entry.output_microusd_per_million));
        Ok(Some(round_microusd(numerator)?))
    }
    pub fn upper_bound(
        &self,
        provider: &str,
        model: &str,
        input: u64,
        output: u64,
    ) -> Result<u64, ModelError> {
        let e = self.entry(provider, model)?;
        let input_rate = std::iter::once(e.input_microusd_per_million)
            .chain(e.cache_read_microusd_per_million)
            .chain(e.cache_write_microusd_per_million_by_ttl.values().copied())
            .max()
            .unwrap();
        let output_rate = e
            .output_microusd_per_million
            .max(e.reasoning_microusd_per_million.unwrap_or(0));
        round_microusd(
            (u128::from(input) * u128::from(input_rate))
                .saturating_add(u128::from(output) * u128::from(output_rate)),
        )
    }
    fn entry(&self, provider: &str, model: &str) -> Result<&ModelPrice, ModelError> {
        self.entries
            .iter()
            .find(|entry| entry.provider == provider && entry.model == model)
            .ok_or_else(|| {
                ModelError::Accounting("provider/model missing from approved price table".into())
            })
    }
}
fn round_microusd(value: u128) -> Result<u64, ModelError> {
    u64::try_from(value.div_ceil(1_000_000))
        .map_err(|_| ModelError::Accounting("cost exceeds supported integer range".into()))
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ModelCallBound {
    pub max_tokens: u64,
    pub max_cost_microusd: u64,
}
