//! Descriptor-bound tree entries for reversible edits. Symbolic-link contents are
//! recorded literally; reading an entry never follows its final component.
use crate::{
    LocalTools, Permission,
    files::{FileEdit, MAX_FILE_BYTES, hash},
};
use excalibur_workspace::{EntryKind, PlannedFile, SnapshotEntry};
use serde_json::json;
use std::{
    collections::BTreeMap,
    io::{Read, Write},
    path::{Component, Path, PathBuf},
};

impl LocalTools {
    pub(crate) fn resolve_entry(&self, path: &str) -> Result<String, String> {
        let path = Path::new(path);
        if path.components().any(|c| matches!(c, Component::ParentDir)) {
            return Err("parent traversal (..) is forbidden".into());
        }
        let full = if path.is_absolute() {
            path.to_owned()
        } else {
            self.root.join(path)
        };
        let name = full.file_name().ok_or("entry filename missing")?;
        let parent = full.parent().ok_or("entry parent missing")?;
        let resolved = if parent == self.root {
            self.root.clone()
        } else {
            self.root
                .join(self.resolve(parent.to_str().ok_or("path must be UTF-8")?)?)
        };
        let entry = resolved.join(name);
        if !entry.starts_with(&self.root)
            || self
                .policy
                .protected_paths
                .iter()
                .any(|p| entry.starts_with(p))
        {
            return Err("security policy: protected or outside host path".into());
        }
        entry
            .strip_prefix(&self.root)
            .map_err(|_| "outside root")?
            .to_str()
            .map(str::to_owned)
            .ok_or_else(|| "entry path must be UTF-8".into())
    }
    /// Read only the entry itself, using the same hard-link authority as file tools.
    pub fn current_entry(&self, path: &str) -> Result<Option<SnapshotEntry>, String> {
        self.current_entry_guarded(path, &mut crate::file_authority::ReadAuthority::default())
    }
    fn current_entry_guarded(
        &self,
        path: &str,
        authority: &mut crate::file_authority::ReadAuthority,
    ) -> Result<Option<SnapshotEntry>, String> {
        let path = self.resolve_entry(path)?;
        let meta = match self.directory.symlink_metadata(&path) {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.to_string()),
        };
        if meta.file_type().is_symlink() {
            let target = self
                .directory
                .read_link_contents(&path)
                .map_err(|e| e.to_string())?;
            #[cfg(unix)]
            let bytes = {
                use std::os::unix::ffi::OsStrExt;
                target.as_os_str().as_bytes().to_vec()
            };
            #[cfg(not(unix))]
            let bytes = target
                .to_str()
                .ok_or("link target must be UTF-8")?
                .as_bytes()
                .to_vec();
            if bytes.len() as u64 > MAX_FILE_BYTES {
                return Err("link target too large".into());
            }
            return Ok(Some(SnapshotEntry::symlink(bytes)));
        }
        if !meta.is_file() {
            return Err("entry is not a regular file or symbolic link".into());
        }
        let mut opts = cap_std::fs::OpenOptions::new();
        opts.read(true);
        #[cfg(unix)]
        {
            use cap_std::fs::OpenOptionsExt;
            opts.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_NOCTTY);
        }
        let mut file = self
            .directory
            .open_with(&path, &opts)
            .map_err(|e| e.to_string())?;
        authority.check(
            &self.directory,
            &self.root,
            &self.policy.protected_paths,
            &file,
        )?;
        let opened = file.metadata().map_err(|e| e.to_string())?;
        if !opened.is_file() {
            return Err("entry changed to a nonregular file".into());
        }
        #[cfg(unix)]
        let executable = {
            use cap_std::fs::PermissionsExt;
            opened.permissions().mode() & 0o111 != 0
        };
        #[cfg(not(unix))]
        let executable = false;
        let mut bytes = Vec::new();
        Read::by_ref(&mut file)
            .take(MAX_FILE_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(|e| e.to_string())?;
        if bytes.len() as u64 > MAX_FILE_BYTES {
            return Err("file exceeds 64 MiB limit".into());
        }
        authority.check(
            &self.directory,
            &self.root,
            &self.policy.protected_paths,
            &file,
        )?;
        Ok(Some(SnapshotEntry::file(bytes, executable)))
    }
    /// Host-authorized, bounded snapshot input. Credential/configuration paths and
    /// protected hard-link aliases are filtered before any content publication.
    pub fn authorized_snapshot_entries(&self) -> Result<BTreeMap<PathBuf, SnapshotEntry>, String> {
        let protected = self.policy.protected_paths.clone();
        let mut out = BTreeMap::new();
        let mut total = 0usize;
        let mut authority = crate::file_authority::ReadAuthority::excluding_credential_names();
        for entry in ignore::WalkBuilder::new(&self.root)
            .hidden(false)
            .git_ignore(true)
            .git_exclude(true)
            .require_git(false)
            .follow_links(false)
            .filter_entry(move |e| {
                !e.path().components().any(|p| {
                    matches!(
                        p.as_os_str().to_str(),
                        Some(".git" | "target" | "node_modules")
                    )
                }) && !protected.iter().any(|p| e.path().starts_with(p))
            })
            .build()
        {
            let entry = entry.map_err(|e| e.to_string())?;
            if !entry
                .file_type()
                .is_some_and(|t| t.is_file() || t.is_symlink())
            {
                continue;
            }
            let relative = entry
                .path()
                .strip_prefix(&self.root)
                .map_err(|e| e.to_string())?;
            let path = self.resolve_entry(relative.to_str().ok_or("path must be UTF-8")?)?;
            if self.check_write_path(&path).is_err()
                || excalibur_sandbox::file_authority::credential_path(Path::new(&path))
            {
                continue;
            }
            if let Ok(Some(value)) = self.current_entry_guarded(&path, &mut authority) {
                if self.policy.redactor.contains_secret(&value.content) {
                    continue;
                }
                total = total
                    .checked_add(value.content.len())
                    .ok_or("snapshot size overflow")?;
                if total > 256 * 1024 * 1024 {
                    return Err("snapshot exceeds 256 MiB limit".into());
                }
                out.insert(PathBuf::from(path), value);
            }
        }
        Ok(out)
    }
    pub(crate) async fn prepare_entry_edit(
        &self,
        run: &str,
        call: &str,
        plan: &PlannedFile,
    ) -> Result<FileEdit, String> {
        self.check_permission(Permission::Write)?;
        let path = self.resolve_entry(plan.path.to_str().ok_or("entry path must be UTF-8")?)?;
        self.check_write_path(&path)?;
        let current = self.current_entry(&path)?;
        if current.as_ref().map(|e| hash(&e.content)) != plan.pre
            || plan.metadata_guard
                && (current.as_ref().map(|e| e.kind) != plan.pre_kind
                    || current.as_ref().map(|e| e.executable) != plan.pre_executable)
        {
            return Err(
                "entry changed after revert/snapshot planning; prepare a fresh plan".into(),
            );
        }
        if let Some(bytes) = plan.post.as_ref()
            && bytes.len() as u64 > MAX_FILE_BYTES
        {
            return Err("write exceeds 64 MiB limit".into());
        }
        let kind = if plan.metadata_guard {
            plan.kind
        } else {
            EntryKind::File
        };
        if kind == EntryKind::Symlink && plan.executable {
            return Err("symbolic-link executable flag must be false".into());
        }
        let executable = if plan.metadata_guard {
            plan.executable
        } else {
            current.as_ref().is_some_and(|e| e.executable)
        };
        if kind == EntryKind::Symlink
            && let Some(bytes) = &plan.post
        {
            self.validate_link(&path, bytes)?;
        }
        let pre = match &current {
            Some(e) => Some(
                self.save_blob(
                    run,
                    call,
                    &format!("pre-{}", hash(path.as_bytes())),
                    e.content.clone(),
                )
                .await?,
            ),
            None => None,
        };
        let post = match &plan.post {
            Some(e) => Some(
                self.save_blob(
                    run,
                    call,
                    &format!("post-{}", hash(path.as_bytes())),
                    e.clone(),
                )
                .await?,
            ),
            None => None,
        };
        Ok(FileEdit {
            path,
            pre,
            post,
            kind,
            executable,
            pre_kind: current.as_ref().map(|e| e.kind),
            pre_executable: current.as_ref().map(|e| e.executable),
            metadata_guard: true,
        })
    }
    fn validate_link(&self, path: &str, bytes: &[u8]) -> Result<(), String> {
        let target = std::str::from_utf8(bytes).map_err(|_| "link target must be UTF-8")?;
        if target.is_empty() || target.contains('\0') {
            return Err("invalid symbolic-link target".into());
        }
        let full = if Path::new(target).is_absolute() {
            PathBuf::from(target)
        } else {
            self.root
                .join(path)
                .parent()
                .ok_or("link parent missing")?
                .join(target)
        };
        let mut normalized = PathBuf::new();
        for c in full.components() {
            match c {
                Component::ParentDir => {
                    if !normalized.pop() {
                        return Err("link target escapes permitted root".into());
                    }
                }
                Component::CurDir => {}
                c => normalized.push(c.as_os_str()),
            }
        }
        if !normalized.starts_with(&self.root) {
            return Err("link target escapes permitted root".into());
        }
        // Existing dangling aliases must not be mistaken for safe missing files.
        let mut prefix = self.root.clone();
        for component in normalized
            .strip_prefix(&self.root)
            .map_err(|_| "outside root")?
            .components()
        {
            prefix.push(component.as_os_str());
            if std::fs::symlink_metadata(&prefix).is_ok_and(|m| m.file_type().is_symlink()) {
                let canonical = prefix
                    .canonicalize()
                    .map_err(|_| "symbolic-link target contains an unresolved alias")?;
                if !canonical.starts_with(&self.root) {
                    return Err("symbolic-link target escapes permitted root".into());
                }
            }
        }
        let resolved = self.resolve(normalized.to_str().ok_or("link target must be UTF-8")?)?;
        self.check_write_path(&resolved)?;
        if excalibur_sandbox::file_authority::credential_path(Path::new(&resolved)) {
            return Err("symbolic-link target is a credential path".into());
        }
        Ok(())
    }
    pub(crate) async fn entry_conflict(
        &self,
        run: &str,
        edit: &FileEdit,
        current: &Option<SnapshotEntry>,
    ) -> Result<String, String> {
        let report = self
            .conflict_report(
                run,
                &edit.path,
                edit.pre.as_deref(),
                edit.pre.as_deref(),
                current.as_ref().map(|e| e.content.as_slice()),
                None,
            )
            .await?;
        let mut report: serde_json::Value =
            serde_json::from_str(&report).map_err(|e| e.to_string())?;
        report["entry_metadata"] = json!({"expected_kind":edit.pre_kind,"current_kind":current.as_ref().map(|e|e.kind),"expected_executable":edit.pre_executable,"current_executable":current.as_ref().map(|e|e.executable)});
        Ok(report.to_string())
    }
    pub(crate) fn atomic_entry(&self, edit: &FileEdit, bytes: Option<&[u8]>) -> Result<(), String> {
        if !edit.metadata_guard {
            return self.atomic_write(&edit.path, bytes);
        }
        self.check_write_path(&edit.path)?;
        if self.resolve_entry(&edit.path)? != edit.path {
            return Err("entry parent changed after preparation".into());
        }
        if edit.kind == EntryKind::Symlink
            && let Some(bytes) = bytes
        {
            self.validate_link(&edit.path, bytes)?;
        }
        let current = self.current_entry(&edit.path)?;
        if entry_matches(edit, &current, true) {
            return Ok(());
        }
        if !entry_matches(edit, &current, false) {
            return Err("entry precondition changed immediately before atomic replacement".into());
        }
        let relative = Path::new(&edit.path);
        let parent = relative.parent().unwrap_or(Path::new(""));
        self.directory
            .create_dir_all(parent)
            .map_err(|e| e.to_string())?;
        let dir = if parent.as_os_str().is_empty() {
            self.directory.try_clone()
        } else {
            self.directory.open_dir(parent)
        }
        .map_err(|e| e.to_string())?;
        let name = relative.file_name().ok_or("filename missing")?;
        if let Some(bytes) = bytes {
            let temp = format!(".excalibur-{}", excalibur_types::JobId::new());
            let result = (|| -> Result<(), String> {
                if edit.kind == EntryKind::Symlink {
                    let target =
                        std::str::from_utf8(bytes).map_err(|_| "link target must be UTF-8")?;
                    #[cfg(unix)]
                    dir.symlink_contents(target, &temp)
                        .map_err(|e| e.to_string())?;
                    #[cfg(windows)]
                    dir.symlink_file(target, &temp).map_err(|e| e.to_string())?;
                    #[cfg(not(any(unix, windows)))]
                    return Err("host symbolic-link adapter unavailable".into());
                } else {
                    let mut opts = cap_std::fs::OpenOptions::new();
                    opts.create_new(true).write(true);
                    let mut file = dir.open_with(&temp, &opts).map_err(|e| e.to_string())?;
                    #[cfg(unix)]
                    {
                        use cap_std::fs::PermissionsExt;
                        let mode = if current.as_ref().is_some_and(|e| e.kind == EntryKind::File) {
                            dir.symlink_metadata(name)
                                .map_err(|e| e.to_string())?
                                .permissions()
                                .mode()
                                & 0o777
                        } else {
                            file.metadata()
                                .map_err(|e| e.to_string())?
                                .permissions()
                                .mode()
                                & 0o777
                        };
                        // New files retain the host umask. Executable state does
                        // not broaden read/write permissions on a private file.
                        let mode = if edit.executable {
                            mode | ((mode & 0o444) >> 2) | 0o100
                        } else {
                            mode & !0o111
                        };
                        file.set_permissions(cap_std::fs::Permissions::from_mode(mode))
                            .map_err(|e| e.to_string())?;
                    }
                    file.write_all(bytes).map_err(|e| e.to_string())?;
                    file.sync_all().map_err(|e| e.to_string())?;
                }
                // Creating/fsyncing the temporary entry can take time. The
                // user's current entry remains authoritative until this final check.
                if self.resolve_entry(&edit.path)? != edit.path
                    || !entry_matches(edit, &self.current_entry(&edit.path)?, false)
                {
                    return Err(
                        "entry changed while staging atomic replacement; no replacement applied"
                            .into(),
                    );
                }
                dir.rename(&temp, &dir, name).map_err(|e| e.to_string())?;
                Ok(())
            })();
            if result.is_err() {
                let _ = dir.remove_file(&temp);
            }
            result?;
        } else if current.is_some() {
            dir.remove_file(name).map_err(|e| e.to_string())?;
        }
        #[cfg(unix)]
        dir.open(".")
            .map_err(|e| e.to_string())?
            .sync_all()
            .map_err(|e| e.to_string())?;
        Ok(())
    }
}
pub(crate) fn entry_matches(edit: &FileEdit, current: &Option<SnapshotEntry>, post: bool) -> bool {
    let fp = current.as_ref().map(|e| hash(&e.content));
    let expected = if post { &edit.post } else { &edit.pre };
    if &fp != expected {
        return false;
    }
    if !edit.metadata_guard {
        return true;
    }
    if expected.is_none() {
        return current.is_none();
    }
    let (kind, executable) = if post {
        (Some(edit.kind), Some(edit.executable))
    } else {
        (edit.pre_kind, edit.pre_executable)
    };
    current.as_ref().map(|e| e.kind) == kind && current.as_ref().map(|e| e.executable) == executable
}

