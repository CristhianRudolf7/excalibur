#![cfg(target_os = "linux")]
use excalibur_tools::terminal::{SandboxConfig, TerminalConfig, TerminalManager};
use serde_json::json;
use std::fs;

#[tokio::test]
async fn missing_runtime_never_selects_an_unimplemented_fallback() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("project");
    fs::create_dir(&root).unwrap();
    let mut config = SandboxConfig::read_only(&root);
    config.bubblewrap = Some(temp.path().join("runtime-does-not-exist"));
    let terminal = TerminalManager::new(TerminalConfig {
        spool_dir: temp.path().join("spool"),
        sandbox: Some(config),
        ..Default::default()
    })
    .unwrap();
    let result = terminal
        .exec(
            "missing-runtime",
            json!({"cmd":"printf COMMAND_STARTED; printf bad > effect-marker","yield_ms":250}),
        )
        .await;
    assert!(
        !result.success,
        "missing confinement must fail before any command starts: {result:?}"
    );
    assert!(!root.join("effect-marker").exists());
    assert!(!result.output.to_string().contains("COMMAND_STARTED"));
    terminal.shutdown().await;
}

#[tokio::test]
async fn general_project_build_and_second_exec_preserve_protected_mounts_after_rename() {
    use excalibur_tools::terminal::prepare_project_boundary;
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("project");
    fs::create_dir_all(root.join("src")).unwrap();
    // A real Rust child exercises std's fallible pre-exec spawn channel, used
    // by process adapters and build tooling; it is not an emulated shell call.
    let mut compiler = std::process::Command::new("rustc")
        .args(["--edition=2024", "-o"])
        .arg(root.join("rust-subprocess"))
        .arg("-")
        .stdin(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    use std::io::Write;
    compiler.stdin.take().unwrap().write_all(br#"use std::os::unix::process::CommandExt;fn main(){let mut child=std::process::Command::new("/usr/bin/true");unsafe{child.pre_exec(||Ok(()));}assert!(child.status().expect("Rust child spawn inside sandbox").success());println!("RUST_SUBPROCESS_OK");}"#).unwrap();
    assert!(compiler.wait().unwrap().success());
    prepare_project_boundary(&root).unwrap();
    fs::write(root.join(".git/config"), b"synthetic root configuration").unwrap();
    fs::write(root.join("src/.agents/policy"), b"synthetic pinned policy").unwrap();
    fs::write(root.join(".env"), b"SYNTHETIC_ENV_PRIVATE").unwrap();
    let outside = temp.path().join("outside");
    fs::write(&outside, b"synthetic outside private").unwrap();
    std::os::unix::fs::symlink(&outside, root.join("outside-alias")).unwrap();
    let external = temp.path().join("external-readonly");
    fs::create_dir(&external).unwrap();
    fs::write(external.join("input"), b"synthetic readonly external").unwrap();
    std::os::unix::fs::symlink(external.join("input"), root.join("readonly-alias")).unwrap();
    let mut config = SandboxConfig::project(&root);
    config.read_roots.push(external.clone());
    let terminal = TerminalManager::new(TerminalConfig {
        spool_dir: temp.path().join("spool"),
        sandbox: Some(config),
        ..Default::default()
    })
    .unwrap();
    let first = r#"python3 - <<'PY'
import os,subprocess
os.makedirs('build/nested/objects')
source=b'#include <stdio.h>\nint main(void){puts("synthetic-build-ok");return 0;}\n'
subprocess.run(['/usr/bin/cc','-x','c','-','-o','build/nested/program.tmp'],input=source,check=True)
os.rename('build/nested/program.tmp','build/nested/program')
assert subprocess.check_output(['build/nested/program'])==b'synthetic-build-ok\n'
assert subprocess.check_output(['./rust-subprocess'])==b'RUST_SUBPROCESS_OK\n'
os.rename('src','moved-source')
for path in ['.git/config','moved-source/.agents/policy']:
    try: open(path,'w').write('BAD');raise AssertionError('protected write succeeded')
    except OSError: pass
try: os.rename('.git','detached-git');raise AssertionError('protected root replaced')
except OSError: pass
try: os.unlink('moved-source/.agents/policy');raise AssertionError('protected file unlinked')
except OSError: pass
try: os.link('.git/config','copied-root-config');raise AssertionError('protected hardlink escaped')
except OSError: pass
for path in ['.env','outside-alias']:
    try: open(path).read();raise AssertionError('private file read succeeded')
    except OSError: pass
assert open('readonly-alias').read()=='synthetic readonly external'
try: open('readonly-alias','w').write('BAD');raise AssertionError('read-only grant elevated through writable project link')
except OSError: pass
# The apparent host /tmp pathname belongs to the private tmpfs here. Creating
# a shadow proves no host write only when the parent checks its real inode.
open('outside-alias','w').write('private temporary shadow')
assert open('outside-alias').read()=='private temporary shadow'
print('BUILD_RENAME_CONFINED')
PY"#;
    let first = terminal
        .exec("project-build", json!({"cmd":first,"yield_ms":30000}))
        .await;
    assert!(first.success, "{first:?}");
    assert_eq!(first.output["exit_code"], 0, "{first:?}");
    assert!(
        first.output["output"]
            .as_str()
            .unwrap()
            .contains("BUILD_RENAME_CONFINED")
    );
    let second = r#"python3 - <<'PY'
import os,subprocess
os.makedirs('build/next/new-directory')
open('build/next/new-directory/output.tmp','w').write('second-exec')
os.rename('build/next/new-directory/output.tmp','build/next/new-directory/output')
assert subprocess.check_output(['build/nested/program'])==b'synthetic-build-ok\n'
for path in ['.git/config','moved-source/.agents/policy']:
    try: open(path,'w').write('BAD');raise AssertionError('reopened protected write succeeded')
    except OSError: pass
print('SECOND_EXEC_CONFINED')
PY"#;
    let second = terminal
        .exec("project-rerun", json!({"cmd":second,"yield_ms":30000}))
        .await;
    assert!(second.success, "{second:?}");
    assert_eq!(second.output["exit_code"], 0, "{second:?}");
    assert!(
        second.output["output"]
            .as_str()
            .unwrap()
            .contains("SECOND_EXEC_CONFINED")
    );
    assert_eq!(
        fs::read(root.join(".git/config")).unwrap(),
        b"synthetic root configuration"
    );
    assert_eq!(
        fs::read(root.join("moved-source/.agents/policy")).unwrap(),
        b"synthetic pinned policy"
    );
    assert_eq!(fs::read(&outside).unwrap(), b"synthetic outside private");
    assert_eq!(
        fs::read(external.join("input")).unwrap(),
        b"synthetic readonly external"
    );
    assert_eq!(
        fs::read(root.join("build/next/new-directory/output")).unwrap(),
        b"second-exec"
    );
    terminal.shutdown().await;
}

struct Origin {
    port: u16,
    requests: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    worker: Option<std::thread::JoinHandle<()>>,
}
impl Origin {
    fn new() -> Self {
        use std::{
            io::{Read, Write},
            sync::{
                Arc,
                atomic::{AtomicBool, AtomicUsize, Ordering},
            },
            thread,
            time::Duration,
        };
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        listener.set_nonblocking(true).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let stopping = stop.clone();
        let requests = Arc::new(AtomicUsize::new(0));
        let counter = requests.clone();
        let worker = thread::spawn(move || {
            while !stopping.load(Ordering::Acquire) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        let counter = counter.clone();
                        thread::spawn(move || {
                            let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
                            let mut header = Vec::new();
                            let mut byte = [0];
                            while header.len() < 32768 {
                                if stream.read_exact(&mut byte).is_err() {
                                    return;
                                }
                                header.push(byte[0]);
                                if header.ends_with(b"\r\n\r\n") {
                                    break;
                                }
                            }
                            counter.fetch_add(1, Ordering::SeqCst);
                            let body = b"synthetic-allowed-origin";
                            let _ = write!(
                                stream,
                                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                                body.len()
                            );
                            let _ = stream.write_all(body);
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
            port,
            requests,
            stop,
            worker: Some(worker),
        }
    }
}
impl Drop for Origin {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::Release);
        if let Some(worker) = self.worker.take() {
            worker.join().unwrap();
        }
    }
}
struct LocalResolver;
impl excalibur_sandbox::network::Resolver for LocalResolver {
    fn resolve(&self, host: &str, port: u16) -> Result<Vec<std::net::SocketAddr>, String> {
        if host != "allowed.test" {
            return Err("unexpected fixture DNS request".into());
        }
        Ok(vec![std::net::SocketAddr::from(([127, 0, 0, 1], port))])
    }
}
#[tokio::test]
async fn project_net_proxy_cannot_escape_via_authority_ip_direct_routes_or_unix_sockets() {
    use excalibur_sandbox::network::{NetworkPolicy, ProxyConfig};
    use excalibur_tools::terminal::prepare_project_boundary;
    use std::{
        net::{IpAddr, Ipv4Addr},
        sync::{Arc, atomic::Ordering},
    };
    let origin = Origin::new();
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("project");
    fs::create_dir(&root).unwrap();
    prepare_project_boundary(&root).unwrap();
    let bridge = std::env::current_exe()
        .unwrap()
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("excalibur-network-bridge");
    assert!(
        bridge.is_file(),
        "build the trusted helper first: cargo build -p excalibur-sandbox --bin excalibur-network-bridge"
    );
    let mut proxy = ProxyConfig::new(NetworkPolicy::Domains(["allowed.test".into()].into()));
    proxy.allowed_ports = [origin.port].into();
    proxy.permitted_private = [IpAddr::V4(Ipv4Addr::LOCALHOST)].into();
    proxy.resolver = Arc::new(LocalResolver);
    let terminal = TerminalManager::new(TerminalConfig {
        spool_dir: temp.path().join("spool"),
        sandbox: Some(SandboxConfig::project_net(&root, proxy, bridge)),
        ..Default::default()
    })
    .unwrap();
    let command = format!(
        r#"python3 - <<'PY'
import errno,json,os,socket,subprocess,urllib.parse
port={port}
allowed='http://allowed.test:'+str(port)+'/'
assert subprocess.check_output(['/usr/bin/curl','--silent','--show-error','--fail','--max-time','10',allowed])==b'synthetic-allowed-origin'
proxy=urllib.parse.urlparse(os.environ['HTTP_PROXY'])
def request(raw):
    with socket.create_connection((proxy.hostname,proxy.port),timeout=5) as stream:
        stream.sendall(raw);stream.shutdown(socket.SHUT_WR);data=b''
        while True:
            part=stream.recv(4096)
            if not part:break
            data+=part
        return data
for request_bytes in [
    ('GET http://allowed.test:'+str(port)+'/ HTTP/1.1\r\nHost: denied.test:'+str(port)+'\r\n\r\n').encode(),
    ('CONNECT allowed.test:'+str(port)+' HTTP/1.1\r\nHost: denied.test:'+str(port)+'\r\n\r\n').encode(),
    ('GET http://2130706433:'+str(port)+'/ HTTP/1.1\r\nHost: 2130706433:'+str(port)+'\r\n\r\n').encode(),
    ('GET http://allowed.test.evil:'+str(port)+'/ HTTP/1.1\r\nHost: allowed.test.evil:'+str(port)+'\r\n\r\n').encode(),
]:
    assert request(request_bytes).startswith(b'HTTP/1.1 403'),request_bytes
with socket.create_connection((proxy.hostname,proxy.port),timeout=5) as stream:
    stream.sendall(bytes([5,1,0]));assert stream.recv(2)==bytes([5,0])
    host=b'allowed.test';stream.sendall(bytes([5,3,0,3,len(host)])+host+port.to_bytes(2,'big'))
    assert stream.recv(10)[:2]==bytes([5,7])
for family,kind in [(socket.AF_UNIX,socket.SOCK_STREAM),(socket.AF_INET,socket.SOCK_DGRAM),(socket.AF_INET6,socket.SOCK_DGRAM),(socket.AF_PACKET,socket.SOCK_RAW)]:
    try: socket.socket(family,kind);raise AssertionError('prohibited direct channel opened')
    except OSError as error:assert error.errno==errno.EPERM
try: socket.create_connection(('127.0.0.1',port),timeout=1);raise AssertionError('host loopback escaped namespace')
except OSError:pass
bypass=subprocess.run(['/usr/bin/curl','--silent','--max-time','2','--noproxy','*','--resolve','allowed.test:'+str(port)+':127.0.0.1',allowed],stdout=subprocess.PIPE,stderr=subprocess.PIPE)
assert bypass.returncode!=0,'proxy environment bypass reached host'
udsbypass=subprocess.run(['/usr/bin/curl','--silent','--max-time','2','--unix-socket','/run/excalibur/proxy.sock',allowed],stdout=subprocess.PIPE,stderr=subprocess.PIPE)
assert udsbypass.returncode!=0,'untrusted child reached broker UDS directly'
print(json.dumps({{'allowed':True,'authority_denied':True,'literal_ip_denied':True,'direct_routes_denied':True,'unix_udp_denied':True}}))
PY"#,
        port = origin.port
    );
    let result = terminal
        .exec("proxy-adversarial", json!({"cmd":command,"yield_ms":30000}))
        .await;
    assert!(result.success, "{result:?}");
    assert_eq!(result.output["exit_code"], 0, "{result:?}");
    let value: serde_json::Value =
        serde_json::from_str(result.output["output"].as_str().unwrap().trim()).unwrap();
    assert_eq!(value["direct_routes_denied"], true);
    assert_eq!(
        origin.requests.load(Ordering::SeqCst),
        1,
        "denied or direct requests must never reach the fixture origin"
    );
    terminal.shutdown().await;
}
