use excalibur_cdp::{Client, Error, MAX_MESSAGE, protocol::Method};
use serde_json::{Value, json};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
fn pair() -> (Client, tokio::io::DuplexStream) {
    let (client, server) = tokio::io::duplex(65536);
    let (read, write) = tokio::io::split(client);
    (Client::connect(read, write), server)
}
#[tokio::test]
async fn framing_correlation_sessions_and_protocol_errors() {
    let (client, server) = pair();
    let mut session_events = client.subscribe(Some("page"));
    let mut root_events = client.subscribe(None);
    let server = tokio::spawn(async move {
        let (read, mut write) = tokio::io::split(server);
        let mut read = BufReader::new(read);
        let mut requests = Vec::new();
        for _ in 0..20 {
            let mut frame = Vec::new();
            read.read_until(0, &mut frame).await.unwrap();
            requests.push(serde_json::from_slice::<Value>(&frame[..frame.len() - 1]).unwrap());
        }
        write
            .write_all(
                b"{\"method\":\"Page.loadEventFired\",\"params\":{},\"sessionId\":\"page\"}\0",
            )
            .await
            .unwrap();
        for r in requests.into_iter().rev() {
            let mut response = json!({"id": r["id"], "sessionId":r["sessionId"],"result":{"value":r["params"]["value"]}});
            if r["params"]["value"] == 7 {
                response.as_object_mut().unwrap().remove("result");
                response["error"] = json!({"code":-32602,"message":"synthetic invalid parameters"});
            }
            let mut bytes = serde_json::to_vec(&response).unwrap();
            bytes.push(0);
            for chunk in bytes.chunks(3) {
                write.write_all(chunk).await.unwrap();
            }
        }
    });
    let mut tasks = Vec::new();
    for value in 0..20 {
        let c = client.clone();
        tasks.push(tokio::spawn(async move {
            (
                value,
                c.send(
                    Method::RuntimeCallFunctionOn,
                    json!({"value":value}),
                    Some("page"),
                )
                .await,
            )
        }));
    }
    for task in tasks {
        let (value, result) = task.await.unwrap();
        if value == 7 {
            assert!(matches!(result, Err(Error::Protocol { code: -32602, .. })));
        } else {
            assert_eq!(result.unwrap()["value"], value);
        }
    }
    assert_eq!(
        session_events.recv().await.unwrap().method,
        "Page.loadEventFired"
    );
    if let Ok(event) = root_events.try_recv() {
        assert_ne!(event.method, "Page.loadEventFired");
    }
    assert_eq!(client.pending_count(), 0);
    server.await.unwrap();
}
#[tokio::test]
async fn timeouts_cancellation_late_responses_and_eof() {
    let (client, server) = pair();
    let server = tokio::spawn(async move {
        let (read, mut write) = tokio::io::split(server);
        let mut read = BufReader::new(read);
        let mut ids = Vec::new();
        for _ in 0..3 {
            let mut frame = Vec::new();
            read.read_until(0, &mut frame).await.unwrap();
            ids.push(
                serde_json::from_slice::<Value>(&frame[..frame.len() - 1]).unwrap()["id"]
                    .as_u64()
                    .unwrap(),
            );
        }
        assert!(ids.windows(2).all(|v| v[0] < v[1]));
        for id in ids {
            let mut bytes = serde_json::to_vec(&json!({"id":id,"result":{}})).unwrap();
            bytes.push(0);
            write.write_all(&bytes).await.unwrap();
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    });
    assert_eq!(
        client
            .send_with_timeout(
                Method::BrowserGetVersion,
                json!({}),
                None,
                Duration::from_millis(20)
            )
            .await
            .unwrap_err(),
        Error::Timeout
    );
    let c = client.clone();
    let task =
        tokio::spawn(async move { c.send(Method::BrowserGetVersion, json!({}), None).await });
    tokio::time::sleep(Duration::from_millis(20)).await;
    task.abort();
    let _ = task.await;
    assert_eq!(client.pending_count(), 0);
    client
        .send(Method::BrowserGetVersion, json!({}), None)
        .await
        .unwrap();
    server.await.unwrap();
    let mut closed = client.closed();
    if closed.borrow().is_none() {
        closed.changed().await.unwrap();
    }
    assert!(matches!(*closed.borrow(), Some(Error::Closed(_))));
}
#[tokio::test]
async fn cancelled_partial_write_closes_transport_and_removes_pending() {
    let (client, _server) = pair();
    let result = client
        .send_with_timeout(
            Method::RuntimeCallFunctionOn,
            json!({"large":"x".repeat(100000)}),
            None,
            Duration::from_millis(20),
        )
        .await;
    assert_eq!(result.unwrap_err(), Error::Timeout);
    assert_eq!(client.pending_count(), 0);
    assert!(matches!(
        client
            .send(Method::BrowserGetVersion, json!({}), None)
            .await,
        Err(Error::Closed(_))
    ));
}
#[tokio::test]
async fn oversized_frame_detaches_only_its_session_and_keeps_stream_synchronized() {
    let (client, mut server) = pair();
    let mut events = client.subscribe(Some("huge"));
    let server = tokio::spawn(async move {
        server
            .write_all(b"{\"method\":\"Runtime.consoleAPICalled\",\"params\":{\"text\":\"")
            .await
            .unwrap();
        let chunk = vec![b'x'; 65536];
        for _ in 0..(MAX_MESSAGE / 65536 + 96) {
            server.write_all(&chunk).await.unwrap();
        }
        server.write_all(b"\"},\"sessionId\":\"huge\"}\0{\"method\":\"Page.loadEventFired\",\"params\":{},\"sessionId\":\"small\"}\0").await.unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
    });
    let mut small = client.subscribe(Some("small"));
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(60), events.recv())
            .await
            .unwrap()
            .unwrap()
            .method,
        "session_crashed"
    );
    assert_eq!(small.recv().await.unwrap().method, "Page.loadEventFired");
    assert!(matches!(
        client
            .send(Method::PageEnable, json!({}), Some("huge"))
            .await,
        Err(Error::SessionCrashed(_))
    ));
    server.await.unwrap();
}
#[tokio::test]
async fn malformed_json_fails_closed() {
    let (client, mut server) = pair();
    server.write_all(b"{broken}\0").await.unwrap();
    let mut closed = client.closed();
    if closed.borrow().is_none() {
        closed.changed().await.unwrap();
    }
    assert!(closed.borrow().is_some());
}

