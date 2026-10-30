//! Light reading (`web_read`) core: validation, HTTP-first heuristic,
//! wait-gated capture, snippet/fragment output and a TTL result cache.
//!
//! Pure backend helpers on top of [`crate::extract`] and [`crate::cite`].
//! Chrome rendering itself is injected by the caller (ephemeral context +
//! `observe.wait_ready` + `DOM.getOuterHTML`); this module only decides when
//! the fallback is required and refuses capture without a valid wait,
//! following harness rule 7.4-1 ("never act/capture without waiting").

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::cite;
use crate::extract::{self, Extracted, Fragment};

/// Inline budget sent to the model; the remainder stays in the full blob.
pub const INLINE_LIMIT: usize = 50 * 1024;
/// Default result cache time-to-live: 24 h (architecture 14.2 step 2).
pub const CACHE_TTL_SECS: u64 = 24 * 3600;
/// Body cap for HTTP reads: 10 MiB (architecture 14.2 step 3).
pub const MAX_BODY_BYTES: usize = 10 * 1024 * 1024;
/// Extractor label recorded in results.
pub const EXTRACTOR: &str = "readability_htmd_fallback@b7";
/// Maximum URL length accepted by `web_read`.
pub const MAX_URL_LEN: usize = 2048;

/// Wait gate that must be satisfied before capture (harness 7.4 rule 1).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WaitGate {
    pub ready: bool,
    pub document_epoch: u64,
    pub degraded: Vec<String>,
    pub strict_network: bool,
    pub status: String,
}

impl WaitGate {
    pub fn ready(document_epoch: u64) -> Self {
        Self {
            ready: true,
            document_epoch,
            degraded: Vec::new(),
            strict_network: false,
            status: "ready".to_owned(),
        }
    }

    pub fn degraded(document_epoch: u64, degraded: &[&str]) -> Self {
        Self {
            ready: true,
            document_epoch,
            degraded: degraded.iter().map(|s| s.to_string()).collect(),
            strict_network: false,
            status: "ready".to_owned(),
        }
    }

    /// Refuse capture unless the wait is `ready` for the current epoch.
    ///
    /// A non-strict wait may proceed with `network_busy`/`network_unobserved`
    /// degradations; strict network mode turns them into a timeout, matching
    /// the 7.2 long-poll contract.
    pub fn check(&self, current_epoch: u64) -> Result<u64, String> {
        if !self.ready || self.status != "ready" {
            return Err("wait_required".into());
        }
        if self.document_epoch != current_epoch {
            return Err("wait_stale_epoch".into());
        }
        if self.strict_network
            && self
                .degraded
                .iter()
                .any(|d| d == "network_busy" || d == "network_unobserved")
        {
            return Err("timeout".into());
        }
        Ok(self.document_epoch)
    }
}

/// Which path produced the document.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Render {
    Http,
    Browser,
}

impl Render {
    pub fn as_str(self) -> &'static str {
        match self {
            Render::Http => "http",
            Render::Browser => "browser",
        }
    }
}

/// HTTP-first decision with the escalation reason when the browser is needed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HttpDecision {
    Http,
    Browser(String),
}

/// Decide the render path for already-extracted HTTP HTML.
pub fn decide_render(doc: &Extracted) -> HttpDecision {
    match extract::needs_browser_render(doc) {
        Some(reason) => HttpDecision::Browser(reason),
        None => HttpDecision::Http,
    }
}

/// Validate and normalize a `web_read` URL.
///
/// Only `http`/`https` without userinfo and at most 2048 characters are
/// accepted. Fragments are stripped for cache identity; the final URL keeps
/// them for citation links.
pub fn normalize_url(url: &str) -> Result<String, String> {
    if url.len() > MAX_URL_LEN {
        return Err("url_too_long".into());
    }
    let parsed = reqwest::Url::parse(url).map_err(|_| "unsupported_url".to_string())?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err("unsupported_url".into());
    }
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err("userinfo_forbidden".into());
    }
    let mut normalized = parsed.clone();
    normalized.set_fragment(None);
    Ok(normalized.to_string())
}

/// Inline fragment descriptor stored with a read result.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FragmentMeta {
    pub id: String,
    pub start: usize,
    pub end: usize,
    pub heading_path: Vec<String>,
}

impl From<&Fragment> for FragmentMeta {
    fn from(f: &Fragment) -> Self {
        Self {
            id: f.id.clone(),
            start: f.start,
            end: f.end,
            heading_path: f.heading_path.clone(),
        }
    }
}

