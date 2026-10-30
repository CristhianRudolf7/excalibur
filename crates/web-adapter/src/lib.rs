//! Trusted host adapter for browser effects, file authority and reference-only credentials.
//! This does not register agent tools or a viewer.
pub mod harness;
mod transport;
use excalibur_browser::observation::actions::ActionRequest;
use excalibur_security::{Action, Decision, FileScanner, SecretExecutor, Security};
use excalibur_store::Store;
use excalibur_types::runtime::{Effect, PreparedTool, ToolCall, ToolResult};
use excalibur_vault::{SecretKind, SecretRef, Vault};
use serde_json::{Value, json};
use std::{
    io::Read,
    path::{Path, PathBuf},
    sync::Arc,
};
pub use transport::{BrowserBackend, LocalBackend, SocketBackend};
pub struct WebAdapter {
    backend: Arc<dyn BrowserBackend>,
    security: Security,
    vault: Arc<Vault>,
    executor: SecretExecutor,
    scanner: Option<Arc<dyn FileScanner>>,
}
// Explicit host identities are intentionally separate from model action arguments.
#[allow(clippy::too_many_arguments)]
impl WebAdapter {
    pub fn new(
        backend: Arc<dyn BrowserBackend>,
        security: Security,
        vault: Arc<Vault>,
        scanner: Option<Arc<dyn FileScanner>>,
    ) -> Result<Self, String> {
        let executor =
            SecretExecutor::new(security.clone(), vault.clone()).map_err(|e| e.to_string())?;
        Ok(Self {
            backend,
            security,
            vault,
            executor,
            scanner,
        })
    }
    pub async fn act(
        &self,
        run: &str,
        op: &str,
        sid: &str,
        tid: &str,
        request: ActionRequest,
        now: i64,
    ) -> Result<ToolResult, String> {
        let args = json!({"session_id":sid,"tab_id":tid,"request":request});
        if self.security.redactor().redact_json(&args) != args {
            return Err("secret literal rejected; use credential references".into());
        }
        let prepared = PreparedTool {
            call: ToolCall {
                id: op.into(),
                name: "__host_browser_action".into(),
                arguments: args.to_string(),
            },
            effect: match request.action.as_str() {
                "scroll" | "hover" => Effect::Pure,
                "type" | "select" => Effect::Idempotent,
                _ => Effect::NonIdempotent,
            },
            resources: vec![],
            exclusive: true,
            data: json!({"session_id":sid,"tab_id":tid}),
        };
        if let Some(receipt) = self
            .security
            .host_effect_receipt(run, &prepared, now)
            .await
            .map_err(|e| e.to_string())?
        {
            return Ok(receipt);
        }
        let plan = self
            .backend
            .request(
                "act.prepare",
                params(run, sid, tid, json!({"request":request})),
            )
            .await?;
        let class = plan["action_class"]
            .as_str()
            .ok_or("missing action class")?;
        let authority = if class == "observe" {
            Action::Data {
                scope: "browser.observe".into(),
            }
        } else {
            Action::Browser {
                class: class.into(),
                origin: plan["origin"].as_str().unwrap_or("").into(),
                target: plan["name"].as_str().unwrap_or("").into(),
                persistent: plan["persistent"] == true,
            }
        };
        let backend = self.backend.clone();
        let p = params(
            run,
            sid,
            tid,
            json!({"plan_id":plan["plan_id"],"op_key":op}),
        );
        let result = self
            .security
            .execute_host_effect(run, &prepared, &[authority], now, move || async move {
                match backend.request("act.perform", p).await {
                    Ok(value) => ToolResult::ok(value),
                    Err(e) => ToolResult::error(e),
                }
            })
            .await
            .map_err(|e| e.to_string())?;
        self.security
            .mark_untrusted(run, "browser_action_output", now)
            .await
            .map_err(|e| e.to_string())?;
        Ok(result)
    }
    pub async fn credential(
        &self,
        run: &str,
        op: &str,
        sid: &str,
        tid: &str,
        snapshot: &str,
        reference: &str,
        secret_ref: &str,
        now: i64,
    ) -> Result<ToolResult, String> {
        let secret_ref = SecretRef::parse(secret_ref.to_owned()).map_err(|e| e.to_string())?;
        let metadata = self
            .vault
            .metadata(&secret_ref)
            .map_err(|e| e.to_string())?;
        if metadata.kind != SecretKind::WebsitePassword {
            return Err("website password required".into());
        }
        let url = metadata.url.as_deref().ok_or("website URL required")?;
        let url = excalibur_browser::credential_origin(url).map_err(|e| e.to_string())?;
        let check = params(
            run,
            sid,
            tid,
            json!({"snapshot_id":snapshot,"ref":reference,"secret_ref":secret_ref.as_str(),"expected_origin":url}),
        );
        self.backend
            .request("act.check_credential_target", check.clone())
            .await?;
        let backend = self.backend.clone();
        let runtime = tokio::runtime::Handle::current();
        self.executor
            .execute(run, op, &secret_ref, now, move |bytes| {
                let Ok(text) = std::str::from_utf8(bytes) else {
                    return ToolResult::error("credential must be UTF-8");
                };
                let mut fill = check;
                fill["value"] = json!(text);
                // Clone for the transport so the adapter copy can be zeroized
                // explicitly after the request (matches actions/rpc practice).
                // The transport also erases its own params copy.
                let outcome = runtime.block_on(backend.request("act.fill_secret", fill.clone()));
                {
                    use zeroize::Zeroize;
                    if let Value::String(s) = &mut fill["value"] {
                        s.zeroize();
                    }
                    if let Some(map) = fill.as_object_mut() {
                        map.remove("value");
                    }
                }
                match outcome {
                    Ok(_) => ToolResult::ok(json!({"filled":true})),
                    Err(_) => ToolResult::error(
                        "credential target changed or receipt unavailable; reconcile",
                    ),
                }
            })
            .await
            .map_err(|e| e.to_string())
    }
    pub async fn stage_path(
        &self,
        run: &str,
        op: &str,
        sid: &str,
        tid: &str,
        path: &Path,
        protected: &[PathBuf],
        now: i64,
    ) -> Result<Value, String> {
        let config = self
            .security
            .run_security(run)
            .await
            .map_err(|e| e.to_string())?;
        let root = config.workspace.canonicalize().map_err(|e| e.to_string())?;
        let path = if path.is_absolute() {
            path.to_owned()
        } else {
            root.join(path)
        };
        let relative = path
            .strip_prefix(&root)
            .map_err(|_| "upload escapes workspace")?;
        if relative
            .components()
            .any(|c| !matches!(c, std::path::Component::Normal(_)))
        {
            return Err("invalid upload path".into());
        }
        let directory = cap_std::fs::Dir::open_ambient_dir(&root, cap_std::ambient_authority())
            .map_err(|e| e.to_string())?;
        let mut file = directory.open(relative).map_err(|e| e.to_string())?;
        #[cfg(unix)]
        let actual = {
            use std::os::fd::AsRawFd;
            std::fs::read_link(format!("/proc/self/fd/{}", file.as_raw_fd()))
                .map_err(|e| e.to_string())?
        };
        #[cfg(not(unix))]
        let actual = path.canonicalize().map_err(|e| e.to_string())?;
        if !actual.starts_with(&root)
            || protected.iter().any(|p| actual.starts_with(p))
            || excalibur_sandbox::file_authority::credential_path(&actual)
        {
            return Err("protected upload path".into());
        }
        match self
            .security
            .authorize(run, &Action::Read { path: actual }, now)
            .await
            .map_err(|e| e.to_string())?
        {
            Decision::Allow => {}
            _ => return Err("upload source read not authorized".into()),
        }
        let mut authority =
            excalibur_sandbox::file_authority::ReadAuthority::excluding_credential_names();
        authority.check(&directory, &root, protected, &file)?;
        if file.metadata().map_err(|e| e.to_string())?.len() > 200 * 1024 * 1024 {
            return Err("upload limit".into());
        }
        let mut bytes = vec![];
        std::io::Read::by_ref(&mut file)
            .take(200 * 1024 * 1024 + 1)
            .read_to_end(&mut bytes)
            .map_err(|e| e.to_string())?;
        if bytes.len() > 200 * 1024 * 1024 {
            return Err("upload limit".into());
        }
        self.stage_bytes(
            run,
            op,
            sid,
            tid,
            path.file_name().and_then(|v| v.to_str()).unwrap_or("file"),
            &bytes,
        )
        .await
    }
    pub async fn stage_blob(
        &self,
        run: &str,
        op: &str,
        sid: &str,
        tid: &str,
        hash: &str,
        name: &str,
        now: i64,
    ) -> Result<Value, String> {
        match self
            .security
            .authorize(
                run,
                &Action::Data {
                    scope: format!("blob:{hash}"),
                },
                now,
            )
            .await
            .map_err(|e| e.to_string())?
        {
            Decision::Allow => {}
            _ => return Err("attachment read denied".into()),
        }
        let bytes = self
            .security
            .database()
            .get_blob(
                hash.parse().map_err(|_| "invalid blob hash")?,
                200 * 1024 * 1024,
            )
            .await
            .map_err(|e| e.to_string())?;
        self.stage_bytes(run, op, sid, tid, name, &bytes).await
    }
    async fn stage_bytes(
        &self,
        run: &str,
        op: &str,
        sid: &str,
        tid: &str,
        name: &str,
        bytes: &[u8],
    ) -> Result<Value, String> {
        use base64::Engine;
        if self.security.redactor().redact_bytes(bytes) != bytes {
            return Err("secret content cannot be staged as a normal upload".into());
        }
        let mut id = None;
        let mut result = Value::Null;
        let chunks = bytes.len().max(1).div_ceil(2 * 1024 * 1024);
        for n in 0..chunks {
            let offset = n * 2 * 1024 * 1024;
            let end = bytes.len().min(offset + 2 * 1024 * 1024);
            let p = params(
                run,
                sid,
                tid,
                json!({"op_key":format!("{op}:chunk:{n}"),"file_name":name,"staged_id":id,"offset":offset,"final":n+1==chunks,"data":base64::engine::general_purpose::STANDARD.encode(&bytes[offset..end])}),
            );
            result = self.backend.request("act.stage_upload", p).await?;
            id = result["staged_id"].as_str().map(str::to_owned);
        }
        Ok(result)
    }
    pub async fn download(
        &self,
        run: &str,
        op: &str,
        sid: &str,
        tid: &str,
        trigger: Value,
        now: i64,
    ) -> Result<ToolResult, String> {
        let prepared = PreparedTool {
            call: ToolCall {
                id: op.into(),
                name: "__host_browser_download".into(),
                arguments: json!({"session_id":sid,"tab_id":tid,"trigger":trigger}).to_string(),
            },
            effect: Effect::NonIdempotent,
            resources: vec![],
            exclusive: true,
            data: json!({}),
        };
        let backend = self.backend.clone();
        let p = params(
            run,
            sid,
            tid,
            json!({"op_key":op,"trigger":trigger,"deadline_ms":120000}),
        );
        let security = self.security.clone();
        let scanner = self.scanner.clone();
        let run_owned = run.to_owned();
        let op_owned = op.to_owned();
        let sid = sid.to_owned();
        let tid = tid.to_owned();
        // Downloads always use the persistent (highest) risk row: the file content is untrusted.
        let authority = Action::Browser {
            class: "download".into(),
            origin: String::new(),
            target: prepared.call.arguments.clone(),
            persistent: true,
        };
        self.security
            .execute_host_effect(run, &prepared, &[authority], now, move || async move {
                let result = async {
                    let (file, bytes) = quarantined(&*backend, &run_owned, &sid, &tid, p).await?;
                    security
                        .admit_download(
                            &run_owned,
                            &op_owned,
                            bytes,
                            file["file_name"].as_str().unwrap_or("file"),
                            scanner.as_deref(),
                            now,
                        )
                        .await
                        .map_err(|e| e.to_string())
                }
                .await;
                match result {
                    Ok(v) => ToolResult::ok(v),
                    Err(e) => ToolResult::error(e),
                }
            })
            .await
            .map_err(|e| e.to_string())
    }
}
/// Completes the webd download and streams its quarantined blob back in bounded windows.
async fn quarantined(
    backend: &dyn BrowserBackend,
    run: &str,
    sid: &str,
    tid: &str,
    p: Value,
) -> Result<(Value, Vec<u8>), String> {
    use base64::Engine;
    let file = backend.request("download", p).await?;
    let hash = file["blob"]["sha256"]
        .as_str()
        .ok_or("missing quarantine blob")?;
    let mut bytes = vec![];
    loop {
        let window = json!({"sha256":hash,"offset":bytes.len(),"length":1048576});
        let chunk = backend
            .request("blob.read", params(run, sid, tid, window))
            .await?;
        let data = base64::engine::general_purpose::STANDARD
            .decode(chunk["data"].as_str().ok_or("invalid blob window")?)
            .map_err(|e| e.to_string())?;
        if data.is_empty() {
            return Err("truncated quarantine blob".into());
        }
        bytes.extend(data);
        if bytes.len() as u64 == chunk["total_bytes"].as_u64().ok_or("missing blob length")? {
            return Ok((file, bytes));
        }
        if bytes.len() > 200 * 1024 * 1024 {
            return Err("download limit".into());
        }
    }
}
fn params(run: &str, sid: &str, tid: &str, mut p: Value) -> Value {
    p["run_id"] = json!(run);
    p["session_id"] = json!(sid);
    p["tab_id"] = json!(tid);
    if p["deadline_ms"].is_null() {
        p["deadline_ms"] = json!(30000)
    }
    p
}
