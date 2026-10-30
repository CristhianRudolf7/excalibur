//! Human intervention viewer: one-time code, cookie auth and strict CSP (9.3).
//!
//! webd serves the viewer on `127.0.0.1:<random>` while an intervention lasts.
//! Authentication never places the code in the URL. The page sends it by POST
//! and receives an `HttpOnly; SameSite=Strict` cookie bound to one tab and
//! deadline; the WebSocket requires that cookie plus a matching `Origin`.
//! Page-controlled strings are rendered with `textContent` only.
use sha2::{Digest, Sha256};
use std::collections::HashMap;

pub const CODE_LEN: usize = 8;
pub const CODE_TTL_SECS: i64 = 600;

const ALPHABET: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789";

pub fn generate_code() -> String {
    let seed = format!(
        "{}-{}-{}",
        crate_viewer_now(),
        std::process::id(),
        uuid_simple()
    );
    let digest = Sha256::digest(seed.as_bytes());
    let mut out = String::with_capacity(CODE_LEN);
    for byte in digest.iter().take(CODE_LEN) {
        out.push(ALPHABET[*byte as usize % ALPHABET.len()] as char);
    }
    out
}

fn uuid_simple() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!("{nanos:x}-{}", std::process::id())
}

fn crate_viewer_now() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

pub fn code_hash(code: &str) -> String {
    format!("{:x}", Sha256::digest(code.as_bytes()))
}

/// Strict viewer response headers (9.3).
pub fn security_headers() -> Vec<(String, String)> {
    vec![
        (
            "content-security-policy".into(),
            "default-src 'none'; img-src data: blob:; script-src 'self'; style-src 'self'; connect-src 'self'; frame-ancestors 'none'; base-uri 'none'; form-action 'self'; require-trusted-types-for 'script'".into(),
        ),
        ("x-content-type-options".into(), "nosniff".into()),
        ("referrer-policy".into(), "no-referrer".into()),
        ("cache-control".into(), "no-store".into()),
    ]
}

/// Escape page-controlled text for the JSON payload and HTML shell.
pub fn escape_text(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#x27;"),
            _ => out.push(ch),
        }
    }
    out
}

/// Minimal viewer shell. Title and origin are escaped; the script uses
/// `textContent` and `canvas`, never `innerHTML`.
pub fn render_shell(origin: &str, title: &str) -> String {
    let origin = escape_text(origin);
    let title = escape_text(title);
    format!(
        r#"<!doctype html><html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1"><title>Intervention</title><link rel="stylesheet" href="/style.css"></head><body><header><div id="origin"></div><div id="state"></div><div id="controls"><button id="done">Terminado</button><button id="giveup">No puedo</button><button id="back">Atrás</button><button id="reload">Recargar</button></div></header><main><canvas id="screen"></canvas><input id="paste" type="text" aria-label="paste box"></main><script>window.__origin={origin_json};window.__title={title_json};</script><script src="/viewer.js"></script></body></html>"#,
        origin_json = serde_json::to_string(&origin).unwrap_or_default(),
        title_json = serde_json::to_string(&title).unwrap_or_default(),
    )
}

/// Viewer credential bound to one tab and deadline.
#[derive(Clone, Debug)]
pub struct ViewerAuth {
    pub tab_id: String,
    pub code_hash: String,
    pub issued_at: i64,
    pub expires_at: i64,
    pub used: bool,
    pub cookie: String,
}

impl ViewerAuth {
    pub fn issue(tab_id: &str, now: i64) -> (Self, String) {
        let code = generate_code();
        let cookie = format!(
            "{:x}",
            Sha256::digest(format!("{code}-{tab_id}-{now}").as_bytes())
        );
        let auth = Self {
            tab_id: tab_id.into(),
            code_hash: code_hash(&code),
            issued_at: now,
            expires_at: now + CODE_TTL_SECS,
            used: false,
            cookie: cookie.clone(),
        };
        (auth, code)
    }

    /// Validate a POSTed code without consuming it.
    pub fn check_code(&self, code: &str, now: i64) -> Result<(), String> {
        if self.used {
            return Err("single_use_code_reused".into());
        }
        if now > self.expires_at {
            return Err("code_expired".into());
        }
        if code_hash(code) != self.code_hash {
            return Err("invalid_code".into());
        }
        Ok(())
    }

    pub fn consume_code(&mut self, code: &str, now: i64) -> Result<String, String> {
        self.check_code(code, now)?;
        self.used = true;
        Ok(self.cookie.clone())
    }

