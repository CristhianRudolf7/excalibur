use super::{IO_TIMEOUT, ProxyConfig, normalize_host};
use std::{
    collections::{BTreeMap, BTreeSet},
    io::{self, BufRead, BufReader, Read, Write},
    net::{Ipv4Addr, Shutdown, TcpStream},
    os::unix::{
        fs::{MetadataExt, PermissionsExt},
        net::{UnixListener, UnixStream},
    },
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
    },
    thread,
    time::Duration,
};
const HEADER_LIMIT: usize = 32768;
const BODY_LIMIT: u64 = 1024 * 1024 * 1024;
const MAX_CONNECTIONS: usize = 64;
const GLOBAL_CONNECTIONS: usize = 256;
static ACTIVE: AtomicUsize = AtomicUsize::new(0);
struct GlobalPermit;
impl GlobalPermit {
    fn acquire() -> Option<Self> {
        let mut used = ACTIVE.load(Ordering::Acquire);
        loop {
            if used >= GLOBAL_CONNECTIONS {
                return None;
            }
            match ACTIVE.compare_exchange_weak(used, used + 1, Ordering::AcqRel, Ordering::Acquire)
            {
                Ok(_) => return Some(Self),
                Err(actual) => used = actual,
            }
        }
    }
}
impl Drop for GlobalPermit {
    fn drop(&mut self) {
        ACTIVE.fetch_sub(1, Ordering::AcqRel);
    }
}

