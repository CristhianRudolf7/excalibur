//! Provider cache controls, opaque namespaces and bounded sibling warming.
use crate::{ModelCallError, ModelError, ModelGateway, ObservedResponse, StreamDelta, StreamSink};
use excalibur_providers::Provider;
use excalibur_types::runtime::{CacheTtl, ModelRequest, PromptCache};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct CacheIdentity {
    pub namespace_key: String,
}
impl CacheIdentity {
    /// Length prefixes prevent ambiguous identities; raw principal/scope strings
    /// are neither returned nor stored in prompts or provider cache keys.
    pub fn new(principal: &str, role: &str, scope: &str) -> Result<Self, ModelError> {
        if [principal, role, scope]
            .iter()
            .any(|v| v.is_empty() || v.len() > 4096)
        {
            return Err(ModelError::Configuration(
                "cache identity requires bounded principal, role and scope".into(),
            ));
        }
        let mut hash = Sha256::new();
        hash.update(b"excalibur-prompt-cache-v1\0");
        for value in [principal, role, scope] {
            hash.update((value.len() as u64).to_be_bytes());
            hash.update(value.as_bytes());
        }
        Ok(Self {
            namespace_key: format!("{:x}", hash.finalize()),
        })
    }
}
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CacheStrategy {
    #[default]
    Unknown,
    ExplicitBreakpoints,
    Automatic,
    ServerPrefix,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct CacheSettings {
    pub strategy: CacheStrategy,
    pub max_breakpoints: Option<usize>,
    pub lookback_blocks: Option<usize>,
    pub first_delta_warming: bool,
    pub namespace_key_supported: bool,
}
impl CacheSettings {
    pub fn provider(provider: Provider) -> Self {
        match provider {
            Provider::Anthropic => Self {
                strategy: CacheStrategy::ExplicitBreakpoints,
                max_breakpoints: Some(4),
                lookback_blocks: Some(20),
                first_delta_warming: true,
                namespace_key_supported: false,
            },
            Provider::OpenAi => Self {
                strategy: CacheStrategy::Automatic,
                namespace_key_supported: true,
                ..Default::default()
            },
            Provider::GoogleAi | Provider::Vertex => Self {
                strategy: CacheStrategy::Automatic,
                ..Default::default()
            },
            _ => Self {
                strategy: CacheStrategy::ServerPrefix,
                ..Default::default()
            },
        }
    }
}
pub fn ttl_seconds(ttl: CacheTtl) -> u64 {
    match ttl {
        CacheTtl::FiveMinutes => 300,
        CacheTtl::OneHour => 3600,
    }
}
pub(crate) fn validate_cache(request: &ModelRequest) -> Result<(), ModelError> {
    let Some(cache) = &request.cache else {
        return Ok(());
    };
    let valid_hash = |s: &str| {
        s.len() == 64
            && s.bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    };
    if !valid_hash(&cache.namespace_key) || !valid_hash(&cache.stable_prefix_hash) {
        return Err(ModelError::Configuration(
            "cache namespace/prefix must be opaque SHA-256 values".into(),
        ));
    }
    if cache.breakpoints.len() > 4
        || cache
            .breakpoints
            .iter()
            .any(|b| b.message_index >= request.messages.len())
        || cache
            .breakpoints
            .windows(2)
            .any(|b| b[0].message_index >= b[1].message_index)
    {
        return Err(ModelError::Configuration(
            "cache boundaries must be ordered, unique and at most four valid message indices"
                .into(),
        ));
    }
    Ok(())
}
pub(crate) fn marked(cache: &Option<PromptCache>, index: usize) -> bool {
    cache
        .as_ref()
        .is_some_and(|c| c.breakpoints.iter().any(|b| b.message_index == index))
}
pub(crate) fn control(cache: &PromptCache) -> serde_json::Value {
    serde_json::json!({"type":"ephemeral","ttl":match cache.ttl{CacheTtl::FiveMinutes=>"5m",CacheTtl::OneHour=>"1h"}})
}
/// Prefix equality is eligibility, never evidence that a provider cache hit.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct CacheObservation {
    pub unchanged_prefix: bool,
    pub input_tokens: u64,
    pub cache_read_tokens: Option<u64>,
    pub cache_write_tokens: Option<u64>,
    pub estimated: bool,
}
impl CacheObservation {
    pub fn compare(
        previous: Option<(&ModelRequest, &ObservedResponse)>,
        current: &ModelRequest,
        observed: &ObservedResponse,
    ) -> Self {
        let unchanged_prefix = previous.is_some_and(|(request, response)| {
            response.metrics.metadata.configuration_hash
                == observed.metrics.metadata.configuration_hash
                && request
                    .cache
                    .as_ref()
                    .zip(current.cache.as_ref())
                    .is_some_and(|(a, b)| {
                        a.namespace_key == b.namespace_key
                            && a.stable_prefix_hash == b.stable_prefix_hash
                    })
        });
        Self {
            unchanged_prefix,
            input_tokens: observed.metrics.usage.input_tokens,
            cache_read_tokens: observed.metrics.usage.cache_read_tokens,
            cache_write_tokens: observed.metrics.usage.cache_write_tokens,
            estimated: observed.metrics.usage.estimated,
        }
    }
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum FanoutRelease {
    Parallel,
    FirstDelta,
    CompletedWithoutDelta,
    LeaderFailed,
}
#[derive(Debug)]
pub struct FanoutBatch {
    /// None means no provider call was made. Failed calls retain actual metrics.
    pub results: Vec<Option<Result<ObservedResponse, ModelCallError>>>,
    pub release: FanoutRelease,
}
/// Start the first real sibling once, then release its siblings when the provider
/// exposes the first useful streamed delta. No extra prewarm request is issued.
/// Dropping this future aborts every locally spawned request through JoinSet.
pub async fn complete_fanout(
    gateway: Arc<dyn ModelGateway>,
    requests: Vec<ModelRequest>,
    concurrency: usize,
) -> Result<FanoutBatch, ModelError> {
    if requests.is_empty() || requests.len() > 64 || concurrency == 0 || concurrency > 64 {
        return Err(ModelError::Configuration(
            "fanout requires 1..64 requests and concurrency 1..64".into(),
        ));
    }
    for request in &requests {
        validate_cache(request)?;
    }
    let warm = gateway.cache_settings().first_delta_warming && requests.len() > 1;
    if warm {
        let first = requests[0].cache.as_ref().ok_or_else(|| {
            ModelError::Configuration(
                "cache warming requires explicit shared prefix metadata".into(),
            )
        })?;
        if requests.iter().any(|r| {
            r.cache.as_ref().is_none_or(|c| {
                c.namespace_key != first.namespace_key
                    || c.stable_prefix_hash != first.stable_prefix_hash
            })
        }) {
            return Err(ModelError::Configuration(
                "siblings must share one cache namespace and stable prefix".into(),
            ));
        }
    }
    let count = requests.len();
    let mut results = (0..count).map(|_| None).collect::<Vec<_>>();
    let permits = Arc::new(tokio::sync::Semaphore::new(concurrency));
    let mut tasks = tokio::task::JoinSet::new();
    let mut requests = requests.into_iter().enumerate();
    let mut release = FanoutRelease::Parallel;
    if warm {
        let (index, first) = requests.next().unwrap();
        let gateway = gateway.clone();
        let permits = permits.clone();
        let (tx, rx) = tokio::sync::oneshot::channel();
        let sender = Arc::new(Mutex::new(Some(tx)));
        let observed = Arc::new(AtomicBool::new(false));
        let seen = observed.clone();
        let sink: StreamSink = Arc::new(move |delta| {
            let useful = match delta {
                StreamDelta::Text { delta, .. } => !delta.is_empty(),
                StreamDelta::Tool {
                    id,
                    name,
                    arguments,
                    ..
                } => !id.is_empty() || !name.is_empty() || !arguments.is_empty(),
                _ => false,
            };
            if useful
                && !seen.swap(true, Ordering::SeqCst)
                && let Some(sender) = sender.lock().unwrap().take()
            {
                let _ = sender.send(());
            }
        });
        tasks.spawn(async move {
            let _permit = permits
                .acquire_owned()
                .await
                .expect("fanout semaphore owned by batch");
            (
                index,
                gateway.complete_cache_warm_observed(first, sink).await,
            )
        });
        tokio::select! {signal=rx=>{if signal.is_ok(){release=FanoutRelease::FirstDelta}else{let(index,result)=tasks.join_next().await.ok_or_else(||ModelError::Configuration("fanout leader missing".into()))?.map_err(|_|ModelError::Consumer)?;let failed=result.is_err();results[index]=Some(result);if failed{return Ok(FanoutBatch{results,release:FanoutRelease::LeaderFailed})}release=FanoutRelease::CompletedWithoutDelta;}},leader=tasks.join_next()=>{let(index,result)=leader.ok_or_else(||ModelError::Configuration("fanout leader missing".into()))?.map_err(|_|ModelError::Consumer)?;let failed=result.is_err();results[index]=Some(result);if failed{return Ok(FanoutBatch{results,release:FanoutRelease::LeaderFailed})}release=if observed.load(Ordering::SeqCst){FanoutRelease::FirstDelta}else{FanoutRelease::CompletedWithoutDelta};}}
    }
    for (index, request) in requests {
        let gateway = gateway.clone();
        let permits = permits.clone();
        tasks.spawn(async move {
            let _permit = permits
                .acquire_owned()
                .await
                .expect("fanout semaphore owned by batch");
            (index, gateway.complete_observed(request).await)
        });
    }
    while let Some(result) = tasks.join_next().await {
        let (index, result) = result.map_err(|_| ModelError::Consumer)?;
        results[index] = Some(result);
    }
    Ok(FanoutBatch { results, release })
}