    pub fn check_cookie(&self, cookie: &str, now: i64) -> Result<(), String> {
        if cookie != self.cookie {
            return Err("missing_viewer_cookie".into());
        }
        if now > self.expires_at {
            return Err("intervention_expired".into());
        }
        Ok(())
    }

    /// WebSocket upgrade requires the viewer cookie and an exact Origin match.
    pub fn check_socket(
        &self,
        cookie: Option<&str>,
        origin: Option<&str>,
        viewer_origin: &str,
        now: i64,
    ) -> Result<(), String> {
        let cookie = cookie.ok_or("missing_viewer_cookie")?;
        self.check_cookie(cookie, now)?;
        match origin {
            Some(value) if value == viewer_origin => Ok(()),
            _ => Err("foreign_origin".into()),
        }
    }

    pub fn set_cookie_header(&self) -> String {
        format!(
            "viewer={}; HttpOnly; SameSite=Strict; Path=/; Max-Age={}",
            self.cookie, CODE_TTL_SECS
        )
    }
}

/// Parse a `Cookie` header into a map without logging values.
pub fn parse_cookies(header: &str) -> HashMap<String, String> {
    let mut out = HashMap::new();
    for part in header.split(';') {
        if let Some((key, value)) = part.trim().split_once('=') {
            out.insert(key.trim().to_owned(), value.trim().to_owned());
        }
    }
    out
}

/// Responsive viewer stylesheet (18.5).
///
/// Small viewports stack the controls and scale the canvas to the width;
/// large viewports keep the canvas 1:1 up to 1280 px and centered. Buttons
/// stay at least 44 px tall on touch sizes.
pub const STYLE_CSS: &str = r#"*{box-sizing:border-box}
body{margin:0;font-family:system-ui,sans-serif;background:#111;color:#eee}
header{padding:8px 12px;border-bottom:1px solid #333}
#origin{font-weight:700;overflow-wrap:anywhere}
#state{color:#9cf}
main{display:flex;flex-direction:column;gap:8px;padding:8px}
#screen{max-width:100%;height:auto;display:block;background:#000}
#controls{display:flex;flex-direction:column;gap:8px}
#controls button{min-height:44px;min-width:44px;font-size:16px}
#paste{min-height:44px;font-size:16px;width:100%}
@media(min-width:900px){
main{align-items:center}
#screen{width:1280px;max-width:100%}
#controls{flex-direction:row}
}"#;

/// Minimal viewer client (9.3). Page-controlled strings only reach the DOM
/// through `textContent`, canvas images and `document.title` assignment;
/// never through `innerHTML`.
pub const VIEWER_JS: &str = r#"(async()=>{
const origin=window.__origin||"";
document.getElementById("origin").textContent=origin;
document.title="Intervention";
const state=document.getElementById("state");
const canvas=document.getElementById("screen");
const ctx=canvas.getContext("2d");
const socket=new WebSocket("ws://"+location.host+"/socket");
socket.binaryType="arraybuffer";
socket.onmessage=(event)=>{
if(typeof event.data==="string"){
try{
const message=JSON.parse(event.data);
if(state&&message.state)state.textContent=String(message.state);
}catch(_){}
return;
}
const url=URL.createObjectURL(new Blob([event.data],{type:"image/jpeg"}));
const image=new Image();
image.onload=()=>{
canvas.width=image.width;
canvas.height=image.height;
ctx.drawImage(image,0,0);
URL.revokeObjectURL(url);
};
image.src=url;
};
const send=(message)=>{if(socket.readyState===1)socket.send(JSON.stringify(message));};
for(const [id,type] of [["done","done"],["giveup","giveup"],["back","back"],["reload","reload"]]){
const button=document.getElementById(id);
if(button)button.addEventListener("click",()=>send({type}));
}
const paste=document.getElementById("paste");
if(paste)paste.addEventListener("change",()=>{
send({type:"text",text:paste.value});
paste.value="";
});
})();"#;

/// Shared stores behind `Arc` forward to the inner implementation.
impl<T: ViewerStore> ViewerStore for std::sync::Arc<T> {
    async fn redeem(&self, code: &str, now: i64) -> Result<String, String> {
        self.as_ref().redeem(code, now).await
    }

    async fn check_page(&self, cookie: &str, now: i64) -> Result<(String, String), String> {
        self.as_ref().check_page(cookie, now).await
    }

