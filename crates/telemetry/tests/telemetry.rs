use excalibur_journal::Event;
use excalibur_telemetry::{
    EventKind, JsonlSink, RunMetadata, Telemetry, TelemetryConfig, atif, decode_jsonl_prefix, otel,
};
use excalibur_vault::{Redactor, SecretValue};
use serde_json::{Value, json};
use std::{
    io::{self, Write},
    sync::{
        Arc,
        atomic::{AtomicI64, Ordering},
    },
};
fn event(sequence: u64, kind: &str, key: &str, payload: Value) -> Event {
    Event {
        sequence,
        kind: kind.into(),
        key: key.into(),
        payload,
    }
}
fn config(directory: &std::path::Path) -> (TelemetryConfig, Arc<AtomicI64>) {
    let clock = Arc::new(AtomicI64::new(1_700_000_000_000));
    let metadata = RunMetadata {
        instruction: "Create a synthetic file".into(),
        provider: Some("mock".into()),
        model: Some("synthetic-v1".into()),
        role: Some("coordinator".into()),
        configuration: json!({"budget":500}),
        task_id: Some("synthetic-task".into()),
        ..Default::default()
    };
    let mut config = TelemetryConfig::new(directory, metadata, Redactor::default());
    let c = clock.clone();
    config.clock = Arc::new(move || c.load(Ordering::SeqCst));
    (config, clock)
}
fn observed() -> Value {
    json!({"metadata":{"provider":"mock","model":"synthetic-v1","role":"coordinator","configuration_hash":"mock-profile","price_table_version":"synthetic-prices-v1","price_table_hash":"abc"},"duration_ms":60,"provider_calls":2,"provider_calls_known":true,"usage_known":true,"usage":{"input_tokens":100,"output_tokens":30,"cache_read_tokens":40,"cache_write_tokens":20,"cache_write_by_ttl":{"5m":20},"reasoning_tokens":10,"estimated":false},"cost_microusd":125,"attempts":[{"index":0},{"index":1}]})
}
async fn record(
    t: &Telemetry,
    c: &AtomicI64,
    seq: u64,
    kind: &str,
    key: &str,
    p: Value,
) -> excalibur_telemetry::EventEnvelope {
    c.fetch_add(100, Ordering::SeqCst);
    t.record_event("run", &event(seq, kind, key, p))
        .await
        .unwrap()
}
#[tokio::test]
async fn accounting_atif_otel_and_incremental_primary_artifacts() {
    let dir = tempfile::tempdir().unwrap();
    let (mut cfg, clock) = config(&dir.path().join("excalibur"));
    cfg.artifact_root = Some(dir.path().into());
    let t = Telemetry::open(cfg).await.unwrap();
    record(
        &t,
        &clock,
        1,
        "run_started",
        "seed",
        json!({"spec":{"criteria":[{"kind":"tool_success"}]}}),
    )
    .await;
    assert!(dir.path().join("trajectory.json").exists());
    let partial: Value =
        serde_json::from_slice(&std::fs::read(dir.path().join("trajectory.json")).unwrap())
            .unwrap();
    atif::validate(&partial).unwrap();
    assert_eq!(partial["extra"]["finalized"], false);
    record(&t,&clock,2,"model_request","model:0:request",json!({"request":{"tools":[{"name":"edit","description":"synthetic edit","input_schema":{"type":"object"}}]}})).await;
    record(&t,&clock,3,"model_response","model:0:response",json!({"response":{"message":{"role":"assistant","content":"editing","tool_calls":[{"id":"call-1","name":"edit","arguments":"{\"path\":\"a.rs\"}"}]},"usage":{"input_tokens":100,"output_tokens":30,"estimated":false}},"metrics":observed()})).await;
    record(&t,&clock,4,"tool_intent","tool:0:0",json!({"index":0,"prepared":{"call":{"id":"call-1","name":"edit","arguments":"{\"path\":\"a.rs\"}"}}})).await;
    let edited=record(&t,&clock,5,"tool_result","tool:0:0:result",json!({"index":0,"name":"edit","duration_ms":35,"result":{"success":true,"output":{"path":"a.rs","diff":"--- a/a.rs\n+++ b/a.rs\n+fn main() {}\n"}}})).await;
    assert_eq!(edited.file_edits.len(), 1);
    assert_eq!(edited.file_edits[0].path.as_deref(), Some("a.rs"));
    record(
        &t,
        &clock,
        6,
        "state_change",
        "paused",
        json!({"state":"paused","reason":"approval"}),
    )
    .await;
    record(
        &t,
        &clock,
        7,
        "directive_received",
        "directive",
        json!({"text":"continue"}),
    )
    .await;
    record(
        &t,
        &clock,
        8,
        "state_change",
        "running",
        json!({"state":"running"}),
    )
    .await;
    record(&t, &clock, 9, "gate_report", "gate", json!({"pending":[]})).await;
    record(
        &t,
        &clock,
        10,
        "state_change",
        "completed",
        json!({"state":"completed"}),
    )
    .await;
    let metrics=t.finalize("run",&json!({"status":"completed","exit_code":0,"spec":{"criteria":[{"kind":"tool_success"}]}})).await.unwrap();
    assert_eq!(metrics.model_calls, 1);
    assert_eq!(metrics.provider_calls, 2);
    assert_eq!(metrics.provider_retries, 1);
    assert_eq!(metrics.cost_microusd, Some(125));
    assert_eq!(metrics.tokens.input_tokens, 100);
    assert_eq!(metrics.tokens.reasoning_tokens, Some(10));
    assert_eq!(metrics.tokens.cache_write_by_ttl["5m"], 20);
    assert_eq!(metrics.cache_hit_rate, Some(0.4));
    assert_eq!(metrics.model_time_ms, Some(60));
    assert_eq!(metrics.tool_time_ms, Some(35));
    assert_eq!(metrics.waiting_user_ms, Some(200));
    assert_eq!(metrics.gate_attempts, 1);
    assert_eq!(metrics.criteria_fulfilled, Some(1));
    assert_eq!(metrics.interventions, 1);
    assert_eq!(metrics.price_tables["synthetic-prices-v1"], "abc");
    let trajectory: Value =
        serde_json::from_slice(&std::fs::read(dir.path().join("trajectory.json")).unwrap())
            .unwrap();
    atif::validate(&trajectory).unwrap();
    assert_eq!(
        trajectory["steps"][1]["observation"]["results"][0]["source_call_id"],
        "call-1"
    );
    assert_eq!(trajectory["final_metrics"]["total_prompt_tokens"], 100);
    let trace: Value =
        serde_json::from_slice(&std::fs::read(dir.path().join("excalibur/otel.json")).unwrap())
            .unwrap();
    otel::validate(&trace).unwrap();
    assert!(
        trace["resourceSpans"][0]["scopeSpans"][0]["spans"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|s| s["attributes"].as_array().unwrap())
            .any(|a| a["key"] == "gen_ai.tool.call.id" && a["value"]["stringValue"] == "call-1")
    );
    assert_eq!(
        trace["resourceSpans"][0]["scopeSpans"][0]["spans"]
            .as_array()
            .unwrap()
            .len(),
        3
    );
    let context: Value =
        serde_json::from_slice(&std::fs::read(dir.path().join("agent-context.json")).unwrap())
            .unwrap();
    assert_eq!(context["n_input_tokens"], 100);
    assert_eq!(context["n_cache_tokens"], 40);
    if let Ok(path) = std::env::var("EXCALIBUR_TELEMETRY_FIXTURES") {
        let path = std::path::Path::new(&path);
        std::fs::create_dir_all(path).unwrap();
        for (name, v) in [
            ("trajectory.json", trajectory),
            ("partial-trajectory.json", partial),
            ("agent-context.json", context),
            ("otel.json", trace),
        ] {
            std::fs::write(path.join(name), serde_json::to_vec_pretty(&v).unwrap()).unwrap();
        }
    }
    t.shutdown().await.unwrap();
}
#[tokio::test]
async fn cursor_reconnect_idempotence_and_all_typed_actions() {
    let dir = tempfile::tempdir().unwrap();
    let (cfg, _) = config(dir.path());
    let t = Telemetry::open(cfg.clone()).await.unwrap();
    let kinds = [
        "run_started",
        "model_request",
        "model_response",
        "tool_intent",
        "tool_result",
        "approval_requested",
        "directive_received",
        "memory_write",
        "compaction",
        "loop_detected",
        "mask_batch",
        "provider_retry",
        "error",
        "file_edit",
        "gate_report",
        "state_change",
    ];
    let mut first = Vec::new();
    for (i, kind) in kinds.iter().enumerate() {
        let payload = if *kind == "file_edit" {
            json!({"path":"synthetic","diff":"+new"})
        } else {
            json!({})
        };
        let e = event(i as u64 + 1, kind, &format!("e{i}"), payload);
        let row = t.record_event("run", &e).await.unwrap();
        assert_ne!(row.kind, EventKind::Other);
        assert_eq!(row.cursor, t.record_event("run", &e).await.unwrap().cursor);
        first.push(row);
    }
    let mut subscription = t.subscribe(first[5].cursor);
    for expected in &first[6..] {
        assert_eq!(&subscription.next().await.unwrap(), expected);
    }
    let mut changed = event(1, "run_started", "e0", json!({"different":true}));
    assert!(t.record_event("run", &changed).await.is_err());
    changed.sequence = 99;
    changed.kind = "checkpoint".into();
    let pending = tokio::spawn(async move { subscription.next().await.unwrap() });
    let last = t.record_event("run", &changed).await.unwrap();
    assert_eq!(pending.await.unwrap().cursor, last.cursor);
    t.shutdown().await.unwrap();
    drop(t);
    let reopened = Telemetry::open(cfg).await.unwrap();
    let rows = reopened.events_after(0, 4096).await.unwrap();
    assert_eq!(rows.len(), 17);
    assert!(rows.windows(2).all(|w| w[0].cursor + 1 == w[1].cursor));
    assert_eq!(
        reopened.events_after(first[8].cursor, 3).await.unwrap(),
        rows[9..12]
    );
    reopened.shutdown().await.unwrap();
}
#[tokio::test]
async fn replay_unknowns_failures_and_external_verifier_remain_honest() {
    let dir = tempfile::tempdir().unwrap();
    let (cfg, clock) = config(dir.path());
    let t = Telemetry::open(cfg).await.unwrap();
    let unknown = event(
        1,
        "model_response",
        "model:0:response",
        json!({"response":{"message":{"role":"assistant","content":"answer","tool_calls":[]},"usage":{"input_tokens":5,"output_tokens":2,"cost_microusd":0,"estimated":true}}}),
    );
    t.record_recovered_event("run", &unknown).await.unwrap();
    let m = t.metrics("run").await.unwrap();
    assert_eq!(m.cost_microusd, None);
    assert_eq!(m.tokens.cache_read_tokens, None);
    assert_eq!(m.elapsed_ms, None);
    let context: Value =
        serde_json::from_slice(&std::fs::read(dir.path().join("agent-context.json")).unwrap())
            .unwrap();
    assert!(context["model_usage"].as_object().unwrap().is_empty());
    assert!(context["n_cache_tokens"].is_null());
    let mut replay = observed();
    replay["provider_calls"] = json!(0);
    replay["usage"] = json!({"input_tokens":0,"output_tokens":0,"cache_read_tokens":0,"cache_write_tokens":0,"reasoning_tokens":0});
    replay["cost_microusd"] = json!(0);
    replay["recorded_usage"] =
        json!({"input_tokens":100,"output_tokens":30,"cache_read_tokens":40});
    replay["recorded_cost_microusd"] = json!(125);
    record(&t,&clock,2,"model_response","model:1:response",json!({"response":{"message":{"role":"assistant","content":"replayed","tool_calls":[]}},"metrics":replay})).await;
    record(
        &t,
        &clock,
        3,
        "model_failure",
        "model:2:failure",
        json!({"metrics":observed(),"failure_class":"server_error"}),
    )
    .await;
    let m = t.metrics("run").await.unwrap();
    assert_eq!(m.model_calls, 3);
    assert_eq!(m.provider_calls, 3);
    assert_eq!(m.tokens.input_tokens, 105);
    assert_eq!(m.recorded_tokens.unwrap().input_tokens, 100);
    assert_eq!(m.recorded_cost_microusd, Some(125));
    assert_eq!(m.errors, 1);
    assert!(
        t.record_verifier("run", Some(f64::NAN), json!({}))
            .await
            .is_err()
    );
    t.record_verifier("run", Some(0.0), json!({"result":"synthetic failure"}))
        .await
        .unwrap();
    let m = t.metrics("run").await.unwrap();
    assert_eq!(m.verifier_reward, Some(0.0));
    assert_eq!(m.verifier_output.unwrap()["result"], "synthetic failure");
    t.shutdown().await.unwrap();
}
#[tokio::test]
async fn secrets_redaction_preserves_json_and_replay_identity() {
    let dir = tempfile::tempdir().unwrap();
    let (mut cfg, _) = config(dir.path());
    let redactor = Redactor::default();
    let secret = SecretValue::new(b"synthetic-secret-0123456789".to_vec()).unwrap();
    redactor.register_secret(&secret);
    cfg.redactor = redactor.clone();
    let t = Telemetry::open(cfg).await.unwrap();
    let e = event(
        1,
        "checkpoint",
        "safe",
        json!({"nested":{"api_key":"credential-value","text":"synthetic-secret-0123456789"},"note":"later-registered-secret"}),
    );
    t.record_event("run", &e).await.unwrap();
    let late = SecretValue::new(b"later-registered-secret".to_vec()).unwrap();
    redactor.register_secret(&late);
    assert!(t.record_event("run", &e).await.is_ok());
    let events = t.events_after(0, 10).await.unwrap();
    let text = serde_json::to_string(&events).unwrap();
    assert!(!text.contains("synthetic-secret-0123456789"));
    assert!(!text.contains("later-registered-secret"));
    assert!(!text.contains("credential-value"));
    for file in [
        "events.jsonl",
        "journal.jsonl",
        "metrics.json",
        "trajectory.json",
        "agent-context.json",
        "otel.json",
    ] {
        let bytes = std::fs::read(dir.path().join(file)).unwrap();
        let text = String::from_utf8(bytes).unwrap();
        assert!(!text.contains("synthetic-secret-0123456789"), "{file}");
        assert!(!text.contains("credential-value"), "{file}");
        if file.ends_with(".json") {
            serde_json::from_str::<Value>(&text).unwrap();
        } else {
            for line in text.lines() {
                serde_json::from_str::<Value>(line).unwrap();
            }
        }
    }
    t.shutdown().await.unwrap();
}
#[tokio::test]
async fn concurrent_samples_and_runs_do_not_share_metrics() {
    let dir = tempfile::tempdir().unwrap();
    let (cfg, _) = config(dir.path());
    let t = Telemetry::open(cfg).await.unwrap();
    t.record_event("a", &event(1, "run_started", "seed", json!({})))
        .await
        .unwrap();
    t.record_event("b", &event(1, "run_started", "seed", json!({})))
        .await
        .unwrap();
    let mut handles = Vec::new();
    for _ in 0..8 {
        let t = t.clone();
        handles.push(tokio::spawn(async move {
            t.sample_resources("a").await.unwrap();
        }));
    }
    for h in handles {
        h.await.unwrap();
    }
    let rows = t.events_after(0, 100).await.unwrap();
    let samples = rows
        .iter()
        .filter(|e| e.source == "resource")
        .collect::<Vec<_>>();
    assert_eq!(samples.len(), 8);
    let mut seq = samples.iter().map(|e| e.sequence).collect::<Vec<_>>();
    seq.sort();
    assert_eq!(seq, (1..=8).collect::<Vec<_>>());
    assert_eq!(t.metrics("a").await.unwrap().resources.sample_count, 8);
    assert_eq!(t.metrics("b").await.unwrap().resources.sample_count, 0);
    t.shutdown().await.unwrap();
}
struct Broken {
    left: usize,
    bytes: Vec<u8>,
}
impl Write for Broken {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if self.left == 0 {
            return Err(io::Error::other("synthetic broken pipe"));
        }
        let n = buf.len().min(self.left);
        self.bytes.extend_from_slice(&buf[..n]);
        self.left -= n;
        Ok(n)
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
#[tokio::test]
async fn jsonl_partial_write_keeps_last_confirmed_cursor() {
    let dir = tempfile::tempdir().unwrap();
    let (cfg, _) = config(dir.path());
    let t = Telemetry::open(cfg).await.unwrap();
    let a = t
        .record_event("run", &event(1, "checkpoint", "one", json!({})))
        .await
        .unwrap();
    let b = t
        .record_event("run", &event(2, "checkpoint", "two", json!({})))
        .await
        .unwrap();
    let n = serde_json::to_vec(&a).unwrap().len() + 1;
    let mut sink = JsonlSink::new(Broken {
        left: n + 12,
        bytes: Vec::new(),
    });
    assert!(sink.write_event(&a).unwrap());
    assert!(!sink.write_event(&a).unwrap());
    assert!(sink.write_event(&b).is_err());
    assert_eq!(sink.cursor(), a.cursor);
    assert!(sink.write_event(&b).is_err());
    let prefix = decode_jsonl_prefix(&sink.into_inner().bytes).unwrap();
    assert_eq!(prefix, vec![a.clone()]);
    let mut reconnect = JsonlSink::after(Vec::new(), a.cursor);
    assert!(reconnect.write_event(&b).unwrap());
    assert_eq!(
        decode_jsonl_prefix(&reconnect.into_inner()).unwrap(),
        vec![b]
    );
    t.shutdown().await.unwrap();
}
#[test]
fn pinned_fixtures_and_relational_validation() {
    let directory = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures");
    for name in [
        "trajectory.json",
        "partial-trajectory.json",
        "pending-trajectory.json",
    ] {
        let path = directory.join(name);
        if !path.exists() {
            continue;
        }
        let value: Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
        atif::validate(&value).unwrap();
        let mut invalid = value.clone();
        invalid["steps"][0]["step_id"] = json!(9);
        assert!(atif::validate(&invalid).is_err());
        let mut invalid = value;
        invalid["steps"][0]["metrics"] = json!({});
        assert!(atif::validate(&invalid).is_err());
    }
}
#[tokio::test]
async fn subscriber_shutdown_and_path_segment_rejection() {
    let dir = tempfile::tempdir().unwrap();
    let (cfg, _) = config(dir.path());
    let t = Telemetry::open(cfg).await.unwrap();
    for id in [".", "..", "../escape", "slash/id"] {
        assert!(
            t.record_event(id, &event(1, "checkpoint", "safe", json!({})))
                .await
                .is_err()
        );
    }
    let mut subscriber = t.subscribe(0);
    let pending = tokio::spawn(async move { subscriber.next().await });
    t.shutdown().await.unwrap();
    assert!(
        tokio::time::timeout(std::time::Duration::from_secs(5), pending)
            .await
            .unwrap()
            .unwrap()
            .is_err()
    );
}
#[tokio::test]
async fn dispatched_model_without_outcome_is_pending_across_restart_then_resolves() {
    let dir = tempfile::tempdir().unwrap();
    let (cfg, clock) = config(dir.path());
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
        "model_request",
        "model:0:request",
        json!({"request":{}}),
    )
    .await;
    let dispatched = record(
        &t,
        &clock,
        3,
        "model_dispatch",
        "model:0:dispatch",
        json!({"metadata":observed()["metadata"]}),
    )
    .await;
    assert_eq!(dispatched.kind, EventKind::ModelDispatched);
    let assert_pending = |m: excalibur_telemetry::TaskMetrics| {
        assert_eq!(m.pending_model_calls, 1);
        assert_eq!(m.model_calls, 1);
        assert_eq!(m.provider_calls, 0);
        assert!(!m.provider_calls_known);
        assert!(!m.usage_known);
        assert_eq!(m.tokens.input_tokens, 0);
        assert_eq!(m.tokens.cache_read_tokens, None);
        assert_eq!(m.cost_microusd, None);
        assert_eq!(m.model_time_ms, None);
        assert_eq!(m.model_usage["synthetic-v1"].pending_calls, 1);
        assert_eq!(m.model_calls_by_role["coordinator"], 1);
    };
    assert_pending(t.metrics("run").await.unwrap());
    let trajectory: Value =
        serde_json::from_slice(&std::fs::read(dir.path().join("trajectory.json")).unwrap())
            .unwrap();
    atif::validate(&trajectory).unwrap();
    assert!(trajectory["final_metrics"]["total_prompt_tokens"].is_null());
    assert_eq!(
        trajectory["final_metrics"]["extra"]["pending_model_calls"],
        1
    );
    let context: Value =
        serde_json::from_slice(&std::fs::read(dir.path().join("agent-context.json")).unwrap())
            .unwrap();
    assert!(context["n_input_tokens"].is_null());
    assert!(context["cost_usd"].is_null());
    if let Ok(path) = std::env::var("EXCALIBUR_TELEMETRY_FIXTURES") {
        let path = std::path::Path::new(&path);
        std::fs::create_dir_all(path).unwrap();
        std::fs::write(
            path.join("pending-trajectory.json"),
            serde_json::to_vec_pretty(&trajectory).unwrap(),
        )
        .unwrap();
        std::fs::write(
            path.join("pending-agent-context.json"),
            serde_json::to_vec_pretty(&context).unwrap(),
        )
        .unwrap();
    }
    t.shutdown().await.unwrap();
    drop(t);
    let t = Telemetry::open(cfg).await.unwrap();
    assert_pending(t.metrics("run").await.unwrap());
    record(&t,&clock,4,"model_response","model:0:response",json!({"response":{"message":{"role":"assistant","content":"done","tool_calls":[]}},"metrics":observed()})).await;
    let m = t.metrics("run").await.unwrap();
    assert_eq!(m.pending_model_calls, 0);
    assert_eq!(m.model_calls, 1);
    assert_eq!(m.provider_calls, 2);
    assert!(m.provider_calls_known);
    assert!(m.usage_known);
    assert_eq!(m.cost_microusd, Some(125));
    assert_eq!(m.model_time_ms, Some(60));
    assert_eq!(m.model_usage["synthetic-v1"].pending_calls, 0);
    record(
        &t,
        &clock,
        5,
        "model_request",
        "model:1:request",
        json!({"request":{}}),
    )
    .await;
    record(
        &t,
        &clock,
        6,
        "model_dispatch",
        "model:1:dispatch",
        json!({"metadata":observed()["metadata"]}),
    )
    .await;
    let m = t.metrics("run").await.unwrap();
    assert_eq!(m.pending_model_calls, 1);
    assert_eq!(m.model_calls, 2);
    assert_eq!(m.tokens.input_tokens, 100);
    assert_eq!(m.provider_calls, 2);
    assert!(!m.provider_calls_known);
    assert_eq!(m.cost_microusd, None);
    record(
        &t,
        &clock,
        7,
        "model_failure",
        "model:1:failure",
        json!({"metrics":observed(),"failure_class":"quota"}),
    )
    .await;
    let m = t.metrics("run").await.unwrap();
    assert_eq!(m.pending_model_calls, 0);
    assert_eq!(m.model_calls, 2);
    assert_eq!(m.provider_calls, 4);
    assert_eq!(m.cost_microusd, Some(250));
    assert_eq!(m.failure_class.as_deref(), Some("quota"));
    assert_eq!(m.model_time_ms, Some(120));
    t.shutdown().await.unwrap();
}
#[tokio::test]
async fn preflight_request_without_dispatch_has_zero_calls_only_with_versioned_protocol() {
    let dir = tempfile::tempdir().unwrap();
    let (cfg, clock) = config(dir.path());
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
        "model_request",
        "model:0:request",
        json!({"request":{}}),
    )
    .await;
    record(
        &t,
        &clock,
        3,
        "state_change",
        "budget",
        json!({"state":"paused","reason":"token budget"}),
    )
    .await;
    let m = t.metrics("run").await.unwrap();
    assert_eq!(m.model_calls, 0);
    assert_eq!(m.pending_model_calls, 0);
    assert_eq!(m.provider_calls, 0);
    assert!(m.provider_calls_known);
    assert!(m.usage_known);
    assert_eq!(m.cost_microusd, Some(0));
    assert_eq!(m.model_time_ms, Some(0));
    t.record_event("legacy", &event(1, "run_started", "seed", json!({})))
        .await
        .unwrap();
    t.record_event(
        "legacy",
        &event(2, "model_request", "model:0:request", json!({"request":{}})),
    )
    .await
    .unwrap();
    let m = t.metrics("legacy").await.unwrap();
    assert_eq!(m.pending_model_calls, 1);
    assert_eq!(m.model_calls, 1);
    assert!(!m.provider_calls_known);
    assert!(!m.usage_known);
    assert_eq!(m.cost_microusd, None);
    assert!(m.limitations.iter().any(|s| s.contains("legacy")));
    // Recovered dispatch timing cannot be measured by a live outcome's timestamp.
    t.record_event(
        "recovered",
        &event(
            1,
            "run_started",
            "seed",
            json!({"model_dispatch_protocol":1}),
        ),
    )
    .await
    .unwrap();
    t.record_event(
        "recovered",
        &event(2, "model_request", "model:0:request", json!({"request":{}})),
    )
    .await
    .unwrap();
    t.record_recovered_event(
        "recovered",
        &event(
            3,
            "model_dispatch",
            "model:0:dispatch",
            json!({"metadata":observed()["metadata"]}),
        ),
    )
    .await
    .unwrap();
    let mut no_duration = observed();
    no_duration.as_object_mut().unwrap().remove("duration_ms");
    t.record_event("recovered",&event(4,"model_response","model:0:response",json!({"response":{"message":{"role":"assistant","content":"done","tool_calls":[]}},"metrics":no_duration}))).await.unwrap();
    let m = t.metrics("recovered").await.unwrap();
    assert_eq!(m.pending_model_calls, 0);
    assert_eq!(m.model_calls, 1);
    assert_eq!(m.model_time_ms, None);
    assert_eq!(m.elapsed_ms, None);
    t.shutdown().await.unwrap();
}