#[tokio::test(start_paused = true)]
async fn default_command_deadlines_are_ten_fifteen_and_twenty_seconds() {
    for (method, seconds) in [
        (Method::PageEnable, 10),
        (Method::PageCaptureScreenshot, 15),
        (Method::DOMSnapshotCaptureSnapshot, 20),
    ] {
        let (client, _server) = pair();
        let task = tokio::spawn(async move { client.send(method, json!({}), None).await });
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_secs(seconds - 1)).await;
        assert!(!task.is_finished());
        tokio::time::advance(Duration::from_secs(1)).await;
        assert_eq!(task.await.unwrap().unwrap_err(), Error::Timeout);
    }
}

#[tokio::test]
async fn routing_error_without_session_id_does_not_kill_other_sessions() {
    let (client, server) = pair();
    let server = tokio::spawn(async move {
        let (read, mut write) = tokio::io::split(server);
        let mut read = BufReader::new(read);
        for index in 0..2 {
            let mut frame = Vec::new();
            read.read_until(0, &mut frame).await.unwrap();
            let value: Value = serde_json::from_slice(&frame[..frame.len() - 1]).unwrap();
            let response = if index == 0 {
                json!({"id":value["id"],"error":{"code":-32001,"message":"Session with given id not found."}})
            } else {
                json!({"id":value["id"],"result":{}})
            };
            let mut bytes = serde_json::to_vec(&response).unwrap();
            bytes.push(0);
            write.write_all(&bytes).await.unwrap();
        }
    });
    assert!(matches!(
        client
            .send(Method::PageEnable, json!({}), Some("gone"))
            .await,
        Err(Error::Protocol { code: -32001, .. })
    ));
    client
        .send(Method::BrowserGetVersion, json!({}), None)
        .await
        .unwrap();
    server.await.unwrap();
}

