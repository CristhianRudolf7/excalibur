use crate::*;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum EntryKind {
    #[default]
    File,
    Symlink,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct SnapshotEntry {
    pub kind: EntryKind,
    pub content: Vec<u8>,
    pub executable: bool,
}
impl SnapshotEntry {
    pub fn file(content: Vec<u8>, executable: bool) -> Self {
        Self {
            kind: EntryKind::File,
            content,
            executable,
        }
    }
    pub fn symlink(target: Vec<u8>) -> Self {
        Self {
            kind: EntryKind::Symlink,
            content: target,
            executable: false,
        }
    }
    pub(crate) fn mode(&self) -> &'static str {
        match self.kind {
            EntryKind::Symlink => "120000",
            EntryKind::File if self.executable => "100755",
            _ => "100644",
        }
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct TreeEntry {
    pub oid: String,
    pub mode: String,
}
pub(crate) fn entry_path(root: &Path, path: &Path) -> Result<PathBuf> {
    let full = if path.is_absolute() {
        path.to_path_buf()
    } else {
        root.join(path)
    };
    if !full.starts_with(root) || path.components().any(|c| matches!(c, Component::ParentDir)) {
        return Err(Error::Invalid("entry outside workspace"));
    }
    let parent = full.parent().ok_or(Error::Invalid("entry has no parent"))?;
    let name = full
        .file_name()
        .ok_or(Error::Invalid("entry has no name"))?;
    Ok(resolve(root, parent)?.join(name))
}
fn allowed_link(root: &Path, path: &Path, content: &[u8]) -> bool {
    let Ok(text) = std::str::from_utf8(content) else {
        return false;
    };
    if text.is_empty() || text.contains('\0') {
        return false;
    }
    let target = Path::new(text);
    let full = if target.is_absolute() {
        target.to_path_buf()
    } else {
        path.parent().unwrap_or(root).join(target)
    };
    let mut normalized = PathBuf::new();
    for component in full.components() {
        match component {
            Component::ParentDir => {
                if !normalized.pop() {
                    return false;
                }
            }
            Component::CurDir => {}
            other => normalized.push(other.as_os_str()),
        }
    }
    normalized.starts_with(root) && resolve(root, &normalized).is_ok()
}
pub(crate) fn merge_entries(
    root: &Path,
    path: PathBuf,
    base: Option<SnapshotEntry>,
    current: Option<SnapshotEntry>,
    desired: Option<SnapshotEntry>,
    redactor: &excalibur_vault::Redactor,
) -> RevertPlan {
    let fp = |entry: &Option<SnapshotEntry>| entry.as_ref().map(|e| hash(&e.content));
    let conflict = |detail: &str| RevertPlan {
        files: vec![],
        conflicts: vec![MergeConflict {
            path: path.clone(),
            expected: fp(&base),
            current: fp(&current),
            desired: fp(&desired),
            detail: detail.into(),
        }],
    };
    if desired == base || current == desired {
        return RevertPlan::default();
    }
    let merged = if current == base {
        desired.clone()
    } else if let (Some(base), Some(current), Some(desired)) = (&base, &current, &desired) {
        if base.kind != EntryKind::File
            || current.kind != EntryKind::File
            || desired.kind != EntryKind::File
        {
            return conflict(
                "entry kind or symbolic-link target changed after the recorded effect",
            );
        }
        let body = crate::history::plan_merge(
            path.clone(),
            Some(base.content.clone()),
            Some(current.content.clone()),
            Some(desired.content.clone()),
            redactor,
        );
        if !body.conflicts.is_empty() {
            return body;
        }
        let content = body
            .files
            .first()
            .and_then(|file| file.post.clone())
            .unwrap_or_else(|| current.content.clone());
        let executable = if current.executable == base.executable {
            desired.executable
        } else {
            current.executable
        };
        Some(SnapshotEntry::file(content, executable))
    } else {
        return conflict("entry creation/deletion conflicts with a later change");
    };
    if merged.as_ref().is_some_and(|entry| {
        entry.kind == EntryKind::Symlink && !allowed_link(root, &path, &entry.content)
    }) {
        return conflict("symbolic-link target escapes the permitted workspace; no effect applied");
    }
    if merged == current {
        return RevertPlan::default();
    }
    let post = merged.as_ref().map(|entry| entry.content.clone());
    let kind = merged.as_ref().map(|e| e.kind).unwrap_or_default();
    let executable = merged.as_ref().is_some_and(|e| e.executable);
    RevertPlan {
        files: vec![PlannedFile {
            path,
            pre: fp(&current),
            post,
            kind,
            executable,
            pre_kind: current.as_ref().map(|entry| entry.kind),
            pre_executable: current.as_ref().map(|entry| entry.executable),
            metadata_guard: true,
        }],
        conflicts: vec![],
    }
}
impl Shadow {
    pub async fn plan_restore_entries(
        &self,
        before: &str,
        current: BTreeMap<PathBuf, Option<SnapshotEntry>>,
        selected: bool,
    ) -> Result<RevertPlan> {
        let snapshot = self
            .lookup(before)
            .await?
            .ok_or(Error::Invalid("snapshot missing"))?;
        let after = self
            .list_snapshots(&snapshot.run_id)
            .await?
            .into_iter()
            .find(|s| s.step == snapshot.step && s.phase == "after")
            .ok_or(Error::Invalid(
                "restore requires paired after-step snapshot",
            ))?;
        self.plan_between_entries(before, &after.id, current, selected)
            .await
    }
    pub async fn plan_between_entries(
        &self,
        desired: &str,
        base: &str,
        current: BTreeMap<PathBuf, Option<SnapshotEntry>>,
        selected: bool,
    ) -> Result<RevertPlan> {
        let desired = self.tree(desired).await?;
        let base = self.tree(base).await?;
        let mut plan = RevertPlan::default();
        for path in desired.keys().chain(base.keys()).collect::<BTreeSet<_>>() {
            if selected && !current.contains_key(path) || desired.get(path) == base.get(path) {
                continue;
            }
            let actual = current
                .get(path)
                .ok_or(Error::Invalid("guarded current entry required"))?
                .clone();
            let mut content = Vec::new();
            for entry in [base.get(path), desired.get(path)] {
                content.push(match entry {
                    Some(entry) => {
                        let bytes = self
                            .git(
                                vec!["cat-file".into(), "blob".into(), entry.oid.clone()],
                                None,
                                None,
                            )
                            .await?;
                        Some(match entry.mode.as_str() {
                            "120000" => SnapshotEntry::symlink(bytes),
                            "100644" => SnapshotEntry::file(bytes, false),
                            "100755" => SnapshotEntry::file(bytes, true),
                            _ => return Err(Error::Invalid("unsupported shadow entry mode")),
                        })
                    }
                    None => None,
                });
            }
            if content
                .iter()
                .flatten()
                .chain(actual.iter())
                .any(|entry| self.workspace.redactor.contains_secret(&entry.content))
            {
                return Err(Error::Invalid("protected entry cannot enter restore plan"));
            }
            let mut merged = merge_entries(
                &self.workspace.root,
                entry_path(&self.workspace.root, path)?,
                content[0].clone(),
                actual,
                content[1].clone(),
                &self.workspace.redactor,
            );
            plan.files.append(&mut merged.files);
            plan.conflicts.append(&mut merged.conflicts);
        }
        Ok(plan)
    }
}