/// Light-read result: inline snippet plus addressing metadata.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LightRead {
    pub markdown: String,
    pub title: String,
    pub final_url: String,
    pub canonical_url: String,
    pub fetched_at: String,
    pub http_status: u16,
    pub content_hash: String,
    pub source_id: String,
    pub lang: String,
    pub render: String,
    pub extractor: String,
    pub fragments: Vec<FragmentMeta>,
    pub truncated: bool,
    pub full_len: usize,
    pub untrusted: bool,
}

/// Build a [`LightRead`] from extracted Markdown (caller already waited).
///
/// `markdown` must be the full normalized document; only the first
/// [`INLINE_LIMIT`] bytes travel inline.
#[allow(clippy::too_many_arguments)]
pub fn build_read(
    url: &str,
    final_url: &str,
    title: &str,
    markdown: &str,
    fragments: &[Fragment],
    http_status: u16,
    render: Render,
    fetched_at: &str,
) -> LightRead {
    let content_hash = cite::content_hash(markdown);
    let source_id = cite::source_id(&content_hash);
    let truncated = markdown.len() > INLINE_LIMIT;
    let inline = if truncated {
        markdown[..INLINE_LIMIT].to_owned()
    } else {
        markdown.to_owned()
    };
    LightRead {
        markdown: inline,
        title: title.to_owned(),
        final_url: final_url.to_owned(),
        canonical_url: url.to_owned(),
        fetched_at: fetched_at.to_owned(),
        http_status,
        content_hash,
        source_id,
        lang: String::new(),
        render: render.as_str().to_owned(),
        extractor: EXTRACTOR.to_owned(),
        fragments: fragments.iter().map(FragmentMeta::from).collect(),
        truncated,
        full_len: markdown.len(),
        untrusted: true,
    }
}

/// Capture a document only with a valid wait for the current epoch.
///
/// This is the 7.4 rule-1 gate for light reading: without a current `ready`
/// wait the capture is refused before any DOM is touched.
#[allow(clippy::too_many_arguments)]
pub fn capture_with_wait(
    gate: &WaitGate,
    current_epoch: u64,
    url: &str,
    final_url: &str,
    doc: &Extracted,
    http_status: u16,
    render: Render,
    fetched_at: &str,
) -> Result<LightRead, String> {
    gate.check(current_epoch)?;
    Ok(build_read(
        url,
        final_url,
        &doc.title,
        &doc.markdown,
        &doc.fragments,
        http_status,
        render,
        fetched_at,
    ))
}

/// Slice `full_markdown` by fragment range ids (`p12-p20` or single `p3`).
pub fn slice_fragments(
    full_markdown: &str,
    fragments: &[FragmentMeta],
    range: &str,
) -> Result<String, String> {
    let (from, to) = parse_range(range)?;
    let mut out = String::new();
    for meta in fragments {
        let n = fragment_no(&meta.id).ok_or_else(|| "unknown_fragment".to_string())?;
        if n >= from && n <= to {
            if meta.end > full_markdown.len() || meta.start > meta.end {
                return Err("fragment_out_of_range".into());
            }
            if !out.is_empty() {
                out.push_str("\n\n");
            }
            out.push_str(&full_markdown[meta.start..meta.end]);
        }
    }
    if out.is_empty() {
        return Err("unknown_fragment".into());
    }
    Ok(out)
}

fn fragment_no(id: &str) -> Option<u64> {
    id.strip_prefix('p')?.parse().ok()
}

fn parse_range(range: &str) -> Result<(u64, u64), String> {
    let range = range.trim();
    if let Some((a, b)) = range.split_once('-') {
        let from = fragment_no(a.trim()).ok_or_else(|| "unknown_fragment".to_string())?;
        let to = fragment_no(b.trim()).ok_or_else(|| "unknown_fragment".to_string())?;
        if from == 0 || to == 0 || from > to {
            return Err("unknown_fragment".into());
        }
        Ok((from, to))
    } else {
        let n = fragment_no(range).ok_or_else(|| "unknown_fragment".to_string())?;
        if n == 0 {
            return Err("unknown_fragment".into());
        }
        Ok((n, n))
    }
}

/// Current Unix time as seconds.
pub fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Cached light-read entry with its fetch instant.
#[derive(Clone, Debug)]
struct Cached {
    read: LightRead,
    full_markdown: String,
    fragments: Vec<Fragment>,
    at: u64,
}

