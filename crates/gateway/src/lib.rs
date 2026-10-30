//! Authenticated client transport. Agent decisions and control remain in shared services.
mod backend;
mod memory;
use axum::{
    Router,
    body::{Body, Bytes, to_bytes},
    extract::{
        DefaultBodyLimit, Path, State, WebSocketUpgrade,
        ws::{Message, WebSocket},
    },
    http::{HeaderMap, Method, Request, StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
};
pub use backend::SharedBackend;
use excalibur_store::{BlobHash, BlobOwner, Database, Priority, ReadLane, Store};
use excalibur_types::runtime::BoxFuture;
use futures_util::{SinkExt, StreamExt};
pub use memory::MemoryAccess;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, Mutex},
};
use subtle::ConstantTimeEq;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RpcError {
    pub code: i32,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}
impl std::fmt::Display for RpcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}
impl std::error::Error for RpcError {}
impl RpcError {
    pub fn invalid(message: impl Into<String>) -> Self {
        Self {
            code: -32602,
            message: message.into(),
            data: None,
        }
    }
    pub fn not_found() -> Self {
        Self {
            code: -32601,
            message: "method not available".into(),
            data: None,
        }
    }
    pub fn internal() -> Self {
        Self {
            code: -32603,
            message: "backend operation failed".into(),
            data: None,
        }
    }
}
/// One instance belongs to exactly one physically isolated principal database.
pub trait RpcBackend: Send + Sync {
    fn events(&self, after: u64, limit: u32) -> BoxFuture<'_, Result<Vec<Value>, RpcError>>;
    fn dispatch<'a>(
        &'a self,
        method: &'a str,
        params: Value,
    ) -> BoxFuture<'a, Result<Value, RpcError>>;
}
/// Host-owned credentials are only digested and never serialized or formatted.
#[derive(Clone)]
pub struct AccessPolicy {
    token_digest: [u8; 32],
    allowed_origins: BTreeSet<String>,
    max_blob_bytes: usize,
}
impl AccessPolicy {
    pub fn new(
        token: &str,
        allowed_origins: BTreeSet<String>,
        max_blob_bytes: usize,
    ) -> Result<Self, RpcError> {
        if token.len() < 32
            || token.len() > 4096
            || token.bytes().any(|b| b.is_ascii_whitespace())
            || max_blob_bytes == 0
            || max_blob_bytes > 64 * 1024 * 1024
        {
            return Err(RpcError::invalid(
                "a strong header token and bounded blob size are required",
            ));
        }
        if allowed_origins.iter().any(|origin| {
            !origin.starts_with("https://")
                && !origin.starts_with("http://localhost:")
                && !origin.starts_with("http://127.0.0.1:")
        }) {
            return Err(RpcError::invalid("explicit secure origins required"));
        }
        Ok(Self {
            token_digest: Sha256::digest(token.as_bytes()).into(),
            allowed_origins,
            max_blob_bytes,
        })
    }
    fn authorizes(&self, headers: &HeaderMap) -> bool {
        if let Some(origin) = headers.get(header::ORIGIN) {
            let Ok(origin) = origin.to_str() else {
                return false;
            };
            if !self.allowed_origins.contains(origin) {
                return false;
            }
        }
        let Some(token) = headers
            .get(header::AUTHORIZATION)
            .and_then(|h| h.to_str().ok())
            .and_then(|h| h.strip_prefix("Bearer "))
        else {
            return false;
        };
        let digest: [u8; 32] = Sha256::digest(token.as_bytes()).into();
        bool::from(digest.ct_eq(&self.token_digest))
    }
}
#[derive(Clone)]
pub struct Gateway {
    backend: Arc<dyn RpcBackend>,
    db: Database,
    policy: AccessPolicy,
    connections: Arc<Mutex<BTreeMap<String, usize>>>,
    closing: tokio::sync::watch::Sender<bool>,
}
impl Gateway {
    pub fn new(backend: Arc<dyn RpcBackend>, db: Database, policy: AccessPolicy) -> Self {
        Self {
            backend,
            db,
            policy,
            connections: Arc::default(),
            closing: tokio::sync::watch::channel(false).0,
        }
    }
    /// Stop accepting new work and close every upgraded client before host teardown.
    pub fn close_connections(&self) {
        self.closing.send_replace(true);
    }
    pub fn router(self) -> Router {
        let limit = self.policy.max_blob_bytes;
        Router::new()
            .route("/rpc", get(upgrade).post(http_rpc))
            .route("/blobs", post(upload_blob))
            .route("/blobs/{hash}", get(download_blob))
            .layer(DefaultBodyLimit::max(limit))
            .layer(middleware::from_fn_with_state(self.clone(), authorize))
            .with_state(self)
    }
}
async fn authorize(
    State(state): State<Gateway>,
    mut request: Request<Body>,
    next: Next,
) -> Response {
    // Authentication parameters are forbidden wholesale, including encoded spellings.
    if request.uri().query().is_some() || !state.policy.authorizes(request.headers()) {
        let reason = if request.uri().query().is_some() {
            "url_parameters"
        } else if request.headers().get(header::ORIGIN).is_some() {
            "authentication_or_origin"
        } else {
            "authentication"
        };
        let _=state.db.write(Priority::Interactive,move|conn|{
            conn.execute_batch("CREATE TABLE IF NOT EXISTS gateway_access_denials(reason TEXT NOT NULL,time_bucket INTEGER NOT NULL,count INTEGER NOT NULL,PRIMARY KEY(reason,time_bucket))")?;
            conn.execute("INSERT INTO gateway_access_denials VALUES(?1,?2,1) ON CONFLICT(reason,time_bucket) DO UPDATE SET count=count+1",(reason,excalibur_store::unix_seconds()/3600))?;
            Ok(())
        }).await;
        return (
            StatusCode::UNAUTHORIZED,
            "header authentication and an allowed Origin are required",
        )
            .into_response();
    }
    let mut closing = state.closing.subscribe();
    if *closing.borrow() {
        return closing_response();
    }
    if request.method() == Method::POST {
        let (parts, body) = request.into_parts();
        // Buffer only the transport body here. Once an admitted backend action
        // starts, its own cancellation/durability contract remains authoritative.
        let bytes = tokio::select! {
            biased;
            _=closing.changed()=>return closing_response(),
            result=to_bytes(body,state.policy.max_blob_bytes)=>match result{
                Ok(bytes)=>bytes,
                Err(_)=>return (StatusCode::PAYLOAD_TOO_LARGE,[(header::CONNECTION,"close")]).into_response(),
            }
        };
        if *closing.borrow() {
            return closing_response();
        }
        request = Request::from_parts(parts, Body::from(bytes));
    }
    next.run(request).await
}
fn closing_response() -> Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        [(header::CONNECTION, "close")],
    )
        .into_response()
}
async fn upgrade(State(state): State<Gateway>, ws: WebSocketUpgrade) -> Response {
    ws.max_message_size(1024 * 1024)
        .max_frame_size(1024 * 1024)
        .on_upgrade(move |socket| connection(socket, state))
        .into_response()
}
async fn http_rpc(State(state): State<Gateway>, body: Bytes) -> Response {
    let response = handle_message(&state.backend, &body).await;
    match response {
        Some(value) => axum::Json(value).into_response(),
        None => StatusCode::NO_CONTENT.into_response(),
    }
}
async fn connection(socket: WebSocket, state: Gateway) {
    let mut closing = state.closing.subscribe();
    if *closing.borrow() {
        return;
    }
    let (mut sender, mut receiver) = socket.split();
    let mut subscription: Option<(u64, Option<String>)> = None;
    let mut active_conversation: Option<String> = None;
    let mut poll = tokio::time::interval(std::time::Duration::from_millis(100));
    poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            _ = closing.changed()=>break,
            message = receiver.next() => {
                let Some(Ok(message)) = message else { break };
                match message {
                    Message::Text(text) => {
                        if let Some(conversation)=&active_conversation {let _=state.backend.dispatch("conversation.activity",json!({"conversation":conversation,"connected":true,"channel":"websocket"})).await;}

                        let input = serde_json::from_str::<Value>(&text).ok();
                        if let Some(response) = handle_message(&state.backend, text.as_bytes()).await {
                            if response.get("error").is_none() && input.as_ref().and_then(|v|v.get("method")).and_then(Value::as_str)==Some("conversation.subscribe") {
                                let params=&input.as_ref().unwrap()["params"];
                                let next=params.get("conversation").and_then(Value::as_str).map(str::to_owned);
                                if next!=active_conversation {
                                    if let Some(old)=active_conversation.take(){connection_activity(&state,&old,false).await;}
                                    if let Some(new)=&next{connection_activity(&state,new,true).await;}
                                    active_conversation=next;
                                }
                                subscription=Some((params.get("cursor").and_then(Value::as_u64).unwrap_or(0),params.get("conversation").and_then(Value::as_str).map(str::to_owned)));
                            }
                            if sender.send(Message::Text(response.to_string().into())).await.is_err() { break; }
                        }
                    }
                    Message::Ping(bytes) => { if sender.send(Message::Pong(bytes)).await.is_err() { break; } }
                    Message::Close(_) => break,
                    Message::Binary(_) | Message::Pong(_) => {}
                }
            }
            _ = poll.tick(), if subscription.is_some() => {
                let (cursor,conversation)=subscription.as_mut().unwrap();
                match state.backend.events(*cursor,100).await {
                    Ok(events) => {
                        for event in events {
                            let Some(sequence)=event.get("cursor").and_then(Value::as_u64) else { break; };
                            if sequence<=*cursor { continue; }
                            *cursor=sequence;
                            if conversation.as_ref().is_some_and(|c| event.get("conversation").and_then(Value::as_str)!=Some(c)) { continue; }
                            let notification=json!({"jsonrpc":"2.0","method":"event","params":event});
                            if sender.send(Message::Text(notification.to_string().into())).await.is_err() { break; }
                        }
                    }
                    Err(error) => {
                        if sender.send(Message::Text(json!({"jsonrpc":"2.0","method":"subscription.error","params":error}).to_string().into())).await.is_err() { break; }
                    }
                }
            }
        }
    }
    let _ = tokio::time::timeout(
        std::time::Duration::from_secs(1),
        sender.send(Message::Close(None)),
    )
    .await;
    if let Some(conversation) = active_conversation {
        connection_activity(&state, &conversation, false).await;
    }
}
async fn connection_activity(state: &Gateway, conversation: &str, connected: bool) {
    let active = {
        let mut counts = state
            .connections
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let count = counts.entry(conversation.to_owned()).or_default();
        if connected {
            *count += 1;
        } else {
            *count = count.saturating_sub(1);
        }
        let active = *count > 0;
        if !active {
            counts.remove(conversation);
        }
        active
    };
    let _ = state
        .backend
        .dispatch(
            "conversation.activity",
            json!({"conversation":conversation,"channel":"websocket","connected":active}),
        )
        .await;
}