    async fn check_socket(
        &self,
        cookie: &str,
        origin: Option<&str>,
        viewer_origin: &str,
        now: i64,
    ) -> Result<(String, String), String> {
        self.as_ref()
            .check_socket(cookie, origin, viewer_origin, now)
            .await
    }
}

/// Minimal HTTP response for the served viewer endpoint.
pub struct HttpResponse {
    pub status: u16,
    pub reason: &'static str,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl HttpResponse {
    fn json(status: u16, reason: &'static str, value: &serde_json::Value) -> Self {
        Self::with_body(
            status,
            reason,
            "application/json",
            serde_json::to_vec(value).unwrap_or_default(),
        )
    }

    fn with_body(status: u16, reason: &'static str, content_type: &str, body: Vec<u8>) -> Self {
        let mut headers = security_headers();
        headers.push(("content-type".into(), content_type.into()));
        Self {
            status,
            reason,
            headers,
            body,
        }
    }

    /// Serialize as a `Connection: close` HTTP/1.1 response.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = format!(
            "HTTP/1.1 {} {}\r\nconnection: close\r\ncontent-length: {}\r\n",
            self.status,
            self.reason,
            self.body.len()
        );
        for (name, value) in &self.headers {
            out.push_str(&format!("{name}: {value}\r\n"));
        }
        out.push_str("\r\n");
        let mut bytes = out.into_bytes();
        bytes.extend_from_slice(&self.body);
        bytes
    }
}

/// Credential store backing the served viewer endpoint.
///
/// The production implementation lives on the browser session manager so
/// issuance stays gated on `human_control`; tests use an in-memory fake.
pub trait ViewerStore: Send + Sync {
    fn redeem(
        &self,
        code: &str,
        now: i64,
    ) -> impl std::future::Future<Output = Result<String, String>> + Send;
    fn check_page(
        &self,
        cookie: &str,
        now: i64,
    ) -> impl std::future::Future<Output = Result<(String, String), String>> + Send;
    fn check_socket(
        &self,
        cookie: &str,
        origin: Option<&str>,
        viewer_origin: &str,
        now: i64,
    ) -> impl std::future::Future<Output = Result<(String, String), String>> + Send;
}

/// Pure SHA-1 for the WebSocket handshake (RFC 6455 §1.3).
fn sha1_hexblock(data: &[u8]) -> [u8; 20] {
    let mut h: [u32; 5] = [0x67452301, 0xEFCDAB89, 0x98BADCFE, 0x10325476, 0xC3D2E1F0];
    let mut message = data.to_vec();
    let bits = (data.len() as u64).wrapping_mul(8);
    message.push(0x80);
    while message.len() % 64 != 56 {
        message.push(0);
    }
    message.extend_from_slice(&bits.to_be_bytes());
    let (blocks, _) = message.as_chunks::<64>();
    for block in blocks {
        let mut w = [0u32; 80];
        for (i, word) in w.iter_mut().take(16).enumerate() {
            *word = u32::from_be_bytes([
                block[4 * i],
                block[4 * i + 1],
                block[4 * i + 2],
                block[4 * i + 3],
            ]);
        }
        for i in 16..80 {
            w[i] = (w[i - 3] ^ w[i - 8] ^ w[i - 14] ^ w[i - 16]).rotate_left(1);
        }
        let (mut a, mut b, mut c, mut d, mut e) = (h[0], h[1], h[2], h[3], h[4]);
        for (i, word) in w.iter().enumerate() {
            let (f, k) = match i {
                0..20 => ((b & c) | ((!b) & d), 0x5A827999),
                20..40 => (b ^ c ^ d, 0x6ED9EBA1),
                40..60 => ((b & c) | (b & d) | (c & d), 0x8F1BBCDC),
                _ => (b ^ c ^ d, 0xCA62C1D6),
            };
            let temp = a
                .rotate_left(5)
                .wrapping_add(f)
                .wrapping_add(e)
                .wrapping_add(k)
                .wrapping_add(*word);
            e = d;
            d = c;
            c = b.rotate_left(30);
            b = a;
            a = temp;
        }
        for (slot, v) in h.iter_mut().zip([a, b, c, d, e]) {
            *slot = slot.wrapping_add(v);
        }
    }
    let mut out = [0u8; 20];
    for (i, word) in h.iter().enumerate() {
        out[4 * i..4 * i + 4].copy_from_slice(&word.to_be_bytes());
    }
    out
}

