//! Bounded JSON-RPC over an authenticated Unix socket, with durable operation keys.
use crate::sessions::{Fault, Manager, Open, Result, now};
use fs2::FileExt;
use rusqlite::params;
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    fs,
    os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::{UnixListener, UnixStream},
    sync::{Mutex, watch},
};
pub const MAX_MESSAGE: usize = 4 * 1024 * 1024;
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub jsonrpc: String,
    pub id: Value,
    pub method: String,
    pub params: Value,
}
struct Operation {
    result: watch::Receiver<Option<Result<Value>>>,
}
pub struct Protocol {
    manager: Arc<Manager>,
    live: Mutex<HashMap<String, Operation>>,
}
impl Protocol {
    pub fn new(manager: Arc<Manager>) -> Arc<Self> {
        Arc::new(Self {
            manager,
            live: Default::default(),
        })
    }
    pub async fn drain(&self) {
        loop {
            let receivers = self
                .live
                .lock()
                .await
                .values()
                .map(|op| op.result.clone())
                .collect::<Vec<_>>();
            if receivers.is_empty() {
                break;
            }
            for mut rx in receivers {
                while rx.borrow().is_none() {
                    if rx.changed().await.is_err() {
                        break;
                    }
                }
            }
            tokio::task::yield_now().await;
        }
    }
    pub async fn request(self: &Arc<Self>, r: Request) -> Value {
        let id = r.id.clone();
        let result =
            if r.jsonrpc != "2.0" || !(r.id.is_string() || r.id.is_number() || r.id.is_null()) {
                Err(Fault::new(
                    "invalid_request",
                    "JSON-RPC 2.0 request expected",
                ))
            } else {
                self.execute(&r).await
            };
        match result {
            Ok(v) => json!({"jsonrpc":"2.0","id":id,"result":v}),
            Err(e) => {
                json!({"jsonrpc":"2.0","id":id,"error":{"code":-32000,"message":e.kind,"data":{"kind":e.kind,"detail":e.detail}}})
            }
        }
    }
    async fn execute(self: &Arc<Self>, r: &Request) -> Result<Value> {
        let p = r
            .params
            .as_object()
            .ok_or_else(|| Fault::new("invalid_params", "object required"))?;
        let run = p
            .get("run_id")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty() && s.len() <= 256)
            .ok_or_else(|| Fault::new("invalid_params", "run_id required"))?;
        let deadline = p
            .get("deadline_ms")
            .and_then(Value::as_u64)
            .filter(|d| *d > 0 && *d <= 300_000)
            .ok_or_else(|| Fault::new("invalid_params", "deadline_ms must be 1..300000"))?;
        if r.method == "op.get" {
            let key = string(&r.params, "op_key")?.to_owned();
            let running = self.live.lock().await.contains_key(&key);
            let run = run.to_owned();
            return self
                .manager
                .db
                .call(move |c| {
                    let (owner, state, result) = c
                        .query_row(
                            "SELECT run_id,state,result FROM web_ops WHERE op_key=?1",
                            [key],
                            |row| {
                                Ok((
                                    row.get::<_, String>(0)?,
                                    row.get::<_, String>(1)?,
                                    row.get::<_, Option<String>>(2)?,
                                ))
                            },
                        )
                        .map_err(|_| Fault::new("unknown_operation", "op_key"))?;
                    if owner != run {
                        return Err(Fault::new("denied", "operation_owner"));
                    }
                    let payload = result
                        .map(|s| serde_json::from_str::<Value>(&s))
                        .transpose()
                        .map_err(|e| Fault::new("storage_error", e))?
                        .unwrap_or(Value::Null);
                    let state = if state == "dispatched" {
                        if running { "working" } else { "uncertain" }
                    } else {
                        &state
                    };
                    Ok(json!({"state":state,"result":payload["ok"],"error":payload["error"]}))
                })
                .await;
        }
        if matches!(
            r.method.as_str(),
            "session.list" | "status.get" | "events.list"
        ) {
            let v = tokio::time::timeout(
                Duration::from_millis(deadline),
                self.dispatch(&r.method, &r.params, run),
            )
            .await
            .map_err(|_| Fault::new("deadline_exceeded", "read deadline"))??;
            return Ok(json!({"state":"completed","result":v}));
        }
        if !matches!(
            r.method.as_str(),
            "session.open"
                | "tab.list"
                | "session.close"
                | "session.suspend"
                | "tab.new"
                | "tab.switch"
                | "tab.close"
                | "site.set_login_state"
        ) {
            return Err(Fault::new("method_not_found", "method is not implemented"));
        }
        let key = string(&r.params, "op_key")?.to_owned();
        if key.len() > 512 {
            return Err(Fault::new("invalid_params", "op_key too long"));
        }
        let mut canonical = r.params.clone();
        canonical.as_object_mut().unwrap().remove("deadline_ms");
        let hash = format!(
            "{:x}",
            Sha256::digest(json!({"method":r.method,"params":canonical}).to_string())
        );
        let mut live = self.live.lock().await;
        let key_db = key.clone();
        let hash_db = hash.clone();
        let run_db = run.to_owned();
        let method = r.method.clone();
        let previous = self
            .manager
            .db
            .call(move |c| {
                let tx = c.transaction()?;
                let row = tx.query_row(
                    "SELECT run_id,request_hash,state,result FROM web_ops WHERE op_key=?1",
                    [&key_db],
                    |r| {
                        Ok((
                            r.get::<_, String>(0)?,
                            r.get::<_, String>(1)?,
                            r.get::<_, String>(2)?,
                            r.get::<_, Option<String>>(3)?,
                        ))
                    },
                );
                let previous = match row {
                    Ok((owner, h, state, result)) => {
                        if owner != run_db || h != hash_db {
                            return Err(Fault::new(
                                "idempotency_conflict",
                                "key reused with different request",
                            ));
                        }
                        Some((state, result))
                    }
                    Err(rusqlite::Error::QueryReturnedNoRows) => {
                        tx.execute(
                            "INSERT INTO web_ops VALUES(?1,?2,?3,?4,'received',NULL,?5,?5)",
                            params![key_db, run_db, method, hash_db, now()],
                        )?;
                        None
                    }
                    Err(e) => return Err(e.into()),
                };
                tx.commit()?;
                Ok(previous)
            })
            .await?;
        if let Some((state, result)) = &previous {
            if matches!(state.as_str(), "completed" | "failed") {
                return saved(state, result.as_deref());
            }
            if state == "dispatched" && !live.contains_key(&key) {
                return Ok(
                    json!({"state":"uncertain","op_key":key,"reason":"webd_restarted_after_dispatch"}),
                );
            }
        }
        if !live.contains_key(&key) && live.len() >= 128 {
            return Err(Fault::new("capacity_exceeded", "128 in-flight operations"));
        }
        let mut rx = if let Some(op) = live.get(&key) {
            op.result.clone()
        } else {
            let (tx, rx) = watch::channel(None);
            let manager = self.clone();
            let method = r.method.clone();
            let params = r.params.clone();
            let run = run.to_owned();
            let task_key = key.clone();
            live.insert(key.clone(), Operation { result: rx.clone() });
            tokio::spawn(async move {
                let k = task_key.clone();
                let started = manager
                    .manager
                    .db
                    .call(move |c| {
                        c.execute(
                            "UPDATE web_ops SET state='dispatched',updated_at=?1 WHERE op_key=?2",
                            params![now(), k],
                        )?;
                        Ok(())
                    })
                    .await;
                let result = match started {
                    Ok(()) => manager.dispatch(&method, &params, &run).await,
                    Err(e) => Err(e),
                };
                let k = task_key.clone();
                let result_json = match &result {
                    Ok(v) => json!({"ok":v}),
                    Err(e) => json!({"error":e}),
                };
                let state = if result.is_ok() {
                    "completed"
                } else {
                    "failed"
                };
                let persisted = manager
                    .manager
                    .db
                    .call(move |c| {
                        c.execute(
                            "UPDATE web_ops SET state=?1,result=?2,updated_at=?3 WHERE op_key=?4",
                            params![state, result_json.to_string(), now(), k],
                        )?;
                        Ok(())
                    })
                    .await;
                let result = if persisted.is_err() {
                    Err(Fault::new(
                        "uncertain",
                        "result could not be durably recorded",
                    ))
                } else {
                    result
                };
                tx.send_replace(Some(result));
                manager.live.lock().await.remove(&task_key);
            });
            rx
        };
        drop(live);
        let wait = async {
            loop {
                if let Some(result) = rx.borrow().clone() {
                    return result.map(|v| json!({"state":"completed","result":v}));
                }
                rx.changed()
                    .await
                    .map_err(|_| Fault::new("uncertain", "operation worker stopped"))?;
            }
        };
        match tokio::time::timeout(Duration::from_millis(deadline.min(2000)), wait).await {
            Ok(v) => v,
            Err(_) => Ok(json!({"state":"working","op_key":key})),
        }
    }
    async fn dispatch(&self, method: &str, p: &Value, run: &str) -> Result<Value> {
        let sid = || string(p, "session_id");
        let tid = || string(p, "tab_id");
        match method {
            "session.open" => {
                let mut args = p.clone();
                let o = args.as_object_mut().unwrap();
                for k in ["op_key", "run_id", "deadline_ms"] {
                    o.remove(k);
                }
                let request: Open =
                    serde_json::from_value(args).map_err(|e| Fault::new("invalid_params", e))?;
                self.manager.open_session(run, request).await
            }
            "session.close" => self.manager.end_session(run, sid()?, false).await,
            "session.suspend" => self.manager.end_session(run, sid()?, true).await,
            "session.list" => Ok(self.manager.list(run).await),
            "tab.list" => self.manager.tabs(run, sid()?).await,
            "tab.new" => {
                self.manager
                    .new_tab(
                        run,
                        sid()?,
                        p["url"].as_str().unwrap_or("about:blank"),
                        p["label"].as_str().map(str::to_owned),
                    )
                    .await
            }
            "tab.switch" => self.manager.switch(run, sid()?, tid()?).await,
            "tab.close" => self.manager.close_tab(run, sid()?, tid()?).await,
            "site.set_login_state" => {
                self.manager
                    .site_state(run, sid()?, string(p, "login_state")?)
                    .await
            }
            "status.get" => Ok(self.manager.status().await),
            "events.list" => Ok(json!(
                self.manager
                    .db
                    .events(p["after"].as_u64().unwrap_or(0))
                    .await?
            )),
            _ => Err(Fault::new("method_not_found", method)),
        }
    }
}
fn saved(state: &str, result: Option<&str>) -> Result<Value> {
    let value: Value =
        serde_json::from_str(result.ok_or_else(|| Fault::new("storage_error", "missing result"))?)
            .map_err(|e| Fault::new("storage_error", e))?;
    if state == "failed" {
        return Err(serde_json::from_value(value["error"].clone())
            .map_err(|e| Fault::new("storage_error", e))?);
    }
    Ok(json!({"state":"completed","result":value["ok"]}))
}
fn string<'a>(p: &'a Value, k: &str) -> Result<&'a str> {
    p[k].as_str()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| Fault::new("invalid_params", format!("{k} required")))
}
/// Owns a lock beside the socket so only a stopped service's socket can be removed.
pub struct Server {
    listener: UnixListener,
    protocol: Arc<Protocol>,
    host_uid: u32,
    path: PathBuf,
    _lock: fs::File,
}
impl Server {
    pub fn bind(
        path: &Path,
        host_uid: u32,
        shared_gid: u32,
        protocol: Arc<Protocol>,
    ) -> Result<Self> {
        let parent = path
            .parent()
            .ok_or_else(|| Fault::new("invalid_config", "socket path"))?;
        let m = fs::symlink_metadata(parent)?;
        if !m.is_dir()
            || m.uid() != unsafe { libc::geteuid() }
            || m.mode() & 0o007 != 0
            || m.mode() & 0o020 != 0
        {
            return Err(Fault::new("denied", "unsafe runtime directory"));
        }
        let lock = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(path.with_file_name(format!(
                "{}.lock",
                path.file_name().unwrap().to_string_lossy()
            )))?;
        lock.try_lock_exclusive()
            .map_err(|_| Fault::new("already_running", "socket is owned by live server"))?;
        if path.exists() {
            use std::os::unix::fs::FileTypeExt;
            let m = fs::symlink_metadata(path)?;
            if !m.file_type().is_socket() || m.uid() != unsafe { libc::geteuid() } {
                return Err(Fault::new("denied", "refusing to replace foreign socket"));
            }
            fs::remove_file(path)?;
        }
        let listener = UnixListener::bind(path)?;
        std::os::unix::fs::chown(path, None, Some(shared_gid))?;
        fs::set_permissions(path, fs::Permissions::from_mode(0o660))?;
        Ok(Self {
            listener,
            protocol,
            host_uid,
            path: path.into(),
            _lock: lock,
        })
    }
    pub async fn run(&self, mut stop: watch::Receiver<bool>) -> Result<()> {
        let permits = Arc::new(tokio::sync::Semaphore::new(64));
        let mut tasks = tokio::task::JoinSet::new();
        loop {
            tokio::select! {biased;_=stop.changed()=>break,Some(_)=tasks.join_next(),if !tasks.is_empty()=>{},connection=self.listener.accept()=>{let(stream,_)=connection?;let peer=stream.peer_cred()?;if peer.uid()!=0&&peer.uid()!=self.host_uid{drop(stream);continue}let Ok(permit)=permits.clone().try_acquire_owned() else{drop(stream);continue};let protocol=self.protocol.clone();let stop=stop.clone();tasks.spawn(async move{let _permit=permit;let _=connection_loop(stream,protocol,stop).await;});}}
        }
        tasks.abort_all();
        while tasks.join_next().await.is_some() {}
        Ok(())
    }
}
impl Drop for Server {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}
async fn write(w: &mut tokio::net::unix::OwnedWriteHalf, v: Value) -> std::io::Result<()> {
    let mut bytes = serde_json::to_vec(&v)?;
    if bytes.len() > MAX_MESSAGE {
        bytes = serde_json::to_vec(
            &json!({"jsonrpc":"2.0","id":v["id"],"error":{"code":-32000,"message":"response_too_large"}}),
        )?;
    }
    bytes.push(b'\n');
    tokio::time::timeout(Duration::from_secs(10), w.write_all(&bytes))
        .await
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::TimedOut, "socket backpressure"))?
}
async fn connection_loop(
    stream: UnixStream,
    protocol: Arc<Protocol>,
    mut stop: watch::Receiver<bool>,
) -> std::io::Result<()> {
    let (read, mut write_half) = stream.into_split();
    let mut read = BufReader::new(read);
    let mut line = Vec::new();
    let mut oversized = false;
    let mut cursor = 0;
    let mut timer = tokio::time::interval(Duration::from_millis(250));
    loop {
        tokio::select! {biased;_=stop.changed()=>return Ok(()),buffer=read.fill_buf()=>{let buffer=buffer?;if buffer.is_empty(){return Ok(())}let end=buffer.iter().position(|b|*b==b'\n');let take=end.map_or(buffer.len(),|n|n+1);if line.len()+take>MAX_MESSAGE{oversized=true;line.clear();}else if !oversized{line.extend_from_slice(&buffer[..take]);}read.consume(take);if end.is_none(){continue}let response=if oversized{oversized=false;json!({"jsonrpc":"2.0","id":null,"error":{"code":-32000,"message":"message_too_large"}})}else {match serde_json::from_slice::<Request>(&line){Ok(request)=>protocol.request(request).await,Err(_)=>json!({"jsonrpc":"2.0","id":null,"error":{"code":-32700,"message":"invalid_json_or_request"}})}};line.clear();write(&mut write_half,response).await?;},_=timer.tick()=>{if let Ok(events)=protocol.manager.db.events(cursor).await{for event in events{let next=event["cursor"].as_u64().unwrap_or(cursor);write(&mut write_half,json!({"jsonrpc":"2.0","method":"event","params":event})).await?;cursor=next;}}}}
    }
}