fn error_response(id: Value, error: RpcError) -> Value {
    json!({"jsonrpc":"2.0","id":id,"error":error})
}
async fn handle_message(backend: &Arc<dyn RpcBackend>, bytes: &[u8]) -> Option<Value> {
    let input: Value = match serde_json::from_slice(bytes) {
        Ok(input) => input,
        Err(_) => {
            return Some(error_response(
                Value::Null,
                RpcError {
                    code: -32700,
                    message: "parse error".into(),
                    data: None,
                },
            ));
        }
    };
    if let Value::Array(requests) = input {
        if requests.is_empty() || requests.len() > 64 {
            return Some(error_response(
                Value::Null,
                RpcError {
                    code: -32600,
                    message: "invalid request batch".into(),
                    data: None,
                },
            ));
        }
        let mut replies = Vec::new();
        for request in requests {
            if let Some(reply) = handle_request(backend, request).await {
                replies.push(reply);
            }
        }
        if replies.is_empty() {
            None
        } else {
            Some(Value::Array(replies))
        }
    } else {
        handle_request(backend, input).await
    }
}
async fn handle_request(backend: &Arc<dyn RpcBackend>, request: Value) -> Option<Value> {
    let id = request.get("id").cloned();
    let valid_id = id
        .as_ref()
        .is_none_or(|id| id.is_null() || id.is_string() || id.is_i64() || id.is_u64());
    let valid_params = request
        .get("params")
        .is_none_or(|p| p.is_object() || p.is_array());
    if !request.is_object()
        || request.get("jsonrpc").and_then(Value::as_str) != Some("2.0")
        || !valid_id
        || !valid_params
        || request.get("method").and_then(Value::as_str).is_none()
    {
        return Some(error_response(
            Value::Null,
            RpcError {
                code: -32600,
                message: "invalid request".into(),
                data: None,
            },
        ));
    }
    let method = request["method"].as_str().unwrap_or_default();
    let result = backend
        .dispatch(
            method,
            request.get("params").cloned().unwrap_or_else(|| json!({})),
        )
        .await;
    id.map(|id| match result {
        Ok(value) => json!({"jsonrpc":"2.0","id":id,"result":value}),
        Err(error) => error_response(id, error),
    })
}
async fn upload_blob(State(state): State<Gateway>, body: Bytes) -> Response {
    let owner_id = excalibur_types::MessageId::new().to_string();
    match state
        .db
        .put_blob(
            body.to_vec(),
            BlobOwner {
                kind: "gateway_upload".into(),
                id: owner_id.clone(),
                slot: "content".into(),
            },
        )
        .await
    {
        Ok(hash) => (
            StatusCode::CREATED,
            axum::Json(json!({"hash":hash.as_str(),"upload_id":owner_id,"size":body.len()})),
        )
            .into_response(),
        Err(_) => StatusCode::SERVICE_UNAVAILABLE.into_response(),
    }
}
async fn download_blob(State(state): State<Gateway>, Path(hash): Path<String>) -> Response {
    let Ok(hash) = hash.parse::<BlobHash>() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    // Hash knowledge does not grant access to arbitrary tool/private-secret blobs.
    let lookup = hash.as_str().to_owned();
    let authorized = state.db.read(ReadLane::Interactive, move |c| Ok(c.query_row("SELECT EXISTS(SELECT 1 FROM blob_refs WHERE hash=?1 AND owner_type='gateway_upload')", [lookup], |r| r.get::<_, bool>(0))?)).await;
    if !matches!(authorized, Ok(true)) {
        return StatusCode::NOT_FOUND.into_response();
    }
    match state
        .db
        .read_blob(hash, state.policy.max_blob_bytes as u64)
        .await
    {
        Ok(bytes) => (
            [
                (header::CONTENT_TYPE, "application/octet-stream"),
                (header::CACHE_CONTROL, "no-store"),
                (header::CONTENT_DISPOSITION, "attachment"),
            ],
            bytes,
        )
            .into_response(),
        Err(_) => StatusCode::NOT_FOUND.into_response(),
    }
}
