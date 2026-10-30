//! Served viewer endpoint over real TCP (9.3, 9.5).
//!
//! No Chrome is involved: the endpoint is exercised through real loopback
//! HTTP against both an in-memory credential store (full accept/reject
//! matrix) and the session-manager store (wiring without issuance).
use excalibur_webd::viewer::{ViewerStore, parse_viewer_http, serve_viewer, ws_accept_key};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

struct MemoryStore {
    codes: Mutex<HashMap<String, (String, String, i64, bool)>>,
}

impl MemoryStore {
    fn new() -> Self {
        let mut codes = HashMap::new();
        codes.insert(
            "GOODCODE".into(),
            ("tab-1".into(), "cookie-good".into(), i64::MAX / 2, false),
        );
        codes.insert(
            "OLDCODE".into(),
            ("tab-1".into(), "cookie-old".into(), 1, false),
        );
        Self {
            codes: Mutex::new(codes),
        }
    }
}

impl ViewerStore for MemoryStore {
    async fn redeem(&self, code: &str, _now: i64) -> Result<String, String> {
        let mut codes = self.codes.lock().unwrap();
        let Some((_, cookie, expires, used)) = codes.get_mut(code) else {
            return Err("invalid_code".into());
        };
        if *used {
            return Err("single_use_code_reused".into());
        }
        if *expires == 1 {
            return Err("code_expired".into());
        }
        *used = true;
        Ok(cookie.clone())
    }

    async fn check_page(&self, cookie: &str, _now: i64) -> Result<(String, String), String> {
        self.codes
            .lock()
            .unwrap()
            .values()
            .find(|(_, c, _, _)| c == cookie)
            .map(|(tab, _, _, _)| (tab.clone(), "https://one.test".into()))
            .ok_or_else(|| "missing_viewer_cookie".to_owned())
    }

    async fn check_socket(
        &self,
        cookie: &str,
        origin: Option<&str>,
        viewer_origin: &str,
        now: i64,
    ) -> Result<(String, String), String> {
        let page = self.check_page(cookie, now).await?;
        match origin {
            Some(value) if value == viewer_origin => Ok(page),
            _ => Err("foreign_origin".into()),
        }
    }
}

async fn roundtrip(port: u16, request: &str) -> (String, HashMap<String, String>, Vec<u8>) {
    let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .unwrap();
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 4096];
    let head_end = loop {
        let n = stream.read(&mut chunk).await.unwrap();
        assert!(n > 0, "server closed the connection");
        buffer.extend_from_slice(&chunk[..n]);
        if let Some(i) = buffer
            .windows(4)
            .position(|w| w == b"\r\n\r\n")
            .map(|i| i + 4)
        {
            break i;
        }
    };
    let head = String::from_utf8_lossy(&buffer[..head_end]).into_owned();
    let mut lines = head.lines();
    let status = lines.next().unwrap_or_default().to_owned();
    let mut headers = HashMap::new();
    for line in lines {
        if let Some((name, value)) = line.split_once(':') {
            headers.insert(name.trim().to_lowercase(), value.trim().to_owned());
        }
    }
    let content_length: usize = headers
        .get("content-length")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let mut body = buffer[head_end..].to_vec();
    while body.len() < content_length {
        let n = stream.read(&mut chunk).await.unwrap();
        assert!(n > 0, "truncated body");
        body.extend_from_slice(&chunk[..n]);
    }
    (status, headers, body)
}

async fn serve(store: Arc<MemoryStore>) -> u16 {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let origin = format!("http://127.0.0.1:{port}");
    tokio::spawn(async move { serve_viewer(listener, store, origin).await });
    port
}

