use super::*;
use crate::sessions::{Manager, now};
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::{Read, Write},
    os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
};
pub(super) struct Staged {
    run: String,
    sid: String,
    path: PathBuf,
    expires: i64,
    complete: bool,
}
const MAX_FILE: u64 = 200 * 1024 * 1024;
fn private(path: &Path) -> Result<()> {
    if !path.exists() {
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(path)?;
    }
    use std::os::unix::fs::MetadataExt;
    let m = fs::symlink_metadata(path)?;
    if !m.is_dir() || m.uid() != unsafe { libc::geteuid() } || m.mode() & 0o077 != 0 {
        return Err(Fault::new("denied", "unsafe file staging directory"));
    }
    Ok(())
}
fn filename(value: &str) -> String {
    let value = value
        .chars()
        .take(180)
        .map(|c| {
            if c.is_alphanumeric() || matches!(c, '.' | '-' | '_') {
                c
            } else {
                '_'
            }
        })
        .collect::<String>();
    let value = value.trim_start_matches('.');
    if value.is_empty() {
        "file".into()
    } else {
        value.into()
    }
}
impl Observation {
    pub(crate) fn upload_paths(&self, run: &str, sid: &str, ids: &[String]) -> Result<Vec<String>> {
        if ids.is_empty() {
            return Err(Fault::new("invalid_params", "staged files required"));
        }
        let map = self.actions.staged.lock().unwrap();
        ids.iter()
            .map(|id| {
                let file = map
                    .get(id)
                    .filter(|f| f.run == run && f.sid == sid && f.expires > now() && f.complete)
                    .ok_or_else(|| Fault::new("denied", "unknown or expired staged upload"))?;
                let m = fs::symlink_metadata(&file.path)?;
                if !m.is_file() || m.len() > MAX_FILE {
                    return Err(Fault::new("denied", "staged upload changed"));
                }
                Ok(file.path.to_string_lossy().into())
            })
            .collect()
    }
    pub(crate) fn cleanup_uploads(&self, sid: Option<&str>) {
        let mut files = self.actions.staged.lock().unwrap();
        files.retain(|_, f| {
            if sid == Some(f.sid.as_str()) || f.expires <= now() {
                let _ = fs::remove_dir_all(f.path.parent().unwrap());
                false
            } else {
                true
            }
        });
    }
}
impl Manager {
    pub async fn stage_upload(&self, run: &str, sid: &str, tid: &str, p: &Value) -> Result<Value> {
        let _lease = self.lease(run, sid, tid, false).await?;
        self.observation_context(run, sid, tid).await?;
        use base64::Engine;
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(
                p["data"]
                    .as_str()
                    .ok_or_else(|| Fault::new("invalid_params", "base64 data"))?,
            )
            .map_err(|_| Fault::new("invalid_params", "invalid base64"))?;
        if bytes.len() > 2 * 1024 * 1024 {
            return Err(Fault::new("invalid_params", "upload chunk limit"));
        }
        self.observation.cleanup_uploads(None);
        let mut files = self.observation.actions.staged.lock().unwrap();
        let id = if let Some(id) = p["staged_id"].as_str() {
            id.to_owned()
        } else {
            if files.len() >= 128 {
                return Err(Fault::new("capacity_exceeded", "staged uploads"));
            }
            let id = uuid::Uuid::new_v4().to_string();
            let root = self.observation.root.join("staging");
            private(&root)?;
            let session = root.join(sid);
            private(&session)?;
            let dir = session.join(&id);
            private(&dir)?;
            let path = dir.join(filename(p["file_name"].as_str().unwrap_or("file")));
            fs::OpenOptions::new()
                .create_new(true)
                .write(true)
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW)
                .open(&path)?;
            files.insert(
                id.clone(),
                Staged {
                    run: run.into(),
                    sid: sid.into(),
                    path,
                    expires: now() + 3600,
                    complete: false,
                },
            );
            id
        };
        let staged = files
            .get_mut(&id)
            .filter(|f| f.run == run && f.sid == sid && !f.complete)
            .ok_or_else(|| Fault::new("denied", "staged upload owner or state"))?;
        let mut file = fs::OpenOptions::new()
            .append(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&staged.path)?;
        let size = file.metadata()?.len();
        if p["offset"].as_u64().unwrap_or(0) != size || size + bytes.len() as u64 > MAX_FILE {
            return Err(Fault::new("invalid_params", "staging offset or size"));
        }
        file.write_all(&bytes)?;
        file.sync_data()?;
        staged.complete = p["final"].as_bool().unwrap_or(true);
        Ok(json!({"staged_id":id,"size":size+bytes.len() as u64,"complete":staged.complete}))
    }
    pub async fn download(&self, run: &str, sid: &str, tid: &str, p: &Value) -> Result<Value> {
        let _lease = self.lease(run, sid, tid, false).await?;
        let mut ctx = self.observation_context(run, sid, tid).await?;
        ctx.session.owner_run = run.into();
        let frames = self.observation.frames(&ctx).await?;
        let lock_key = format!(
            "{}:{}",
            ctx.session.instance_id,
            ctx.session.context_id.as_deref().unwrap_or("default")
        );
        let lock = self
            .observation
            .actions
            .download_locks
            .lock()
            .unwrap()
            .entry(lock_key)
            .or_default()
            .clone();
        let context_lock = tokio::time::timeout(Duration::from_secs(30), lock.lock_owned())
            .await
            .map_err(|_| Fault::new("tab_busy", "download context"))?;
        let root = self.observation.root.join("quarantine");
        private(&root)?;
        let session = root.join(sid);
        private(&session)?;
        let dir = session.join(uuid::Uuid::new_v4().to_string());
        private(&dir)?;
        let mut args = json!({"behavior":"allowAndName","downloadPath":dir,"eventsEnabled":true});
        if let Some(id) = &ctx.session.context_id {
            args["browserContextId"] = json!(id)
        }
        let mut events = ctx.client.subscribe_all();
        let ids = Arc::new(Mutex::new(Vec::<String>::new()));
        let _guard = DownloadGuard {
            client: ctx.client.clone(),
            context: ctx.session.context_id.clone(),
            dir: dir.clone(),
            lock: Some(context_lock),
            ids: ids.clone(),
        };
        ctx.client
            .send(Method::BrowserSetDownloadBehavior, args, None)
            .await?;
        let trigger = &p["trigger"];
        if let Some(url) = trigger["url"].as_str() {
            self.document_allowed(&ctx.session, url, true)?;
            ctx.client
                .send(
                    Method::PageNavigate,
                    json!({"url":url}),
                    ctx.tab.cdp_session.as_deref(),
                )
                .await?;
        } else {
            let request: actions::ActionRequest = serde_json::from_value(
                json!({"action":"click","snapshot_id":trigger["snapshot_id"],"ref":trigger["ref"]}),
            )
            .map_err(|_| Fault::new("invalid_params", "download trigger"))?;
            let target = self
                .observation
                .target(
                    &ctx,
                    &request,
                    request
                        .reference
                        .as_deref()
                        .ok_or_else(|| Fault::new("invalid_params", "trigger ref"))?,
                )
                .await?;
            self.document_allowed(&ctx.session, &target.frame.url, true)?;
            actions::download_click(&ctx, target).await?;
        }
        let deadline = tokio::time::Instant::now()
            + Duration::from_millis(p["timeout_ms"].as_u64().unwrap_or(120000).clamp(1, 120000));
        let mut guid: Option<String> = None;
        let mut suggested = String::new();
        loop {
            if tokio::time::Instant::now() >= deadline {
                return Err(Fault::new("download_timeout", "download deadline"));
            }
            if let Some(id) = &guid
                && [dir.join(id), dir.join(format!("{id}.crdownload"))]
                    .iter()
                    .any(|p| fs::metadata(p).is_ok_and(|m| m.len() > MAX_FILE))
            {
                cancel(&ctx.client, id, &ctx.session.context_id).await;
                return Err(Fault::new("download_too_large", "200 MiB limit"));
            }
            let event = match tokio::time::timeout(Duration::from_millis(50), events.recv()).await {
                Ok(Ok(e)) => e,
                Ok(Err(_)) => return Err(Fault::new("uncertain", "download events lost")),
                Err(_) => continue,
            };
            if event.method == "Browser.downloadWillBegin" {
                let id = event.params["guid"]
                    .as_str()
                    .ok_or_else(|| Fault::new("download_error", "missing GUID"))?;
                if uuid::Uuid::parse_str(id).is_err() {
                    return Err(Fault::new("download_error", "invalid GUID"));
                }
                if !frames.iter().any(|f| event.params["frameId"] == f.id) || guid.is_some() {
                    cancel(&ctx.client, id, &ctx.session.context_id).await;
                    continue;
                }
                if self
                    .document_allowed(
                        &ctx.session,
                        event.params["url"].as_str().unwrap_or(""),
                        true,
                    )
                    .is_err()
                {
                    cancel(&ctx.client, id, &ctx.session.context_id).await;
                    return Err(Fault::new("denied", "download_origin"));
                }
                ids.lock().unwrap().push(id.to_owned());
                guid = Some(id.to_owned());
                suggested = filename(event.params["suggestedFilename"].as_str().unwrap_or("file"));
            }
            if event.method == "Browser.downloadProgress"
                && guid.as_deref() == event.params["guid"].as_str()
            {
                let id = guid.as_ref().unwrap();
                if event.params["receivedBytes"].as_f64().unwrap_or(0.) > MAX_FILE as f64 {
                    cancel(&ctx.client, id, &ctx.session.context_id).await;
                    return Err(Fault::new("download_too_large", "200 MiB limit"));
                }
                if event.params["state"] == "canceled" {
                    return Err(Fault::new("download_canceled", "Chrome canceled download"));
                }
                if event.params["state"] == "completed" {
                    let path = dir.join(id);
                    let metadata = fs::symlink_metadata(&path)?;
                    if !metadata.is_file() || metadata.len() > MAX_FILE {
                        return Err(Fault::new("download_too_large", "download size or type"));
                    }
                    fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
                    let file = fs::OpenOptions::new()
                        .read(true)
                        .custom_flags(libc::O_NOFOLLOW)
                        .open(&path)?;
                    let mut bytes = vec![];
                    file.take(MAX_FILE + 1).read_to_end(&mut bytes)?;
                    if bytes.len() as u64 > MAX_FILE {
                        return Err(Fault::new("download_too_large", "download limit"));
                    }
                    let sha = format!("{:x}", Sha256::digest(&bytes));
                    let size = bytes.len();
                    let mime = magic(&bytes);
                    let marker = format!("download:{}", uuid::Uuid::new_v4());
                    let blob = self
                        .observation
                        .blob(run, &marker, "download", bytes, mime)
                        .await?;
                    let h = blob.sha256.clone();
                    let owner = run.to_owned();
                    self.db
                        .call(move |c| {
                            c.execute(
                                "INSERT INTO web_observation_blobs VALUES(?1,?2,?3,?4)",
                                rusqlite::params![h, owner, marker, now()],
                            )?;
                            Ok(())
                        })
                        .await?;
                    fs::remove_file(&path)?;
                    return Ok(
                        json!({"blob":blob,"file_name":suggested,"mime":mime,"size":size,"sha256":sha,"quarantine_status":"host_file_control_required"}),
                    );
                }
            }
        }
    }
}
/// Content sniffing shared with the `file_control` gate: PDF, PNG, JPEG,
/// ZIP (`PK\x03\x04`) and UTF-8 text, otherwise opaque bytes.
/// `file_control::content_type` remains the enforcing admission check for
/// downloads; this mirror only labels the intermediate webd blob.
fn magic(bytes: &[u8]) -> &'static str {
    if bytes.starts_with(b"%PDF-") {
        "application/pdf"
    } else if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        "image/png"
    } else if bytes.starts_with(b"\xff\xd8\xff") {
        "image/jpeg"
    } else if bytes.starts_with(b"PK\x03\x04") {
        "application/zip"
    } else if std::str::from_utf8(bytes).is_ok() {
        "text/plain"
    } else {
        "application/octet-stream"
    }
}
async fn cancel(client: &Client, id: &str, context: &Option<String>) {
    let mut p = json!({"guid":id});
    if let Some(c) = context {
        p["browserContextId"] = json!(c)
    }
    let _ = client
        .send_with_timeout(
            Method::BrowserCancelDownload,
            p,
            None,
            Duration::from_millis(500),
        )
        .await;
}
struct DownloadGuard {
    client: Client,
    context: Option<String>,
    dir: PathBuf,
    lock: Option<tokio::sync::OwnedMutexGuard<()>>,
    ids: Arc<Mutex<Vec<String>>>,
}
impl Drop for DownloadGuard {
    fn drop(&mut self) {
        let c = self.client.clone();
        let context = self.context.clone();
        let dir = self.dir.clone();
        let lock = self.lock.take();
        let ids = self.ids.lock().unwrap().clone();
        tokio::spawn(async move {
            let _lock = lock;
            let mut p = json!({"behavior":"deny","eventsEnabled":true});
            if let Some(id) = &context {
                p["browserContextId"] = json!(id)
            }
            let _ = c
                .send_with_timeout(
                    Method::BrowserSetDownloadBehavior,
                    p,
                    None,
                    Duration::from_millis(500),
                )
                .await;
            for id in ids {
                cancel(&c, &id, &context).await;
            }
            let _ = fs::remove_dir_all(dir);
        });
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn suggested_names_never_become_paths() {
        assert_eq!(filename("../../etc/passwd"), "_.._etc_passwd");
        assert!(!filename("a/b\\c\0d").contains(['/', '\\', '\0']));
        assert_eq!(filename("..."), "file");
        assert_eq!(filename(""), "file");
        assert_eq!(filename(&"x".repeat(400)).len(), 180);
    }
    #[test]
    fn types_come_from_content() {
        assert_eq!(magic(b"%PDF-1.4"), "application/pdf");
        assert_eq!(magic(b"\x89PNG\r\n\x1a\nrest"), "image/png");
        assert_eq!(magic(b"PK\x03\x04rest"), "application/zip");
        assert_eq!(magic(&[0xff, 0xfe, 0x00, 0x80]), "application/octet-stream");
    }
}
