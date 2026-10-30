use excalibur_gateway::{AccessPolicy, Gateway, RpcBackend, RpcError};
use excalibur_store::{BlobOwner, Database, DatabaseKind, StorageConfig, Store};
use excalibur_types::runtime::BoxFuture;
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use std::{
    collections::BTreeSet,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};
use tokio_tungstenite::{
    connect_async,
    tungstenite::{
        Message,
        client::IntoClientRequest,
        http::{HeaderValue, StatusCode},
    },
};
const TOKEN: &str = "synthetic-development-token-0123456789abcdef";
#[derive(Default)]
struct Fixture {
    events: Mutex<Vec<Value>>,
    calls: AtomicUsize,
}
impl RpcBackend for Fixture {
    fn events(&self, after: u64, limit: u32) -> BoxFuture<'_, Result<Vec<Value>, RpcError>> {
        Box::pin(async move {
            Ok(self
                .events
                .lock()
                .unwrap()
                .iter()
                .filter(|e| e["cursor"].as_u64().unwrap() > after)
                .take(limit as usize)
                .cloned()
                .collect())
        })
    }
    fn dispatch<'a>(
        &'a self,
        method: &'a str,
        params: Value,
    ) -> BoxFuture<'a, Result<Value, RpcError>> {
        Box::pin(async move {
            self.calls.fetch_add(1, Ordering::SeqCst);
            match method {
                "echo" => Ok(params),
                "conversation.subscribe" => Ok(json!({"subscribed":true})),
                _ => Err(RpcError::not_found()),
            }
        })
    }
}
async fn server() -> (
    tempfile::TempDir,
    Database,
    Arc<Fixture>,
    String,
    tokio::task::JoinHandle<()>,
    Gateway,
) {
    let temp = tempfile::tempdir().unwrap();
    let db = Database::open(
        temp.path().join("main.db"),
        DatabaseKind::Main,
        StorageConfig::default(),
    )
    .unwrap();
    let fixture = Arc::new(Fixture::default());
    let policy = AccessPolicy::new(
        TOKEN,
        BTreeSet::from(["https://client.example".to_owned()]),
        1024,
    )
    .unwrap();
    let gateway = Gateway::new(fixture.clone(), db.clone(), policy);
    let router = gateway.clone().router();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let task = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    (
        temp,
        db,
        fixture,
        format!("http://{address}"),
        task,
        gateway,
    )
}
#[tokio::test]
async fn authentication_is_required_locally_and_url_tokens_and_unlisted_origins_are_rejected() {
    let (_temp, db, _fixture, url, task, _gateway) = server().await;
    let client = reqwest::Client::new();
    for request in [
        client.post(format!("{url}/rpc")),
        client
            .post(format!("{url}/rpc?token={TOKEN}"))
            .bearer_auth(TOKEN),
        client
            .post(format!("{url}/rpc"))
            .bearer_auth(TOKEN)
            .header("Origin", "https://evil.example"),
    ] {
        assert_eq!(
            request
                .json(&json!({"jsonrpc":"2.0","id":1,"method":"echo","params":{}}))
                .send()
                .await
                .unwrap()
                .status()
                .as_u16(),
            401
        );
    }
    let response = client
        .post(format!("{url}/rpc"))
        .bearer_auth(TOKEN)
        .header("Origin", "https://client.example")
        .json(&json!({"jsonrpc":"2.0","id":"safe","method":"echo","params":{"v":1}}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status().as_u16(), 200);
    assert_eq!(response.json::<Value>().await.unwrap()["result"]["v"], 1);
    task.abort();
    db.shutdown().await.unwrap();
}
#[tokio::test]
async fn jsonrpc_batches_notifications_and_invalid_requests_follow_the_protocol() {
    let (_temp, db, _fixture, url, task, _gateway) = server().await;
    let client = reqwest::Client::new();
    let body = json!([{"jsonrpc":"2.0","id":1,"method":"echo","params":{"n":1}},{"jsonrpc":"2.0","method":"echo","params":{"n":2}},{"jsonrpc":"2.0","id":3,"method":"absent"}]);
    let result: Value = client
        .post(format!("{url}/rpc"))
        .bearer_auth(TOKEN)
        .json(&body)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(result.as_array().unwrap().len(), 2);
    assert_eq!(result[0]["id"], 1);
    assert_eq!(result[1]["error"]["code"], -32601);
    let response = client
        .post(format!("{url}/rpc"))
        .bearer_auth(TOKEN)
        .json(&json!({"jsonrpc":"2.0","method":"echo","params":{}}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status().as_u16(), 204);
    let invalid: Value = client
        .post(format!("{url}/rpc"))
        .bearer_auth(TOKEN)
        .body("{")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(invalid["error"]["code"], -32700);
    task.abort();
    db.shutdown().await.unwrap();
}
#[tokio::test]
async fn blobs_are_bounded_verified_and_private_tool_hashes_are_not_downloadable() {
    let (_temp, db, _fixture, url, task, _gateway) = server().await;
    let client = reqwest::Client::new();
    let uploaded: Value = client
        .post(format!("{url}/blobs"))
        .bearer_auth(TOKEN)
        .body("uploaded bytes")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let bytes = client
        .get(format!(
            "{url}/blobs/{}",
            uploaded["hash"].as_str().unwrap()
        ))
        .bearer_auth(TOKEN)
        .send()
        .await
        .unwrap()
        .bytes()
        .await
        .unwrap();
    assert_eq!(&bytes[..], b"uploaded bytes");
    let hash = db
        .put_blob(
            b"private-tool-output".to_vec(),
            BlobOwner {
                kind: "tool_result".into(),
                id: "private".into(),
                slot: "full".into(),
            },
        )
        .await
        .unwrap();
    assert_eq!(
        client
            .get(format!("{url}/blobs/{}", hash.as_str()))
            .bearer_auth(TOKEN)
            .send()
            .await
            .unwrap()
            .status()
            .as_u16(),
        404
    );
    assert_eq!(
        client
            .post(format!("{url}/blobs"))
            .bearer_auth(TOKEN)
            .body(vec![0; 1025])
            .send()
            .await
            .unwrap()
            .status()
            .as_u16(),
        413
    );
    task.abort();
    db.shutdown().await.unwrap();
}
#[tokio::test]
async fn websocket_replays_from_cursor_without_gaps_or_duplicates_after_reconnection() {
    let (_temp, db, fixture, url, task, _gateway) = server().await;
    fixture.events.lock().unwrap().extend((1..=3).map(|cursor|json!({"cursor":cursor,"conversation":"c","type":"message","text":format!("m{cursor}")})));
    let ws_url = url.replacen("http://", "ws://", 1) + "/rpc";
    let unauth = connect_async(ws_url.clone()).await.unwrap_err();
    assert!(
        matches!(unauth,tokio_tungstenite::tungstenite::Error::Http(response) if response.status()==StatusCode::UNAUTHORIZED)
    );
    for (after, expected) in [(0, vec![1, 2, 3]), (2, vec![3])] {
        let mut request = ws_url.clone().into_client_request().unwrap();
        request.headers_mut().insert(
            "Authorization",
            HeaderValue::from_str(&format!("Bearer {TOKEN}")).unwrap(),
        );
        request
            .headers_mut()
            .insert("Origin", HeaderValue::from_static("https://client.example"));
        let (mut socket, _) = connect_async(request).await.unwrap();
        socket.send(Message::Text(json!({"jsonrpc":"2.0","id":1,"method":"conversation.subscribe","params":{"cursor":after,"conversation":"c"}}).to_string().into())).await.unwrap();
        let receipt: Value =
            serde_json::from_str(socket.next().await.unwrap().unwrap().to_text().unwrap()).unwrap();
        assert_eq!(receipt["id"], 1);
        for cursor in expected {
            let next = tokio::time::timeout(std::time::Duration::from_secs(2), socket.next())
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            let event: Value = serde_json::from_str(next.to_text().unwrap()).unwrap();
            assert_eq!(event["params"]["cursor"], cursor);
        }
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(160), socket.next())
                .await
                .is_err()
        );
        socket.close(None).await.unwrap();
    }
    task.abort();
    db.shutdown().await.unwrap();
}

