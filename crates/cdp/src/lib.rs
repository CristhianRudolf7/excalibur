//! Bounded NUL-delimited CDP transport. No TCP endpoint or browser policy here.
mod framing;
pub mod protocol;
use framing::Metadata;
use protocol::Method;
use serde_json::{Value, json};
use std::{
    collections::{HashMap, HashSet},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    sync::{broadcast, oneshot, watch},
};

pub const MAX_MESSAGE: usize = 64 * 1024 * 1024;
#[derive(Clone, Debug, thiserror::Error, PartialEq)]
pub enum Error {
    #[error("invalid CDP typed payload")]
    InvalidPayload,
    #[error("cdp_timeout")]
    Timeout,
    #[error("instance_crashed: {0}")]
    Closed(String),
    #[error("session_crashed: {0}")]
    SessionCrashed(String),
    #[error("CDP error {code}: {message}")]
    Protocol { code: i64, message: String },
}
#[derive(Clone, Debug)]
pub struct Event {
    pub method: String,
    pub params: Value,
    pub session_id: Option<String>,
}
type Pending = HashMap<u64, (Option<String>, oneshot::Sender<Result<Value, Error>>)>;
struct State {
    pending: Mutex<Pending>,
    failed_sessions: Mutex<HashSet<String>>,
    topics: Mutex<HashMap<String, broadcast::Sender<Event>>>,
    all: broadcast::Sender<Event>,
    lifecycle: broadcast::Sender<Event>,
    observation: broadcast::Sender<Event>,
    console: broadcast::Sender<Event>,
    attachments: Mutex<HashMap<String, Event>>,
    closed: watch::Sender<Option<Error>>,
    next_id: AtomicU64,
}
impl State {
    fn close(&self, error: Error) {
        self.closed.send_if_modified(|v| {
            if v.is_some() {
                false
            } else {
                *v = Some(error.clone());
                true
            }
        });
        for (_, (_, sender)) in self.pending.lock().unwrap().drain() {
            let _ = sender.send(Err(error.clone()));
        }
    }
    fn emit(&self, event: Event) {
        // Control traffic has its own bounded channel. The active attachment
        // snapshot is updated first so a slow supervisor can reconcile on Lagged.
        if event.method == "Target.attachedToTarget" {
            if let Some(session) = event.params["sessionId"].as_str() {
                self.attachments
                    .lock()
                    .unwrap()
                    .insert(session.into(), event.clone());
            }
        } else if event.method == "Target.detachedFromTarget"
            && let Some(session) = event.params["sessionId"].as_str()
        {
            self.attachments.lock().unwrap().remove(session);
        }
        if matches!(
            event.method.as_str(),
            "Target.attachedToTarget"
                | "Target.detachedFromTarget"
                | "Target.targetCrashed"
                | "Inspector.targetCrashed"
                | "session_crashed"
                | "instance_crashed"
        ) {
            let _ = self.lifecycle.send(event.clone());
        }
        if let Some(topic) = self
            .topics
            .lock()
            .unwrap()
            .get(event.session_id.as_deref().unwrap_or(""))
        {
            let _ = topic.send(event.clone());
        }
        if event.method.starts_with("Page.")
            || matches!(
                event.method.as_str(),
                "Network.requestWillBeSent"
                    | "Network.responseReceived"
                    | "Network.loadingFinished"
                    | "Network.loadingFailed"
                    | "Network.requestServedFromCache"
            )
        {
            let _ = self.observation.send(event.clone());
        }
        if matches!(
            event.method.as_str(),
            "Runtime.consoleAPICalled" | "Runtime.exceptionThrown" | "Log.entryAdded"
        ) {
            let _ = self.console.send(event.clone());
        }
        let _ = self.all.send(event);
    }
    fn fail_session(&self, session: &str) {
        self.failed_sessions.lock().unwrap().insert(session.into());
        self.pending
            .lock()
            .unwrap()
            .retain(|_, (sid, _)| sid.as_deref() != Some(session));
        self.emit(Event {
            method: "session_crashed".into(),
            params: json!({"reason":"message_too_large"}),
            session_id: Some(session.into()),
        });
    }
}
/// Dropping a pending future removes it even during write-lock or pipe backpressure.
struct PendingGuard {
    state: Arc<State>,
    id: u64,
}
impl Drop for PendingGuard {
    fn drop(&mut self) {
        self.state.pending.lock().unwrap().remove(&self.id);
    }
}
#[derive(Clone)]
pub struct Client {
    _lifetime: Arc<Lifetime>,
    state: Arc<State>,
    writer: Arc<tokio::sync::Mutex<Box<dyn AsyncWrite + Unpin + Send>>>,
}
impl Client {
    pub fn connect<R, W>(reader: R, writer: W) -> Self
    where
        R: AsyncRead + Unpin + Send + 'static,
        W: AsyncWrite + Unpin + Send + 'static,
    {
        let (all, _) = broadcast::channel(1024);
        let (lifecycle, _) = broadcast::channel(256);
        let (observation, _) = broadcast::channel(1024);
        let (console, _) = broadcast::channel(2048);
        let (closed, _) = watch::channel(None);
        let state = Arc::new(State {
            pending: Mutex::new(HashMap::new()),
            failed_sessions: Mutex::new(HashSet::new()),
            topics: Mutex::new(HashMap::new()),
            all,
            lifecycle,
            observation,
            console,
            attachments: Mutex::new(HashMap::new()),
            closed,
            next_id: AtomicU64::new(1),
        });
        let client = Self {
            _lifetime: Arc::new(Lifetime(state.clone())),
            state: state.clone(),
            writer: Arc::new(tokio::sync::Mutex::new(Box::new(writer))),
        };
        let weak_writer = Arc::downgrade(&client.writer);
        tokio::spawn(async move {
            let error = read_frames(reader, &state, weak_writer).await;
            state.close(error.clone());
            state.emit(Event {
                method: "instance_crashed".into(),
                params: json!({"reason": error.to_string()}),
                session_id: None,
            });
        });
        client
    }
    /// Readiness events exclude console, response bodies and streaming frame traffic.
    pub fn subscribe_observation(&self) -> broadcast::Receiver<Event> {
        self.state.observation.subscribe()
    }
    /// Bounded console events; loss here never changes readiness or browser lifecycle.
    pub fn subscribe_console(&self) -> broadcast::Receiver<Event> {
        self.state.console.subscribe()
    }
    pub fn subscribe_all(&self) -> broadcast::Receiver<Event> {
        self.state.all.subscribe()
    }
    /// Bounded supervision events only; console/network traffic cannot displace these.
    /// On Lagged reconcile active attachments with `attached_sessions` and inspect `closed`.
    pub fn subscribe_lifecycle(&self) -> broadcast::Receiver<Event> {
        self.state.lifecycle.subscribe()
    }
    /// Current active attachment events, including events missed by a slow receiver.
    pub fn attached_sessions(&self) -> Vec<Event> {
        self.state
            .attachments
            .lock()
            .unwrap()
            .values()
            .cloned()
            .collect()
    }
    pub fn subscribe(&self, session: Option<&str>) -> broadcast::Receiver<Event> {
        self.state
            .topics
            .lock()
            .unwrap()
            .entry(session.unwrap_or("").into())
            .or_insert_with(|| broadcast::channel(256).0)
            .subscribe()
    }
    pub fn closed(&self) -> watch::Receiver<Option<Error>> {
        self.state.closed.subscribe()
    }
    pub fn pending_count(&self) -> usize {
        self.state.pending.lock().unwrap().len()
    }
    pub fn shutdown(&self) {
        self.state.close(Error::Closed("closed by host".into()));
    }
    pub async fn execute<P: serde::Serialize, R: serde::de::DeserializeOwned>(
        &self,
        command: protocol::Command<P>,
        session: Option<&str>,
    ) -> Result<R, Error> {
        let params = serde_json::to_value(command.params).map_err(|_| Error::InvalidPayload)?;
        let result = self.send(command.method, params, session).await?;
        serde_json::from_value(result).map_err(|_| Error::InvalidPayload)
    }
    pub async fn send(
        &self,
        method: Method,
        params: Value,
        session: Option<&str>,
    ) -> Result<Value, Error> {
        self.send_with_timeout(method, params, session, method.timeout())
            .await
    }
    pub async fn send_with_timeout(
        &self,
        method: Method,
        params: Value,
        session: Option<&str>,
        timeout: Duration,
    ) -> Result<Value, Error> {
        let deadline = tokio::time::Instant::now() + timeout;
        if let Some(error) = self.state.closed.borrow().clone() {
            return Err(error);
        }
        if session.is_some_and(|s| self.state.failed_sessions.lock().unwrap().contains(s)) {
            return Err(Error::SessionCrashed(session.unwrap().into()));
        }
        let id = self
            .state
            .next_id
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |v| v.checked_add(1))
            .map_err(|_| Error::Closed("request ID exhausted".into()))?;
        let (sender, receiver) = oneshot::channel();
        let guard = PendingGuard {
            state: self.state.clone(),
            id,
        };
        self.state
            .pending
            .lock()
            .unwrap()
            .insert(id, (session.map(str::to_owned), sender));
        let mut request = json!({"id": id, "method": method.as_str(), "params": params});
        if let Some(session) = session {
            request["sessionId"] = session.into();
        }
        let mut bytes = serde_json::to_vec(&request).map_err(|e| Error::Closed(e.to_string()))?;
        bytes.push(0);
        let mut closed = self.closed();
        let operation = async {
            write_frame(&self.writer, &self.state, &bytes).await?;
            receiver.await.unwrap_or_else(|_| {
                Err(session.map_or_else(
                    || Error::Closed("response channel closed".into()),
                    |s| Error::SessionCrashed(s.into()),
                ))
            })
        };
        let result = tokio::select! {
            biased;
            result = tokio::time::timeout_at(deadline, operation) => result.unwrap_or(Err(Error::Timeout)),
            _ = closed.changed() => Err(closed.borrow().clone().unwrap_or(Error::Closed("closed".into()))),
        };
        drop(guard);
        result
    }
}
async fn read_frames<R: AsyncRead + Unpin>(
    mut reader: R,
    state: &Arc<State>,
    writer: std::sync::Weak<tokio::sync::Mutex<Box<dyn AsyncWrite + Unpin + Send>>>,
) -> Error {
    let mut data = Vec::new();
    let mut metadata = Metadata::default();
    let mut oversized = false;
    let mut buffer = [0; 65536];
    let mut closed = state.closed.subscribe();
    loop {
        let count = tokio::select! { result = reader.read(&mut buffer) => match result { Ok(0) => return Error::Closed("EOF on descriptor 4".into()), Ok(n) => n, Err(e) => return Error::Closed(e.to_string()) }, _ = closed.changed() => return Error::Closed("closed by host".into()) };
        for &byte in &buffer[..count] {
            if byte != 0 {
                metadata.feed(byte);
                if !oversized {
                    if data.len() == MAX_MESSAGE {
                        oversized = true;
                        data.clear();
                    } else {
                        data.push(byte);
                    }
                }
                continue;
            }
            if oversized {
                let session = metadata.session.take().or_else(|| {
                    metadata.id.and_then(|id| {
                        state
                            .pending
                            .lock()
                            .unwrap()
                            .get(&id)
                            .and_then(|(s, _)| s.clone())
                    })
                });
                if let Some(session) = session {
                    state.fail_session(&session);
                    if let Some(writer) = writer.upgrade() {
                        let id = match state.next_id.try_update(
                            Ordering::Relaxed,
                            Ordering::Relaxed,
                            |value| value.checked_add(1),
                        ) {
                            Ok(id) => id,
                            Err(_) => return Error::Closed("request ID exhausted".into()),
                        };
                        // A bounded detach write runs outside the reader so other tabs keep receiving.
                        let state = state.clone();
                        tokio::spawn(async move {
                            let mut bytes = serde_json::to_vec(&json!({"id":id,"method":"Target.detachFromTarget","params":{"sessionId":session}})).unwrap();
                            bytes.push(0);
                            let _ = tokio::time::timeout(
                                Duration::from_secs(1),
                                write_frame(&writer, &state, &bytes),
                            )
                            .await;
                        });
                    }
                } else {
                    return Error::Closed("oversized browser-level message".into());
                }
            } else if !data.is_empty() {
                let value: Value = match serde_json::from_slice(&data) {
                    Ok(v) => v,
                    Err(_) => return Error::Closed("invalid CDP JSON".into()),
                };
                if let Some(id) = value["id"].as_u64() {
                    if let Some((session, sender)) = state.pending.lock().unwrap().remove(&id) {
                        if value.get("error").is_none()
                            && value["sessionId"].as_str() != session.as_deref()
                        {
                            let _ =
                                sender.send(Err(Error::Closed("response session mismatch".into())));
                            return Error::Closed("response session mismatch".into());
                        }
                        let result = if let Some(error) = value.get("error") {
                            Err(Error::Protocol {
                                code: error["code"].as_i64().unwrap_or(-1),
                                message: error["message"]
                                    .as_str()
                                    .unwrap_or("unknown protocol error")
                                    .into(),
                            })
                        } else if let Some(result) = value.get("result") {
                            Ok(result.clone())
                        } else {
                            Err(Error::Closed("response has no result".into()))
                        };
                        let _ = sender.send(result);
                    }
                } else if let Some(method) = value["method"].as_str() {
                    state.emit(Event {
                        method: method.into(),
                        params: value["params"].clone(),
                        session_id: value["sessionId"].as_str().map(str::to_owned),
                    });
                } else {
                    return Error::Closed("invalid CDP envelope".into());
                }
            }
            data.clear();
            metadata = Metadata::default();
            oversized = false;
        }
    }
}

struct WriteGuard {
    state: Arc<State>,
    complete: bool,
}
impl Drop for WriteGuard {
    fn drop(&mut self) {
        if !self.complete {
            self.state
                .close(Error::Closed("interrupted CDP write".into()));
        }
    }
}
async fn write_frame(
    writer: &tokio::sync::Mutex<Box<dyn AsyncWrite + Unpin + Send>>,
    state: &Arc<State>,
    bytes: &[u8],
) -> Result<(), Error> {
    let mut writer = writer.lock().await;
    if let Some(error) = state.closed.borrow().clone() {
        return Err(error);
    }
    let mut guard = WriteGuard {
        state: state.clone(),
        complete: false,
    };
    writer
        .write_all(bytes)
        .await
        .map_err(|e| Error::Closed(e.to_string()))?;
    guard.complete = true;
    Ok(())
}

struct Lifetime(Arc<State>);
impl Drop for Lifetime {
    fn drop(&mut self) {
        self.0.close(Error::Closed("last client dropped".into()));
    }
}
