#![cfg(target_os = "linux")]
use excalibur_daemon::watchdog::ServiceNotifier;
use std::{
    ffi::OsString,
    os::{
        linux::net::SocketAddrExt,
        unix::net::{SocketAddr, UnixDatagram},
    },
    process::{Command, Stdio},
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::sync::{Notify, watch};

fn config(
    socket: &std::ffi::OsStr,
    usec: Option<&str>,
    pid: Option<&str>,
) -> Result<Option<ServiceNotifier>, String> {
    ServiceNotifier::from_values(
        Some(socket.to_owned()),
        usec.map(OsString::from),
        pid.map(OsString::from),
        std::process::id(),
    )
}
fn receive(socket: &UnixDatagram) -> String {
    let mut bytes = [0; 128];
    let n = socket.recv(&mut bytes).unwrap();
    String::from_utf8(bytes[..n].to_vec()).unwrap()
}
#[test]
fn validates_protocol_without_mutating_global_environment() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("notify");
    let socket = UnixDatagram::bind(&path).unwrap();
    assert!(config(path.as_os_str(), Some("0"), None).is_err());
    assert!(config(path.as_os_str(), Some("+500"), None).is_err());
    assert!(config(path.as_os_str(), Some("18446744073709551616"), None).is_err());
    assert!(config(path.as_os_str(), None, Some("0")).is_err());
    assert!(config(std::ffi::OsStr::new("relative"), None, None).is_err());
    assert!(ServiceNotifier::from_values(None, Some("1000".into()), None, 123).is_err());
    assert!(
        ServiceNotifier::from_values(None, None, None, 123)
            .unwrap()
            .is_none()
    );
    let n = config(path.as_os_str(), Some("200000"), None)
        .unwrap()
        .unwrap();
    assert_eq!(n.interval(), Some(Duration::from_millis(100)));
    let foreign = (std::process::id() as u64 + 1).to_string();
    let n = config(
        path.as_os_str(),
        Some("malformed-for-foreign-process"),
        Some(&foreign),
    )
    .unwrap()
    .unwrap();
    assert_eq!(n.interval(), None);
    drop(socket);
}
#[test]
fn ready_heartbeat_and_stopping_use_filesystem_and_abstract_sockets() {
    for abstract_socket in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("notify");
        let abstract_name = format!(
            "excalibur-watchdog-{}-{}",
            std::process::id(),
            dir.path().display()
        );
        let (socket, address) = if abstract_socket {
            (
                UnixDatagram::bind_addr(
                    &SocketAddr::from_abstract_name(abstract_name.as_bytes()).unwrap(),
                )
                .unwrap(),
                OsString::from(format!("@{abstract_name}")),
            )
        } else {
            (UnixDatagram::bind(&path).unwrap(), path.into_os_string())
        };
        socket
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();
        let (stop, stopping) = watch::channel(false);
        let task = config(&address, Some("200000"), None)
            .unwrap()
            .unwrap()
            .start(runtime.handle(), Arc::new(Notify::new()), stopping);
        assert_eq!(receive(&socket), "READY=1");
        assert_eq!(receive(&socket), "WATCHDOG=1");
        stop.send(true).unwrap();
        loop {
            if receive(&socket) == "STOPPING=1" {
                break;
            }
        }
        runtime.block_on(task).unwrap().unwrap();
    }
}
#[test]
fn lost_notification_sink_requests_unclean_shutdown() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("notify");
    let socket = UnixDatagram::bind(&path).unwrap();
    let notifier = config(path.as_os_str(), Some("100000"), None)
        .unwrap()
        .unwrap();
    drop(socket);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let shutdown = Arc::new(Notify::new());
        let (_stop, stopping) = watch::channel(false);
        let task = notifier.start(
            &tokio::runtime::Handle::current(),
            shutdown.clone(),
            stopping,
        );
        tokio::time::timeout(Duration::from_secs(2), shutdown.notified())
            .await
            .unwrap();
        assert!(task.await.unwrap().is_err());
    });
}
struct ChildGuard(std::process::Child);
impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
#[test]
fn external_watchdog_detects_blocked_interactive_executor() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("notify");
    let socket = UnixDatagram::bind(&path).unwrap();
    socket
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    let child = Command::new(std::env::current_exe().unwrap())
        .args([
            "--ignored",
            "--exact",
            "watchdog_blocked_runtime_helper",
            "--nocapture",
        ])
        .env("EXCALIBUR_WATCHDOG_TEST_SOCKET", &path)
        .stdout(Stdio::null())
        .spawn()
        .unwrap();
    let mut child = ChildGuard(child);
    assert_eq!(receive(&socket), "READY=1");
    assert_eq!(receive(&socket), "WATCHDOG=1");
    let deadline = Instant::now() + Duration::from_millis(700);
    socket
        .set_read_timeout(Some(Duration::from_millis(300)))
        .unwrap();
    let mut last = Instant::now();
    loop {
        let mut bytes = [0; 128];
        match socket.recv(&mut bytes) {
            Ok(n) => {
                assert_eq!(&bytes[..n], b"WATCHDOG=1");
                last = Instant::now();
            }
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
                ) =>
            {
                break;
            }
            Err(e) => panic!("supervisor receive: {e}"),
        }
        assert!(
            Instant::now() < deadline,
            "blocked runtime incorrectly kept heartbeat alive"
        );
    }
    assert!(last.elapsed() >= Duration::from_millis(200));
    assert!(
        child.0.try_wait().unwrap().is_none(),
        "child exited instead of hanging"
    );
    child.0.kill().unwrap();
    child.0.wait().unwrap();
}
#[test]
#[ignore = "subprocess fixture invoked by the external watchdog test"]
fn watchdog_blocked_runtime_helper() {
    let Some(path) = std::env::var_os("EXCALIBUR_WATCHDOG_TEST_SOCKET") else {
        panic!("fixture socket missing")
    };
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let (_stop, stopping) = watch::channel(false);
        let _task = config(&path, Some("200000"), None).unwrap().unwrap().start(
            &tokio::runtime::Handle::current(),
            Arc::new(Notify::new()),
            stopping,
        );
        tokio::time::sleep(Duration::from_millis(40)).await;
        // Deliberately freeze this fixture's interactive executor; supervisor is another process.
        std::thread::sleep(Duration::from_secs(30));
    });
}
