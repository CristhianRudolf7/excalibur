use excalibur_decider::*;
use excalibur_providers::{Provider, ProviderClient, Secret};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::sync::Arc;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};
async fn server(status: &str, body: Value) -> (ProviderClient, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}/v1/", listener.local_addr().unwrap());
    let body = body.to_string();
    let reply = format!(
        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let task = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut data = Vec::new();
        loop {
            let mut buf = [0; 4096];
            let read = stream.read(&mut buf).await.unwrap();
            assert_ne!(read, 0);
            data.extend_from_slice(&buf[..read]);
            if let Some(pos) = data.windows(4).position(|s| s == b"\r\n\r\n") {
                let head = String::from_utf8_lossy(&data[..pos]);
                let length = head
                    .lines()
                    .find_map(|line| {
                        line.to_ascii_lowercase()
                            .strip_prefix("content-length:")
                            .and_then(|s| s.trim().parse::<usize>().ok())
                    })
                    .unwrap_or(0);
                if data.len() >= pos + 4 + length {
                    break;
                }
            }
        }
        stream.write_all(reply.as_bytes()).await.unwrap();
    });
    (
        ProviderClient::with_base_url(
            Provider::OpenAi,
            Secret::new("synthetic-local-key").unwrap(),
            &base,
        )
        .unwrap(),
        task,
    )
}
fn request() -> DecisionRequest {
    DecisionRequest::new(
        DecisionPort::SkillRelevance,
        DecisionInput::Text("local test".into()),
        vec![Question::predicate("relevant", "Is this relevant?")],
    )
}
fn response() -> Value {
    json!({"model":"same-model", "answers":[{"name":"relevant", "type":"predicate", "probability":0.7}]})
}
#[tokio::test]
async fn same_model_on_different_endpoints_does_not_share_calibration() {
    let (client_a, task_a) = server("200 OK", response()).await;
    let (client_b, task_b) = server("200 OK", response()).await;
    let root = tempfile::tempdir().unwrap();
    let ledger = DecisionLedger::open(root.path().join("decisions.db"))
        .await
        .unwrap();
    let a = DecisionService::new(Arc::new(
        OpenAiDecisions::new(client_a, "same-model").unwrap(),
    ))
    .with_ledger(ledger.clone())
    .await
    .unwrap();
    let result_a = a.decide(&request()).await.unwrap();
    let key = &result_a.answers[0].calibration_key;
    ledger
        .label(
            result_a.answers[0].record_id.as_ref().unwrap(),
            true,
            LabelSource::Review,
        )
        .await
        .unwrap();
    a.calibrate(key, 0.9, 1).await.unwrap();
    let b = DecisionService::new(Arc::new(
        OpenAiDecisions::new(client_b, "same-model").unwrap(),
    ))
    .with_ledger(ledger.clone())
    .await
    .unwrap();
    let result_b = b.decide(&request()).await.unwrap();
    assert_ne!(
        result_a.answers[0].origin.backend,
        result_b.answers[0].origin.backend
    );
    assert_eq!(
        result_a.answers[0].origin.model,
        result_b.answers[0].origin.model
    );
    assert!(!result_b.answers[0].thresholds.calibrated);
    assert_eq!(result_b.answers[0].verdict, Verdict::Defer);
    task_a.await.unwrap();
    task_b.await.unwrap();
    ledger.shutdown().await.unwrap();
}
#[tokio::test]
async fn quota_fallback_records_the_effective_transport_and_route() {
    let (primary, primary_task) = server("429 Too Many Requests", json!({"error":{"code":"insufficient_quota","type":"insufficient_quota","message":"synthetic quota fixture"}})).await;
    let (backup, backup_task) = server("200 OK", response()).await;
    let backup_identity = backup.configuration_identity();
    let decider = OpenAiDecisions::new(primary, "same-model")
        .unwrap()
        .with_quota_fallback(backup);
    let result = decider.decide(&request()).await.unwrap();
    let expected = format!(
        "openai-decisions:{:x}:quota_fallback",
        Sha256::digest(backup_identity.as_bytes())
    );
    assert_eq!(result.backend, expected);
    assert_eq!(result.fallback_questions, vec!["relevant"]);
    assert!(!result.backend.contains("synthetic-local-key"));
    assert!(!result.backend.contains(&backup_identity));
    assert_eq!(
        result.origin("relevant").model.as_deref(),
        Some("same-model")
    );
    primary_task.await.unwrap();
    backup_task.await.unwrap();
}