enum OpenStream {
    Unix(UnixStream),
    Tcp(TcpStream),
}
impl OpenStream {
    fn close(&self) {
        let _ = match self {
            Self::Unix(stream) => stream.shutdown(Shutdown::Both),
            Self::Tcp(stream) => stream.shutdown(Shutdown::Both),
        };
    }
}
struct Shared {
    closed: AtomicBool,
    next: AtomicU64,
    connections: Mutex<BTreeMap<u64, Vec<OpenStream>>>,
}
struct Connection {
    shared: Arc<Shared>,
    id: u64,
    _permit: GlobalPermit,
}
impl Drop for Connection {
    fn drop(&mut self) {
        self.shared
            .connections
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&self.id);
    }
}
/// Owns the private pathname, listener and active network streams. Drop revokes
/// every connection; the socket is never bound on the host's TCP interfaces.
pub struct HostProxy {
    private: tempfile::TempDir,
    path: PathBuf,
    shared: Arc<Shared>,
    listener: Option<thread::JoinHandle<()>>,
}
impl HostProxy {
    pub fn start(config: ProxyConfig) -> Result<Self, String> {
        // Never follow TMPDIR: a workload may already have write authority
        // there. The standalone host adapter uses a fixed runtime location.
        for parent in [Path::new("/tmp"), Path::new("/var/tmp")] {
            if let Ok(metadata) = std::fs::metadata(parent)
                && metadata.is_dir()
                && metadata.uid() == 0
                && (metadata.permissions().mode() & 0o022 == 0
                    || metadata.permissions().mode() & 0o1000 != 0)
            {
                return Self::start_in(config, parent);
            }
        }
        Err("no trusted fixed proxy runtime directory is available".into())
    }
    /// The host supplies an absolute directory outside every workload write
    /// root. PreparedSandbox anchors this under its own protected runtime tree.
    pub fn start_in(config: ProxyConfig, parent: &Path) -> Result<Self, String> {
        config.validate()?;
        if !parent.is_absolute() {
            return Err("proxy runtime parent must be absolute".into());
        }
        let parent = parent
            .canonicalize()
            .map_err(|_| "proxy runtime parent is unavailable")?;
        if !parent.is_dir() {
            return Err("proxy runtime parent must be a directory".into());
        }
        let private = tempfile::Builder::new()
            .prefix("excalibur-egress-")
            .tempdir_in(parent)
            .map_err(|_| "proxy directory creation failed")?;
        let path = private.path().join("proxy.sock");
        let listener = UnixListener::bind(&path).map_err(|_| "proxy socket bind failed")?;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
            .map_err(|_| "proxy socket permissions failed")?;
        listener
            .set_nonblocking(true)
            .map_err(|_| "proxy listener setup failed")?;
        let shared = Arc::new(Shared {
            closed: AtomicBool::new(false),
            next: AtomicU64::new(0),
            connections: Mutex::new(BTreeMap::new()),
        });
        let worker = shared.clone();
        let config = Arc::new(config);
        let handle = thread::spawn(move || {
            while !worker.closed.load(Ordering::Acquire) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        let id = worker.next.fetch_add(1, Ordering::Relaxed);
                        let mut active =
                            worker.connections.lock().unwrap_or_else(|e| e.into_inner());
                        if active.len() >= MAX_CONNECTIONS {
                            continue;
                        }
                        let Ok(tracked) = stream.try_clone() else {
                            continue;
                        };
                        let Some(permit) = GlobalPermit::acquire() else {
                            continue;
                        };
                        active.insert(id, vec![OpenStream::Unix(tracked)]);
                        drop(active);
                        let connection = Connection {
                            shared: worker.clone(),
                            id,
                            _permit: permit,
                        };
                        let config = config.clone();
                        let _ = thread::Builder::new()
                            .name("excalibur-egress".into())
                            .spawn(move || {
                                let _ = serve(stream, &config, &connection);
                            });
                    }
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(5))
                    }
                    Err(_) => break,
                }
            }
        });
        Ok(Self {
            private,
            path,
            shared,
            listener: Some(handle),
        })
    }
    pub fn socket_path(&self) -> &Path {
        &self.path
    }
    pub fn private_directory(&self) -> &Path {
        self.private.path()
    }
}
impl Drop for HostProxy {
    fn drop(&mut self) {
        self.shared.closed.store(true, Ordering::Release);
        for streams in self
            .shared
            .connections
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .values()
        {
            for stream in streams {
                stream.close();
            }
        }
        if let Some(listener) = self.listener.take() {
            let _ = listener.join();
        }
    }
}
fn serve(
    mut client: UnixStream,
    config: &ProxyConfig,
    connection: &Connection,
) -> Result<(), String> {
    client
        .set_read_timeout(Some(IO_TIMEOUT))
        .map_err(|_| "client timeout setup failed")?;
    client
        .set_write_timeout(Some(IO_TIMEOUT))
        .map_err(|_| "client timeout setup failed")?;
    let mut first = [0];
    client
        .read_exact(&mut first)
        .map_err(|_| "empty proxy request")?;
    if first[0] == 5 {
        socks(client, config, connection)
    } else {
        http(client, first[0], config, connection)
    }
}
fn track(upstream: &TcpStream, connection: &Connection) -> Result<(), String> {
    let clone = upstream
        .try_clone()
        .map_err(|_| "upstream tracking failed")?;
    let mut active = connection
        .shared
        .connections
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if connection.shared.closed.load(Ordering::Acquire) {
        return Err("proxy closed".into());
    }
    active
        .get_mut(&connection.id)
        .ok_or("connection closed")?
        .push(OpenStream::Tcp(clone));
    Ok(())
}
fn tunnel(client: UnixStream, upstream: TcpStream) -> Result<(), String> {
    let mut client_read = client.try_clone().map_err(|_| "client clone failed")?;
    let mut upstream_write = upstream.try_clone().map_err(|_| "upstream clone failed")?;
    thread::scope(|scope| {
        scope.spawn(move || {
            let _ = io::copy(&mut client_read, &mut upstream_write);
            let _ = upstream_write.shutdown(Shutdown::Write);
        });
        let (mut upstream, mut client) = (upstream, client);
        let _ = io::copy(&mut upstream, &mut client);
        let _ = client.shutdown(Shutdown::Both);
        let _ = upstream.shutdown(Shutdown::Both);
    });
    Ok(())
}
fn socks(
    mut client: UnixStream,
    config: &ProxyConfig,
    connection: &Connection,
) -> Result<(), String> {
    let mut count = [0];
    client
        .read_exact(&mut count)
        .map_err(|_| "incomplete SOCKS greeting")?;
    let mut methods = vec![0; count[0] as usize];
    client
        .read_exact(&mut methods)
        .map_err(|_| "incomplete SOCKS methods")?;
    if !methods.contains(&0) {
        let _ = client.write_all(&[5, 255]);
        return Err("SOCKS authentication method unsupported".into());
    }
    client
        .write_all(&[5, 0])
        .map_err(|_| "SOCKS response failed")?;
    let mut head = [0; 4];
    client
        .read_exact(&mut head)
        .map_err(|_| "incomplete SOCKS request")?;
    if head[0] != 5 || head[1] != 1 || head[2] != 0 {
        let _ = client.write_all(&[5, 7, 0, 1, 0, 0, 0, 0, 0, 0]);
        return Err("only SOCKS5 TCP CONNECT is supported".into());
    }
    let host = match head[3] {
        1 => {
            let mut ip = [0; 4];
            client.read_exact(&mut ip).map_err(|_| "incomplete IPv4")?;
            Ipv4Addr::from(ip).to_string()
        }
        3 => {
            let mut n = [0];
            client
                .read_exact(&mut n)
                .map_err(|_| "incomplete hostname")?;
            let mut bytes = vec![0; n[0] as usize];
            client
                .read_exact(&mut bytes)
                .map_err(|_| "incomplete hostname")?;
            String::from_utf8(bytes).map_err(|_| "invalid SOCKS hostname")?
        }
        4 => {
            let mut ip = [0; 16];
            client.read_exact(&mut ip).map_err(|_| "incomplete IPv6")?;
            std::net::Ipv6Addr::from(ip).to_string()
        }
        _ => {
            let _ = client.write_all(&[5, 8, 0, 1, 0, 0, 0, 0, 0, 0]);
            return Err("SOCKS address type unsupported".into());
        }
    };
    let mut port = [0; 2];
    client
        .read_exact(&mut port)
        .map_err(|_| "incomplete SOCKS port")?;
    let upstream = match config.connect(&host, u16::from_be_bytes(port), &connection.shared.closed)
    {
        Ok(stream) => stream,
        Err(error) => {
            let _ = client.write_all(&[5, 2, 0, 1, 0, 0, 0, 0, 0, 0]);
            return Err(error);
        }
    };
    track(&upstream, connection)?;
    client
        .write_all(&[5, 0, 0, 1, 0, 0, 0, 0, 0, 0])
        .map_err(|_| "SOCKS response failed")?;
    tunnel(client, upstream)
}
fn authority(value: &str, default: Option<u16>) -> Result<(String, u16), String> {
    if value.bytes().any(|b| b <= 32)
        || value.contains('@')
        || value.contains('/')
        || value.contains('\\')
    {
        return Err("invalid authority".into());
    }
    let url = url::Url::parse(&format!("http://{value}")).map_err(|_| "invalid authority")?;
    if !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
        || url.query().is_some()
        || url.path() != "/"
    {
        return Err("invalid authority".into());
    }
    let explicit = if value.starts_with('[') {
        value.rsplit_once("]:").map(|(_, port)| port)
    } else {
        value.rsplit_once(':').map(|(_, port)| port)
    };
    let port = match explicit {
        Some(port) => port
            .parse::<u16>()
            .ok()
            .filter(|p| *p != 0)
            .ok_or("invalid port")?,
        None => default.ok_or("CONNECT requires explicit port")?,
    };
    Ok((
        normalize_host(
            url.host_str()
                .ok_or("hostname missing")?
                .trim_matches(['[', ']']),
        )?,
        port,
    ))
}
fn reply_error(client: &mut UnixStream, status: u16) {
    let body = b"Network proxy request denied\n";
    let _ = write!(
        client,
        "HTTP/1.1 {status} Denied\r\nConnection: close\r\nContent-Length: {}\r\n\r\n",
        body.len()
    );
    let _ = client.write_all(body);
}
fn http(
    mut client: UnixStream,
    first: u8,
    config: &ProxyConfig,
    connection: &Connection,
) -> Result<(), String> {
    let mut head = vec![first];
    while !head.ends_with(b"\r\n\r\n") {
        if head.len() >= HEADER_LIMIT {
            reply_error(&mut client, 431);
            return Err("request headers exceed limit".into());
        }
        let mut byte = [0];
        client
            .read_exact(&mut byte)
            .map_err(|_| "incomplete HTTP headers")?;
        head.push(byte[0]);
    }
    let result = http_request(&mut client, &head, config, connection);
    if let Err(error) = result {
        reply_error(&mut client, 403);
        return Err(error);
    }
    Ok(())
}
fn http_request(
    client: &mut UnixStream,
    head: &[u8],
    config: &ProxyConfig,
    connection: &Connection,
) -> Result<(), String> {
    let mut headers = [httparse::EMPTY_HEADER; 128];
    let mut request = httparse::Request::new(&mut headers);
    if !request
        .parse(head)
        .map_err(|_| "malformed HTTP request")?
        .is_complete()
    {
        return Err("incomplete HTTP request".into());
    }
    let method = request.method.ok_or("missing HTTP method")?;
    let target = request.path.ok_or("missing HTTP target")?;
    let hosts = request
        .headers
        .iter()
        .filter(|h| h.name.eq_ignore_ascii_case("Host"))
        .collect::<Vec<_>>();
    if hosts.len() != 1 {
        return Err("exactly one Host header required".into());
    }
    let host_value = std::str::from_utf8(hosts[0].value).map_err(|_| "invalid Host")?;
    let lengths = request
        .headers
        .iter()
        .filter(|h| h.name.eq_ignore_ascii_case("Content-Length"))
        .map(|h| {
            std::str::from_utf8(h.value)
                .ok()
                .filter(|s| !s.is_empty() && s.bytes().all(|byte| byte.is_ascii_digit()))
                .and_then(|s| s.parse::<u64>().ok())
                .ok_or("invalid Content-Length")
        })
        .collect::<Result<Vec<_>, _>>()?;
    let transfer = request
        .headers
        .iter()
        .filter(|h| h.name.eq_ignore_ascii_case("Transfer-Encoding"))
        .collect::<Vec<_>>();
    if lengths.len() > 1
        || transfer.len() > 1
        || !lengths.is_empty() && !transfer.is_empty()
        || transfer
            .first()
            .is_some_and(|h| !h.value.eq_ignore_ascii_case(b"chunked"))
    {
        return Err("ambiguous HTTP request framing".into());
    }
    let (host, port, path) = if method == "CONNECT" {
        let (host, port) = authority(target, None)?;
        (host, port, String::new())
    } else {
        let url = url::Url::parse(target).map_err(|_| "HTTP proxy requires absolute-form URI")?;
        if url.scheme() != "http"
            || !url.username().is_empty()
            || url.password().is_some()
            || url.fragment().is_some()
        {
            return Err("unsupported HTTP URI".into());
        }
        let host = normalize_host(
            url.host_str()
                .ok_or("hostname required")?
                .trim_matches(['[', ']']),
        )?;
        let port = url.port_or_known_default().ok_or("port required")?;
        let mut path = url.path().to_owned();
        if let Some(query) = url.query() {
            path.push('?');
            path.push_str(query);
        }
        (host, port, path)
    };
    if authority(
        host_value,
        Some(if method == "CONNECT" { port } else { 80 }),
    )? != (host.clone(), port)
    {
        return Err("Host header differs from proxy destination".into());
    }
    if method == "CONNECT"
        && (!transfer.is_empty() || lengths.first().is_some_and(|length| *length != 0))
    {
        return Err("CONNECT request bodies are forbidden".into());
    }
    let length = lengths.first().copied().unwrap_or(0);
    if length > BODY_LIMIT {
        return Err("HTTP request body exceeds limit".into());
    }
    let mut hop_headers = BTreeSet::new();
    for header in request
        .headers
        .iter()
        .filter(|header| header.name.eq_ignore_ascii_case("Connection"))
    {
        let value = std::str::from_utf8(header.value).map_err(|_| "invalid Connection header")?;
        for token in value.split(',').map(str::trim) {
            if token.is_empty()
                || !token
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&byte))
            {
                return Err("invalid Connection option".into());
            }
            let token = token.to_ascii_lowercase();
            if matches!(
                token.as_str(),
                "host" | "content-length" | "transfer-encoding"
            ) {
                return Err("Connection cannot change request authority or framing".into());
            }
            hop_headers.insert(token);
        }
    }
    let mut upstream = config.connect(&host, port, &connection.shared.closed)?;
    track(&upstream, connection)?;
    if method == "CONNECT" {
        client
            .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
            .map_err(|_| "CONNECT response failed")?;
        return tunnel(
            client.try_clone().map_err(|_| "client clone failed")?,
            upstream,
        );
    }
    write!(upstream, "{method} {path} HTTP/1.1\r\n").map_err(|_| "upstream write failed")?;
    for header in request.headers.iter() {
        if [
            "Host",
            "Connection",
            "Proxy-Connection",
            "Proxy-Authorization",
            "Keep-Alive",
            "Upgrade",
            "TE",
            "Trailer",
        ]
        .iter()
        .any(|name| header.name.eq_ignore_ascii_case(name))
            || hop_headers.contains(&header.name.to_ascii_lowercase())
        {
            continue;
        }
        upstream
            .write_all(header.name.as_bytes())
            .and_then(|_| upstream.write_all(b": "))
            .and_then(|_| upstream.write_all(header.value))
            .and_then(|_| upstream.write_all(b"\r\n"))
            .map_err(|_| "upstream headers failed")?;
    }
    let authority = if host.contains(':') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    };
    write!(upstream, "Host: {authority}\r\n").map_err(|_| "upstream authority write failed")?;
    upstream
        .write_all(b"Connection: close\r\n\r\n")
        .map_err(|_| "upstream headers failed")?;
    let mut input = BufReader::new(client.try_clone().map_err(|_| "client clone failed")?);
    let mut writer = upstream.try_clone().map_err(|_| "upstream clone failed")?;
    thread::scope(|scope| {
        scope.spawn(move || {
            if !transfer.is_empty() {
                let _ = chunked(&mut input, &mut writer);
            } else {
                let _ = io::copy(&mut input.take(length), &mut writer);
            }
            let _ = writer.shutdown(Shutdown::Write);
        });
        let _ = io::copy(&mut upstream, client);
        let _ = client.shutdown(Shutdown::Both);
        let _ = upstream.shutdown(Shutdown::Both);
    });
    Ok(())
}
fn chunked(input: &mut impl BufRead, output: &mut impl Write) -> Result<(), String> {
    let mut total = 0_u64;
    loop {
        let mut line = Vec::new();
        input
            .take(1025)
            .read_until(b'\n', &mut line)
            .map_err(|_| "chunk header failed")?;
        if line.len() > 1024 || !line.ends_with(b"\r\n") {
            return Err("invalid chunk header".into());
        }
        let value = std::str::from_utf8(&line[..line.len() - 2])
            .map_err(|_| "invalid chunk size")?
            .split(';')
            .next()
            .ok_or("chunk size missing")?;
        if value.is_empty() || !value.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err("invalid chunk size".into());
        }
        let size = u64::from_str_radix(value, 16).map_err(|_| "invalid chunk size")?;
        total = total.checked_add(size).ok_or("chunk size overflow")?;
        if total > BODY_LIMIT {
            return Err("request body exceeds limit".into());
        }
        write!(output, "{size:x}\r\n").map_err(|_| "chunk write failed")?;
        if size == 0 {
            let mut used = 0;
            loop {
                let mut trailer = Vec::new();
                input
                    .take(HEADER_LIMIT as u64 + 1)
                    .read_until(b'\n', &mut trailer)
                    .map_err(|_| "trailer read failed")?;
                used += trailer.len();
                if used > HEADER_LIMIT || !trailer.ends_with(b"\r\n") {
                    return Err("invalid trailer".into());
                }
                if trailer == b"\r\n" {
                    break;
                }
            }
            output.write_all(b"\r\n").map_err(|_| "chunk end failed")?;
            return Ok(());
        }
        let copied = io::copy(&mut input.take(size), output).map_err(|_| "chunk body failed")?;
        if copied != size {
            return Err("incomplete chunk body".into());
        }
        let mut crlf = [0; 2];
        input
            .read_exact(&mut crlf)
            .map_err(|_| "chunk delimiter missing")?;
        if crlf != *b"\r\n" {
            return Err("invalid chunk delimiter".into());
        }
        output
            .write_all(b"\r\n")
            .map_err(|_| "chunk write failed")?;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn global_connection_quota_is_shared_bounded_and_released_by_raii() {
        let permits = (0..GLOBAL_CONNECTIONS)
            .map(|_| GlobalPermit::acquire().unwrap())
            .collect::<Vec<_>>();
        assert!(GlobalPermit::acquire().is_none());
        drop(permits);
        let permit = GlobalPermit::acquire();
        assert!(permit.is_some());
        drop(permit);
        assert_eq!(ACTIVE.load(Ordering::Acquire), 0);
    }
}
