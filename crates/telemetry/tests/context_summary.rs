use excalibur_journal::Event;
use excalibur_telemetry::{RunMetadata, Telemetry, TelemetryConfig, atif, otel};
use excalibur_vault::Redactor;
use serde_json::{Value, json};
use std::sync::{
    Arc,
    atomic::{AtomicI64, Ordering},
};
fn config(root: &std::path::Path) -> (TelemetryConfig, Arc<AtomicI64>) {
    let clock = Arc::new(AtomicI64::new(1_700_000_000_000));
    let mut cfg = TelemetryConfig::new(
        root.join("telemetry"),
        RunMetadata {
            instruction: "Synthetic task".into(),
            provider: Some("mock".into()),
            model: Some("executor".into()),
            role: Some("coordinator".into()),
            ..Default::default()
        },
        Redactor::default(),
    );
    cfg.artifact_root = Some(root.into());
    let now = clock.clone();
    cfg.clock = Arc::new(move || now.load(Ordering::SeqCst));
    (cfg, clock)
}
fn measured(model: &str, input: u64, output: u64, cost: u64) -> Value {
    json!({"metadata":{"provider":"mock","model":model,"role":"coordinator"},"usage":{"input_tokens":input,"output_tokens":output,"cache_read_tokens":0,"cache_write_tokens":0,"reasoning_tokens":0},"provider_calls":1,"provider_calls_known":true,"usage_known":true,"cost_microusd":cost,"duration_ms":12})
}
async fn record(t: &Telemetry, clock: &AtomicI64, seq: u64, kind: &str, key: &str, payload: Value) {
    clock.fetch_add(50, Ordering::SeqCst);
    t.record_event(
        "run",
        &Event {
            sequence: seq,
            kind: kind.into(),
            key: key.into(),
            payload,
        },
    )
    .await
    .unwrap();
}
#[tokio::test]
async fn actual_summary_usage_reaches_metrics_atif_and_otlp_without_stealing_tool_observation() {
    let temp = tempfile::tempdir().unwrap();
    let (cfg, clock) = config(temp.path());
    let t = Telemetry::open(cfg).await.unwrap();
    record(
        &t,
        &clock,
        1,
        "run_started",
        "seed",
        json!({"model_dispatch_protocol":1}),
    )
    .await;
    record(&t,&clock,2,"model_response","model:0:response",json!({"response":{"message":{"role":"assistant","content":"Executor will read","tool_calls":[{"id":"read-1","name":"read_file","arguments":"{}"}]}},"metrics":measured("executor",20,5,12)})).await;
    record(
        &t,
        &clock,
        3,
        "context_summary_started",
        "context-summary:summary-1:started",
        json!({"id":"summary-1","bound":{"max_tokens":500},"request":{"messages":[]}}),
    )
    .await;
    record(&t,&clock,4,"context_summary_completed","context-summary:summary-1:completed",json!({"id":"summary-1","outcome":{"status":"added","request_id":"summary-1","delta":{"findings":["Synthetic summary"]}},"usage":{"input_tokens":11,"output_tokens":3},"metrics":measured("extractor",11,3,9),"model_calls":1})).await;
    record(
        &t,
        &clock,
        5,
        "tool_intent",
        "tool:0:0",
        json!({"index":0,"prepared":{"call":{"id":"read-1","name":"read_file","arguments":"{}"}}}),
    )
    .await;
    record(&t,&clock,6,"tool_result","tool:0:0:result",json!({"index":0,"name":"read_file","result":{"success":true,"output":{"literal":"actual observation"}}})).await;
    let m = t.metrics("run").await.unwrap();
    assert_eq!(m.model_calls, 2);
    assert_eq!(m.pending_model_calls, 0);
    assert_eq!(m.provider_calls, 2);
    assert_eq!(m.tokens.input_tokens, 31);
    assert_eq!(m.tokens.output_tokens, 8);
    assert_eq!(m.cost_microusd, Some(21));
    assert_eq!(m.model_calls_by_role["context-summary"], 1);
    assert_eq!(m.model_calls_by_role["coordinator"], 1);
    assert_eq!(m.model_usage["extractor"].calls, 1);
    let trajectory: Value =
        serde_json::from_slice(&std::fs::read(temp.path().join("trajectory.json")).unwrap())
            .unwrap();
    atif::validate(&trajectory).unwrap();
    let steps = trajectory["steps"].as_array().unwrap();
    let executor = steps.iter().find(|s| s["tool_calls"].is_array()).unwrap();
    assert!(
        executor["observation"]
            .to_string()
            .contains("actual observation")
    );
    assert_eq!(
        steps
            .iter()
            .filter(|s| s["extra"]["purpose"] == "context-summary")
            .count(),
        1
    );
    assert_eq!(trajectory["final_metrics"]["total_prompt_tokens"], 31);
    let trace: Value =
        serde_json::from_slice(&std::fs::read(temp.path().join("telemetry/otel.json")).unwrap())
            .unwrap();
    otel::validate(&trace).unwrap();
    assert!(trace.to_string().contains("excalibur.model.purpose"));
    t.shutdown().await.unwrap();
}
#[tokio::test]
async fn interrupted_and_pending_summary_calls_are_unknown_not_free_or_measured_reservations() {
    let temp = tempfile::tempdir().unwrap();
    let (cfg, clock) = config(temp.path());
    let t = Telemetry::open(cfg.clone()).await.unwrap();
    record(
        &t,
        &clock,
        1,
        "run_started",
        "seed",
        json!({"model_dispatch_protocol":1}),
    )
    .await;
    record(
        &t,
        &clock,
        2,
        "context_summary_started",
        "context-summary:summary-1:started",
        json!({"id":"summary-1","bound":{"max_tokens":500,"max_cost_microusd":900},"request":{}}),
    )
    .await;
    let pending = t.metrics("run").await.unwrap();
    assert_eq!(pending.model_calls, 1);
    assert_eq!(pending.pending_model_calls, 1);
    assert!(!pending.provider_calls_known);
    assert!(!pending.usage_known);
    assert_eq!(pending.cost_microusd, None);
    assert_eq!(pending.model_calls_by_role["context-summary"], 1);
    t.shutdown().await.unwrap();
    drop(t);
    let t = Telemetry::open(cfg).await.unwrap();
    assert_eq!(t.metrics("run").await.unwrap().pending_model_calls, 1);
    record(&t,&clock,3,"context_summary_completed","context-summary:summary-1:completed",json!({"id":"summary-1","outcome":{"status":"unavailable","reason":"interrupted"},"usage":{"input_tokens":500,"output_tokens":0,"cost_microusd":900,"estimated":true},"model_calls":1,"interrupted":true})).await;
    let m = t.metrics("run").await.unwrap();
    assert_eq!(m.model_calls, 1);
    assert_eq!(m.pending_model_calls, 0);
    assert_eq!(m.unmeasured_model_calls, 1);
    assert!(!m.provider_calls_known);
    assert!(!m.usage_known);
    assert_eq!(m.tokens.input_tokens, 0, "reservation is not an invoice");
    assert_eq!(m.cost_microusd, None);
    assert_eq!(m.model_time_ms, None);
    assert!(m.limitations.iter().any(|s| s.contains("reservation")));
    let trajectory: Value =
        serde_json::from_slice(&std::fs::read(temp.path().join("trajectory.json")).unwrap())
            .unwrap();
    atif::validate(&trajectory).unwrap();
    assert!(trajectory["final_metrics"]["total_prompt_tokens"].is_null());
    let aux = trajectory["steps"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["extra"]["purpose"] == "context-summary")
        .unwrap();
    assert!(aux["llm_call_count"].is_null());
    assert!(aux["metrics"].is_null());
    assert_eq!(
        aux["extra"]["model_call_metrics"]["reservation"]["input_tokens"],
        500
    );
    t.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_confirmed_auxiliary_provider_failure_retains_error_and_http_evidence_without_inventing_usage()
 {
    let temp = tempfile::tempdir().unwrap();
    let (cfg, clock) = config(temp.path());
    let t = Telemetry::open(cfg).await.unwrap();
    record(
        &t,
        &clock,
        1,
        "run_started",
        "seed",
        json!({"model_dispatch_protocol":1}),
    )
    .await;
    record(
        &t,
        &clock,
        2,
        "context_summary_started",
        "context-summary:failed:started",
        json!({"id":"failed","request":{}}),
    )
    .await;
    let mut metrics = measured("extractor", 0, 0, 0);
    metrics["usage_known"] = json!(false);
    metrics["usage"] = json!({});
    metrics["cost_microusd"] = Value::Null;
    metrics["failure_class"] = json!("quota");
    record(&t,&clock,3,"context_summary_completed","context-summary:failed:completed",json!({"id":"failed","outcome":{"status":"unavailable","reason":"provider failure"},"usage":{},"metrics":metrics,"model_calls":1})).await;
    let m = t.metrics("run").await.unwrap();
    assert_eq!(m.model_calls, 1);
    assert_eq!(m.provider_calls, 1);
    assert!(m.provider_calls_known);
    assert!(!m.usage_known);
    assert_eq!(m.unmeasured_model_calls, 1);
    assert_eq!(m.cost_microusd, None);
    assert_eq!(m.model_time_ms, Some(12));
    assert_eq!(m.errors, 1);
    assert_eq!(
        m.failure_class, None,
        "an optional extractor failure is not the executor final failure"
    );
    let trajectory: Value =
        serde_json::from_slice(&std::fs::read(temp.path().join("trajectory.json")).unwrap())
            .unwrap();
    atif::validate(&trajectory).unwrap();
    assert!(trajectory["final_metrics"]["total_prompt_tokens"].is_null());
    assert_eq!(trajectory["steps"][1]["llm_call_count"], 1);
    assert_eq!(
        trajectory["steps"][1]["extra"]["model_call_metrics"]["failure_class"],
        "quota"
    );
    let trace: Value =
        serde_json::from_slice(&std::fs::read(temp.path().join("telemetry/otel.json")).unwrap())
            .unwrap();
    otel::validate(&trace).unwrap();
    assert!(trace.to_string().contains("quota"));
    t.shutdown().await.unwrap();
}
