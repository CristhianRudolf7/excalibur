//! Fail-closed Linux process confinement. Configuration belongs to the trusted host.
//! Writable projects use a prepared hierarchy; general project/network profiles
//! require additional policy support and are never silently substituted.
use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
};
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SandboxProfile {
    ReadOnly,
    ProjectRestricted,
    ProjectNet,
    FileAnalysis,
    ToolPackage,
    Eval,
}
#[derive(Clone, Debug)]
pub struct SandboxConfig {
    pub profile: SandboxProfile,
    /// The working directory is selected by the host, never inferred from a model argument.
    pub workspace: PathBuf,
    pub read_roots: Vec<PathBuf>,
    pub write_roots: Vec<PathBuf>,
    pub hidden_paths: Vec<PathBuf>,
    pub bubblewrap: Option<PathBuf>,
}
impl SandboxConfig {
    pub fn project(root: impl Into<PathBuf>) -> Self {
        let root = root.into();
        Self {
            profile: SandboxProfile::ProjectRestricted,
            workspace: root.clone(),
            read_roots: vec![root.clone()],
            write_roots: vec![root],
            hidden_paths: Vec::new(),
            bubblewrap: None,
        }
    }
    pub fn read_only(root: impl Into<PathBuf>) -> Self {
        let mut s = Self::project(root);
        s.profile = SandboxProfile::ReadOnly;
        s.write_roots.clear();
        s
    }
    pub fn file_analysis(root: impl Into<PathBuf>) -> Self {
        let mut s = Self::read_only(root);
        s.profile = SandboxProfile::FileAnalysis;
        s
    }
    pub fn tool_package(root: impl Into<PathBuf>) -> Self {
        let mut s = Self::project(root);
        s.profile = SandboxProfile::ToolPackage;
        s
    }
    pub fn restricted_by(&self, base: &Self) -> Result<Self, String> {
        let mut next = self.clone();
        next.workspace = canonical(&next.workspace)?;
        if next.workspace != canonical(&base.workspace)? {
            return Err("sandbox working directory differs from host configuration".into());
        }
        let readable = base
            .read_roots
            .iter()
            .chain(base.write_roots.iter())
            .map(|p| canonical(p))
            .collect::<Result<Vec<_>, _>>()?;
        let writable = base
            .write_roots
            .iter()
            .map(|p| canonical(p))
            .collect::<Result<Vec<_>, _>>()?;
        for root in &next.read_roots {
            let root = canonical(root)?;
            if !readable.iter().any(|base| root.starts_with(base)) {
                return Err("sandbox read authority exceeds host boundary".into());
            }
        }
        for root in &next.write_roots {
            let root = canonical(root)?;
            if !writable.iter().any(|base| root.starts_with(base)) {
                return Err("sandbox write authority exceeds host boundary".into());
            }
        }
        next.hidden_paths.extend(base.hidden_paths.clone());
        next.hidden_paths.sort();
        next.hidden_paths.dedup();
        next.bubblewrap = base.bubblewrap.clone();
        Ok(next)
    }
}
fn canonical(path: &Path) -> Result<PathBuf, String> {
    if !path.is_absolute() {
        return Err("sandbox paths must be absolute".into());
    }
    let result = path.canonicalize().map_err(|e| e.to_string())?;
    if result == Path::new("/") {
        return Err("host filesystem root cannot be a project boundary".into());
    }
    Ok(result)
}
/// The host explicitly provisions these boundaries; command execution never creates them.
pub const PROTECTED_DIRECTORIES: &[&str] = &[
    ".git",
    ".agents",
    ".excalibur",
    ".codex",
    ".aws",
    ".ssh",
    ".github",
    ".gitlab",
    ".circleci",
    ".config",
];
const MAX_DIRECTORIES: usize = 4096;
const MAX_ENTRIES: usize = 100_000;
fn ordinary_directories(root: &Path, excluded: &[PathBuf]) -> Result<Vec<PathBuf>, String> {
    let mut pending = vec![root.to_path_buf()];
    let mut found = Vec::new();
    let mut count = 0;
    while let Some(dir) = pending.pop() {
        if found.len() >= MAX_DIRECTORIES {
            return Err(
                "sandbox hierarchy exceeds 4096 directories; use a narrower configured root".into(),
            );
        }
        for entry in std::fs::read_dir(&dir).map_err(|e| e.to_string())? {
            count += 1;
            if count > MAX_ENTRIES {
                return Err(
                    "sandbox hierarchy exceeds 100000 entries; use a narrower configured root"
                        .into(),
                );
            }
            let entry = entry.map_err(|e| e.to_string())?;
            if excluded.iter().any(|path| entry.path().starts_with(path)) {
                continue;
            }
            let name = entry.file_name();
            if PROTECTED_DIRECTORIES.iter().any(|n| name == *n) {
                continue;
            }
            let kind = entry.file_type().map_err(|e| e.to_string())?;
            if kind.is_dir() {
                pending.push(entry.path());
            }
        }
        found.push(dir);
    }
    found.sort();
    Ok(found)
}
/// Explicit host setup, outside the agent tool catalog. Creates empty protective
/// directories only when absent; preserves existing metadata and file content.
/// It is not invoked automatically by `exec` or `LocalTools::new`.
pub fn prepare_project_boundary(root: &Path) -> Result<(), String> {
    let root = canonical(root)?;
    let directories = ordinary_directories(&root, &[])?;
    for directory in directories {
        for name in PROTECTED_DIRECTORIES {
            let path = directory.join(name);
            match std::fs::symlink_metadata(&path) {
                Ok(m) if m.file_type().is_symlink() => {
                    return Err("protected metadata cannot be a symlink".into());
                }
                Ok(_) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    std::fs::create_dir(&path).map_err(|e| e.to_string())?
                }
                Err(e) => return Err(e.to_string()),
            }
        }
    }
    Ok(())
}
fn protected_paths(
    root: &Path,
    require_prepared: bool,
    excluded: &[PathBuf],
) -> Result<Vec<PathBuf>, String> {
    let mut paths = BTreeSet::new();
    for dir in ordinary_directories(root, excluded)? {
        for name in PROTECTED_DIRECTORIES {
            let path = dir.join(name);
            match std::fs::symlink_metadata(&path) {
                Ok(m) if m.file_type().is_symlink() => {
                    return Err("protected metadata cannot be a symlink".into());
                }
                Ok(_) => {
                    paths.insert(path);
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound && !require_prepared => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    return Err(format!(
                        "project sandbox requires host-provisioned metadata boundaries: {}",
                        path.display()
                    ));
                }
                Err(e) => return Err(e.to_string()),
            }
        }
        for entry in std::fs::read_dir(&dir).map_err(|e| e.to_string())? {
            let entry = entry.map_err(|e| e.to_string())?;
            if entry.file_name() == ".gitlab-ci.yml" || entry.file_name() == "Jenkinsfile" {
                paths.insert(entry.path());
            }
        }
    }
    Ok(paths.into_iter().collect())
}
#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
pub use linux::PreparedSandbox;
#[cfg(not(target_os = "linux"))]
pub struct PreparedSandbox;
#[cfg(not(target_os = "linux"))]
impl PreparedSandbox {
    pub fn new(_config: &SandboxConfig) -> Result<Self, String> {
        Err("a mandatory native sandbox is not implemented on this target".into())
    }
}
