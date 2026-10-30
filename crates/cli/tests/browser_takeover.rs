//! CLI `browser takeover` plumbing over a local synthetic webd socket.
//!
//! A fake Unix-socket server speaks the newline-delimited JSON-RPC framing
//! webd uses; no daemon, no Chrome, no network.
use std::{
    io::{BufRead, BufReader, Write},
    os::unix::net::UnixListener,
    process::Command,
    thread,
};

use serde_json::{Value, json};

fn fake_webd(socket: std::path::PathBuf, replies: Vec<Value>) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        let listener = UnixListener::bind(&socket).unwrap();
        for reply in replies {
            let (stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(std::time::Duration::from_secs(20)))
                .unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            let request: Value = serde_json::from_str(&line).unwrap();
            assert_eq!(request["jsonrpc"], json!("2.0"));
            let envelope = json!({
                "jsonrpc": "2.0",
                "id": request["id"],
                "result": {"state": "completed", "result": reply},
            });
            let mut stream = stream;
            stream
                .write_all(format!("{envelope}\n").as_bytes())
                .unwrap();
        }
    })
}

fn command(socket: &std::path::Path) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_excalibur"));
    cmd.arg("browser")
        .arg("takeover")
        .arg("s-1")
        .arg("--socket")
        .arg(socket)
        .arg("--run")
        .arg("run-cli")
        .env_remove("EXCALIBUR_WEB_SOCKET");
    cmd
}

#[test]
fn browser_takeover_prints_takeover_state_as_json() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("webd.sock");
    let server = fake_webd(
        socket.clone(),
        vec![json!({"session_id": "s-1", "state": "human_control", "started_at": 7})],
    );
    let output = command(&socket).arg("--json").output().unwrap();
    assert_eq!(
        output.status.code(),
        Some(0),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let parsed: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(parsed["session_id"], json!("s-1"));
    assert_eq!(
        parsed["takeover"]["result"]["state"],
        json!("human_control")
    );
    server.join().unwrap();
}

#[test]
fn browser_takeover_with_tab_issues_a_viewer_code() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("webd.sock");
    let server = fake_webd(
        socket.clone(),
        vec![
            json!({"session_id": "s-1", "state": "human_control", "started_at": 7}),
            json!({"tab_id": "t-1", "code": "ABCDEFGH", "expires_at": 607}),
        ],
    );
    let output = command(&socket)
        .arg("--tab")
        .arg("t-1")
        .arg("--json")
        .output()
        .unwrap();
    assert_eq!(
        output.status.code(),
        Some(0),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let parsed: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(parsed["viewer"]["result"]["code"], json!("ABCDEFGH"));
    server.join().unwrap();
}

#[test]
fn browser_takeover_without_socket_fails_with_usage() {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_excalibur"));
    cmd.arg("browser")
        .arg("takeover")
        .arg("s-1")
        .arg("--run")
        .arg("run-cli")
        .env_remove("EXCALIBUR_WEB_SOCKET");
    let output = cmd.output().unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&output.stderr).contains("EXCALIBUR_WEB_SOCKET"));
}

#[test]
fn browser_takeover_reports_daemon_errors_without_new_auth() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("webd.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let server = thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(std::time::Duration::from_secs(20)))
            .unwrap();
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        let request: Value = serde_json::from_str(&line).unwrap();
        let envelope = json!({
            "jsonrpc": "2.0",
            "id": request["id"],
            "error": {"code": -32000, "message": "denied",
                      "data": {"kind": "denied", "detail": "session_owner"}},
        });
        let mut stream = stream;
        stream
            .write_all(format!("{envelope}\n").as_bytes())
            .unwrap();
    });
    let output = command(&socket).arg("--json").output().unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&output.stderr).contains("denied"));
    server.join().unwrap();
}
