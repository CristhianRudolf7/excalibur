#![cfg(unix)]
use excalibur_journal::Journal;
use excalibur_store::{Database, DatabaseKind, StorageConfig};
use excalibur_telemetry::{RunMetadata, Telemetry, TelemetryConfig, atif};
use excalibur_vault::Redactor;
use serde_json::{Value, json};
use std::{
    io::{BufRead, BufReader, Write},
    process::{Command, Stdio},
    sync::Arc,
};
fn config(path: &std::path::Path) -> TelemetryConfig {
    let mut c = TelemetryConfig::new(
        path.join("artifacts/excalibur"),
        RunMetadata {
            instruction: "synthetic interruption".into(),
            ..Default::default()
        },
        Redactor::default(),
    );
    c.artifact_root = Some(path.join("artifacts"));
    c.clock = Arc::new(|| 1_700_000_000_000);
    c
}
#[test]
#[ignore = "subprocess fixture invoked by sigkill test"]
fn interrupted_export_child() {
    let Some(root) = std::env::var_os("EXCALIBUR_TELEMETRY_CRASH_ROOT") else {
        return;
    };
    let path = std::path::PathBuf::from(root);
    let runtime = tokio::runtime::Runtime::new().unwrap();
    runtime.block_on(async{
        let main=Database::open(path.join("main.db"),DatabaseKind::Main,StorageConfig::default()).unwrap();let journal=Journal::new(main).await.unwrap();let run=journal.create_run("synthetic-crash",json!({"started_at":1_700_000_000})).await.unwrap();let telemetry=Telemetry::open(config(&path)).await.unwrap();telemetry.reconcile(&journal,&run).await.unwrap();
        journal.append(&run,"model_request","model:0:request",json!({"request":{"tools":[]}})).await.unwrap();for e in journal.replay(&run,1).await.unwrap(){telemetry.record_event(&run,&e).await.unwrap();}
        // This source event is durable but has not reached the observer. A kill
        // exercises the precise journal-to-telemetry recovery gap.
        journal.append(&run,"model_response","model:0:response",json!({"response":{"message":{"role":"assistant","content":"durable before interruption","tool_calls":[]},"usage":{"input_tokens":11,"output_tokens":4,"estimated":false}}})).await.unwrap();
        println!("TELEMETRY_READY {run}");std::io::stdout().flush().unwrap();std::future::pending::<()>().await;
    });
}
#[tokio::test]
async fn sigkill_preserves_valid_partial_artifacts_and_backfills_exactly_once() {
    let dir = tempfile::tempdir().unwrap();
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "interrupted_export_child",
            "--ignored",
            "--nocapture",
        ])
        .env("EXCALIBUR_TELEMETRY_CRASH_ROOT", dir.path())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap();
    let reader = BufReader::new(child.stdout.take().unwrap());
    let run = reader
        .lines()
        .find_map(|l| {
            l.unwrap()
                .split("TELEMETRY_READY ")
                .nth(1)
                .map(str::to_owned)
        })
        .expect("child ready signal");
    assert_eq!(unsafe { libc::kill(child.id() as i32, libc::SIGKILL) }, 0);
    let status = child.wait().unwrap();
    use std::os::unix::process::ExitStatusExt;
    assert_eq!(status.signal(), Some(libc::SIGKILL));
    let root = dir.path().join("artifacts");
    let partial: Value =
        serde_json::from_slice(&std::fs::read(root.join("trajectory.json")).unwrap()).unwrap();
    atif::validate(&partial).unwrap();
    assert_eq!(partial["extra"]["finalized"], false);
    for name in ["events.jsonl", "journal.jsonl"] {
        let text = std::fs::read_to_string(root.join("excalibur").join(name)).unwrap();
        assert_eq!(text.lines().count(), 2);
        for line in text.lines() {
            serde_json::from_str::<Value>(line).unwrap();
        }
    }
    let main = Database::open(
        dir.path().join("main.db"),
        DatabaseKind::Main,
        StorageConfig::default(),
    )
    .unwrap();
    let journal = Journal::new(main).await.unwrap();
    let telemetry = Telemetry::open(config(dir.path())).await.unwrap();
    telemetry.reconcile(&journal, &run).await.unwrap();
    telemetry.reconcile(&journal, &run).await.unwrap();
    let rows = telemetry.events_after(0, 100).await.unwrap();
    assert_eq!(rows.len(), 3);
    assert_eq!(
        rows.iter()
            .filter(|e| e.journal_kind == "model_response")
            .count(),
        1
    );
    assert!(rows[2].recovered);
    let metrics = telemetry.metrics(&run).await.unwrap();
    assert_eq!(metrics.model_calls, 1);
    assert_eq!(metrics.tokens.input_tokens, 11);
    assert_eq!(metrics.elapsed_ms, None);
    let repaired: Value =
        serde_json::from_slice(&std::fs::read(root.join("trajectory.json")).unwrap()).unwrap();
    atif::validate(&repaired).unwrap();
    assert_eq!(
        repaired["steps"][1]["message"],
        "durable before interruption"
    );
    telemetry.shutdown().await.unwrap();
    journal.database().shutdown().await.unwrap();
}
