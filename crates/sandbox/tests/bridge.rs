#![cfg(target_os = "linux")]
use std::{
    fs,
    io::{Read, Write},
    os::unix::net::UnixListener,
    path::{Path, PathBuf},
    process::{Command, Output},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::Duration,
};
struct Echo {
    path: PathBuf,
    stop: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}
impl Echo {
    fn new(root: &Path) -> Self {
        let path = root.join("broker.sock");
        let listener = UnixListener::bind(&path).unwrap();
        listener.set_nonblocking(true).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let shutdown = stop.clone();
        let thread = thread::spawn(move || {
            while !shutdown.load(Ordering::Acquire) {
                match listener.accept() {
                    Ok((mut connection, _)) => {
                        thread::spawn(move || {
                            let _ = connection.set_read_timeout(Some(Duration::from_secs(5)));
                            let mut buf = [0; 4096];
                            while let Ok(n) = connection.read(&mut buf) {
                                if n == 0 {
                                    break;
                                }
                                if connection.write_all(&buf[..n]).is_err() {
                                    break;
                                }
                            }
                        });
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(5))
                    }
                    Err(_) => break,
                }
            }
        });
        Self {
            path,
            stop,
            thread: Some(thread),
        }
    }
}
impl Drop for Echo {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            thread.join().unwrap();
        }
    }
}
fn helper(socket: &Path, script: &str) -> Output {
    Command::new(env!("CARGO_BIN_EXE_excalibur-network-bridge"))
        .args([
            "--socket",
            socket.to_str().unwrap(),
            "--listen",
            "127.0.0.1:0",
            "--",
            "/usr/bin/python3",
            "-c",
            script,
        ])
        .env("NO_PROXY", "*")
        .env("no_proxy", "*")
        .env("FIXTURE_BROKER_PATH", socket)
        .output()
        .unwrap()
}
#[test]
fn bootstrap_sets_proxy_before_child_and_relay_preserves_bytes_with_unix_descriptors_denied() {
    let root = tempfile::tempdir().unwrap();
    let broker = Echo::new(root.path());
    let result = helper(
        &broker.path,
        r#"
import ctypes,errno,json,os,socket,urllib.parse
assert 'NO_PROXY' not in os.environ and 'no_proxy' not in os.environ
for key in ['HTTP_PROXY','http_proxy','HTTPS_PROXY','https_proxy']:
    assert os.environ[key].startswith('http://127.0.0.1:')
assert os.environ['ALL_PROXY'].startswith('socks5h://127.0.0.1:')
proxy=urllib.parse.urlparse(os.environ['HTTP_PROXY'])
payload=bytes(range(256))*1024
with socket.create_connection((proxy.hostname,proxy.port),timeout=5) as stream:
    stream.sendall(payload);received=b''
    while len(received)<len(payload):
        chunk=stream.recv(65536);assert chunk;received+=chunk
    assert received==payload
for family,kind,proto in [(socket.AF_UNIX,socket.SOCK_STREAM,0),(socket.AF_UNIX,socket.SOCK_DGRAM,0),(socket.AF_INET,socket.SOCK_DGRAM,0),(socket.AF_INET6,socket.SOCK_DGRAM,0)]:
    try: socket.socket(family,kind,proto);raise AssertionError('prohibited socket opened')
    except OSError as error: assert error.errno==errno.EPERM
for kind in [socket.SOCK_STREAM,socket.SOCK_SEQPACKET]:
    left,right=socket.socketpair(socket.AF_UNIX,kind)
    address=ctypes.create_string_buffer(b'\x00\x00')
    libc=ctypes.CDLL(None,use_errno=True)
    assert libc.connect(left.fileno(),address,2)==-1,'anonymous pair disconnected for retargeting'
    try: left.connect(os.getenv('FIXTURE_BROKER_PATH'));raise AssertionError('anonymous pair retargeted to broker')
    except OSError:pass
    left.sendall(b'private-local-ipc');assert right.recv(64)==b'private-local-ipc'
    left.close();right.close()
try: socket.socketpair(socket.AF_UNIX,socket.SOCK_DGRAM);raise AssertionError('retargetable UNIX datagram channel created')
except OSError as error: assert error.errno==errno.EPERM
for name in os.listdir('/proc/self/fd'):
    try: target=os.readlink('/proc/self/fd/'+name)
    except OSError: continue
    assert not target.startswith('socket:'),target
assert 'Seccomp:\t2' in open('/proc/self/status').read()
print(json.dumps({'relay':True,'unix_denied':True,'udp_denied':True,'no_inherited_sockets':True}))
"#,
    );
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let data: serde_json::Value = serde_json::from_slice(&result.stdout).unwrap();
    assert_eq!(data["relay"], true);
    assert_eq!(data["no_inherited_sockets"], true);
}
#[test]
fn bootstrap_rejects_unavailable_broker_and_nonloopback_before_child_effects() {
    let root = tempfile::tempdir().unwrap();
    let marker = root.path().join("marker");
    let script = format!("open({:?},'w').write('BAD')", marker.to_str().unwrap());
    let result = helper(&root.path().join("missing.sock"), &script);
    assert!(!result.status.success());
    assert!(!marker.exists());
    let broker = Echo::new(root.path());
    let result = Command::new(env!("CARGO_BIN_EXE_excalibur-network-bridge"))
        .args([
            "--socket",
            broker.path.to_str().unwrap(),
            "--listen",
            "0.0.0.0:3128",
            "--",
            "/usr/bin/python3",
            "-c",
            &script,
        ])
        .output()
        .unwrap();
    assert!(!result.status.success());
    assert!(!marker.exists());
    fs::write(root.path().join("regular-file"), b"not a socket").unwrap();
    let result = helper(&root.path().join("regular-file"), &script);
    assert!(!result.status.success());
    assert!(!marker.exists());
}
