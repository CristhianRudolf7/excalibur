//! Authenticated HTTP against the real foreground daemon and local provider.
use serde_json::{Value, json};
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};
const TOKEN: &str = "synthetic-local-provider-recovery-token";
struct ChildGuard(std::process::Child);
impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
async fn rpc(
    client: &reqwest::Client,
    address: std::net::SocketAddr,
    method: &str,
    params: Value,
) -> Value {
    client
        .post(format!("http://{address}/rpc"))
        .bearer_auth(TOKEN)
        .json(&json!({"jsonrpc":"2.0","id":1,"method":method,"params":params}))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap()
}
async fn send(
    client: &reqwest::Client,
    address: std::net::SocketAddr,
    id: &str,
    text: &str,
    conversation: &str,
) -> Value {
    rpc(
        client,
        address,
        "conversation.send",
        json!({"conversation":conversation,"client_id":"fixture","client_msg_id":id,"text":text}),
    )
    .await
}
#[test]
#[ignore = "native foreground daemon subprocess fixture"]
fn daemon_process_helper() {
    let args: Vec<String> =
        serde_json::from_str(&std::env::var("EXCALIBUR_RECOVERY_ARGS").unwrap()).unwrap();
    assert_eq!(excalibur_daemon::command::run(&args), 0);
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn full_inbox_fast_controls_and_recovered_provider_work_through_real_http_without_restart() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("project");
    std::fs::create_dir(&root).unwrap();
    let available = Arc::new(AtomicBool::new(false));
    let calls = Arc::new(AtomicUsize::new(0));
    let ready = available.clone();
    let counted = calls.clone();
    let provider=axum::Router::new().route("/v1/chat/completions",axum::routing::post(move || {let ready=ready.clone();let counted=counted.clone();async move{
        counted.fetch_add(1,Ordering::SeqCst);
        if ready.load(Ordering::SeqCst){(axum::http::StatusCode::OK,axum::Json(json!({"choices":[{"finish_reason":"stop","message":{"role":"assistant","content":"Provider recovered; queued request processed."}}],"usage":{"prompt_tokens":2,"completion_tokens":2}})))}else{(axum::http::StatusCode::SERVICE_UNAVAILABLE,axum::Json(json!({"error":{"message":"synthetic provider unavailable"}})))}
    }}));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/v1/", listener.local_addr().unwrap());
    let provider_server =
        tokio::spawn(async move { axum::serve(listener, provider).await.unwrap() });
    let address = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap();
    let args = vec![
        "start".to_owned(),
        "--project".into(),
        root.to_string_lossy().into_owned(),
        "--data-dir".into(),
        temp.path().join("data").to_string_lossy().into_owned(),
        "--listen".into(),
        address.to_string(),
        "--model".into(),
        "local-fixture".into(),
        "--base-url".into(),
        url,
    ];
    let stderr = std::fs::File::create(temp.path().join("daemon.stderr")).unwrap();
    let mut child = ChildGuard(
        std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "daemon_process_helper",
                "--ignored",
                "--nocapture",
            ])
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env(
                "EXCALIBUR_RECOVERY_ARGS",
                serde_json::to_string(&args).unwrap(),
            )
            .env("EXCALIBUR_DAEMON_TOKEN", TOKEN)
            .env("OPENAI_API_KEY", "synthetic-no-real-credential")
            .stdout(std::process::Stdio::null())
            .stderr(stderr)
            .spawn()
            .unwrap(),
    );
    tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            if tokio::net::TcpStream::connect(address).await.is_ok() {
                break;
            }
            assert!(
                child.0.try_wait().unwrap().is_none(),
                "daemon exited: {}",
                std::fs::read_to_string(temp.path().join("daemon.stderr")).unwrap()
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .unwrap();
    assert!(
        send(
            &client,
            address,
            "queued-0",
            "Explain the queued fixture",
            "outage"
        )
        .await
        .get("result")
        .is_some()
    );
    tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            let events = rpc(
                &client,
                address,
                "events.replay",
                json!({"cursor":0,"limit":200}),
            )
            .await;
            if events.to_string().contains("model_unavailable") {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    for index in 1..128 {
        assert!(
            send(
                &client,
                address,
                &format!("queued-{index}"),
                "Another queued fixture",
                "outage"
            )
            .await
            .get("result")
            .is_some()
        );
    }
    assert!(
        send(&client, address, "overflow", "This inbox is full", "outage")
            .await
            .get("error")
            .is_some()
    );
    for (index, command) in ["/status", "/stop", "/stop all"].iter().enumerate() {
        let response = send(
            &client,
            address,
            &format!("control-{index}"),
            command,
            "outage",
        )
        .await;
        assert!(response.get("result").is_some(), "{response}");
    }
    assert!(
        send(
            &client,
            address,
            "after-stop",
            "Process this after recovery",
            "outage"
        )
        .await
        .get("result")
        .is_some()
    );
    assert!(
        send(
            &client,
            address,
            "new-conversation",
            "Process another conversation",
            "new-conversation"
        )
        .await
        .get("result")
        .is_some()
    );
    available.store(true, Ordering::SeqCst);
    // No host method is called by the test. The real service's housekeeping
    // must discover recovery and schedule both conversations on its own.
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let mut complete = true;
            for conversation in ["outage", "new-conversation"] {
                let history = rpc(
                    &client,
                    address,
                    "conversation.history",
                    json!({"conversation":conversation,"limit":200}),
                )
                .await;
                complete &= history["result"].as_array().is_some_and(|messages| {
                    messages.iter().any(|message| {
                        message["role"] == "assistant"
                            && message["text"]
                                .as_str()
                                .is_some_and(|text| text.contains("Provider recovered"))
                    })
                });
            }
            if complete {
                break;
            }
            assert!(
                child.0.try_wait().unwrap().is_none(),
                "daemon exited during recovery"
            );
            tokio::time::sleep(Duration::from_millis(30)).await;
        }
    })
    .await
    .unwrap();
    assert!(calls.load(Ordering::SeqCst) >= 2);
    let stopped = rpc(&client, address, "daemon.stop", json!({})).await;
    assert_eq!(stopped["result"]["stopping"], true);
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            if let Some(status) = child.0.try_wait().unwrap() {
                assert!(status.success());
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    provider_server.abort();
}
