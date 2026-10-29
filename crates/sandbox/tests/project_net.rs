#![cfg(target_os = "linux")]
use excalibur_sandbox::network::{NetworkPolicy, ProxyConfig, Resolver};
use excalibur_sandbox::{PreparedSandbox, SandboxConfig};
use std::{
    io::{Read, Write},
    net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener},
    process::Command,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    thread,
};
struct Dns {
    ip: IpAddr,
    calls: Arc<AtomicUsize>,
}
impl Resolver for Dns {
    fn resolve(&self, _: &str, port: u16) -> Result<Vec<SocketAddr>, String> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(vec![SocketAddr::new(self.ip, port)])
    }
}
fn cfg(root: &std::path::Path, ports: &[u16], calls: Arc<AtomicUsize>) -> SandboxConfig {
    excalibur_sandbox::prepare_project_boundary(root).unwrap();
    let mut network = ProxyConfig::new(NetworkPolicy::Domains(["allowed.test".into()].into()));
    network.allowed_ports.extend(ports);
    network
        .permitted_private
        .insert(IpAddr::V4(Ipv4Addr::LOCALHOST));
    network.resolver = Arc::new(Dns {
        ip: IpAddr::V4(Ipv4Addr::LOCALHOST),
        calls,
    });
    SandboxConfig::project_net(
        root,
        network,
        env!("CARGO_BIN_EXE_excalibur-network-bridge"),
    )
}
fn run(config: &SandboxConfig, script: &str) -> std::process::Output {
    let sandbox = PreparedSandbox::new(config).unwrap();
    let mut command = Command::new(&sandbox.program);
    sandbox.configure(
        &mut command,
        std::path::Path::new("/bin/sh"),
        &["-c".into(), script.into()],
        false,
    );
    let fd = sandbox.filter_fd();
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFD);
        assert_eq!(libc::fcntl(fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC), 0);
    }
    let output = command.output().unwrap();
    drop(sandbox);
    output
}
fn http_server(listener: TcpListener, requests: usize, redirect: bool) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        for _ in 0..requests {
            let (mut stream, _) = listener.accept().unwrap();
            let mut bytes = Vec::new();
            while !bytes.ends_with(b"\r\n\r\n") {
                let mut byte = [0];
                stream.read_exact(&mut byte).unwrap();
                bytes.push(byte[0]);
            }
            let response = if redirect {
                "HTTP/1.1 302 Found\r\nLocation: http://denied.test/never\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            } else {
                "HTTP/1.1 200 OK\r\nContent-Length: 9\r\nConnection: close\r\n\r\nSYNTHETIC"
            };
            stream.write_all(response.as_bytes()).unwrap();
        }
    })
}
#[test]
fn actual_namespaced_curl_http_socks_and_redirect_enforce_the_same_domain_policy() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("workspace");
    std::fs::create_dir(&root).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let calls = Arc::new(AtomicUsize::new(0));
    let config = cfg(&root, &[port], calls.clone());
    let server = http_server(listener, 2, false);
    let output = run(
        &config,
        &format!(
            "set -eu; curl -fsS --max-time 5 --proxy \"$HTTP_PROXY\" http://allowed.test:{port}/http; curl -fsS --max-time 5 --proxy \"$ALL_PROXY\" http://allowed.test:{port}/socks; if curl -fsS --max-time 5 --proxy \"$HTTP_PROXY\" http://denied.test:{port}/blocked; then exit 90; fi"
        ),
    );
    assert!(
        output.status.success(),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        "SYNTHETICSYNTHETIC"
    );
    server.join().unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = http_server(listener, 1, true);
    let config = cfg(&root, &[port], calls.clone());
    let output = run(
        &config,
        &format!(
            "if curl -fsSL --max-time 5 --proxy \"$HTTP_PROXY\" http://allowed.test:{port}/redirect; then exit 91; fi"
        ),
    );
    assert!(output.status.success());
    server.join().unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 3);
}
#[test]
fn actual_namespaced_curl_https_connect_keeps_certificate_validation_end_to_end() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("workspace");
    std::fs::create_dir(&root).unwrap();
    let certified = rcgen::generate_simple_self_signed(vec!["allowed.test".into()]).unwrap();
    std::fs::write(root.join("synthetic-ca.crt"), certified.cert.pem()).unwrap();
    let tls = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .with_no_client_auth()
    .with_single_cert(
        vec![certified.cert.der().clone()],
        rustls::pki_types::PrivatePkcs8KeyDer::from(certified.signing_key.serialize_der()).into(),
    )
    .unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = thread::spawn(move || {
        let tls = Arc::new(tls);
        for _ in 0..2 {
            let (stream, _) = listener.accept().unwrap();
            let connection = rustls::ServerConnection::new(tls.clone()).unwrap();
            let mut stream = rustls::StreamOwned::new(connection, stream);
            let mut bytes = Vec::new();
            while !bytes.ends_with(b"\r\n\r\n") {
                let mut byte = [0];
                stream.read_exact(&mut byte).unwrap();
                bytes.push(byte[0]);
            }
            assert!(String::from_utf8_lossy(&bytes).contains("Host: allowed.test:"));
            stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Length: 6\r\nConnection: close\r\n\r\nTLS-OK",
                )
                .unwrap();
            stream.flush().unwrap();
            stream.conn.send_close_notify();
            let _ = stream.flush();
        }
    });
    let calls = Arc::new(AtomicUsize::new(0));
    let config = cfg(&root, &[port], calls.clone());
    let output = run(
        &config,
        &format!(
            "set -eu; curl -fsS --max-time 5 --cacert synthetic-ca.crt --proxy \"$HTTPS_PROXY\" https://allowed.test:{port}/tls; curl -fsS --max-time 5 --cacert synthetic-ca.crt --proxy \"$ALL_PROXY\" https://allowed.test:{port}/tls-socks"
        ),
    );
    assert!(
        output.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(output.stdout, b"TLS-OKTLS-OK");
    server.join().unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 2);
}

#[test]
fn full_network_policy_accepts_ip_destinations_only_with_explicit_host_address_permission() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("workspace");
    std::fs::create_dir(&root).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let calls = Arc::new(AtomicUsize::new(0));
    let mut config = cfg(&root, &[port], calls.clone());
    config.network.as_mut().unwrap().policy = NetworkPolicy::Full;
    let server = http_server(listener, 2, false);
    let output = run(
        &config,
        &format!(
            "set -eu; curl -fsS --max-time 5 --proxy \"$HTTP_PROXY\" http://127.0.0.1:{port}/full; curl -fsS --max-time 5 --proxy \"$ALL_PROXY\" http://127.0.0.1:{port}/full-socks"
        ),
    );
    assert!(
        output.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(output.stdout, b"SYNTHETICSYNTHETIC");
    server.join().unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}