#[tokio::test]
async fn viewer_serves_cookie_auth_origin_checks_and_csp_over_tcp() {
    let port = serve(Arc::new(MemoryStore::new())).await;
    let origin = format!("http://127.0.0.1:{port}");

    // Unknown code over the wire rejects closed with strict headers.
    let body = r#"{"code":"NOPE"}"#;
    let (status, headers, body) = roundtrip(
        port,
        &format!(
            "POST /auth HTTP/1.1\r\nhost: x\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        ),
    )
    .await;
    assert!(status.contains("403"), "{status}");
    assert!(String::from_utf8_lossy(&body).contains("invalid_code"));
    assert!(headers.contains_key("content-security-policy"));
    assert!(
        headers["content-security-policy"].contains("frame-ancestors 'none'"),
        "{headers:?}"
    );

    // Code in the URL is ignored: an empty body still rejects.
    let (status, _, _) = roundtrip(
        port,
        "POST /auth?code=GOODCODE HTTP/1.1\r\nhost: x\r\ncontent-length: 2\r\nconnection: close\r\n\r\n{}",
    )
    .await;
    assert!(status.contains("403"), "{status}");

    // Expired code rejects.
    let body = r#"{"code":"OLDCODE"}"#;
    let (status, _, body) = roundtrip(
        port,
        &format!(
            "POST /auth HTTP/1.1\r\nhost: x\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        ),
    )
    .await;
    assert!(status.contains("403"), "{status}");
    assert!(String::from_utf8_lossy(&body).contains("code_expired"));

    // Valid redeem sets the HttpOnly SameSite cookie.
    let body = r#"{"code":"GOODCODE"}"#;
    let (status, headers, _) = roundtrip(
        port,
        &format!(
            "POST /auth HTTP/1.1\r\nhost: x\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        ),
    )
    .await;
    assert!(status.contains("200"), "{status}");
    let set_cookie = headers.get("set-cookie").cloned().unwrap_or_default();
    assert!(set_cookie.contains("HttpOnly"), "{set_cookie}");
    assert!(set_cookie.contains("SameSite=Strict"), "{set_cookie}");
    let cookie = set_cookie.split(';').next().unwrap_or_default().to_owned();

    // Reused code rejects after the first redeem.
    let (status, _, body) = roundtrip(
        port,
        &format!(
            "POST /auth HTTP/1.1\r\nhost: x\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        ),
    )
    .await;
    assert!(status.contains("403"), "{status}");
    assert!(String::from_utf8_lossy(&body).contains("single_use_code_reused"));

    // Page loads require the cookie and carry CSP headers.
    let (status, _, _) = roundtrip(
        port,
        "GET / HTTP/1.1\r\nhost: x\r\nconnection: close\r\n\r\n",
    )
    .await;
    assert!(status.contains("403"), "{status}");
    let (status, headers, body) = roundtrip(
        port,
        &format!("GET / HTTP/1.1\r\nhost: x\r\ncookie: {cookie}\r\nconnection: close\r\n\r\n"),
    )
    .await;
    assert!(status.contains("200"), "{status}");
    assert!(headers.contains_key("content-security-policy"));
    assert!(headers["content-type"].contains("text/html"), "{headers:?}");
    assert!(String::from_utf8_lossy(&body).contains("https://one.test"));

    // Socket upgrade with a foreign Origin rejects; the exact origin upgrades.
    let key = "dGhlIHNhbXBsZSBub25jZQ==";
    let (status, _, body) = roundtrip(
        port,
        &format!(
            "GET /socket HTTP/1.1\r\nhost: x\r\nupgrade: websocket\r\nsec-websocket-key: {key}\r\ncookie: {cookie}\r\norigin: http://evil.test\r\nconnection: close\r\n\r\n"
        ),
    )
    .await;
    assert!(status.contains("403"), "{status}");
    assert!(String::from_utf8_lossy(&body).contains("foreign_origin"));
    let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .unwrap();
    stream
        .write_all(
            format!(
                "GET /socket HTTP/1.1\r\nhost: x\r\nupgrade: websocket\r\nsec-websocket-key: {key}\r\ncookie: {cookie}\r\norigin: {origin}\r\nconnection: Upgrade\r\n\r\n"
            )
            .as_bytes(),
        )
        .await
        .unwrap();
    let mut buffer = vec![0u8; 1024];
    let mut head = Vec::new();
    loop {
        let n = stream.read(&mut buffer).await.unwrap();
        assert!(n > 0);
        head.extend_from_slice(&buffer[..n]);
        if head.windows(4).any(|w| w == b"\r\n\r\n") {
            break;
        }
    }
    let head_text = String::from_utf8_lossy(&head);
    assert!(head_text.contains("101"), "{head_text}");
    assert!(head_text.contains(&ws_accept_key(key)), "{head_text}");
}

#[tokio::test]
async fn manager_store_rejects_unknown_codes_and_missing_cookies_over_tcp() {
    let dir = tempfile::tempdir().unwrap();
    let manager =
        excalibur_browser::sessions::Manager::open(excalibur_browser::sessions::Options {
            data_dir: dir.path().into(),
            binary: "/not-used".into(),
            proxy: "127.0.0.1:12345".parse().unwrap(),
            profiles: std::collections::BTreeMap::new(),
            compact: false,
            version: excalibur_browser::version::VersionPolicy::default(),
            chrome_args: vec![],
        })
        .await
        .unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let origin = format!("http://127.0.0.1:{port}");
    tokio::spawn(serve_viewer(listener, Arc::new(manager), origin));

    let body = r#"{"code":"NOPE"}"#;
    let (status, headers, body) = roundtrip(
        port,
        &format!(
            "POST /auth HTTP/1.1\r\nhost: x\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        ),
    )
    .await;
    assert!(status.contains("403"), "{status}");
    assert!(String::from_utf8_lossy(&body).contains("invalid_code"));
    assert!(headers.contains_key("content-security-policy"));
    let (status, _, _) = roundtrip(
        port,
        "GET / HTTP/1.1\r\nhost: x\r\nconnection: close\r\n\r\n",
    )
    .await;
    assert!(status.contains("403"), "{status}");
    // The pure parser backing the loop rejects oversized bodies closed.
    assert!(parse_viewer_http(b"GET / HTTP/1.1\r\n\r\n").is_some());
    let oversized = "POST /auth HTTP/1.1\r\ncontent-length: 70000\r\n\r\n";
    assert!(parse_viewer_http(oversized.as_bytes()).is_none());
}
