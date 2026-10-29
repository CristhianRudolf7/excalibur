use crate::*;
use serde::{Deserialize, Serialize};
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FileVersion {
    pub id: String,
    pub path: PathBuf,
    pub preimage_blob: Option<String>,
    pub postimage_blob: Option<String>,
    pub run_id: Option<String>,
    pub tool_call_id: Option<String>,
    pub recorded_at: i64,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct PlannedFile {
    pub path: PathBuf,
    pub pre: Option<String>,
    pub post: Option<Vec<u8>>,
    #[serde(default)]
    pub kind: EntryKind,
    #[serde(default)]
    pub executable: bool,
    #[serde(default)]
    pub pre_kind: Option<EntryKind>,
    #[serde(default)]
    pub pre_executable: Option<bool>,
    #[serde(default)]
    pub metadata_guard: bool,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MergeConflict {
    pub path: PathBuf,
    pub expected: Option<String>,
    pub current: Option<String>,
    pub desired: Option<String>,
    pub detail: String,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct RevertPlan {
    pub files: Vec<PlannedFile>,
    pub conflicts: Vec<MergeConflict>,
}
#[derive(Clone)]
pub struct History {
    workspace: Workspace,
}
impl History {
    pub(crate) fn new(workspace: Workspace) -> Self {
        Self { workspace }
    }
    pub async fn file_history(&self, path: &Path, limit: u32) -> Result<Vec<FileVersion>> {
        if !(1..=1000).contains(&limit) {
            return Err(Error::Invalid("history limit must be 1..1000"));
        }
        let path = crate::typed_entries::entry_path(&self.workspace.root, path)?
            .to_string_lossy()
            .into_owned();
        Ok(self.workspace.db.read(ReadLane::Interactive,move|c|{let mut q=c.prepare("SELECT id,path,preimage_blob,postimage_blob,run_id,tool_call_id,recorded_at FROM file_versions WHERE path=?1 ORDER BY recorded_at DESC,rowid DESC LIMIT ?2")?;Ok(q.query_map(excalibur_store::rusqlite::params![path,limit],|r|Ok(FileVersion{id:r.get(0)?,path:PathBuf::from(r.get::<_,String>(1)?),preimage_blob:r.get(2)?,postimage_blob:r.get(3)?,run_id:r.get(4)?,tool_call_id:r.get(5)?,recorded_at:r.get(6)?}))?.collect::<std::result::Result<Vec<_>,_>>()?)}).await?)
    }
    pub async fn version(&self, edit_id: &str) -> Result<FileVersion> {
        let id = edit_id.to_owned();
        let version=self.workspace.db.read(ReadLane::Interactive,move|c|Ok(c.query_row("SELECT id,path,preimage_blob,postimage_blob,run_id,tool_call_id,recorded_at FROM file_versions WHERE id=?1",[id],|r|Ok(FileVersion{id:r.get(0)?,path:PathBuf::from(r.get::<_,String>(1)?),preimage_blob:r.get(2)?,postimage_blob:r.get(3)?,run_id:r.get(4)?,tool_call_id:r.get(5)?,recorded_at:r.get(6)?}))?)).await?;
        crate::typed_entries::entry_path(&self.workspace.root, &version.path)?;
        Ok(version)
    }
    /// Pure preparation: callers must publish blobs and revalidate `pre` under their
    /// lease immediately before the durable editing pipeline applies this plan.
    pub async fn plan_revert(&self, edit_id: &str, current: Option<Vec<u8>>) -> Result<RevertPlan> {
        let id = edit_id.to_owned();
        let (path, pre, post) = self
            .workspace
            .db
            .read(ReadLane::Interactive, move |c| {
                Ok(c.query_row(
                    "SELECT path,preimage_blob,postimage_blob FROM file_versions WHERE id=?1",
                    [id],
                    |r| {
                        Ok((
                            r.get::<_, String>(0)?,
                            r.get::<_, Option<String>>(1)?,
                            r.get::<_, Option<String>>(2)?,
                        ))
                    },
                )?)
            })
            .await?;
        let path = self.workspace.resolve(Path::new(&path))?;
        let desired = self.blob(pre).await?;
        let base = self.blob(post).await?;
        if base
            .iter()
            .chain(desired.iter())
            .chain(current.iter())
            .any(|bytes| self.workspace.redactor.contains_secret(bytes))
        {
            return Err(Error::Invalid("protected content cannot enter revert plan"));
        }
        Ok(plan_merge(
            path,
            base,
            current,
            desired,
            &self.workspace.redactor,
        ))
    }
    pub async fn plan_revert_entry(
        &self,
        edit_id: &str,
        current: Option<SnapshotEntry>,
    ) -> Result<RevertPlan> {
        let version = self.version(edit_id).await?;
        let id = edit_id.to_owned();
        let metadata=self.workspace.db.read(ReadLane::Interactive,move|c|Ok(c.query_row("SELECT pre_kind,pre_executable,post_kind,post_executable FROM file_version_metadata WHERE version_id=?1",[id],|r|Ok((r.get::<_,Option<String>>(0)?,r.get::<_,Option<bool>>(1)?,r.get::<_,Option<String>>(2)?,r.get::<_,Option<bool>>(3)?))).optional()?)).await?;
        let Some((pre_kind, pre_exec, post_kind, post_exec)) = metadata else {
            if current
                .as_ref()
                .is_some_and(|entry| entry.kind == EntryKind::Symlink)
            {
                return Err(Error::Invalid(
                    "legacy edit has no symbolic-link metadata; explicit resolution required",
                ));
            }
            return self
                .plan_revert(edit_id, current.map(|entry| entry.content))
                .await;
        };
        let desired = self.blob(version.preimage_blob).await?;
        let base = self.blob(version.postimage_blob).await?;
        let make = |content: Option<Vec<u8>>,
                    kind: Option<String>,
                    executable: Option<bool>|
         -> Result<Option<SnapshotEntry>> {
            match content {
                None => Ok(None),
                Some(content) => Ok(Some(match kind.as_deref() {
                    Some("file") => SnapshotEntry::file(
                        content,
                        executable.ok_or(Error::Invalid("version executable metadata missing"))?,
                    ),
                    Some("symlink") => SnapshotEntry::symlink(content),
                    _ => return Err(Error::Invalid("version kind metadata missing")),
                })),
            }
        };
        let (base, desired) = (
            make(base, post_kind, post_exec)?,
            make(desired, pre_kind, pre_exec)?,
        );
        if base
            .iter()
            .chain(desired.iter())
            .chain(current.iter())
            .any(|entry| self.workspace.redactor.contains_secret(&entry.content))
        {
            return Err(Error::Invalid("protected entry cannot enter revert plan"));
        }
        Ok(crate::typed_entries::merge_entries(
            &self.workspace.root,
            crate::typed_entries::entry_path(&self.workspace.root, &version.path)?,
            base,
            current,
            desired,
            &self.workspace.redactor,
        ))
    }
    async fn blob(&self, id: Option<String>) -> Result<Option<Vec<u8>>> {
        match id {
            Some(id) => Ok(Some(
                self.workspace
                    .db
                    .read_blob(id.parse()?, 64 * 1024 * 1024)
                    .await?,
            )),
            None => Ok(None),
        }
    }
}
pub(crate) fn plan_merge(
    path: PathBuf,
    base: Option<Vec<u8>>,
    current: Option<Vec<u8>>,
    desired: Option<Vec<u8>>,
    redactor: &Redactor,
) -> RevertPlan {
    let fp = |data: &Option<Vec<u8>>| data.as_ref().map(|data| hash(data));
    let expected = fp(&base);
    let actual = fp(&current);
    let wanted = fp(&desired);
    let merged = if current == base {
        Ok(desired.clone())
    } else if desired == base || current == desired {
        Ok(current.clone())
    } else if let (Some(base), Some(current), Some(desired)) = (&base, &current, &desired) {
        match (
            std::str::from_utf8(base),
            std::str::from_utf8(current),
            std::str::from_utf8(desired),
        ) {
            (Ok(base), Ok(current), Ok(desired)) => diffy::merge(base, current, desired)
                .map(|text| Some(text.into_bytes()))
                .map_err(|conflict| {
                    redactor.redact_text(&conflict.chars().take(4096).collect::<String>())
                }),
            _ => Err("binary changes require explicit conflict resolution".into()),
        }
    } else {
        Err("file creation/deletion conflicts with a later change".into())
    };
    match merged {
        Ok(post) => RevertPlan {
            files: if post == current {
                vec![]
            } else {
                vec![PlannedFile {
                    path,
                    pre: actual,
                    post,
                    ..Default::default()
                }]
            },
            conflicts: vec![],
        },
        Err(detail) => RevertPlan {
            files: vec![],
            conflicts: vec![MergeConflict {
                path,
                expected,
                current: actual,
                desired: wanted,
                detail,
            }],
        },
    }
}
