use crate::{LocalTools, Permission};
use cap_std::fs::Dir;
use excalibur_store::{BlobHash, Priority, ReadLane};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    io::{Read, Write},
    path::{Component, Path, PathBuf},
    str::FromStr,
};

pub(crate) const MAX_FILE_BYTES: u64 = 64 * 1024 * 1024;
pub(crate) fn hash(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct FileEdit {
    pub path: String,
    pub pre: Option<String>,
    pub post: Option<String>,
}
impl LocalTools {
    pub(crate) fn resolve(&self, path: &str) -> Result<String, String> {
        let path = Path::new(path);
        if path.components().any(|p| matches!(p, Component::ParentDir)) {
            return Err("parent traversal (..) is forbidden".into());
        }
        let full = if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.root.join(path)
        };
        if !full.starts_with(&self.root) {
            return Err("path is outside permitted root".into());
        }
        let mut ancestor = full.as_path();
        let mut tail = Vec::new();
        while !ancestor.exists() {
            tail.push(
                ancestor
                    .file_name()
                    .ok_or("path has no existing parent")?
                    .to_os_string(),
            );
            ancestor = ancestor.parent().ok_or("path has no existing parent")?;
        }
        let mut resolved = ancestor.canonicalize().map_err(|e| e.to_string())?;
        if !resolved.starts_with(&self.root) {
            return Err("symlink resolves outside permitted root".into());
        }
        for name in tail.into_iter().rev() {
            resolved.push(name);
        }
        if self
            .policy
            .protected_paths
            .iter()
            .any(|p| resolved.starts_with(p))
        {
            return Err("security policy: protected host path is inaccessible".into());
        }
        let relative = resolved
            .strip_prefix(&self.root)
            .map_err(|_| "outside root")?;
        if relative.as_os_str().is_empty() {
            return Err("file path must not be root".into());
        }
        relative
            .to_str()
            .map(str::to_owned)
            .ok_or_else(|| "path is not UTF-8".into())
    }
    pub(crate) fn content(&self, path: &str) -> Result<Option<Vec<u8>>, String> {
        match self.directory.open(path) {
            Ok(file) => {
                let mut out = Vec::new();
                file.take(MAX_FILE_BYTES + 1)
                    .read_to_end(&mut out)
                    .map_err(|e| e.to_string())?;
                if out.len() as u64 > MAX_FILE_BYTES {
                    return Err("file exceeds 64 MiB limit".into());
                }
                Ok(Some(out))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.to_string()),
        }
    }
    pub(crate) async fn save_blob(
        &self,
        run: &str,
        call: &str,
        slot: &str,
        bytes: Vec<u8>,
    ) -> Result<String, String> {
        if self.policy.redactor.redact_bytes(&bytes) != bytes {
            return Err(
                "security policy: secret content cannot be persisted in file snapshots".into(),
            );
        }
        let blobs = self.db.blobs();
        let prepared = tokio::task::spawn_blocking(move || blobs.prepare(&bytes))
            .await
            .map_err(|e| e.to_string())?
            .map_err(|e| e.to_string())?;
        self.db
            .attach_blob(
                prepared,
                "tool_prepare".into(),
                format!("{run}:{call}"),
                slot.into(),
            )
            .await
            .map(|h| h.as_str().to_owned())
            .map_err(|e| e.to_string())
    }
    pub(crate) async fn read_snapshot(
        &self,
        run: &str,
        path: &str,
    ) -> Result<Option<(String, String, i64)>, String> {
        let (run, path) = (
            run.to_owned(),
            self.root.join(path).to_string_lossy().into_owned(),
        );
        self.db
            .read(ReadLane::Interactive, move |conn| {
                use excalibur_store::rusqlite::OptionalExtension;
                Ok(conn
                    .query_row(
                        "SELECT fp,blob,mismatches FROM tool_readset WHERE run_id=?1 AND path=?2",
                        [run, path],
                        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                    )
                    .optional()?)
            })
            .await
            .map_err(|e| e.to_string())
    }
    pub(crate) async fn remember(
        &self,
        run: &str,
        call: &str,
        path: &str,
        bytes: Vec<u8>,
    ) -> Result<String, String> {
        let fp = hash(&bytes);
        let blob = self
            .save_blob(
                run,
                call,
                &format!(
                    "read-{}",
                    hash(self.root.join(path).to_string_lossy().as_bytes())
                ),
                bytes,
            )
            .await?;
        let (run, path, copy) = (
            run.to_owned(),
            self.root.join(path).to_string_lossy().into_owned(),
            fp.clone(),
        );
        self.db.write(Priority::Normal,move|conn|{
            conn.execute("INSERT INTO tool_readset(run_id,path,fp,blob,mismatches) VALUES(?1,?2,?3,?4,0) ON CONFLICT(run_id,path) DO UPDATE SET fp=excluded.fp,blob=excluded.blob,mismatches=0",[&run,&path,&copy,&blob])?;
            conn.execute("DELETE FROM tool_conflicts WHERE run_id=?1 AND path=?2",[run,path])?;Ok(())
        }).await.map_err(|e|e.to_string())?;
        Ok(fp)
    }
    pub(crate) async fn ensure_read(
        &self,
        run: &str,
        path: &str,
        current: &[u8],
        supplied: Option<&str>,
    ) -> Result<(), String> {
        let Some((fp, blob, _)) = self.read_snapshot(run, path).await? else {
            return Err("existing file must be read_file in this run before writing".into());
        };
        let actual = hash(current);
        if fp == actual && supplied.is_none_or(|s| s == fp) {
            return Ok(());
        }
        Err(self
            .conflict_report(run, path, Some(&fp), Some(&blob), Some(current), supplied)
            .await?)
    }

    /// A single durable counter/report path for both preparation and execution.
    /// For an admitted write, expected bytes come from its intention, not a newer readset.
    pub(crate) async fn conflict_report(
        &self,
        run: &str,
        path: &str,
        expected: Option<&str>,
        preimage: Option<&str>,
        current: Option<&[u8]>,
        supplied: Option<&str>,
    ) -> Result<String, String> {
        let (run_copy, path_copy) = (
            run.to_owned(),
            self.root.join(path).to_string_lossy().into_owned(),
        );
        let failures=self.db.write(Priority::Interactive,move|conn|{
            let count=conn.query_row("INSERT INTO tool_conflicts(run_id,path,mismatches) VALUES(?1,?2,1) ON CONFLICT(run_id,path) DO UPDATE SET mismatches=mismatches+1 RETURNING mismatches",[&run_copy,&path_copy],|r|r.get::<_,i64>(0))?;
            conn.execute("UPDATE tool_readset SET mismatches=?3 WHERE run_id=?1 AND path=?2",excalibur_store::rusqlite::params![run_copy,path_copy,count])?;Ok(count)
        }).await.map_err(|e|e.to_string())?;
        let prior = match preimage {
            Some(blob) => self
                .db
                .read_blob(
                    BlobHash::from_str(blob).map_err(|e| e.to_string())?,
                    MAX_FILE_BYTES,
                )
                .await
                .map_err(|e| e.to_string())?,
            None => Vec::new(),
        };
        let prior = String::from_utf8_lossy(&prior);
        let current_text = String::from_utf8_lossy(current.unwrap_or_default());
        let expected_label = if expected.is_some() {
            "expected"
        } else {
            "expected (file absent)"
        };
        let current_label = if current.is_some() {
            "current"
        } else {
            "current (file absent)"
        };
        let diff = similar::TextDiff::from_lines(prior.as_ref(), current_text.as_ref())
            .unified_diff()
            .context_radius(2)
            .header(expected_label, current_label)
            .to_string();
        let mut ranges = diff
            .lines()
            .filter(|line| line.starts_with("@@"))
            .take(9)
            .map(str::to_owned)
            .collect::<Vec<_>>();
        let ranges_truncated = ranges.len() > 8;
        ranges.truncate(8);
        Ok(json!({"error":"fingerprint_conflict","path":path,"expected_fp":expected,"current_fp":current.map(hash),"supplied_fp":supplied,
            "expected_exists":expected.is_some(),"current_exists":current.is_some(),"diff":crate::head_tail(&diff,4096),
            "affected_region":{"line_ranges":ranges,"line_ranges_truncated":ranges_truncated,"existence_changed":expected.is_some()!=current.is_some()},
            "mismatches":failures,"escalate_to_parent":failures>=3}).to_string())
    }

    pub(crate) async fn forget_snapshot(&self, run: &str, path: &str) -> Result<(), String> {
        let (run, path) = (
            run.to_owned(),
            self.root.join(path).to_string_lossy().into_owned(),
        );
        self.db
            .write(Priority::Normal, move |conn| {
                conn.execute(
                    "DELETE FROM tool_readset WHERE run_id=?1 AND path=?2",
                    [&run, &path],
                )?;
                conn.execute(
                    "DELETE FROM tool_conflicts WHERE run_id=?1 AND path=?2",
                    [run, path],
                )?;
                Ok(())
            })
            .await
            .map_err(|e| e.to_string())
    }

    pub(crate) async fn prepare_edit(
        &self,
        run: &str,
        call: &str,
        path: &str,
        new: Option<Vec<u8>>,
        fp: Option<&str>,
    ) -> Result<FileEdit, String> {
        self.check_permission(Permission::Write)?;
        let path = self.resolve(path)?;
        self.check_write_path(&path)?;
        let current = self.content(&path)?;
        if let Some(content) = current.as_deref() {
            self.ensure_read(run, &path, content, fp).await?;
        } else if let Some((expected, blob, _)) = self.read_snapshot(run, &path).await? {
            return Err(self
                .conflict_report(run, &path, Some(&expected), Some(&blob), None, fp)
                .await?);
        } else if fp.is_some() {
            return Err("fp supplied for a nonexistent file".into());
        }
        let pre = match current {
            Some(bytes) => Some(
                self.save_blob(
                    run,
                    call,
                    &format!(
                        "pre-{}",
                        hash(self.root.join(&path).to_string_lossy().as_bytes())
                    ),
                    bytes,
                )
                .await?,
            ),
            None => None,
        };
        let post = match new {
            Some(bytes) => {
                if bytes.len() as u64 > MAX_FILE_BYTES {
                    return Err("write exceeds 64 MiB limit".into());
                }
                Some(
                    self.save_blob(
                        run,
                        call,
                        &format!(
                            "post-{}",
                            hash(self.root.join(&path).to_string_lossy().as_bytes())
                        ),
                        bytes,
                    )
                    .await?,
                )
            }
            None => None,
        };
        Ok(FileEdit { path, pre, post })
    }
    pub(crate) fn check_write_path(&self, path: &str) -> Result<(), String> {
        let components = std::path::Path::new(path)
            .components()
            .map(|c| c.as_os_str().to_string_lossy().to_ascii_lowercase())
            .collect::<Vec<_>>();
        if components.iter().any(|c| {
            matches!(
                c.as_str(),
                ".git"
                    | ".excalibur"
                    | ".agents"
                    | ".codex"
                    | ".ssh"
                    | ".aws"
                    | ".github"
                    | ".gitlab"
            )
        }) || components
            .windows(2)
            .any(|c| c[0] == ".config" && c[1] == "gcloud")
            || components
                .iter()
                .any(|c| matches!(c.as_str(), ".gitlab-ci.yml" | "jenkinsfile"))
        {
            return Err("security policy: sensitive configuration is read-only".into());
        }
        Ok(())
    }
    pub(crate) fn atomic_write(&self, path: &str, bytes: Option<&[u8]>) -> Result<(), String> {
        self.check_write_path(path)?;
        let normalized = self.resolve(path)?;
        if normalized != path {
            return Err("path changed after preparation".into());
        }
        let relative = Path::new(path);
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
        match bytes {
            None => dir.remove_file(name).map_err(|e| e.to_string())?,
            Some(bytes) => {
                let temp = format!(".excalibur-{}", excalibur_types::JobId::new());
                let mut opts = cap_std::fs::OpenOptions::new();
                opts.write(true).create_new(true);
                let mut file = dir.open_with(&temp, &opts).map_err(|e| e.to_string())?;
                if let Ok(meta) = dir.metadata(name) {
                    file.set_permissions(meta.permissions())
                        .map_err(|e| e.to_string())?;
                }
                let result = (|| {
                    file.write_all(bytes)?;
                    file.sync_all()?;
                    dir.rename(&temp, &dir, name)?;
                    Ok::<_, std::io::Error>(())
                })();
                if let Err(e) = result {
                    let _ = dir.remove_file(&temp);
                    return Err(e.to_string());
                }
            }
        }
        #[cfg(unix)]
        dir.open(".")
            .map_err(|e| e.to_string())?
            .sync_all()
            .map_err(|e| e.to_string())?;
        Ok(())
    }
    pub(crate) async fn record_edit(
        &self,
        run: &str,
        key: &str,
        call_id: &str,
        edit: &FileEdit,
    ) -> Result<(), String> {
        let (id, path, pre, post, run, call) = (
            format!(
                "file_{}",
                hash(format!("{key}:{}", self.root.join(&edit.path).display()).as_bytes())
            ),
            self.root.join(&edit.path).to_string_lossy().into_owned(),
            edit.pre.clone(),
            edit.post.clone(),
            run.to_owned(),
            call_id.to_owned(),
        );
        self.db.write(Priority::Normal,move|conn|{conn.execute("INSERT INTO file_versions(id,path,preimage_blob,postimage_blob,run_id,tool_call_id) VALUES(?1,?2,?3,?4,?5,?6) ON CONFLICT(id) DO NOTHING",excalibur_store::rusqlite::params![id,path,pre,post,run,call])?;Ok(())}).await.map_err(|e|e.to_string())
    }
}
pub(crate) fn open_root(root: PathBuf) -> Result<(PathBuf, Dir), String> {
    let root = root.canonicalize().map_err(|e| e.to_string())?;
    let dir =
        Dir::open_ambient_dir(&root, cap_std::ambient_authority()).map_err(|e| e.to_string())?;
    Ok((root, dir))
}
pub(crate) fn required_str<'a>(args: &'a Value, key: &str) -> Result<&'a str, String> {
    args.get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("{key}: expected string"))
}
