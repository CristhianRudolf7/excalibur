#![cfg(unix)]
use serde_json::{Value, json};
use std::{
    io::{Read, Write},
    net::{SocketAddr, TcpListener, TcpStream},
    path::Path,
    process::{Command, Stdio},
    time::{Duration, Instant},
};
struct Daemon(std::process::Child);
impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
const TOKEN: &str = "synthetic-startup-recovery-token-000000";
fn rpc(address: SocketAddr, method: &str, params: Value) -> std::io::Result<Value> {
    let body = json!({"jsonrpc":"2.0","id":1,"method":method,"params":params}).to_string();
    let mut socket = TcpStream::connect_timeout(&address, Duration::from_secs(2))?;
    socket.set_read_timeout(Some(Duration::from_secs(3)))?;
    socket.set_write_timeout(Some(Duration::from_secs(3)))?;
    write!(
        socket,
        "POST /rpc HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer {TOKEN}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )?;
    let mut response = String::new();
    socket.read_to_string(&mut response)?;
    let (_, body) = response
        .split_once("\r\n\r\n")
        .ok_or_else(|| std::io::Error::other("missing HTTP body"))?;
    serde_json::from_str(body).map_err(std::io::Error::other)
}
fn start(project: &Path, data: &Path, log: &Path) -> (Daemon, SocketAddr) {
    let address = TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap();
    let mut daemon = Daemon(
        Command::new(env!("CARGO_BIN_EXE_excalibur"))
            .args(["daemon", "start", "--project"])
            .arg(project)
            .arg("--data-dir")
            .arg(data)
            .args(["--listen", &address.to_string()])
            .env("EXCALIBUR_DAEMON_TOKEN", TOKEN)
            .env_remove("OPENAI_MODEL")
            .env_remove("OPENAI_API_KEY")
            .env_remove("OPENAI_FALLBACK_MODEL")
            .env_remove("OPENAI_FALLBACK_API_KEY")
            .stdout(Stdio::null())
            .stderr(std::fs::File::create(log).unwrap())
            .spawn()
            .unwrap(),
    );
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if rpc(address, "daemon.recovery.status", json!({})).is_ok() {
            return (daemon, address);
        }
        assert!(
            daemon.0.try_wait().unwrap().is_none(),
            "daemon stopped: {}",
            std::fs::read_to_string(log).unwrap()
        );
        assert!(
            Instant::now() < deadline,
            "daemon startup deadline: {}",
            std::fs::read_to_string(log).unwrap()
        );
        std::thread::sleep(Duration::from_millis(30));
    }
}
fn result(address: SocketAddr, method: &str, params: Value) -> Value {
    let response = rpc(address, method, params).unwrap();
    assert!(response.get("error").is_none(), "{response}");
    response["result"].clone()
}
fn stop(daemon: &mut Daemon, address: SocketAddr) {
    result(address, "daemon.stop", json!({}));
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        if let Some(status) = daemon.0.try_wait().unwrap() {
            assert!(status.success());
            return;
        }
        assert!(Instant::now() < deadline, "daemon did not stop");
        std::thread::sleep(Duration::from_millis(30));
    }
}
#[test]
fn foreground_sigkill_boots_enter_safe_mode_keep_http_and_master_alive_and_record_clean_stop() {
    let temp = tempfile::tempdir().unwrap();
    let project = temp.path().join("project");
    std::fs::create_dir(&project).unwrap();
    let data = temp.path().join("data");
    for boot in 0..4 {
        let (mut daemon, address) = start(
            &project,
            &data,
            &temp.path().join(format!("boot-{boot}.log")),
        );
        let status = result(address, "daemon.recovery.status", json!({}));
        assert_eq!(status["recent_crashes"], boot);
        assert_eq!(status["safe_mode"], boot == 3);
        assert_eq!(status["recovering"], false);
        result(
            address,
            "conversation.send",
            json!({"conversation":"recovery-test","client_id":"synthetic-client","client_msg_id":format!("status-{boot}"),"text":"/status","scope":"general"}),
        );
        assert!(
            !result(address, "conversation.list", json!({}))
                .as_array()
                .unwrap()
                .is_empty()
        );
        if boot < 3 {
            daemon.0.kill().unwrap();
            let status = daemon.0.wait().unwrap();
            use std::os::unix::process::ExitStatusExt;
            assert_eq!(status.signal(), Some(9));
        } else {
            let events = result(
                address,
                "notification.list",
                json!({"conversation":"recovery-test"}),
            );
            let notes = events
                .as_array()
                .unwrap()
                .iter()
                .filter(|event| {
                    serde_json::from_str::<Value>(event["content"].as_str().unwrap_or(""))
                        .is_ok_and(|content| {
                            content["kind"] == "recovery_summary"
                                && content["boot_id"] == status["boot_id"]
                        })
                })
                .count();
            assert_eq!(
                notes, 1,
                "one durable recovery summary per conversation and boot"
            );
            result(
                address,
                "daemon.recovery.exit_safe_mode",
                json!({"operation":"human-exit-boot4"}),
            );
            assert_eq!(
                result(address, "daemon.recovery.status", json!({}))["safe_mode"],
                false
            );
            stop(&mut daemon, address);
        }
    }
    // A clean foreground exit is not counted as a fourth crash and does not
    // re-arm the manually exited mode from historical crash receipts.
    let (mut daemon, address) = start(&project, &data, &temp.path().join("clean-restart.log"));
    let status = result(address, "daemon.recovery.status", json!({}));
    assert_eq!(status["recent_crashes"], 3);
    assert_eq!(status["safe_mode"], false);
    stop(&mut daemon, address);
}
