use crate::resources::ResourceConfig;
use excalibur_vault::Redactor;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::Arc,
};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct RunMetadata {
    pub agent_name: String,
    pub agent_version: String,
    pub excalibur_commit: Option<String>,
    pub instruction: String,
    pub profile: String,
    pub profile_version: Option<String>,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub role: Option<String>,
    pub reasoning_effort: Option<String>,
    pub trial_id: Option<String>,
    pub task_id: Option<String>,
    pub task_digest: Option<String>,
    pub benchmark: Option<String>,
    pub harbor_version: Option<String>,
    pub configuration: Value,
    pub configuration_hash: Option<String>,
    pub tool_catalog_hash: Option<String>,
    pub skill_catalog_hash: Option<String>,
    pub seed: Option<u64>,
    pub temperature: Option<f64>,
    pub environment: Value,
    pub extra: BTreeMap<String, Value>,
}
impl Default for RunMetadata {
    fn default() -> Self {
        Self {
            agent_name: "excalibur".into(),
            agent_version: env!("CARGO_PKG_VERSION").into(),
            excalibur_commit: None,
            instruction: String::new(),
            profile: "development".into(),
            profile_version: None,
            provider: None,
            model: None,
            role: None,
            reasoning_effort: None,
            trial_id: None,
            task_id: None,
            task_digest: None,
            benchmark: None,
            harbor_version: None,
            configuration: serde_json::json!({}),
            configuration_hash: None,
            tool_catalog_hash: None,
            skill_catalog_hash: None,
            seed: None,
            temperature: None,
            environment: serde_json::json!({}),
            extra: BTreeMap::new(),
        }
    }
}
#[derive(Clone)]
pub struct TelemetryConfig {
    pub directory: PathBuf,
    pub metadata: RunMetadata,
    /// Primary trajectory/context location; other aliases remain in directory.
    pub artifact_root: Option<PathBuf>,
    pub redactor: Redactor,
    /// Explicit host clock, Unix milliseconds. It is never model/tool input.
    pub clock: Arc<dyn Fn() -> i64 + Send + Sync>,
    pub resources: ResourceConfig,
    pub export_otel: bool,
}
impl std::fmt::Debug for TelemetryConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TelemetryConfig")
            .field("directory", &self.directory)
            .field("redactor", &"[REDACTED]")
            .finish_non_exhaustive()
    }
}
impl TelemetryConfig {
    pub fn new(directory: impl AsRef<Path>, metadata: RunMetadata, redactor: Redactor) -> Self {
        Self {
            directory: directory.as_ref().to_path_buf(),
            metadata,
            artifact_root: None,
            redactor,
            clock: Arc::new(|| {
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_or(0, |v| v.as_millis().min(i64::MAX as u128) as i64)
            }),
            resources: ResourceConfig::default(),
            export_otel: true,
        }
    }
}