#[tokio::test]
async fn host_shutdown_closes_an_idle_subscriber_and_rejects_new_work() {
    let (_temp, db, _fixture, url, task, gateway) = server().await;
    let mut request = (url.replacen("http://", "ws://", 1) + "/rpc")
        .into_client_request()
        .unwrap();
    request.headers_mut().insert(
        "Authorization",
        HeaderValue::from_str(&format!("Bearer {TOKEN}")).unwrap(),
    );
    let (mut socket, _) = connect_async(request).await.unwrap();
    socket
        .send(Message::Text(
            json!({"jsonrpc":"2.0","id":1,"method":"conversation.subscribe","params":{"cursor":0}})
                .to_string()
                .into(),
        ))
        .await
        .unwrap();
    let receipt = socket.next().await.unwrap().unwrap();
    assert!(receipt.is_text());
    gateway.close_connections();
    let closed = tokio::time::timeout(std::time::Duration::from_secs(2), socket.next())
        .await
        .unwrap();
    assert!(
        closed.is_none() || closed.is_some_and(|message| matches!(message, Ok(Message::Close(_))))
    );
    let response = reqwest::Client::new()
        .post(format!("{url}/rpc"))
        .bearer_auth(TOKEN)
        .json(&json!({"jsonrpc":"2.0","id":1,"method":"echo"}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status().as_u16(), 503);
    task.abort();
    db.shutdown().await.unwrap();
}

#[tokio::test]
async fn shutdown_drains_an_authenticated_http_request_with_an_unfinished_body() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let temp = tempfile::tempdir().unwrap();
    let db = Database::open(
        temp.path().join("main.db"),
        DatabaseKind::Main,
        StorageConfig::default(),
    )
    .unwrap();
    let fixture = Arc::new(Fixture::default());
    let gateway = Gateway::new(
        fixture.clone(),
        db.clone(),
        AccessPolicy::new(TOKEN, BTreeSet::new(), 1024).unwrap(),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let router = gateway.clone().router();
    let mut server = tokio::spawn(async move {
        axum::serve(listener, router)
            .with_graceful_shutdown(async {
                let _ = stopped.await;
            })
            .await
            .unwrap();
    });
    let mut client = tokio::net::TcpStream::connect(address).await.unwrap();
    let headers = format!(
        "POST /rpc HTTP/1.1\r\nHost: {address}\r\nAuthorization: Bearer {TOKEN}\r\nContent-Type: application/json\r\nContent-Length: 512\r\nExpect: 100-continue\r\n\r\n"
    );
    client.write_all(headers.as_bytes()).await.unwrap();
    let mut response = Vec::new();
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        let mut byte = [0; 1];
        while !response.ends_with(b"\r\n\r\n") {
            client.read_exact(&mut byte).await.unwrap();
            response.push(byte[0]);
        }
    })
    .await
    .unwrap();
    assert!(
        response.starts_with(b"HTTP/1.1 100 Continue"),
        "request body must actually have been admitted"
    );
    client
        .write_all(
            br#"{"jsonrpc":"2.0","id":1,"method":"echo","params":{"must_not_execute":true}}"#,
        )
        .await
        .unwrap();
    gateway.close_connections();
    stop.send(()).unwrap();
    let drained = tokio::time::timeout(std::time::Duration::from_secs(3), &mut server).await;
    if drained.is_err() {
        server.abort();
        db.shutdown().await.unwrap();
        panic!("graceful shutdown waited for the client to finish its request body");
    }
    drained.unwrap().unwrap();
    assert_eq!(fixture.calls.load(Ordering::SeqCst), 0);
    db.shutdown().await.unwrap();
}
