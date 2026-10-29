use excalibur_journal::Journal;
use excalibur_store::{Database, DatabaseKind, StorageConfig};
use excalibur_telemetry::{RunMetadata, Telemetry, TelemetryConfig};
use excalibur_vault::Redactor;
use serde_json::json;
use sha2::{Digest, Sha256};

#[tokio::test]
async fn context_receipts_are_compact_source_bound_and_leave_principal_state_and_requests_intact() {
    let temp = tempfile::tempdir().unwrap();
    let db = Database::open(
        temp.path().join("main.db"),
        DatabaseKind::Main,
        StorageConfig::default(),
    )
    .unwrap();
    let journal = Journal::new(db.clone()).await.unwrap();
    let run = journal
        .create_run("compact-telemetry-fixture", json!({}))
        .await
        .unwrap();
    let full = json!({"session":{"base":{"layers":["literal-original", "catalogue-only-in-principal"]},"history":["synthetic history ".repeat(131072)]},"receipt":{"estimated_input_tokens":700000,"guarded_input_tokens":770000,"stable_prefix_hash":"stable-fixture","mask":{"ids":["obs-1"],"before_tokens":30000,"after_tokens":12000}},"history_cursor":52,"instruction_digest":"instruction-fixture"});
    assert!(serde_json::to_vec(&full).unwrap().len() > 2 * 1024 * 1024);
    let seq = journal
        .append(&run, "context_prepared", "context:0:prepared", full.clone())
        .await
        .unwrap();
    let request = json!({"request":{"messages":[{"role":"user","content":"exact compiled request","tool_calls":[]}],"tools":[],"tools_enabled":false},"hash":"request-fixture"});
    let request_sequence = journal
        .append(&run, "model_request", "model:0:request", request.clone())
        .await
        .unwrap();
    let cfg = TelemetryConfig::new(
        temp.path().join("telemetry"),
        RunMetadata::default(),
        Redactor::default(),
    );
    let telemetry = Telemetry::open(cfg.clone()).await.unwrap();
    telemetry.reconcile(&journal, &run).await.unwrap();
    let rows = telemetry.events_after(0, 10).await.unwrap();
    let receipt = rows.iter().find(|event| event.sequence == seq).unwrap();
    assert!(receipt.payload.get("session").is_none());
    assert_eq!(receipt.payload["receipt"], full["receipt"]);
    assert_eq!(receipt.payload["history_cursor"], 52);
    assert_eq!(receipt.payload["instruction_digest"], "instruction-fixture");
    assert_eq!(receipt.payload["source"]["run_id"], run);
    assert_eq!(receipt.payload["source"]["sequence"], seq);
    assert_eq!(receipt.payload["source"]["event_kind"], "context_prepared");
    assert_eq!(receipt.payload["source"]["event_key"], "context:0:prepared");
    let expected = format!("{:x}", Sha256::digest(serde_json::to_vec(&full).unwrap()));
    assert_eq!(receipt.payload["source"]["payload_sha256"], expected);
    assert!(serde_json::to_vec(&receipt.payload).unwrap().len() < 4096);
    assert_eq!(
        rows.iter()
            .find(|event| event.sequence == request_sequence)
            .unwrap()
            .payload,
        request,
        "model requests retain the existing exact projection contract"
    );
    let original = journal
        .entries(&run, seq - 1, 1)
        .await
        .unwrap()
        .pop()
        .unwrap();
    assert_eq!(original.payload, full);
    assert_eq!(
        telemetry
            .record_event(&run, &original)
            .await
            .unwrap()
            .cursor,
        receipt.cursor
    );
    let mut changed = original.clone();
    changed.payload["session"]["base"]["layers"][0] = json!("changed omitted source data");
    assert!(
        telemetry.record_event(&run, &changed).await.is_err(),
        "omitting a session must not weaken identity checks"
    );
    let exported = std::fs::read_to_string(temp.path().join("telemetry/journal.jsonl")).unwrap();
    assert!(!exported.contains("catalogue-only-in-principal"));
    assert!(exported.contains("instruction-fixture"));
    telemetry.shutdown().await.unwrap();
    drop(telemetry);
    let reopened = Telemetry::open(cfg).await.unwrap();
    reopened.reconcile(&journal, &run).await.unwrap();
    let recovered = reopened.events_after(0, 10).await.unwrap();
    assert_eq!(recovered.len(), rows.len());
    assert_eq!(
        recovered
            .iter()
            .find(|event| event.sequence == seq)
            .unwrap()
            .payload,
        receipt.payload
    );
    assert_eq!(
        journal.entries(&run, seq - 1, 1).await.unwrap()[0].payload,
        full
    );
    reopened.shutdown().await.unwrap();
    db.shutdown().await.unwrap();
}
