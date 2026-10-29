#![cfg(unix)]
use excalibur_sandbox::network::{HostProxy, NetworkPolicy, ProxyConfig, Resolver};
use std::{
    io::{Read, Write},
    net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener},
    os::unix::net::UnixStream,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    thread,
    time::Duration,
};
struct Dns {
    address: SocketAddr,
    calls: Arc<AtomicUsize>,
    rebind: bool,
}
impl Resolver for Dns {
    fn resolve(&self, _: &str, port: u16) -> Result<Vec<SocketAddr>, String> {
        let n = self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(if self.rebind && n > 0 {
            vec![SocketAddr::from(([169, 254, 169, 254], port))]
        } else {
            vec![self.address]
        })
    }
}
fn config(address: SocketAddr, calls: Arc<AtomicUsize>) -> ProxyConfig {
    let mut config = ProxyConfig::new(NetworkPolicy::Domains(["allowed.test".into()].into()));
    config.allowed_ports.insert(address.port());
    config
        .permitted_private
        .insert(IpAddr::V4(Ipv4Addr::LOCALHOST));
    config.resolver = Arc::new(Dns {
        address,
        calls,
        rebind: false,
    });
    config
}
fn request(proxy: &HostProxy, request: &str) -> String {
    let mut stream = UnixStream::connect(proxy.socket_path()).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    stream.write_all(request.as_bytes()).unwrap();
    let mut out = String::new();
    stream.read_to_string(&mut out).unwrap();
    out
}
#[test]
fn http_allowlist_host_consistency_ambiguous_framing_and_private_dns_never_escape() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let proxy = HostProxy::start(config(address, calls.clone())).unwrap();
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut head = Vec::new();
        while !head.ends_with(b"\r\n\r\n") {
            let mut byte = [0];
            stream.read_exact(&mut byte).unwrap();
            head.push(byte[0]);
        }
        let head = String::from_utf8(head).unwrap();
        assert!(head.starts_with("GET /ok?q=1 HTTP/1.1"));
        assert!(!head.to_ascii_lowercase().contains("proxy-authorization"));
        stream
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nOK")
            .unwrap();
    });
    let response = request(
        &proxy,
        &format!(
            "GET http://allowed.test:{}/ok?q=1 HTTP/1.1\r\nHost: allowed.test:{}\r\nProxy-Authorization: Basic synthetic\r\n\r\n",
            address.port(),
            address.port()
        ),
    );
    assert!(response.ends_with("OK"));
    server.join().unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    for bad in [
        format!(
            "GET http://denied.test:{0}/ HTTP/1.1\r\nHost: denied.test:{0}\r\n\r\n",
            address.port()
        ),
        format!(
            "GET http://allowed.test:{0}/ HTTP/1.1\r\nHost: denied.test:{0}\r\n\r\n",
            address.port()
        ),
        format!(
            "CONNECT allowed.test:{0} HTTP/1.1\r\nHost: denied.test:{0}\r\n\r\n",
            address.port()
        ),
        format!(
            "GET http://allowed.test:{0}/ HTTP/1.1\r\nHost: allowed.test:{0}\r\nContent-Length: 1\r\nTransfer-Encoding: chunked\r\n\r\n",
            address.port()
        ),
    ] {
        assert!(request(&proxy, &bad).starts_with("HTTP/1.1 403"));
    }
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    let mut private = config(address, calls.clone());
    private.permitted_private.clear();
    let private = HostProxy::start(private).unwrap();
    assert!(
        request(
            &private,
            &format!(
                "GET http://allowed.test:{0}/ HTTP/1.1\r\nHost: allowed.test:{0}\r\n\r\n",
                address.port()
            )
        )
        .starts_with("HTTP/1.1 403")
    );
}
#[test]
fn dns_is_revalidated_and_pinned_for_each_new_connection() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let mut cfg = config(address, calls.clone());
    cfg.resolver = Arc::new(Dns {
        address,
        calls: calls.clone(),
        rebind: true,
    });
    let proxy = HostProxy::start(cfg).unwrap();
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut buf = [0; 4096];
        let _ = stream.read(&mut buf);
        stream
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
            .unwrap();
    });
    let target = format!(
        "GET http://allowed.test:{0}/ HTTP/1.1\r\nHost: allowed.test:{0}\r\n\r\n",
        address.port()
    );
    assert!(request(&proxy, &target).starts_with("HTTP/1.1 200"));
    server.join().unwrap();
    assert!(request(&proxy, &target).starts_with("HTTP/1.1 403"));
    assert_eq!(calls.load(Ordering::SeqCst), 2);
}
#[test]
fn socks5_checks_name_before_dns_and_rejects_udp_and_ip_literal_bypass() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let proxy = HostProxy::start(config(address, calls.clone())).unwrap();
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut bytes = [0; 4];
        stream.read_exact(&mut bytes).unwrap();
        assert_eq!(&bytes, b"PING");
        stream.write_all(b"PONG").unwrap();
    });
    let mut stream = UnixStream::connect(proxy.socket_path()).unwrap();
    stream.write_all(&[5, 1, 0]).unwrap();
    let mut reply = [0; 2];
    stream.read_exact(&mut reply).unwrap();
    assert_eq!(reply, [5, 0]);
    let host = b"allowed.test";
    let mut req = vec![5, 1, 0, 3, host.len() as u8];
    req.extend_from_slice(host);
    req.extend_from_slice(&address.port().to_be_bytes());
    stream.write_all(&req).unwrap();
    let mut accepted = [0; 10];
    stream.read_exact(&mut accepted).unwrap();
    assert_eq!(accepted[1], 0);
    stream.write_all(b"PING").unwrap();
    let mut pong = [0; 4];
    stream.read_exact(&mut pong).unwrap();
    assert_eq!(&pong, b"PONG");
    drop(stream);
    server.join().unwrap();
    for request in [
        vec![5, 3, 0, 1, 127, 0, 0, 1, 0, 80],
        vec![
            5,
            1,
            0,
            1,
            127,
            0,
            0,
            1,
            (address.port() >> 8) as u8,
            address.port() as u8,
        ],
    ] {
        let mut stream = UnixStream::connect(proxy.socket_path()).unwrap();
        stream.write_all(&[5, 1, 0]).unwrap();
        stream.read_exact(&mut reply).unwrap();
        stream.write_all(&request).unwrap();
        stream.read_exact(&mut accepted).unwrap();
        assert_ne!(accepted[1], 0);
    }
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[test]
fn per_proxy_connection_bound_and_shutdown_revoke_all_open_streams() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let proxy = HostProxy::start(config(address, Arc::new(AtomicUsize::new(0)))).unwrap();
    let mut streams = Vec::new();
    for _ in 0..64 {
        let mut stream = UnixStream::connect(proxy.socket_path()).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        stream.write_all(&[5, 1, 0]).unwrap();
        let mut response = [0; 2];
        stream.read_exact(&mut response).unwrap();
        assert_eq!(response, [5, 0]);
        streams.push(stream);
    }
    let mut overflow = UnixStream::connect(proxy.socket_path()).unwrap();
    overflow
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let _ = overflow.write_all(&[5, 1, 0]);
    let mut byte = [0];
    assert!(closed_stream(overflow.read(&mut byte)));
    let path = proxy.socket_path().to_owned();
    drop(proxy);
    assert!(!path.exists());
    for mut stream in streams {
        assert!(closed_stream(stream.read(&mut byte)));
    }
}