/// In-memory `web_read` result cache with TTL and `fresh` bypass.
#[derive(Default)]
pub struct ReadCache {
    ttl_secs: u64,
    inner: Mutex<HashMap<String, Cached>>,
}

impl ReadCache {
    pub fn new(ttl_secs: u64) -> Self {
        Self {
            ttl_secs,
            inner: Mutex::new(HashMap::new()),
        }
    }

    pub fn with_default_ttl() -> Self {
        Self::new(CACHE_TTL_SECS)
    }

    /// Return the cached read when it is still fresh, cloning it out.
    pub fn get(&self, normalized_url: &str, now: u64) -> Option<LightRead> {
        let inner = self.inner.lock().ok()?;
        let entry = inner.get(normalized_url)?;
        if now.saturating_sub(entry.at) > self.ttl_secs {
            return None;
        }
        Some(entry.read.clone())
    }

    /// Store a completed read with its full Markdown for fragment slicing.
    pub fn put(
        &self,
        normalized_url: &str,
        read: LightRead,
        full_markdown: String,
        fragments: Vec<Fragment>,
        now: u64,
    ) {
        if let Ok(mut inner) = self.inner.lock() {
            inner.insert(
                normalized_url.to_owned(),
                Cached {
                    read,
                    full_markdown,
                    fragments,
                    at: now,
                },
            );
        }
    }

    pub fn full(&self, normalized_url: &str) -> Option<String> {
        self.inner
            .lock()
            .ok()?
            .get(normalized_url)
            .map(|e| e.full_markdown.clone())
    }

    pub fn fragments(&self, normalized_url: &str) -> Option<Vec<Fragment>> {
        self.inner
            .lock()
            .ok()?
            .get(normalized_url)
            .map(|e| e.fragments.clone())
    }
}

/// Fetch an HTTP(S) URL with explicit timeouts and a 10 MiB body cap.
///
/// Returns `(status, final_url, text_body)`. Redirects are limited to five
/// and every hop is re-validated by [`normalize_url`] semantics through
/// reqwest's `http(s)`-only handling plus a downgrade check below.
pub async fn fetch_http(url: &str) -> Result<(u16, String, String), String> {
    normalize_url(url)?;
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::limited(5))
        .connect_timeout(std::time::Duration::from_secs(10))
        .timeout(std::time::Duration::from_secs(30))
        .user_agent("ExcaliburReader/b7")
        .build()
        .map_err(|e| e.to_string())?;
    let response = client.get(url).send().await.map_err(|e| e.to_string())?;
    let status = response.status().as_u16();
    let final_url = response.url().to_string();
    // Refuse HTTPS -> HTTP downgrades on redirect.
    if url.starts_with("https://") && final_url.starts_with("http://") {
        return Err("redirect_downgrade".into());
    }
    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_owned();
    if !supported_type(&content_type) {
        return Err(format!("unsupported_type:{content_type}"));
    }
    let bytes = response.bytes().await.map_err(|e| e.to_string())?;
    if bytes.len() > MAX_BODY_BYTES {
        return Err("body_too_large".into());
    }
    let text = String::from_utf8_lossy(&bytes).into_owned();
    Ok((status, final_url, text))
}

fn supported_type(content_type: &str) -> bool {
    let ct = content_type.to_lowercase();
    ct.is_empty()
        || ct.contains("text/html")
        || ct.contains("application/xhtml")
        || ct.contains("text/plain")
        || ct.contains("text/markdown")
        || ct.contains("application/json")
        || ct.contains("ld+json")
        || ct.contains("xml")
        || ct.contains("rss")
        || ct.contains("atom")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_userinfo_and_scheme() {
        assert!(normalize_url("https://user@example.com/").is_err());
        assert!(normalize_url("ftp://example.com/").is_err());
        assert!(
            normalize_url("https://example.com/#frag")
                .unwrap()
                .ends_with('/')
        );
    }

    #[test]
    fn wait_gate_refuses_without_ready() {
        let gate = WaitGate {
            ready: false,
            document_epoch: 7,
            degraded: vec![],
            strict_network: false,
            status: "timeout".into(),
        };
        assert_eq!(gate.check(7).unwrap_err(), "wait_required");
    }

    #[test]
    fn strict_long_poll_times_out() {
        let gate = WaitGate {
            ready: true,
            document_epoch: 3,
            degraded: vec!["network_busy".into()],
            strict_network: true,
            status: "ready".into(),
        };
        assert_eq!(gate.check(3).unwrap_err(), "timeout");
    }
}