#[tokio::test]
async fn console_backpressure_does_not_displace_lifecycle_or_responses() {
    let (client, server) = pair();
    let mut observer = client.subscribe_all();
    let mut lifecycle = client.subscribe_lifecycle();
    let (release, done) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let (read, mut write) = tokio::io::split(server);
        let mut read = BufReader::new(read);
        let events = [
            json!({"method":"Target.attachedToTarget","params":{"sessionId":"child"}}),
            json!({"method":"Inspector.targetCrashed","params":{},"sessionId":"child"}),
            json!({"method":"Target.detachedFromTarget","params":{"sessionId":"child"}}),
        ];
        for event in events {
            let mut bytes = serde_json::to_vec(&event).unwrap();
            bytes.push(0);
            write.write_all(&bytes).await.unwrap();
            for _ in 0..4000 {
                write
                    .write_all(b"{\"method\":\"Runtime.consoleAPICalled\",\"params\":{}}\0")
                    .await
                    .unwrap();
            }
        }
        let mut bytes = Vec::new();
        read.read_until(0, &mut bytes).await.unwrap();
        let request: Value = serde_json::from_slice(&bytes[..bytes.len() - 1]).unwrap();
        let mut response = serde_json::to_vec(&json!({"id":request["id"],"result":{}})).unwrap();
        response.push(0);
        write.write_all(&response).await.unwrap();
        let _ = done.await;
    });
    client
        .send(Method::BrowserGetVersion, json!({}), None)
        .await
        .unwrap();
    assert!(matches!(
        observer.try_recv(),
        Err(tokio::sync::broadcast::error::TryRecvError::Lagged(_))
    ));
    for expected in [
        "Target.attachedToTarget",
        "Inspector.targetCrashed",
        "Target.detachedFromTarget",
    ] {
        assert_eq!(lifecycle.recv().await.unwrap().method, expected);
    }
    assert!(client.attached_sessions().is_empty());
    release.send(()).unwrap();
    server.await.unwrap();
}

#[tokio::test]
async fn lifecycle_lag_can_reconcile_every_active_attachment_without_closing() {
    let (client, server) = pair();
    let mut lifecycle = client.subscribe_lifecycle();
    let (release, done) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let (read, mut write) = tokio::io::split(server);
        let mut read = BufReader::new(read);
        for id in 0..400 {
            let mut bytes=serde_json::to_vec(&json!({"method":"Target.attachedToTarget","params":{"sessionId":format!("child-{id}")}})).unwrap();
            bytes.push(0);
            write.write_all(&bytes).await.unwrap();
        }
        let mut bytes = Vec::new();
        read.read_until(0, &mut bytes).await.unwrap();
        let request: Value = serde_json::from_slice(&bytes[..bytes.len() - 1]).unwrap();
        let mut response = serde_json::to_vec(&json!({"id":request["id"],"result":{}})).unwrap();
        response.push(0);
        write.write_all(&response).await.unwrap();
        let _ = done.await;
    });
    client
        .send(Method::BrowserGetVersion, json!({}), None)
        .await
        .unwrap();
    assert!(matches!(
        lifecycle.try_recv(),
        Err(tokio::sync::broadcast::error::TryRecvError::Lagged(_))
    ));
    let active = client.attached_sessions();
    assert_eq!(active.len(), 400);
    let ids: std::collections::HashSet<_> = active
        .iter()
        .map(|e| e.params["sessionId"].as_str().unwrap())
        .collect();
    assert_eq!(ids.len(), 400);
    assert!(client.closed().borrow().is_none());
    release.send(()).unwrap();
    server.await.unwrap();
}

#[tokio::test]
async fn console_backpressure_does_not_displace_readiness_events() {
    let (client, mut server) = pair();
    let mut readiness = client.subscribe_observation();
    let mut console = client.subscribe_console();
    let writer = tokio::spawn(async move {
        for _ in 0..10000 {
            server.write_all(b"{\"method\":\"Runtime.consoleAPICalled\",\"sessionId\":\"page\",\"params\":{}}\0").await.unwrap();
        }
        server.write_all(b"{\"method\":\"Network.loadingFinished\",\"sessionId\":\"page\",\"params\":{\"requestId\":\"fetch\"}}\0").await.unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
    });
    let event = tokio::time::timeout(Duration::from_secs(5), readiness.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(event.method, "Network.loadingFinished");
    assert!(matches!(
        console.try_recv(),
        Err(tokio::sync::broadcast::error::TryRecvError::Lagged(_))
    ));
    writer.await.unwrap();
}
