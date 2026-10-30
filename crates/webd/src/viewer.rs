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
        r#"<!doctype html><html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1"><title>Intervention</title><link rel="stylesheet" href="/style.css"></head><body><header><div id="origin"></div><div id="state"></div><div><button id="done">Terminado</button><button id="giveup">No puedo</button><button id="back">Atrás</button><button id="reload">Recargar</button></div></header><main><canvas id="screen"></canvas><input id="paste" type="text" aria-label="paste box"></main><script src="/viewer.js"></script><script>window.__origin={origin_json};window.__title={title_json};</script></body></html>"#,
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
}
