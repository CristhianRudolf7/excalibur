use std::{
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    process::{Command, Stdio},
    time::{Duration, Instant},
};
struct Guard(std::process::Child);
impl Drop for Guard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
#[test]
fn daemon_start_requires_header_token_and_stop_closes_the_principal_database() {
    let temp = tempfile::tempdir().unwrap();
    let project = temp.path().join("project");
    std::fs::create_dir(&project).unwrap();
    let data = temp.path().join("data");
    let address = TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap();
    let bin = env!("CARGO_BIN_EXE_excalibur");
    let token = "synthetic-daemon-test-token-000000000";
    let mut child = Guard(
        Command::new(bin)
            .args(["daemon", "start", "--project"])
            .arg(&project)
            .arg("--data-dir")
            .arg(&data)
            .args(["--listen", &address.to_string()])
            .env("EXCALIBUR_DAEMON_TOKEN", token)
            .env_remove("OPENAI_MODEL")
            .env_remove("OPENAI_API_KEY")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        if TcpStream::connect(address).is_ok() {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "daemon did not start: {:?}",
            child.0.try_wait()
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    let mut unauthorized = TcpStream::connect(address).unwrap();
    unauthorized
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    unauthorized.write_all(b"POST /rpc HTTP/1.1\r\nHost: localhost\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}").unwrap();
    let mut response = String::new();
    unauthorized.read_to_string(&mut response).unwrap();
    assert!(response.starts_with("HTTP/1.1 401"));
    let stopped = Command::new(bin)
        .args(["daemon", "stop", "--data-dir"])
        .arg(&data)
        .args(["--listen", &address.to_string()])
        .env("EXCALIBUR_DAEMON_TOKEN", token)
        .output()
        .unwrap();
    assert!(
        stopped.status.success(),
        "{}",
        String::from_utf8_lossy(&stopped.stderr)
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(status) = child.0.try_wait().unwrap() {
            assert!(status.success());
            break;
        }
        assert!(Instant::now() < deadline, "daemon did not exit after stop");
        std::thread::sleep(Duration::from_millis(20));
    }
}
