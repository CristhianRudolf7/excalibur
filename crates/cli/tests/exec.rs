use serde_json::{Value, json};
use std::{
    io::{Read, Write},
    net::TcpListener,
    process::Command,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    thread,
};
fn server(responses: Vec<(u16, Value)>) -> (String, Arc<AtomicUsize>, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let count = calls.clone();
    let worker = thread::spawn(move || {
        for (status, payload) in responses {
            let (mut socket, _) = listener.accept().unwrap();
            socket
                .set_read_timeout(Some(std::time::Duration::from_secs(20)))
                .unwrap();
            let mut buf = Vec::new();
            let mut bytes = [0; 4096];
            let header_end = loop {
                let n = socket.read(&mut bytes).unwrap();
                assert!(n > 0);
                buf.extend_from_slice(&bytes[..n]);
                if let Some(at) = buf.windows(4).position(|v| v == b"\r\n\r\n") {
                    break at + 4;
                }
            };
            let headers = String::from_utf8_lossy(&buf[..header_end]);
            let length = headers
                .lines()
                .find_map(|line| {
                    line.to_ascii_lowercase()
                        .strip_prefix("content-length:")
                        .map(|v| v.trim().parse::<usize>().unwrap())
                })
                .unwrap_or(0);
            while buf.len() < header_end + length {
                let n = socket.read(&mut bytes).unwrap();
                assert!(n > 0);
                buf.extend_from_slice(&bytes[..n]);
            }
            let request: Value =
                serde_json::from_slice(&buf[header_end..header_end + length]).unwrap();
            assert_eq!(request["model"], "synthetic-model");
            count.fetch_add(1, Ordering::SeqCst);
            let body = payload.to_string();
            write!(socket,"HTTP/1.1 {status} Fixture\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len()).unwrap();
        }
    });
    (format!("http://{address}/v1/"), calls, worker)
}
fn response(message: Value) -> Value {
    json!({"choices":[{"message":message,"finish_reason":"stop"}],"usage":{"prompt_tokens":10,"completion_tokens":5,"prompt_tokens_details":{"cached_tokens":4},"completion_tokens_details":{"reasoning_tokens":2}}})
}
fn command(root: &std::path::Path, logs: &std::path::Path) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_excalibur"));
    cmd.args([
        "exec",
        "Create result.txt then return JSON",
        "--json",
        "--project",
    ])
    .arg(root)
    .arg("--logs-dir")
    .arg(logs)
    .env_remove("EXCALIBUR_MODEL")
    .env_remove("EXCALIBUR_BASE_URL")
    .env_remove("EXCALIBUR_API_KEY");
    cmd
}
#[test]
fn cli_creates_a_file_emits_jsonl_and_produces_valid_evaluation_artifacts_without_real_services() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("project");
    std::fs::create_dir(&root).unwrap();
    let logs = temp.path().join("logs");
    let schema = temp.path().join("schema.json");
    std::fs::write(&schema,json!({"type":"object","required":["done"],"properties":{"done":{"type":"boolean"}},"additionalProperties":false}).to_string()).unwrap();
    let (first, second) = (
        response(
            json!({"role":"assistant","content":"","tool_calls":[{"id":"w1","type":"function","function":{"name":"write_file","arguments":"{\"path\":\"result.txt\",\"content\":\"synthetic result\\n\"}"}}]}),
        ),
        response(json!({"role":"assistant","content":"{\"done\":true}"})),
    );
    let (endpoint, calls, worker) = server(vec![(200, first), (200, second)]);
    let output = command(&root, &logs)
        .args([
            "--base-url",
            &endpoint,
            "--model",
            "synthetic-model",
            "--output-schema",
        ])
        .arg(&schema)
        .env(
            "EXCALIBUR_API_KEY",
            "synthetic-provider-secret-for-local-mock",
        )
        .output()
        .unwrap();
    assert_eq!(
        output.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    worker.join().unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert_eq!(
        std::fs::read_to_string(root.join("result.txt")).unwrap(),
        "synthetic result\n"
    );
    let records = String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .collect::<Vec<_>>();
    assert!(records.len() > 3);
    assert!(
        records.iter().any(|e| e["file_edits"]
            .as_array()
            .is_some_and(|edits| edits.iter().any(|edit| edit["path"] == "result.txt"
                && edit["diff"]
                    .as_str()
                    .is_some_and(|d| d.contains("+synthetic result")))))
    );
    assert_eq!(records.last().unwrap()["type"], "result");
    assert_eq!(records.last().unwrap()["result"]["exit_code"], 0);
    let metrics: Value =
        serde_json::from_slice(&std::fs::read(logs.join("excalibur/metrics.json")).unwrap())
            .unwrap();
    assert_eq!(metrics["tokens"]["input_tokens"], 20);
    assert_eq!(metrics["tokens"]["cache_read_tokens"], 8);
    assert_eq!(metrics["tokens"]["reasoning_tokens"], 4);
    assert_eq!(metrics["provider_calls"], 2);
    assert!(metrics["cost_microusd"].is_null());
    for file in [
        logs.join("trajectory.json"),
        logs.join("agent-context.json"),
        logs.join("excalibur/metrics.json"),
        logs.join("excalibur/events.jsonl"),
        logs.join("excalibur/metadata.json"),
    ] {
        assert!(
            !std::fs::read_to_string(file)
                .unwrap()
                .contains("synthetic-provider-secret-for-local-mock")
        );
    }
}
#[test]
fn missing_model_blocks_without_reading_a_provider_secret_or_starting_an_inference() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("project");
    std::fs::create_dir(&root).unwrap();
    let logs = temp.path().join("logs");
    let out = command(&root, &logs).output().unwrap();
    assert_eq!(
        out.status.code(),
        Some(3),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let last = String::from_utf8(out.stdout)
        .unwrap()
        .lines()
        .last()
        .unwrap()
        .to_owned();
    assert_eq!(
        serde_json::from_str::<Value>(&last).unwrap()["result"]["exit_code"],
        3
    );
    assert!(logs.join("trajectory.json").exists());
}
#[test]
fn provider_authentication_failure_has_exit_5_and_classified_artifacts() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("project");
    std::fs::create_dir(&root).unwrap();
    let logs = temp.path().join("logs");
    let (endpoint, _, worker) = server(vec![(
        401,
        json!({"error":{"message":"should not be copied verbatim"}}),
    )]);
    let out = command(&root, &logs)
        .args(["--base-url", &endpoint, "--model", "synthetic-model"])
        .env("EXCALIBUR_API_KEY", "synthetic-auth-test-secret")
        .output()
        .unwrap();
    assert_eq!(
        out.status.code(),
        Some(5),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    worker.join().unwrap();
    let events = std::fs::read_to_string(logs.join("excalibur/events.jsonl")).unwrap();
    assert!(events.contains("authentication"));
    let failure: Value =
        serde_json::from_slice(&std::fs::read(logs.join("excalibur/failure.json")).unwrap())
            .unwrap();
    assert_eq!(failure["exception_type"], "AgentAuthenticationError");
    assert!(!events.contains("should not be copied verbatim"));
    assert!(!events.contains("synthetic-auth-test-secret"));
}
#[test]
fn argument_validation_rejects_unknown_flags_and_fractional_budget_overflow() {
    for args in [
        vec!["exec", "task", "--not-a-real-option", "value"],
        vec!["exec", "task", "--budget-usd", "0.0000001"],
    ] {
        let out = Command::new(env!("CARGO_BIN_EXE_excalibur"))
            .args(args)
            .output()
            .unwrap();
        assert_eq!(out.status.code(), Some(1));
    }
    assert_eq!(excalibur_cli::parse_usd("1.234567").unwrap(), 1_234_567);
    assert!(excalibur_cli::parse_usd("-1").is_err());
    assert!(excalibur_cli::parse_usd("NaN").is_err());
}
#[test]
fn token_reservation_pauses_before_any_http_connection() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("project");
    std::fs::create_dir(&root).unwrap();
    let logs = temp.path().join("logs");
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let url = format!("http://{}/v1/", listener.local_addr().unwrap());
    let out = command(&root, &logs)
        .args([
            "--base-url",
            &url,
            "--model",
            "synthetic-model",
            "--max-tokens",
            "1",
        ])
        .env("EXCALIBUR_API_KEY", "synthetic-token-reservation-key")
        .output()
        .unwrap();
    assert_eq!(
        out.status.code(),
        Some(4),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        listener.accept().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );
}
#[test]
fn pending_backend_criterion_has_exit_2_after_bounded_completion_gate() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("project");
    std::fs::create_dir(&root).unwrap();
    let logs = temp.path().join("logs");
    let criterion = temp.path().join("criterion.json");
    std::fs::write(
        &criterion,
        json!({"kind":"tool_success","name":"never_called"}).to_string(),
    )
    .unwrap();
    let final_reply = response(json!({"role":"assistant","content":"done"}));
    let (endpoint, calls, worker) = server(vec![(200, final_reply.clone()), (200, final_reply)]);
    let out = command(&root, &logs)
        .args([
            "--base-url",
            &endpoint,
            "--model",
            "synthetic-model",
            "--criterion",
        ])
        .arg(criterion)
        .env("EXCALIBUR_API_KEY", "synthetic-pending-test-key")
        .output()
        .unwrap();
    assert_eq!(
        out.status.code(),
        Some(2),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    worker.join().unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    let result = String::from_utf8(out.stdout)
        .unwrap()
        .lines()
        .last()
        .unwrap()
        .to_owned();
    assert_eq!(
        serde_json::from_str::<Value>(&result).unwrap()["result"]["status"],
        "completed_with_pending"
    );
}
#[test]
fn replay_cli_uses_no_provider_and_stops_at_the_first_changed_request() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("project");
    std::fs::create_dir(&root).unwrap();
    let logs = temp.path().join("recorded");
    let (endpoint, calls, worker) = server(vec![(
        200,
        response(json!({"role":"assistant","content":"recorded final answer"})),
    )]);
    let out = command(&root, &logs)
        .args([
            "--base-url",
            &endpoint,
            "--model",
            "synthetic-model",
            "--seed",
            "42",
            "--temperature",
            "0",
        ])
        .env("EXCALIBUR_API_KEY", "synthetic-recording-key")
        .output()
        .unwrap();
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    worker.join().unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    let replay = temp.path().join("replayed");
    let out = command(&root, &replay)
        .arg("--replay")
        .arg(logs.join("excalibur/replay.json"))
        .output()
        .unwrap();
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let metrics: Value =
        serde_json::from_slice(&std::fs::read(replay.join("excalibur/metrics.json")).unwrap())
            .unwrap();
    assert_eq!(metrics["provider_calls"], 0);
    assert_eq!(metrics["cost_microusd"], 0);
    assert_eq!(metrics["tokens"]["input_tokens"], 0);
    assert_eq!(metrics["recorded_tokens"]["input_tokens"], 10);
    let changed_sampling = temp.path().join("changed-sampling");
    let out = command(&root, &changed_sampling)
        .arg("--replay")
        .arg(logs.join("excalibur/replay.json"))
        .args(["--seed", "43"])
        .output()
        .unwrap();
    assert_eq!(
        out.status.code(),
        Some(5),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let divergence = temp.path().join("diverged");
    let out = command(&root, &divergence)
        .arg("--replay")
        .arg(logs.join("excalibur/replay.json"))
        .arg("a changed requirement")
        .output()
        .unwrap();
    assert_eq!(
        out.status.code(),
        Some(5),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let events = std::fs::read_to_string(divergence.join("excalibur/events.jsonl")).unwrap();
    assert!(events.contains("replay_divergence"));
    assert!(!events.contains("synthetic-recording-key"));
}
#[cfg(target_os = "linux")]
#[test]
fn eval_cli_uses_the_declared_kernel_profile_and_does_not_inherit_the_provider_key() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("project");
    std::fs::create_dir(&root).unwrap();
    let logs = temp.path().join("eval");
    let cmd =
        "mkdir -p nested && printf kernel-eval >nested/result && ! printenv EXCALIBUR_API_KEY";
    let tool = response(
        json!({"role":"assistant","content":"","tool_calls":[{"id":"exec1","type":"function","function":{"name":"exec","arguments":json!({"cmd":cmd,"yield_ms":30000}).to_string()}}]}),
    );
    let final_reply = response(json!({"role":"assistant","content":"done"}));
    let (endpoint, _, worker) = server(vec![(200, tool), (200, final_reply)]);
    let out = command(&root, &logs)
        .args([
            "--base-url",
            &endpoint,
            "--model",
            "synthetic-model",
            "--profile",
            "eval",
            "--container-runtime",
            "synthetic-host-declaration",
        ])
        .env("EXCALIBUR_EVAL_CONTAINER", "1")
        .env(
            "EXCALIBUR_API_KEY",
            "synthetic-secret-must-not-reach-processes",
        )
        .output()
        .unwrap();
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    worker.join().unwrap();
    assert_eq!(
        std::fs::read_to_string(root.join("nested/result")).unwrap(),
        "kernel-eval"
    );
    let events = std::fs::read_to_string(logs.join("excalibur/events.jsonl")).unwrap();
    let journal = std::fs::read_to_string(logs.join("excalibur/journal.jsonl")).unwrap();
    let exec_result = journal
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .find(|e| e["kind"] == "tool_result" && e["payload"]["name"] == "exec")
        .unwrap();
    assert_eq!(
        exec_result["payload"]["result"]["output"]["exit_code"], 0,
        "{exec_result}"
    );
    assert!(!events.contains("synthetic-secret-must-not-reach-processes"));
    let metadata: Value =
        serde_json::from_slice(&std::fs::read(logs.join("excalibur/metadata.json")).unwrap())
            .unwrap();
    assert_eq!(metadata["profile_version"], "eval@1");
    assert_eq!(
        metadata["extra"]["container_attestation"]["container_engine_independently_verified"],
        false
    );
}
#[cfg(unix)]
#[test]
fn sigterm_during_a_confirmed_http_request_cancels_with_exit_130_and_partial_logs() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("project");
    std::fs::create_dir(&root).unwrap();
    let logs = temp.path().join("signals");
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let endpoint = format!("http://{}/v1/", listener.local_addr().unwrap());
    let (ready, received) = std::sync::mpsc::channel();
    let (release, released) = std::sync::mpsc::channel();
    let worker = thread::spawn(move || {
        let (mut socket, _) = listener.accept().unwrap();
        let mut bytes = [0; 4096];
        assert!(socket.read(&mut bytes).unwrap() > 0);
        ready.send(()).unwrap();
        let _ = released.recv_timeout(std::time::Duration::from_secs(20));
    });
    let child = command(&root, &logs)
        .args(["--base-url", &endpoint, "--model", "synthetic-model"])
        .env("EXCALIBUR_API_KEY", "synthetic-signal-test-key")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    received
        .recv_timeout(std::time::Duration::from_secs(20))
        .unwrap();
    assert!(
        Command::new("kill")
            .args(["-TERM", &child.id().to_string()])
            .status()
            .unwrap()
            .success()
    );
    let out = child.wait_with_output().unwrap();
    release.send(()).unwrap();
    worker.join().unwrap();
    assert_eq!(
        out.status.code(),
        Some(130),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let metrics: Value =
        serde_json::from_slice(&std::fs::read(logs.join("excalibur/metrics.json")).unwrap())
            .unwrap();
    assert_eq!(metrics["exit_code"], 130);
    assert_eq!(metrics["pending_model_calls"], 1);
    assert_eq!(metrics["provider_calls_known"], false);
    assert_eq!(metrics["usage_known"], false);
    assert!(metrics["cost_microusd"].is_null());
    assert_eq!(metrics["final_state"], "cancelled");
    assert!(logs.join("trajectory.json").exists());
}
