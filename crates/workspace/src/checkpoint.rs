//! Capability-aware staging: only host-authorized content enters the task commit.
use crate::git::{command, identity_env};
use crate::*;
use excalibur_types::runtime::BoxFuture;
use std::collections::BTreeMap;
/// The host reuses its capability-aware reader, including private-hardlink
/// rejection, and compares every selected file with its captured content.
pub trait CheckpointGuard: Send + Sync {
    fn verify<'a>(
        &'a self,
        tree: &'a Worktree,
        expected: &'a BTreeMap<PathBuf, Option<Vec<u8>>>,
    ) -> BoxFuture<'a, Result<()>>;
}
/// The host verifies both literal content and entry metadata through its
/// capability-aware reader. Symlink targets are bytes, never followed here.
pub trait CheckpointEntriesGuard: Send + Sync {
    fn verify<'a>(
        &'a self,
        tree: &'a Worktree,
        expected: &'a BTreeMap<PathBuf, Option<SnapshotEntry>>,
    ) -> BoxFuture<'a, Result<()>>;
}
struct LegacyGuard<'a> {
    workspace: &'a GitWorkspace,
    head: String,
    files: &'a BTreeMap<PathBuf, Option<Vec<u8>>>,
    guard: &'a dyn CheckpointGuard,
}
impl CheckpointEntriesGuard for LegacyGuard<'_> {
    fn verify<'a>(
        &'a self,
        tree: &'a Worktree,
        _expected: &'a BTreeMap<PathBuf, Option<SnapshotEntry>>,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            if self
                .workspace
                .git(&tree.path, &["rev-parse", "HEAD"])
                .await?
                .trim()
                != self.head
            {
                return Err(Error::Invalid(
                    "checkpoint HEAD changed during legacy mode capture",
                ));
            }
            self.guard.verify(tree, self.files).await
        })
    }
}
impl GitWorkspace {
    pub async fn list_tracked_paths(&self, task: &str, owner: &str) -> Result<Vec<PathBuf>> {
        let tree = self.worktree(task, owner).await?;
        self.validate_owned(&tree.path, &tree.branch).await?;
        let (ok, out, error) = command(
            &tree.path,
            &[
                "ls-tree".into(),
                "-r".into(),
                "--name-only".into(),
                "-z".into(),
                "HEAD".into(),
            ],
            &[],
            None,
            &self.workspace.redactor,
        )
        .await?;
        if !ok {
            return Err(Error::Git(error));
        }
        out.split(|b| *b == 0)
            .filter(|b| !b.is_empty())
            .map(|b| {
                Ok(PathBuf::from(std::str::from_utf8(b).map_err(|_| {
                    Error::Invalid("tracked path is not UTF-8")
                })?))
            })
            .collect()
    }
    pub async fn checkpoint_files(
        &self,
        task: &str,
        owner: &str,
        message: &str,
        identity: &GitIdentity,
        files: BTreeMap<PathBuf, Option<Vec<u8>>>,
        guard: &dyn CheckpointGuard,
    ) -> Result<String> {
        let tree = self.worktree(task, owner).await?;
        self.validate_owned(&tree.path, &tree.branch).await?;
        let head = self
            .git(&tree.path, &["rev-parse", "HEAD"])
            .await?
            .trim()
            .to_owned();
        let mut entries = BTreeMap::new();
        for (path, content) in &files {
            validate_path(path)?;
            let entry = if let Some(content) = content {
                let path_text = path
                    .to_str()
                    .ok_or(Error::Invalid("checkpoint path must be UTF-8"))?;
                let previous = self
                    .git(&tree.path, &["ls-tree", &head, "--", path_text])
                    .await?;
                Some(
                    match previous.split_whitespace().next().unwrap_or("100644") {
                        "100644" => SnapshotEntry::file(content.clone(), false),
                        "100755" => SnapshotEntry::file(content.clone(), true),
                        "120000" => SnapshotEntry::symlink(content.clone()),
                        _ => {
                            return Err(Error::Invalid("unsupported legacy checkpoint entry mode"));
                        }
                    },
                )
            } else {
                None
            };
            entries.insert(path.clone(), entry);
        }
        let adapter = LegacyGuard {
            workspace: self,
            head,
            files: &files,
            guard,
        };
        self.checkpoint_entries(task, owner, message, identity, entries, &adapter)
            .await
    }
    pub async fn checkpoint_entries(
        &self,
        task: &str,
        owner: &str,
        message: &str,
        identity: &GitIdentity,
        files: BTreeMap<PathBuf, Option<SnapshotEntry>>,
        guard: &dyn CheckpointEntriesGuard,
    ) -> Result<String> {
        let tree = self.worktree(task, owner).await?;
        self.validate_owned(&tree.path, &tree.branch).await?;
        let _lock = self.lock(&format!("checkpoint:{task}:{owner}"))?;
        if message.trim().is_empty()
            || message.len() > 65536
            || self.workspace.redactor.contains_secret(message.as_bytes())
            || self
                .workspace
                .redactor
                .contains_secret(&serde_json::to_vec(identity)?)
        {
            return Err(Error::Invalid(
                "safe explicit checkpoint identity and message required",
            ));
        }
        let mut total = 0usize;
        for (path, content) in &files {
            validate_path(path)?;
            if let Some(entry) = content {
                let bytes = &entry.content;
                if entry.kind == EntryKind::Symlink
                    && (entry.executable || bytes.is_empty() || bytes.contains(&0))
                {
                    return Err(Error::Invalid(
                        "symlink checkpoint requires a nonempty literal target without NUL or executable mode",
                    ));
                }
                total = total
                    .checked_add(bytes.len())
                    .ok_or(Error::Invalid("checkpoint size overflow"))?;
                if bytes.len() > 64 * 1024 * 1024 || total > 256 * 1024 * 1024 {
                    return Err(Error::Invalid("checkpoint content budget exceeded"));
                }
                if self.workspace.redactor.contains_secret(bytes) {
                    return Err(Error::Invalid(
                        "protected content cannot enter a git checkpoint",
                    ));
                }
            }
        }
        guard.verify(&tree, &files).await?;
        let head = self
            .git(&tree.path, &["rev-parse", "HEAD"])
            .await?
            .trim()
            .to_owned();
        let directory = self.workspace.data.join("git-indices");
        std::fs::create_dir_all(&directory)?;
        let index = directory.join(excalibur_types::JobId::new().to_string());
        let env = vec![(
            "GIT_INDEX_FILE".into(),
            index.to_string_lossy().into_owned(),
        )];
        let result = async {
            let (ok, _, error) = command(
                &tree.path,
                &["read-tree".into(), head.clone()],
                &env,
                None,
                &self.workspace.redactor,
            )
            .await?;
            if !ok {
                return Err(Error::Git(error));
            }
            let mut staged = Vec::new();
            for (path, content) in &files {
                let path = path
                    .to_str()
                    .ok_or(Error::Invalid("checkpoint path must be UTF-8"))?;
                let args = if let Some(entry) = content {
                    let (ok, oid, error) = command(
                        &tree.path,
                        &["hash-object".into(), "-w".into(), "--stdin".into()],
                        &env,
                        Some(entry.content.clone()),
                        &self.workspace.redactor,
                    )
                    .await?;
                    if !ok {
                        return Err(Error::Git(error));
                    }
                    let oid = String::from_utf8(oid)
                        .map_err(|_| Error::Invalid("invalid object id"))?
                        .trim()
                        .to_owned();
                    let mode = entry.mode();
                    vec![
                        "update-index".into(),
                        "--add".into(),
                        "--cacheinfo".into(),
                        format!("{mode},{oid},{path}"),
                    ]
                } else {
                    vec![
                        "update-index".into(),
                        "--force-remove".into(),
                        "--".into(),
                        path.into(),
                    ]
                };
                let (ok, _, error) =
                    command(&tree.path, &args, &env, None, &self.workspace.redactor).await?;
                if !ok {
                    return Err(Error::Git(error));
                }
                staged.push(args);
            }
            let (ok, object, error) = command(
                &tree.path,
                &["write-tree".into()],
                &env,
                None,
                &self.workspace.redactor,
            )
            .await?;
            if !ok {
                return Err(Error::Git(error));
            }
            let object = String::from_utf8(object)
                .map_err(|_| Error::Invalid("invalid commit tree"))?
                .trim()
                .to_owned();
            guard.verify(&tree, &files).await?;
            self.validate_owned(&tree.path, &tree.branch).await?;
            let mut environment = env.clone();
            environment.extend(identity_env(identity)?);
            let (ok, commit, error) = command(
                &tree.path,
                &[
                    "commit-tree".into(),
                    object,
                    "-p".into(),
                    head.clone(),
                    "-F".into(),
                    "-".into(),
                ],
                &environment,
                Some(message.as_bytes().to_vec()),
                &self.workspace.redactor,
            )
            .await?;
            if !ok {
                return Err(Error::Git(error));
            }
            let commit = String::from_utf8(commit)
                .map_err(|_| Error::Invalid("invalid commit id"))?
                .trim()
                .to_owned();
            guard.verify(&tree, &files).await?;
            self.git(
                &tree.path,
                &[
                    "update-ref",
                    &format!("refs/heads/{}", tree.branch),
                    &commit,
                    &head,
                ],
            )
            .await?;
            // Update only authorized entries in this owned child index; unrelated staged
            // or protected entries are preserved and will keep enqueue visibly dirty.
            for args in staged {
                let (ok, _, error) =
                    command(&tree.path, &args, &[], None, &self.workspace.redactor).await?;
                if !ok {
                    return Err(Error::Git(error));
                }
            }
            Ok(commit)
        }
        .await;
        let _ = std::fs::remove_file(index);
        result
    }
}
fn validate_path(path: &Path) -> Result<()> {
    if path.as_os_str().is_empty()
        || path.is_absolute()
        || path
            .components()
            .any(|c| !matches!(c, Component::Normal(_)))
        || path.components().any(|c| c.as_os_str() == ".git")
    {
        return Err(Error::Invalid(
            "checkpoint paths must be normalized relative files",
        ));
    }
    Ok(())
}