fn closed_stream(result: std::io::Result<usize>) -> bool {
    match result {
        Ok(0) => true,
        Err(error) => matches!(
            error.kind(),
            std::io::ErrorKind::ConnectionReset
                | std::io::ErrorKind::ConnectionAborted
                | std::io::ErrorKind::BrokenPipe
        ),
        _ => false,
    }
}

#[test]
fn request_bodies_are_framed_once_and_pipelined_requests_cannot_change_the_origin() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let proxy = HostProxy::start(config(address, calls.clone())).unwrap();
    let server = thread::spawn(move || {
        for expected in [
            b"hello world".as_slice(),
            b"5\r\nhello\r\n6\r\n world\r\n0\r\n\r\n".as_slice(),
        ] {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut head = Vec::new();
            while !head.ends_with(b"\r\n\r\n") {
                let mut byte = [0];
                stream.read_exact(&mut byte).unwrap();
                head.push(byte[0]);
            }
            let mut body = Vec::new();
            stream.read_to_end(&mut body).unwrap();
            assert_eq!(body, expected);
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nOK")
                .unwrap();
        }
    });
    let suffix = "GET http://denied.test/never HTTP/1.1\r\nHost: denied.test\r\n\r\n";
    let length = format!(
        "POST http://allowed.test:{0}/upload HTTP/1.1\r\nHost: allowed.test:{0}\r\nContent-Length: 11\r\n\r\nhello world{suffix}",
        address.port()
    );
    assert!(request(&proxy, &length).ends_with("OK"));
    let chunked = format!(
        "POST http://allowed.test:{0}/upload HTTP/1.1\r\nHost: allowed.test:{0}\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n6\r\n world\r\n0\r\nX-Trailer: synthetic\r\n\r\n{suffix}",
        address.port()
    );
    assert!(request(&proxy, &chunked).ends_with("OK"));
    server.join().unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 2);
}

#[test]
fn explicit_runtime_parent_anchors_the_socket_and_rejects_relative_paths() {
    let parent = tempfile::tempdir().unwrap();
    let config = ProxyConfig::new(NetworkPolicy::Full);
    assert!(HostProxy::start_in(config.clone(), std::path::Path::new("relative-runtime")).is_err());
    let proxy = HostProxy::start_in(config, parent.path()).unwrap();
    let private = proxy.private_directory().to_owned();
    let socket = proxy.socket_path().to_owned();
    assert!(private.starts_with(parent.path()));
    assert!(socket.exists());
    drop(proxy);
    assert!(!private.exists());
    assert!(parent.path().is_dir());
}