fn base64_encode(data: &[u8]) -> String {
    const TABLE: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in data.chunks(3) {
        let mut n = 0u32;
        for (i, byte) in chunk.iter().enumerate() {
            n |= (*byte as u32) << (16 - 8 * i);
        }
        let chars = (chunk.len() * 8).div_ceil(6);
        for i in 0..4 {
            if i < chars {
                out.push(TABLE[((n >> (18 - 6 * i)) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// Compute `Sec-WebSocket-Accept` for a client key (RFC 6455 example vector
/// is covered by unit tests).
pub fn ws_accept_key(client_key: &str) -> String {
    base64_encode(&sha1_hexblock(
        format!("{client_key}258EAFA5-E914-47DA-95CA-C5AB0DC85B11").as_bytes(),
    ))
}

/// Split an HTTP target into its path, ignoring any query string.
///
/// Authentication codes are never read from the URL, so queries are ignored
/// by construction: `POST /auth?code=...` still requires the code in the
/// POST body.
pub fn viewer_path(target: &str) -> &str {
    target.split('?').next().unwrap_or(target)
}

fn bearer_cookie(headers: &std::collections::HashMap<String, String>) -> Option<String> {
    let header = headers.get("cookie")?;
    parse_cookies(header).get("viewer").cloned()
}

/// Serve one viewer HTTP request against the credential store (9.3).
///
/// Rejections are deterministic: unknown codes, reused codes, expired
/// codes, missing cookies and foreign origins all fail closed with a JSON
/// error and the strict viewer headers.
pub async fn handle_viewer_request<S: ViewerStore>(
    store: &S,
    method: &str,
    target: &str,
    headers: &std::collections::HashMap<String, String>,
    body: &str,
    now: i64,
    viewer_origin: &str,
) -> HttpResponse {
    let path = viewer_path(target);
    match (method, path) {
        ("POST", "/auth") => {
            let code = serde_json::from_str::<serde_json::Value>(body)
                .ok()
                .and_then(|v| v["code"].as_str().map(str::to_owned))
                .unwrap_or_default();
            match store.redeem(&code, now).await {
                Ok(cookie) => {
                    let auth = ViewerAuth {
                        tab_id: String::new(),
                        code_hash: String::new(),
                        issued_at: now,
                        expires_at: now + CODE_TTL_SECS,
                        used: true,
                        cookie,
                    };
                    let mut response =
                        HttpResponse::json(200, "OK", &serde_json::json!({"ok": true}));
                    response
                        .headers
                        .push(("set-cookie".into(), auth.set_cookie_header()));
                    response
                }
                Err(error) => {
                    HttpResponse::json(403, "Forbidden", &serde_json::json!({"error": error}))
                }
            }
        }
        ("GET", "/" | "/viewer") => match bearer_cookie(headers) {
            Some(cookie) => match store.check_page(&cookie, now).await {
                Ok((_, origin)) => HttpResponse::with_body(
                    200,
                    "OK",
                    "text/html; charset=utf-8",
                    render_shell(&origin, "").into_bytes(),
                ),
                Err(error) => {
                    HttpResponse::json(403, "Forbidden", &serde_json::json!({"error": error}))
                }
            },
            None => HttpResponse::json(
                403,
                "Forbidden",
                &serde_json::json!({"error": "missing_viewer_cookie"}),
            ),
        },
        ("GET", "/viewer.js") => match bearer_cookie(headers) {
            Some(cookie) => match store.check_page(&cookie, now).await {
                Ok(_) => HttpResponse::with_body(
                    200,
                    "OK",
                    "application/javascript",
                    VIEWER_JS.as_bytes().to_vec(),
                ),
                Err(error) => {
                    HttpResponse::json(403, "Forbidden", &serde_json::json!({"error": error}))
                }
            },
            None => HttpResponse::json(
                403,
                "Forbidden",
                &serde_json::json!({"error": "missing_viewer_cookie"}),
            ),
        },
        ("GET", "/style.css") => match bearer_cookie(headers) {
            Some(cookie) => match store.check_page(&cookie, now).await {
                Ok(_) => {
                    HttpResponse::with_body(200, "OK", "text/css", STYLE_CSS.as_bytes().to_vec())
                }
                Err(error) => {
                    HttpResponse::json(403, "Forbidden", &serde_json::json!({"error": error}))
                }
            },
            None => HttpResponse::json(
                403,
                "Forbidden",
                &serde_json::json!({"error": "missing_viewer_cookie"}),
            ),
        },
        ("GET", "/socket") => {
            let upgrade = headers
                .get("upgrade")
                .is_some_and(|v| v.eq_ignore_ascii_case("websocket"));
            if !upgrade {
                return HttpResponse::json(
                    400,
                    "Bad Request",
                    &serde_json::json!({"error": "websocket_upgrade_required"}),
                );
            }
            let key = headers
                .get("sec-websocket-key")
                .cloned()
                .unwrap_or_default();
            let cookie = bearer_cookie(headers);
            let origin = headers.get("origin").map(String::as_str);
            let checked = match cookie {
                Some(cookie) => {
                    store
                        .check_socket(&cookie, origin, viewer_origin, now)
                        .await
                }
                None => Err("missing_viewer_cookie".into()),
            };
            match checked {
                Ok(_) => {
                    let mut response = HttpResponse {
                        status: 101,
                        reason: "Switching Protocols",
                        headers: vec![
                            ("upgrade".into(), "websocket".into()),
                            ("connection".into(), "Upgrade".into()),
                            ("sec-websocket-accept".into(), ws_accept_key(&key)),
                        ],
                        body: Vec::new(),
                    };
                    response.headers.extend(security_headers());
                    response
                }
                Err(error) => {
                    HttpResponse::json(403, "Forbidden", &serde_json::json!({"error": error}))
                }
            }
        }
        _ => HttpResponse::json(
            404,
            "Not Found",
            &serde_json::json!({"error": "unknown_viewer_route"}),
        ),
    }
}

/// Serve viewer credentials from the session manager (9.3).
///
/// Issuance stays gated on `human_control` inside the manager; redemption
/// and cookie/origin checks run here over the HTTP listener.
impl ViewerStore for excalibur_browser::sessions::Manager {
    async fn redeem(&self, code: &str, now: i64) -> Result<String, String> {
        self.viewer_redeem(code, now).await.map_err(|e| e.kind)
    }

    async fn check_page(&self, cookie: &str, now: i64) -> Result<(String, String), String> {
        self.viewer_check_page(cookie, now)
            .await
            .map_err(|e| e.kind)
    }

    async fn check_socket(
        &self,
        cookie: &str,
        origin: Option<&str>,
        viewer_origin: &str,
        now: i64,
    ) -> Result<(String, String), String> {
        self.viewer_check(cookie, origin, viewer_origin, now)
            .await
            .map_err(|e| e.kind)
    }
}

/// Minimal single-request HTTP/1.1 parsing for the viewer listener.
///
/// Returns method, target, lowercased headers and body or `None` when the
/// buffered bytes do not yet form a complete request within the limits.
/// Parsed viewer HTTP request: method, target, headers and body.
pub type ViewerRequest = (
    String,
    String,
    std::collections::HashMap<String, String>,
    Vec<u8>,
);

pub fn parse_viewer_http(buffer: &[u8]) -> Option<ViewerRequest> {
    const LIMIT: usize = 65536;
    if buffer.len() > LIMIT + 65536 {
        return None;
    }
    let head_end = buffer
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .map(|i| i + 4)?;
    let head = std::str::from_utf8(&buffer[..head_end]).ok()?;
    let mut lines = head.lines();
    let request_line = lines.next().unwrap_or_default();
    let mut parts = request_line.split_whitespace();
    let (method, target) = (parts.next()?, parts.next()?);
    if method.len() > 16 || target.len() > 2048 {
        return None;
    }
    let mut headers = std::collections::HashMap::new();
    for line in lines {
        if line.trim().is_empty() {
            continue;
        }
        let (name, value) = line.split_once(':')?;
        headers.insert(name.trim().to_lowercase(), value.trim().to_owned());
    }
    let content_length: usize = headers
        .get("content-length")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    if content_length > LIMIT {
        return None;
    }
    let body = buffer.get(head_end..head_end + content_length)?.to_vec();
    Some((method.to_owned(), target.to_owned(), headers, body))
}

/// Run the served viewer endpoint until the listener closes (9.3).
///
/// One request per connection (`Connection: close`); every accepted peer is
/// already loopback-local because the listener binds `127.0.0.1`. On a valid
/// socket upgrade the server answers `101` and sends a single ready frame;
/// screencast streaming stays pending on harness integration.
pub async fn serve_viewer<S: ViewerStore + 'static>(
    listener: tokio::net::TcpListener,
    store: std::sync::Arc<S>,
    viewer_origin: String,
) -> std::io::Result<()> {
    loop {
        let (mut stream, _) = listener.accept().await?;
        let store = store.clone();
        let viewer_origin = viewer_origin.clone();
        tokio::spawn(async move {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let mut buffer = Vec::new();
            let mut chunk = [0u8; 4096];
            let request = loop {
                match stream.read(&mut chunk).await {
                    Ok(0) => return,
                    Ok(n) => {
                        buffer.extend_from_slice(&chunk[..n]);
                        if buffer.len() > 131_072 {
                            return;
                        }
                        if let Some(request) = parse_viewer_http(&buffer) {
                            break request;
                        }
                    }
                    Err(_) => return,
                }
            };
            let (method, target, headers, body) = request;
            let body_text = String::from_utf8_lossy(&body).into_owned();
            let now = crate_viewer_now();
            let is_socket_upgrade = target.split('?').next() == Some("/socket")
                && method == "GET"
                && headers
                    .get("upgrade")
                    .is_some_and(|v| v.eq_ignore_ascii_case("websocket"));
            let response = handle_viewer_request(
                store.as_ref(),
                &method,
                &target,
                &headers,
                &body_text,
                now,
                &viewer_origin,
            )
            .await;
            if stream.write_all(&response.encode()).await.is_err() {
                return;
            }
            if is_socket_upgrade && response.status == 101 {
                // Deterministic ready frame; screencast streaming is pending.
                let payload = b"{\"viewer\":\"ready\"}";
                let mut frame = vec![0x81, payload.len() as u8];
                frame.extend_from_slice(payload);
                let _ = stream.write_all(&frame).await;
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_are_single_use_and_expire() {
        let now = 1_700_000_000;
        let (mut auth, code) = ViewerAuth::issue("tab-1", now);
        assert_eq!(code.len(), CODE_LEN);
        assert!(auth.check_code(&code, now + 10).is_ok());
        let cookie = auth.consume_code(&code, now + 10).unwrap();
        assert!(auth.check_code(&code, now + 11).is_err());
        assert!(auth.check_cookie(&cookie, now + 11).is_ok());
        assert!(auth.check_cookie("wrong", now + 11).is_err());
        let (auth2, code2) = ViewerAuth::issue("tab-2", now);
        assert!(auth2.check_code(&code2, now + CODE_TTL_SECS + 1).is_err());
        // Cookie binds to the value, not the code string.
        assert!(!cookie.is_empty());
    }

    #[test]
    fn socket_requires_cookie_and_exact_origin() {
        let now = 1000;
        let (mut auth, code) = ViewerAuth::issue("tab-1", now);
        let cookie = auth.consume_code(&code, now).unwrap();
        let viewer = "http://127.0.0.1:9";
        assert!(
            auth.check_socket(Some(&cookie), Some(viewer), viewer, now)
                .is_ok()
        );
        assert!(auth.check_socket(None, Some(viewer), viewer, now).is_err());
        assert!(
            auth.check_socket(Some(&cookie), Some("http://evil.test"), viewer, now)
                .is_err()
        );
        assert!(auth.check_socket(Some(&cookie), None, viewer, now).is_err());
    }

    #[test]
    fn headers_forbid_framing_and_mixed_sources() {
        let headers: HashMap<_, _> = security_headers().into_iter().collect();
        let csp = headers["content-security-policy"].clone();
        assert!(csp.contains("frame-ancestors 'none'"));
        assert!(csp.contains("default-src 'none'"));
        assert!(csp.contains("require-trusted-types-for 'script'"));
        assert!(!csp.contains("unsafe-inline"));
    }

    #[test]
    fn page_title_with_markup_is_escaped() {
        let shell = render_shell("https://example.com", "<img src=x onerror=alert(1)>");
        assert!(!shell.contains("<img src=x"));
        assert!(shell.contains("&lt;img"));
        // JSON transport round-trips exactly; markup safety comes from the
        // escaped shell rendering above plus `textContent` at runtime.
        let payload = serde_json::to_string(&"<img src=x onerror=alert(1)>").unwrap();
        assert_eq!(
            serde_json::from_str::<String>(&payload).unwrap(),
            "<img src=x onerror=alert(1)>"
        );
    }

    #[test]
    fn auth_never_reads_code_from_url() {
        // The viewer only accepts the code in a POST body; query strings are
        // ignored by construction: no helper parses them.
        let (auth, _) = ViewerAuth::issue("tab-1", 100);
        assert!(auth.check_code("?code=ABCDEFGH", 101).is_err());
    }

    // --- Served endpoint (9.3, 9.5): rejection matrix over a fake store. ---
    use std::collections::HashMap;
    use std::sync::Mutex;

    struct MemoryStore {
        codes: Mutex<HashMap<String, (String, String, i64, bool)>>,
    }

    impl MemoryStore {
        fn seeded() -> Self {
            let mut codes = HashMap::new();
            codes.insert(
                "GOODCODE".into(),
                ("tab-1".into(), "cookie-good".into(), 10_600, false),
            );
            codes.insert(
                "USEDCODE".into(),
                ("tab-1".into(), "cookie-used".into(), 10_600, true),
            );
            codes.insert(
                "OLDCODE".into(),
                ("tab-1".into(), "cookie-old".into(), 9_000, false),
            );
            Self {
                codes: Mutex::new(codes),
            }
        }

        fn authed(headers: &[(&str, &str)]) -> HashMap<String, String> {
            headers
                .iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                .collect()
        }
    }

    impl super::ViewerStore for MemoryStore {
        async fn redeem(&self, code: &str, now: i64) -> Result<String, String> {
            let mut codes = self.codes.lock().unwrap();
            let Some((_, cookie, expires, used)) = codes.get_mut(code) else {
                return Err("invalid_code".into());
            };
            if *used {
                return Err("single_use_code_reused".into());
            }
            if now > *expires {
                return Err("code_expired".into());
            }
            *used = true;
            Ok(cookie.clone())
        }

        async fn check_page(&self, cookie: &str, now: i64) -> Result<(String, String), String> {
            let codes = self.codes.lock().unwrap();
            codes
                .values()
                .find(|(_, c, _, _)| c == cookie)
                .filter(|(_, _, expires, _)| now <= *expires)
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

    fn headers(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        MemoryStore::authed(pairs)
    }

    #[tokio::test]
    async fn auth_redeems_once_then_rejects_reuse_and_expiry() {
        let store = MemoryStore::seeded();
        let origin = "http://127.0.0.1:9";
        let ok = super::handle_viewer_request(
            &store,
            "POST",
            "/auth",
            &headers(&[]),
            r#"{"code":"GOODCODE"}"#,
            10_000,
            origin,
        )
        .await;
        assert_eq!(ok.status, 200);
        assert!(ok.headers.iter().any(|(k, v)| k == "set-cookie"
            && v.contains("HttpOnly")
            && v.contains("SameSite=Strict")));
        let reused = super::handle_viewer_request(
            &store,
            "POST",
            "/auth",
            &headers(&[]),
            r#"{"code":"GOODCODE"}"#,
            10_001,
            origin,
        )
        .await;
        assert_eq!(reused.status, 403);
        assert!(String::from_utf8_lossy(&reused.body).contains("single_use_code_reused"));
        let expired = super::handle_viewer_request(
            &store,
            "POST",
            "/auth",
            &headers(&[]),
            r#"{"code":"OLDCODE"}"#,
            10_000,
            origin,
        )
        .await;
        assert_eq!(expired.status, 403);
        assert!(String::from_utf8_lossy(&expired.body).contains("code_expired"));
        let unknown = super::handle_viewer_request(
            &store,
            "POST",
            "/auth",
            &headers(&[]),
            r#"{"code":"NOPE"}"#,
            10_000,
            origin,
        )
        .await;
        assert_eq!(unknown.status, 403);
        assert!(String::from_utf8_lossy(&unknown.body).contains("invalid_code"));
    }

    #[tokio::test]
    async fn code_in_url_is_never_accepted() {
        let store = MemoryStore::seeded();
        let origin = "http://127.0.0.1:9";
        // Query strings are ignored by construction: an empty body rejects.
        let response = super::handle_viewer_request(
            &store,
            "POST",
            "/auth?code=GOODCODE",
            &headers(&[]),
            "{}",
            10_000,
            origin,
        )
        .await;
        assert_eq!(response.status, 403);
    }

    #[tokio::test]
    async fn pages_require_cookie_and_carry_strict_headers() {
        let store = MemoryStore::seeded();
        let origin = "http://127.0.0.1:9";
        let missing =
            super::handle_viewer_request(&store, "GET", "/", &headers(&[]), "", 10_000, origin)
                .await;
        assert_eq!(missing.status, 403);
        let page = super::handle_viewer_request(
            &store,
            "GET",
            "/",
            &headers(&[("cookie", "viewer=cookie-good")]),
            "",
            10_000,
            origin,
        )
        .await;
        assert_eq!(page.status, 200);
        let names: Vec<_> = page.headers.iter().map(|(k, _)| k.as_str()).collect();
        assert!(names.contains(&"content-security-policy"));
        let csp = page
            .headers
            .iter()
            .find(|(k, _)| k == "content-security-policy")
            .unwrap()
            .1
            .clone();
        assert!(csp.contains("frame-ancestors 'none'"));
        assert!(!csp.contains("unsafe-inline"));
        assert!(
            page.headers
                .iter()
                .any(|(k, _)| k == "x-content-type-options")
        );
        let body = String::from_utf8_lossy(&page.body);
        assert!(body.contains("https://one.test"));
        assert!(!body.contains("<img src=x"));
        // Static assets share the cookie gate and CSP headers.
        for (path, content_type) in [
            ("/viewer.js", "application/javascript"),
            ("/style.css", "text/css"),
        ] {
            let asset = super::handle_viewer_request(
                &store,
                "GET",
                path,
                &headers(&[("cookie", "viewer=cookie-good")]),
                "",
                10_000,
                origin,
            )
            .await;
            assert_eq!(asset.status, 200, "{path}");
            assert!(
                asset
                    .headers
                    .iter()
                    .any(|(k, v)| k == "content-type" && v == content_type)
            );
            assert!(
                asset
                    .headers
                    .iter()
                    .any(|(k, _)| k == "content-security-policy")
            );
        }
        let denied = super::handle_viewer_request(
            &store,
            "GET",
            "/viewer.js",
            &headers(&[("cookie", "viewer=nope")]),
            "",
            10_000,
            origin,
        )
        .await;
        assert_eq!(denied.status, 403);
    }

    #[tokio::test]
    async fn socket_upgrade_checks_cookie_and_exact_origin() {
        let store = MemoryStore::seeded();
        let origin = "http://127.0.0.1:9";
        async fn socket_call<S: super::ViewerStore>(
            store: &S,
            origin: &str,
            cookie: Option<String>,
            ws_origin: Option<String>,
        ) -> super::HttpResponse {
            let mut headers = HashMap::new();
            headers.insert("upgrade".to_owned(), "websocket".to_owned());
            headers.insert(
                "sec-websocket-key".to_owned(),
                "dGhlIHNhbXBsZSBub25jZQ==".to_owned(),
            );
            if let Some(cookie) = cookie {
                headers.insert("cookie".to_owned(), cookie);
            }
            if let Some(ws_origin) = ws_origin {
                headers.insert("origin".to_owned(), ws_origin);
            }
            super::handle_viewer_request(store, "GET", "/socket", &headers, "", 10_000, origin)
                .await
        }
        let socket = |cookie: Option<&str>, ws_origin: Option<&str>| {
            socket_call(
                &store,
                origin,
                cookie.map(str::to_owned),
                ws_origin.map(str::to_owned),
            )
        };
        let missing = socket(None, Some(origin)).await;
        assert_eq!(missing.status, 403);
        let foreign = socket(Some("viewer=cookie-good"), Some("http://evil.test")).await;
        assert_eq!(foreign.status, 403);
        assert!(String::from_utf8_lossy(&foreign.body).contains("foreign_origin"));
        let upgraded = socket(Some("viewer=cookie-good"), Some(origin)).await;
        assert_eq!(upgraded.status, 101);
        assert!(
            upgraded
                .headers
                .iter()
                .any(|(k, v)| k == "sec-websocket-accept" && v == "s3pPLMBiTxaQ9kYGzzhZRbK+xOo=")
        );
        assert!(
            upgraded
                .headers
                .iter()
                .any(|(k, _)| k == "content-security-policy")
        );
    }

    #[test]
    fn websocket_accept_matches_the_rfc_vector() {
        assert_eq!(
            super::ws_accept_key("dGhlIHNhbXBsZSBub25jZQ=="),
            "s3pPLMBiTxaQ9kYGzzhZRbK+xOo="
        );
    }

    #[test]
    fn viewer_client_uses_text_content_only() {
        assert!(super::VIEWER_JS.contains("textContent"));
        assert!(!super::VIEWER_JS.contains("innerHTML"));
    }

    #[test]
    fn stylesheet_covers_small_and_large_viewports() {
        assert!(super::STYLE_CSS.contains("max-width:100%"));
        assert!(super::STYLE_CSS.contains("44px"));
        assert!(super::STYLE_CSS.contains("@media(min-width:900px)"));
        assert!(super::STYLE_CSS.contains("1280px"));
    }
}
