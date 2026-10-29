//! Layered configuration; hosts choose platform paths and lifecycle policy.
//!
//! The shared core never infers a user's home, operating-system directories,
//! credentials, or the mount's network/local status. Adapters supply those facts.
use serde::{Deserialize, Serialize};
use std::{
    fs,
    io::Write,
    path::{Component, Path, PathBuf},
};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("configuration I/O failed at {path}: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("invalid TOML configuration at {path}: {source}")]
    Parse {
        path: PathBuf,
        source: toml::de::Error,
    },
    #[error("invalid merged configuration: {0}")]
    Decode(toml::de::Error),
    #[error("cannot serialize configuration: {0}")]
    Encode(#[from] toml::ser::Error),
    #[error("invalid configuration: {0}")]
    Invalid(&'static str),
    #[error("configuration key {key} is forbidden in {layer:?} layer at {path}")]
    ForbiddenLayerKey {
        layer: Layer,
        key: String,
        path: PathBuf,
    },
}

/// Initial storage limits. Paths, mmap permission and mount classification belong to hosts.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct StorageConfig {
    pub queue_capacity: usize,
    pub read_connections: usize,
    pub busy_timeout_ms: u64,
    pub busy_retries: u32,
    pub group_commit_ms: u64,
    pub group_commit_max_operations: usize,
    pub wal_alert_bytes: u64,
    pub writer_cache_kib: u32,
    pub reader_cache_kib: u32,
    pub mmap_bytes: u64,
    pub network_filesystem: bool,
}
impl Default for StorageConfig {
    fn default() -> Self {
        Self {
            queue_capacity: 1024,
            // At least two connections preserve a dedicated interactive lane on one-core hosts.
            read_connections: std::thread::available_parallelism()
                .map_or(2, usize::from)
                .clamp(2, 4),
            busy_timeout_ms: 5_000,
            busy_retries: 3,
            group_commit_ms: 5,
            group_commit_max_operations: 200,
            wal_alert_bytes: 256 * 1024 * 1024,
            writer_cache_kib: 8 * 1024,
            reader_cache_kib: 2 * 1024,
            mmap_bytes: 0,
            network_filesystem: false,
        }
    }
}
impl StorageConfig {
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.queue_capacity == 0 {
            return Err(ConfigError::Invalid("queue capacity must be positive"));
        }
        if !(2..=256).contains(&self.read_connections) {
            return Err(ConfigError::Invalid(
                "read pool needs 2..=256 connections including its interactive lane",
            ));
        }
        if self.busy_timeout_ms == 0 || self.busy_timeout_ms > 300_000 {
            return Err(ConfigError::Invalid(
                "busy timeout must be 1..=300000 milliseconds",
            ));
        }
        if self.busy_retries > 16 {
            return Err(ConfigError::Invalid("busy retries must not exceed 16"));
        }
        if self.group_commit_ms == 0 || self.group_commit_ms > 5 {
            return Err(ConfigError::Invalid(
                "group commit budget must be 1..=5 milliseconds",
            ));
        }
        if self.group_commit_max_operations == 0 || self.group_commit_max_operations > 200 {
            return Err(ConfigError::Invalid(
                "group commit must contain 1..=200 operations",
            ));
        }
        if self.wal_alert_bytes == 0 || self.writer_cache_kib == 0 || self.reader_cache_kib == 0 {
            return Err(ConfigError::Invalid(
                "WAL and cache limits must be positive",
            ));
        }
        if self.writer_cache_kib > i32::MAX as u32
            || self.reader_cache_kib > i32::MAX as u32
            || self.mmap_bytes > i64::MAX as u64
        {
            return Err(ConfigError::Invalid("SQLite memory limit is out of range"));
        }
        Ok(())
    }
    /// Host adapters use this for suspended/mobile lifecycle profiles, never OS guessing.
    pub fn disable_mmap(&mut self) {
        self.mmap_bytes = 0;
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Installation root supplied explicitly by the embedding host.
    pub data_dir: PathBuf,
    pub storage: StorageConfig,
}
impl Config {
    pub fn defaults(data_dir: impl Into<PathBuf>) -> Self {
        Self {
            data_dir: data_dir.into(),
            storage: StorageConfig::default(),
        }
    }
    pub fn validate(&self) -> Result<(), ConfigError> {
        if !self.data_dir.is_absolute() {
            return Err(ConfigError::Invalid("data directory must be absolute"));
        }
        self.storage.validate()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Layer {
    System,
    User,
    Project,
    Scope,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Source {
    pub layer: Layer,
    pub path: PathBuf,
}
#[derive(Clone, Debug)]
pub struct LoadedConfig {
    pub config: Config,
    pub sources: Vec<Source>,
}

/// Missing optional files are skipped; malformed or unreadable files are errors.
#[derive(Clone, Debug, Default)]
pub struct ConfigLayers {
    pub system: Option<PathBuf>,
    pub user: Option<PathBuf>,
    pub project: Option<PathBuf>,
    pub scope: Option<PathBuf>,
}
impl ConfigLayers {
    /// Load absolute host paths. Relative `data_dir` values declared by a trusted
    /// file resolve against that file's directory; the process cwd is never consulted.
    pub fn load(&self, mut defaults: Config) -> Result<LoadedConfig, ConfigError> {
        defaults.validate()?;
        defaults.data_dir = normalize_absolute(&defaults.data_dir)?;
        for path in [&self.system, &self.user, &self.project, &self.scope]
            .into_iter()
            .flatten()
        {
            if !path.is_absolute() {
                return Err(ConfigError::Invalid(
                    "configuration file path must be absolute; use load_from with an explicit base",
                ));
            }
        }
        let mut merged = toml::Value::try_from(defaults)?;
        let mut sources = Vec::new();
        for (layer, path) in [
            (Layer::System, &self.system),
            (Layer::User, &self.user),
            (Layer::Project, &self.project),
            (Layer::Scope, &self.scope),
        ] {
            let Some(path) = path else { continue };
            let path = normalize_absolute(path)?;
            let text = match fs::read_to_string(&path) {
                Ok(text) => text,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(source) => return Err(ConfigError::Io { path, source }),
            };
            let mut value: toml::Value =
                toml::from_str(&text).map_err(|source| ConfigError::Parse {
                    path: path.clone(),
                    source,
                })?;
            validate_layer_keys(layer, &path, &value)?;
            if let Some(data_dir) = value.get_mut("data_dir") {
                let raw_path = data_dir
                    .as_str()
                    .ok_or(ConfigError::Invalid("data_dir must be a path string"))?;
                let resolved = resolve_from(
                    Path::new(raw_path),
                    path.parent().ok_or(ConfigError::Invalid(
                        "configuration file needs a parent directory",
                    ))?,
                )?;
                *data_dir = toml::Value::try_from(resolved)?;
            }
            merge(&mut merged, value);
            sources.push(Source { layer, path });
        }
        let config: Config = merged.try_into().map_err(ConfigError::Decode)?;
        config.validate()?;
        Ok(LoadedConfig { config, sources })
    }

    /// Resolve relative host-supplied defaults and file paths against an explicit
    /// absolute base, then apply `load`. File-declared paths still use their owner file.
    pub fn load_from(
        &self,
        mut defaults: Config,
        base: &Path,
    ) -> Result<LoadedConfig, ConfigError> {
        let base = normalize_absolute(base)?;
        defaults.data_dir = resolve_from(&defaults.data_dir, &base)?;
        let resolve_layer = |path: &Option<PathBuf>| {
            path.as_ref()
                .map(|path| resolve_from(path, &base))
                .transpose()
        };
        let layers = Self {
            system: resolve_layer(&self.system)?,
            user: resolve_layer(&self.user)?,
            project: resolve_layer(&self.project)?,
            scope: resolve_layer(&self.scope)?,
        };
        layers.load(defaults)
    }
}

/// All current settings are installation-global. Lower-trust project/scope files
/// intentionally have an empty key allowlist until genuinely local settings exist.
fn validate_layer_keys(layer: Layer, path: &Path, value: &toml::Value) -> Result<(), ConfigError> {
    if matches!(layer, Layer::System | Layer::User) {
        return Ok(());
    }
    let Some(table) = value.as_table() else {
        return Err(ConfigError::Invalid("configuration root must be a table"));
    };
    if let Some((key, value)) = table.iter().next() {
        // Report a concrete leaf when available, including wrong-typed values.
        let mut key = key.clone();
        let mut value = value;
        while let Some((nested, child)) = value.as_table().and_then(|t| t.iter().next()) {
            key.push('.');
            key.push_str(nested);
            value = child;
        }
        return Err(ConfigError::ForbiddenLayerKey {
            layer,
            key,
            path: path.to_owned(),
        });
    }
    Ok(())
}

fn resolve_from(path: &Path, base: &Path) -> Result<PathBuf, ConfigError> {
    if path.as_os_str().is_empty() {
        return Err(ConfigError::Invalid("path is empty"));
    }
    normalize_absolute(&if path.is_absolute() {
        path.to_owned()
    } else {
        base.join(path)
    })
}

/// Lexical normalization permits new installation directories that do not yet exist.
/// It does not canonicalize symlinks or pretend to enforce a filesystem sandbox.
fn normalize_absolute(path: &Path) -> Result<PathBuf, ConfigError> {
    if !path.is_absolute() {
        return Err(ConfigError::Invalid("path must be absolute"));
    }
    let mut result = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                result.pop();
            }
            other => result.push(other.as_os_str()),
        }
    }
    Ok(result)
}

fn merge(target: &mut toml::Value, value: toml::Value) {
    match (target, value) {
        (toml::Value::Table(target), toml::Value::Table(value)) => {
            for (key, value) in value {
                match target.get_mut(&key) {
                    Some(existing) => merge(existing, value),
                    None => {
                        target.insert(key, value);
                    }
                }
            }
        }
        (target, value) => *target = value,
    }
}

/// Create explicit installation directories and a TOML file without overwriting an existing file.
/// Returns `false` when configuration already exists; the caller can then load its layers.
pub fn initialize(config: &Config, config_path: &Path) -> Result<bool, ConfigError> {
    config.validate()?;
    if !config_path.is_absolute() {
        return Err(ConfigError::Invalid(
            "configuration file path must be absolute",
        ));
    }
    fs::create_dir_all(&config.data_dir).map_err(|source| ConfigError::Io {
        path: config.data_dir.clone(),
        source,
    })?;
    if let Some(parent) = config_path.parent().filter(|p| !p.as_os_str().is_empty()) {
        fs::create_dir_all(parent).map_err(|source| ConfigError::Io {
            path: parent.to_owned(),
            source,
        })?;
    }
    let text = toml::to_string_pretty(config)?;
    let mut file = match fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(config_path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => return Ok(false),
        Err(source) => {
            return Err(ConfigError::Io {
                path: config_path.to_owned(),
                source,
            });
        }
    };
    file.write_all(text.as_bytes())
        .and_then(|()| file.sync_all())
        .map_err(|source| ConfigError::Io {
            path: config_path.to_owned(),
            source,
        })?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn trusted_layers_merge_recursively_and_empty_local_layers_preserve_them() {
        let dir = tempfile::tempdir().unwrap();
        let mut paths = Vec::new();
        for (name, text) in [
            (
                "system",
                "[storage]\nqueue_capacity=10\nread_connections=3\n",
            ),
            (
                "user",
                "[storage]\nqueue_capacity=20\nnetwork_filesystem=true\n",
            ),
            (
                "project",
                "# No project-local settings are currently defined.\n",
            ),
            ("scope", ""),
        ] {
            let path = dir.path().join(format!("{name}.toml"));
            fs::write(&path, text).unwrap();
            paths.push(path);
        }
        let layers = ConfigLayers {
            system: Some(paths[0].clone()),
            user: Some(paths[1].clone()),
            project: Some(paths[2].clone()),
            scope: Some(paths[3].clone()),
        };
        let loaded = layers
            .load(Config::defaults(dir.path().join("data")))
            .unwrap();
        assert_eq!(loaded.config.storage.queue_capacity, 20);
        assert_eq!(loaded.config.storage.read_connections, 3);
        assert_eq!(loaded.config.storage.busy_timeout_ms, 5_000);
        assert!(loaded.config.storage.network_filesystem);
        assert_eq!(
            loaded.sources.iter().map(|s| s.layer).collect::<Vec<_>>(),
            vec![Layer::System, Layer::User, Layer::Project, Layer::Scope]
        );
    }
    #[test]
    fn initialization_preserves_existing_configuration() {
        let dir = tempfile::tempdir().unwrap();
        let config = Config::defaults(dir.path().join("principal"));
        let path = dir.path().join("config/user.toml");
        assert!(initialize(&config, &path).unwrap());
        fs::write(&path, "[storage]\nqueue_capacity=64\n").unwrap();
        assert!(!initialize(&config, &path).unwrap());
        let layers = ConfigLayers {
            user: Some(path),
            ..Default::default()
        };
        assert_eq!(
            layers.load(config).unwrap().config.storage.queue_capacity,
            64
        );
    }
    #[test]
    fn invalid_or_unknown_settings_fail_without_silent_fallback() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let layers = ConfigLayers {
            user: Some(path.clone()),
            ..Default::default()
        };
        for text in [
            "[storage]\nqueue_capacity=0",
            "[storage]\nqueue_capcity=1",
            "[storage]\ngroup_commit_ms=10",
            "[storage]\nread_connections=1",
            "[storage]\nbusy_retries=99",
            "bad = [",
        ] {
            fs::write(&path, text).unwrap();
            assert!(layers.load(Config::defaults(dir.path())).is_err(), "{text}");
        }
        fs::remove_file(path).unwrap();
        assert!(
            layers
                .load(Config::defaults(dir.path()))
                .unwrap()
                .sources
                .is_empty()
        );
    }

    #[test]
    fn system_and_user_accept_all_current_global_settings() {
        let dir = tempfile::tempdir().unwrap();
        let expected = Config::defaults(dir.path().join("trusted-data"));
        let path = dir.path().join("trusted.toml");
        fs::write(&path, toml::to_string(&expected).unwrap()).unwrap();
        for layer in [Layer::System, Layer::User] {
            let layers = ConfigLayers {
                system: (layer == Layer::System).then(|| path.clone()),
                user: (layer == Layer::User).then(|| path.clone()),
                ..Default::default()
            };
            assert_eq!(
                layers
                    .load(Config::defaults(dir.path().join("defaults")))
                    .unwrap()
                    .config,
                expected
            );
        }
    }

    #[test]
    fn project_and_scope_reject_every_global_key_with_a_typed_error() {
        let dir = tempfile::tempdir().unwrap();
        let user = dir.path().join("user.toml");
        fs::write(&user, "[storage]\nqueue_capacity=16\n").unwrap();
        let restricted = dir.path().join("restricted.toml");
        let all_storage = toml::Value::try_from(StorageConfig::default()).unwrap();
        let mut dangerous = vec![("data_dir".to_owned(), "data_dir='elsewhere'".to_owned())];
        for (key, value) in all_storage.as_table().unwrap() {
            dangerous.push((
                format!("storage.{key}"),
                format!("[storage]\n{key}={value}\n"),
            ));
        }
        dangerous.push(("storage".into(), "[storage]".into()));
        dangerous.push((
            "unknown".into(),
            "unknown='not an allowlisted setting'".into(),
        ));
        for layer in [Layer::Project, Layer::Scope] {
            for (key, text) in &dangerous {
                fs::write(&restricted, text).unwrap();
                let layers = ConfigLayers {
                    user: Some(user.clone()),
                    project: (layer == Layer::Project).then(|| restricted.clone()),
                    scope: (layer == Layer::Scope).then(|| restricted.clone()),
                    ..Default::default()
                };
                match layers.load(Config::defaults(dir.path().join("data"))) {
                    Err(ConfigError::ForbiddenLayerKey {
                        layer: actual,
                        key: actual_key,
                        path,
                    }) => {
                        assert_eq!(actual, layer);
                        assert_eq!(actual_key, *key);
                        assert_eq!(path, restricted);
                    }
                    other => panic!("{layer:?} {key} unexpectedly accepted or untyped: {other:?}"),
                }
            }
        }
    }

    #[test]
    fn trusted_file_paths_resolve_against_their_own_directory() {
        let dir = tempfile::tempdir().unwrap();
        let system_dir = dir.path().join("system");
        let user_dir = dir.path().join("user");
        fs::create_dir_all(&system_dir).unwrap();
        fs::create_dir_all(&user_dir).unwrap();
        let system = system_dir.join("system.toml");
        let user = user_dir.join("user.toml");
        fs::write(&system, "data_dir='../system-data/./state'\n").unwrap();
        fs::write(&user, "data_dir='./user-data/../persist'\n").unwrap();
        let layers = ConfigLayers {
            system: Some(system.clone()),
            user: Some(user.clone()),
            ..Default::default()
        };
        assert_eq!(
            layers
                .load(Config::defaults(dir.path().join("defaults")))
                .unwrap()
                .config
                .data_dir,
            user_dir.join("persist")
        );
        let layers = ConfigLayers {
            system: Some(system),
            ..Default::default()
        };
        assert_eq!(
            layers
                .load(Config::defaults(dir.path().join("defaults")))
                .unwrap()
                .config
                .data_dir,
            dir.path().join("system-data/state")
        );
    }

    #[test]
    fn implicit_cwd_paths_are_rejected_and_explicit_base_is_normalized() {
        let dir = tempfile::tempdir().unwrap();
        assert!(Config::defaults("relative").validate().is_err());
        assert!(
            ConfigLayers::default()
                .load(Config::defaults("relative"))
                .is_err()
        );
        let layers = ConfigLayers {
            user: Some("relative.toml".into()),
            ..Default::default()
        };
        assert!(layers.load(Config::defaults(dir.path())).is_err());
        assert!(
            layers
                .load_from(Config::defaults("data"), Path::new("relative-base"))
                .is_err()
        );
        let loaded = layers
            .load_from(Config::defaults("data/../stable"), dir.path())
            .unwrap();
        assert_eq!(loaded.config.data_dir, dir.path().join("stable"));
        assert!(initialize(&loaded.config, Path::new("relative-config.toml")).is_err());
    }

    #[test]
    fn explicit_base_resolution_is_invariant_across_process_working_directories() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("installation");
        fs::create_dir_all(base.join("settings")).unwrap();
        fs::write(
            base.join("settings/user.toml"),
            "data_dir='../persist/./state'\n",
        )
        .unwrap();
        for child_cwd in [dir.path().join("cwd-one"), dir.path().join("cwd-two")] {
            fs::create_dir_all(&child_cwd).unwrap();
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--ignored",
                    "--exact",
                    "tests::cwd_resolution_child",
                    "--nocapture",
                ])
                .env("EXCALIBUR_TEST_CONFIG_BASE", &base)
                .current_dir(child_cwd)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        }
    }

    #[test]
    #[ignore = "subprocess helper invoked by explicit_base_resolution_is_invariant_across_process_working_directories"]
    fn cwd_resolution_child() {
        let base = PathBuf::from(
            std::env::var_os("EXCALIBUR_TEST_CONFIG_BASE")
                .expect("parent supplies synthetic absolute base"),
        );
        let layers = ConfigLayers {
            user: Some("settings/user.toml".into()),
            ..Default::default()
        };
        let loaded = layers
            .load_from(Config::defaults("defaults/../data"), &base)
            .unwrap();
        assert_eq!(loaded.config.data_dir, base.join("persist/state"));
        assert_eq!(loaded.sources[0].path, base.join("settings/user.toml"));
    }
}
