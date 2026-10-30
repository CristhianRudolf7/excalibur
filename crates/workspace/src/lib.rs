//! Shared editing leases, isolated Git workspaces and reversible local history.
mod checkpoint;
mod git;
pub use checkpoint::{
    CheckpointEntriesGuard, CheckpointEntriesGuard as EntryCheckpointGuard, CheckpointGuard,
};
mod history;
mod leases;
mod shadow;
mod typed_entries;
use excalibur_store::{Database, Priority, ReadLane, rusqlite::OptionalExtension};
use excalibur_vault::Redactor;
pub use git::{
    GitIdentity, GitWorkspace, IntegrationChecks, IntegrationReport, Overlap, ProjectMode,
    TestReport, Worktree, overlaps,
};
pub use history::{FileVersion, History, MergeConflict, PlannedFile, RevertPlan};
pub use leases::{LeaseGuard, LeaseManager, LeaseOwner};
use sha2::{Digest, Sha256};
pub use shadow::{Shadow, Snapshot};
use std::path::{Component, Path, PathBuf};
pub use typed_entries::{EntryKind, SnapshotEntry};
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("workspace storage: {0}")]
    Storage(#[from] excalibur_store::Error),
    #[error("workspace I/O: {0}")]
    Io(#[from] std::io::Error),
    #[error("workspace JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("{0}")]
    Invalid(&'static str),
    #[error("file lease held by {0:?}")]
    Busy(LeaseOwner),
    #[error("git operation failed: {0}")]
    Git(String),
}
pub type Result<T> = std::result::Result<T, Error>;
impl From<excalibur_store::rusqlite::Error> for Error {
    fn from(e: excalibur_store::rusqlite::Error) -> Self {
        Self::Storage(e.into())
    }
}
pub(crate) fn hash(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
#[derive(Clone)]
pub struct Workspace {
    db: Database,
    root: PathBuf,
    data: PathBuf,
    redactor: Redactor,
    leases: LeaseManager,
}
impl Workspace {
    pub async fn open(
        db: Database,
        root: PathBuf,
        data: PathBuf,
        redactor: Redactor,
    ) -> Result<Self> {
        let root = root.canonicalize()?;
        if !root.is_dir() || !data.is_absolute() {
            return Err(Error::Invalid(
                "workspace and absolute host data directory required",
            ));
        }
        std::fs::create_dir_all(&data)?;
        let data = data.canonicalize()?;
        let sql = include_str!("schema.sql");
        let checksum = hash(sql.as_bytes());
        let assignments = include_str!("assignments.sql");
        let assignments_checksum = hash(assignments.as_bytes());
        db.write(Priority::Journal, move |c| {
            c.execute_batch(sql)?;
            let old: Option<String> = c
                .query_row(
                    "SELECT checksum FROM workspace_schema WHERE version=1",
                    [],
                    |r| r.get(0),
                )
                .optional()?;
            if old.as_ref().is_some_and(|x| x != &checksum) {
                return Err(excalibur_store::Error::Invalid(
                    "workspace schema checksum mismatch",
                ));
            }
            c.execute(
                "INSERT OR IGNORE INTO workspace_schema VALUES(1,?1)",
                [checksum],
            )?;
            Ok(())
        })
        .await?;
        db.write(Priority::Journal, move |c| {
            let previous: Option<String> = c
                .query_row(
                    "SELECT checksum FROM workspace_schema WHERE version=2",
                    [],
                    |r| r.get(0),
                )
                .optional()?;
            if previous
                .as_ref()
                .is_some_and(|v| v != &assignments_checksum)
            {
                return Err(excalibur_store::Error::Invalid(
                    "assignment schema checksum mismatch",
                ));
            }
            c.execute_batch(assignments)?;
            c.execute(
                "INSERT OR IGNORE INTO workspace_schema VALUES(2,?1)",
                [assignments_checksum],
            )?;
            Ok(())
        })
        .await?;

        let metadata = include_str!("file_metadata.sql");
        let metadata_checksum = hash(metadata.as_bytes());
        db.write(Priority::Journal, move |c| {
            let previous: Option<String> = c
                .query_row(
                    "SELECT checksum FROM workspace_schema WHERE version=3",
                    [],
                    |r| r.get(0),
                )
                .optional()?;
            if previous.as_ref().is_some_and(|v| v != &metadata_checksum) {
                return Err(excalibur_store::Error::Invalid(
                    "file metadata schema checksum mismatch",
                ));
            }
            c.execute_batch(metadata)?;
            c.execute(
                "INSERT OR IGNORE INTO workspace_schema VALUES(3,?1)",
                [metadata_checksum],
            )?;
            Ok(())
        })
        .await?;
        let leases = LeaseManager::open(
            db.clone(),
            root.clone(),
            data.join("locks"),
            redactor.clone(),
        )?;
        Ok(Self {
            db,
            root,
            data,
            redactor,
            leases,
        })
    }
    /// Persist one implementer's canonical file set and report all intersections
    /// for the task. Graph expansion and parent notification belong to the host.
    pub async fn assign_files(
        &self,
        task: &str,
        parent: &str,
        owner: &str,
        paths: &[PathBuf],
    ) -> Result<Vec<Overlap>> {
        if [task, parent, owner]
            .iter()
            .any(|id| id.is_empty() || id.len() > 256 || id.chars().any(char::is_control))
        {
            return Err(Error::Invalid(
                "task, parent and owner identifiers required",
            ));
        }
        let paths = paths
            .iter()
            .map(|path| self.resolve(path))
            .collect::<Result<std::collections::BTreeSet<_>>>()?;
        let (root, task, parent, owner) = (
            self.root.to_string_lossy().into_owned(),
            task.to_owned(),
            parent.to_owned(),
            owner.to_owned(),
        );
        Ok(self.db.write(Priority::Normal,move|c|{
            let previous:Option<String>=c.query_row("SELECT parent FROM workspace_assignment_parents WHERE root=?1 AND task=?2",excalibur_store::rusqlite::params![root,task],|r|r.get(0)).optional()?;
            if previous.as_ref().is_some_and(|v|v!=&parent){return Err(excalibur_store::Error::Invalid("another parent owns the task file assignment"));}
            c.execute("INSERT OR IGNORE INTO workspace_assignment_parents VALUES(?1,?2,?3)",excalibur_store::rusqlite::params![root,task,parent])?;
            c.execute("DELETE FROM workspace_assignments WHERE root=?1 AND task=?2 AND owner=?3",excalibur_store::rusqlite::params![root,task,owner])?;
            for path in paths{let path=path.to_str().ok_or(excalibur_store::Error::Invalid("assignment paths must be UTF-8"))?;c.execute("INSERT INTO workspace_assignments VALUES(?1,?2,?3,?4)",excalibur_store::rusqlite::params![root,task,owner,path])?;}
            let mut q=c.prepare("SELECT owner,path FROM workspace_assignments WHERE root=?1 AND task=?2 ORDER BY owner,path")?;
            let rows=q.query_map(excalibur_store::rusqlite::params![root,task],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?)))?.collect::<std::result::Result<Vec<_>,_>>()?;
            let mut assignments=std::collections::BTreeMap::<String,std::collections::BTreeSet<PathBuf>>::new();for(owner,path)in rows{assignments.entry(owner).or_default().insert(PathBuf::from(path));}
            Ok(overlaps(&assignments))
        }).await?)
    }
    pub fn leases(&self) -> LeaseManager {
        self.leases.clone()
    }
    pub fn history(&self) -> History {
        History::new(self.clone())
    }
    pub fn shadow(&self) -> Shadow {
        Shadow::new(self.clone())
    }
    pub fn git(&self) -> GitWorkspace {
        GitWorkspace::new(self.clone())
    }
    /// Host-owned generated worktree authority, separate from the user's tree.
    pub fn database_path(&self) -> &Path {
        self.db.path()
    }
    pub fn managed_worktree_root(&self) -> PathBuf {
        self.data.join("worktrees")
    }
    pub fn root(&self) -> &Path {
        &self.root
    }
    pub(crate) fn resolve(&self, path: &Path) -> Result<PathBuf> {
        resolve(&self.root, path)
    }
}
pub(crate) fn resolve(root: &Path, path: &Path) -> Result<PathBuf> {
    if path.components().any(|p| matches!(p, Component::ParentDir)) {
        return Err(Error::Invalid("parent traversal forbidden"));
    }
    let full = if path.is_absolute() {
        path.to_path_buf()
    } else {
        root.join(path)
    };
    if !full.starts_with(root) {
        return Err(Error::Invalid("path outside workspace"));
    }
    let mut ancestor = full.as_path();
    let mut tail = Vec::new();
    while !ancestor.exists() {
        let name = ancestor
            .file_name()
            .ok_or(Error::Invalid("invalid workspace path"))?;
        tail.push(name.to_owned());
        ancestor = ancestor
            .parent()
            .ok_or(Error::Invalid("invalid workspace path"))?;
    }
    let mut canonical = ancestor.canonicalize()?;
    if !canonical.starts_with(root) {
        return Err(Error::Invalid("symlink escapes workspace"));
    }
    for name in tail.into_iter().rev() {
        canonical.push(name);
    }
    Ok(canonical)
}
