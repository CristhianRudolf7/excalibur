//! Optional host-bound positive filesystem authority for multi-root tasks.
use crate::LocalTools;
use cap_std::fs::Dir;
use std::{
    path::{Path, PathBuf},
    sync::Arc,
};
#[derive(Clone)]
pub(crate) struct PathScope {
    pub roots: Vec<PathBuf>,
    handles: Vec<(PathBuf, Arc<Dir>, bool)>,
}
impl PathScope {
    fn open(roots: Vec<PathBuf>) -> Result<Self, String> {
        let mut handles = Vec::new();
        for root in &roots {
            let file = root.is_file();
            let directory = if file {
                root.parent().ok_or("scope file parent missing")?
            } else {
                root.as_path()
            };
            handles.push((
                root.clone(),
                Arc::new(
                    Dir::open_ambient_dir(directory, cap_std::ambient_authority())
                        .map_err(|e| e.to_string())?,
                ),
                file,
            ));
        }
        Ok(Self { roots, handles })
    }
    fn directory(&self, full: &Path) -> Result<(Arc<Dir>, PathBuf), String> {
        let (root, dir, file) = self
            .handles
            .iter()
            .filter(|(root, _, file)| {
                if *file {
                    full == root
                } else {
                    full.starts_with(root)
                }
            })
            .max_by_key(|(root, _, _)| root.components().count())
            .ok_or("path exceeds explicit task roots")?;
        Ok((
            dir.clone(),
            if *file {
                PathBuf::from(root.file_name().ok_or("scope file name missing")?)
            } else {
                full.strip_prefix(root)
                    .map_err(|_| "outside scope root")?
                    .to_path_buf()
            },
        ))
    }
}
impl LocalTools {
    /// Host-only narrowing. Paths are canonical, existing roots; repeated
    /// restriction cannot broaden a previously bound positive authority.
    pub fn restrict_scope_roots(mut self, mut roots: Vec<PathBuf>) -> Result<Self, String> {
        if roots.is_empty() || roots.len() > 256 {
            return Err("explicit task roots must contain 1..=256 entries".into());
        }
        for root in &mut roots {
            *root = root.canonicalize().map_err(|e| e.to_string())?;
            if !root.starts_with(&self.root)
                || self
                    .path_scope
                    .as_ref()
                    .is_some_and(|old| !old.roots.iter().any(|r| root.starts_with(r)))
            {
                return Err("explicit task roots exceed existing host authority".into());
            }
        }
        roots.sort();
        roots.dedup();
        self.path_scope = Some(Arc::new(PathScope::open(roots)?));
        Ok(self)
    }
    pub fn restrict_scope_paths(
        mut self,
        roots: Vec<PathBuf>,
        denied: Vec<PathBuf>,
    ) -> Result<Self, String> {
        for path in denied {
            if !path.is_absolute() {
                return Err("protected scope paths must be absolute".into());
            }
            let path = match path.canonicalize() {
                Ok(path) => path,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => path,
                Err(error) => return Err(error.to_string()),
            };
            self.policy.protected_paths.push(path);
        }
        self.policy.protected_paths.sort();
        self.policy.protected_paths.dedup();
        self.restrict_scope_roots(roots)
    }
    pub(crate) fn scope_allows(&self, path: &Path) -> bool {
        self.path_scope
            .as_ref()
            .is_none_or(|s| s.roots.iter().any(|root| path.starts_with(root)))
    }
    pub(crate) fn scope_traverses(&self, path: &Path) -> bool {
        self.path_scope.as_ref().is_none_or(|s| {
            s.roots
                .iter()
                .any(|root| path.starts_with(root) || root.starts_with(path))
        })
    }
    pub(crate) fn scope_roots(&self) -> Option<&[PathBuf]> {
        self.path_scope.as_ref().map(|s| s.roots.as_slice())
    }
    pub(crate) fn scoped_directory(&self, path: &str) -> Result<(Arc<Dir>, PathBuf), String> {
        let full = self.root.join(path);
        if let Some(scope) = &self.path_scope {
            scope.directory(&full)
        } else {
            Ok((self.directory.clone(), PathBuf::from(path)))
        }
    }
    pub(crate) fn scoped_open(&self, path: &str) -> std::io::Result<cap_std::fs::File> {
        if self.path_scope.is_none() {
            return self.directory.open(path);
        }
        let (directory, relative) = self
            .scoped_directory(path)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::PermissionDenied, e))?;
        let mut options = cap_std::fs::OpenOptions::new();
        options.read(true);
        #[cfg(unix)]
        {
            use cap_std::fs::OpenOptionsExt;
            options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_NOCTTY);
        }
        directory.open_with(relative, &options)
    }
}