#[derive(Clone, serde::Serialize)]
pub(crate) struct ResourceOwner {
    pub resource: String,
    pub holder: String,
    pub activity: String,
    nonce: u64,
}
fn resource_owners() -> &'static std::sync::Mutex<BTreeMap<String, ResourceOwner>> {
    static OWNERS: std::sync::OnceLock<std::sync::Mutex<BTreeMap<String, ResourceOwner>>> =
        std::sync::OnceLock::new();
    OWNERS.get_or_init(Default::default)
}
pub(crate) struct ResourceActivity(Vec<(String, u64)>);
impl ResourceActivity {
    pub(crate) fn new() -> Self {
        Self(Vec::new())
    }
    pub(crate) fn acquired(&mut self, key: &str, run: &str, activity: &str) {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        let nonce = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        resource_owners()
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(
                key.into(),
                ResourceOwner {
                    resource: key.into(),
                    holder: run.into(),
                    activity: activity.into(),
                    nonce,
                },
            );
        self.0.push((key.into(), nonce));
    }
    pub(crate) fn owner(key: &str) -> Option<ResourceOwner> {
        resource_owners()
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(key)
            .cloned()
    }
}
impl Drop for ResourceActivity {
    fn drop(&mut self) {
        let mut map = resource_owners().lock().unwrap_or_else(|p| p.into_inner());
        for (key, nonce) in &self.0 {
            if map.get(key).is_some_and(|owner| owner.nonce == *nonce) {
                map.remove(key);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Access, ToolPolicy};
    use excalibur_store::{Database, DatabaseKind, Priority, ReadLane, StorageConfig};
    use excalibur_types::runtime::{
        Effect, PreparedTool, Reconcile, Resource, ToolCall, ToolRuntime,
    };
    async fn fixture() -> (tempfile::TempDir, Database, LocalTools) {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("project");
        std::fs::create_dir(&root).unwrap();
        let db = Database::open(
            temp.path().join("data/main.db"),
            DatabaseKind::Main,
            StorageConfig::default(),
        )
        .unwrap();
        db.write(Priority::Journal, |c| {
            c.execute("INSERT INTO runs(id) VALUES('one'),('two')", [])?;
            Ok(())
        })
        .await
        .unwrap();
        let tools = LocalTools::new(
            db.clone(),
            root,
            ToolPolicy {
                write: Access::Allow,
                ..Default::default()
            },
        )
        .await
        .unwrap();
        (temp, db, tools)
    }
    async fn planned(
        tools: &LocalTools,
        id: &str,
        path: &str,
        desired: Option<SnapshotEntry>,
    ) -> PreparedTool {
        let current = tools.current_entry(path).unwrap();
        let plan = PlannedFile {
            path: PathBuf::from(path),
            pre: current.as_ref().map(|e| hash(&e.content)),
            post: desired.as_ref().map(|e| e.content.clone()),
            kind: desired.as_ref().map(|e| e.kind).unwrap_or_default(),
            executable: desired.as_ref().is_some_and(|e| e.executable),
            pre_kind: current.as_ref().map(|e| e.kind),
            pre_executable: current.as_ref().map(|e| e.executable),
            metadata_guard: true,
        };
        let edit = tools.prepare_entry_edit("one", id, &plan).await.unwrap();
        PreparedTool {
            call: ToolCall {
                id: id.into(),
                name: "workspace_snapshot".into(),
                arguments: "{}".into(),
            },
            effect: Effect::Idempotent,
            resources: vec![Resource {
                key: format!("file:{}", tools.root.join(path).display()),
                write: true,
            }],
            exclusive: false,
            data: json!({"root":tools.root,"version":1,"edits":[edit]}),
        }
    }
    async fn count(db: &Database) -> u64 {
        db.read(ReadLane::Interactive, |c| {
            Ok(c.query_row("SELECT count(*) FROM file_versions", [], |r| r.get(0))?)
        })
        .await
        .unwrap()
    }
    #[cfg(unix)]
    #[tokio::test]
    async fn chmod_only_interrupted_effect_reconciles_and_history_restores_metadata() {
        use std::os::unix::fs::PermissionsExt;
        let (_temp, db, tools) = fixture().await;
        let path = tools.root.join("script.sh");
        std::fs::write(&path, "echo synthetic\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        let prepared = planned(
            &tools,
            "mode",
            "script.sh",
            Some(SnapshotEntry::file(b"echo synthetic\n".to_vec(), false)),
        )
        .await;
        let edit: FileEdit = serde_json::from_value(prepared.data["edits"][0].clone()).unwrap();
        // Cut after the actual atomic effect, before the durable version receipt.
        tools
            .atomic_entry(&edit, Some(b"echo synthetic\n"))
            .unwrap();
        assert_eq!(count(&db).await, 0);
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o111,
            0
        );
        let reopened = LocalTools::new(
            db.clone(),
            tools.root.clone(),
            ToolPolicy {
                write: Access::Allow,
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert!(matches!(
            reopened.reconcile("one", "mode", &prepared).await,
            Reconcile::Done(crate::ToolResult { success: true, .. })
        ));
        assert!(matches!(
            reopened.reconcile("one", "mode", &prepared).await,
            Reconcile::Done(crate::ToolResult { success: true, .. })
        ));
        assert_eq!(count(&db).await, 1);
        let version = reopened
            .workspace
            .history()
            .file_history(Path::new("script.sh"), 10)
            .await
            .unwrap()
            .remove(0);
        let plan = reopened
            .workspace
            .history()
            .plan_revert_entry(&version.id, reopened.current_entry("script.sh").unwrap())
            .await
            .unwrap();
        assert!(plan.conflicts.is_empty());
        assert!(plan.files[0].executable);
        assert!(plan.files[0].metadata_guard);
        let undo = reopened
            .prepare_entry_edit("one", "restore-mode", &plan.files[0])
            .await
            .unwrap();
        let mut restore = prepared.clone();
        restore.call.id = "restore-mode".into();
        restore.data["edits"] = json!([undo]);
        assert!(
            reopened
                .perform_edits("one", "restore-mode", &restore)
                .await
                .is_ok()
        );
        assert_eq!(
            std::fs::metadata(path).unwrap().permissions().mode() & 0o111,
            0o111
        );
        db.shutdown().await.unwrap();
    }
    #[cfg(unix)]
    #[tokio::test]
    async fn symlink_roundtrip_is_literal_and_never_modifies_its_target() {
        let (_temp, db, tools) = fixture().await;
        std::fs::write(tools.root.join("target"), "target stays\n").unwrap();
        std::fs::write(tools.root.join("entry"), "regular original\n").unwrap();
        let prepared = planned(
            &tools,
            "link",
            "entry",
            Some(SnapshotEntry::symlink(b"target".to_vec())),
        )
        .await;
        assert!(tools.perform_edits("one", "link", &prepared).await.is_ok());
        assert!(
            std::fs::symlink_metadata(tools.root.join("entry"))
                .unwrap()
                .is_symlink()
        );
        assert_eq!(
            tools.current_entry("entry").unwrap(),
            Some(SnapshotEntry::symlink(b"target".to_vec()))
        );
        assert!(matches!(
            tools.reconcile("one", "link", &prepared).await,
            Reconcile::Done(crate::ToolResult { success: true, .. })
        ));
        assert_eq!(count(&db).await, 1);
        let version = tools
            .workspace
            .history()
            .file_history(Path::new("entry"), 10)
            .await
            .unwrap()
            .remove(0);
        let plan = tools
            .workspace
            .history()
            .plan_revert_entry(&version.id, tools.current_entry("entry").unwrap())
            .await
            .unwrap();
        assert!(plan.conflicts.is_empty());
        let undo = tools
            .prepare_entry_edit("one", "unlink", &plan.files[0])
            .await
            .unwrap();
        let mut restore = prepared.clone();
        restore.call.id = "unlink".into();
        restore.data["edits"] = json!([undo]);
        tools
            .perform_edits("one", "unlink", &restore)
            .await
            .unwrap();
        assert_eq!(
            std::fs::read(tools.root.join("entry")).unwrap(),
            b"regular original\n"
        );
        assert!(
            !std::fs::symlink_metadata(tools.root.join("entry"))
                .unwrap()
                .is_symlink()
        );
        assert_eq!(
            std::fs::read(tools.root.join("target")).unwrap(),
            b"target stays\n"
        );
        db.shutdown().await.unwrap();
    }
    #[cfg(unix)]
    #[tokio::test]
    async fn metadata_conflicts_escalate_without_false_history_and_unsafe_links_are_rejected() {
        use std::os::unix::fs::PermissionsExt;
        let (temp, db, tools) = fixture().await;
        let path = tools.root.join("entry");
        std::fs::write(&path, "same bytes").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        let prepared = planned(
            &tools,
            "change",
            "entry",
            Some(SnapshotEntry::file(b"new bytes".to_vec(), false)),
        )
        .await;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        for i in 1..=3 {
            let error = tools
                .perform_edits("one", &format!("conflict-{i}"), &prepared)
                .await
                .unwrap_err();
            assert!(error.contains("entry_metadata"), "{error}");
            if i == 3 {
                assert!(error.contains("escalat"), "{error}");
            }
        }
        assert_eq!(count(&db).await, 0);
        assert_eq!(std::fs::read(&path).unwrap(), b"same bytes");
        assert!(tools.validate_link("entry", b"../outside").is_err());
        assert!(tools.validate_link("entry", b".ssh/id_rsa").is_err());
        assert!(tools.validate_link("entry", b".env").is_err());
        let outside = temp.path().join("outside");
        std::fs::write(&outside, "not read").unwrap();
        std::os::unix::fs::symlink(&outside, tools.root.join("outside-link")).unwrap();
        assert_eq!(
            tools.current_entry("outside-link").unwrap().unwrap().kind,
            EntryKind::Symlink
        );
        db.shutdown().await.unwrap();
    }
    #[tokio::test(start_paused = true)]
    async fn resource_lock_wait_is_bounded_and_reports_actual_holder_activity() {
        let (_temp, db, tools) = fixture().await;
        let key = format!("file:{}", tools.root.join("blocked").display());
        let lock = crate::resource_lock(&key);
        let held = lock.lock().await;
        let mut holder = ResourceActivity::new();
        holder.acquired(&key, "two", "long synthetic write");
        let tool = PreparedTool {
            call: ToolCall {
                id: "blocked".into(),
                name: "write_file".into(),
                arguments: "{}".into(),
            },
            effect: Effect::Idempotent,
            resources: vec![Resource { key, write: true }],
            exclusive: false,
            data: json!({"root":tools.root,"version":1,"edits":[]}),
        };
        let mut future = std::pin::pin!(tools.execute_locked("one", "blocked", &tool));
        std::future::poll_fn(|cx| {
            assert!(future.as_mut().poll(cx).is_pending());
            std::task::Poll::Ready(())
        })
        .await;
        tokio::time::advance(std::time::Duration::from_secs(30)).await;
        let result = future.await;
        assert!(!result.success);
        assert!(result.output.to_string().contains("long synthetic write"));
        assert!(result.output.to_string().contains("two"));
        drop(held);
        drop(holder);
        db.shutdown().await.unwrap();
    }
    #[tokio::test]
    async fn durable_leases_survive_calls_and_real_control_releases_them() {
        let (_temp, db, tools) = fixture().await;
        db.write(Priority::Journal, |c| {
            c.execute("UPDATE runs SET state='running' WHERE id='one'", [])?;
            Ok(())
        })
        .await
        .unwrap();
        tools.execution_state("one", "running", None).await.unwrap();
        let prepared = planned(
            &tools,
            "first",
            "entry",
            Some(SnapshotEntry::file(b"first".to_vec(), false)),
        )
        .await;
        tools
            .perform_edits("one", "first", &prepared)
            .await
            .unwrap();
        assert_eq!(tools.workspace.leases().owners().await.unwrap().len(), 1);
        db.write(Priority::Journal, |c| {
            c.execute(
                "UPDATE runs SET state='paused',reason='user' WHERE id='one'",
                [],
            )?;
            Ok(())
        })
        .await
        .unwrap();
        tools
            .execution_state("one", "paused", Some("user"))
            .await
            .unwrap();
        assert!(tools.workspace.leases().owners().await.unwrap().is_empty());
        // Even a stale direct effect cannot revive a human-stopped run.
        assert!(
            tools
                .perform_edits("one", "stale", &prepared)
                .await
                .is_err()
        );
        db.shutdown().await.unwrap();
    }
}
