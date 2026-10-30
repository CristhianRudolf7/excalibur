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

/// Floor `limit` to the nearest preceding UTF-8 char boundary in `s`.
///
/// Slicing Markdown at a raw byte index panics when the index lands inside a
/// multibyte character (non-ASCII documents over [`INLINE_LIMIT`]). This
/// helper backs the inline truncation so the 50 KiB cut never panics.
pub fn floor_char_boundary(s: &str, limit: usize) -> usize {
    let mut end = limit.min(s.len());
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    end
}

/// Build a [`LightRead`] from extracted Markdown (caller already waited).
///
/// `markdown` must be the full normalized document; only the first
/// [`INLINE_LIMIT`] bytes travel inline (floored to a char boundary).
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
        let end = floor_char_boundary(markdown, INLINE_LIMIT);
        markdown[..end].to_owned()
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

/// Maximum redirects followed manually (architecture 14.4 matrix).
pub const MAX_REDIRECTS: usize = 5;
/// User-agent advertised by light reads.
pub const USER_AGENT: &str = "ExcaliburReader/b7";
/// Cloud metadata endpoint that must never be fetched (SSRF guard).
pub const METADATA_IP: &str = "169.254.169.254";

/// Fetch policy for light reads.
///
/// `allow_loopback` exists only for local synthetic fixtures exercised with
/// `FetchPolicy::test_loopback`; the secure default denies loopback, private
/// ranges and the metadata IP. `isolate_cookies` records the intent that no
/// cookie jar is used (the reqwest client is built without one); production
/// persistence decisions stay broker-owned. `respect_robots` and
/// `enqueue_index` expose the robots/index hooks the broker (B9 lane) drives:
/// the browser crate evaluates supplied `robots.txt` bodies and page `<meta
/// name=robots>` markers, while `robots.txt` retrieval, DNS-bound egress and
/// actual queue insertion remain broker-owned (pending B9).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FetchPolicy {
    pub allow_loopback: bool,
    pub isolate_cookies: bool,
    pub respect_robots: bool,
    pub enqueue_index: bool,
}

impl Default for FetchPolicy {
    fn default() -> Self {
        Self {
            allow_loopback: false,
            isolate_cookies: true,
            respect_robots: true,
            enqueue_index: true,
        }
    }
}

impl FetchPolicy {
    /// Secure default: no loopback, isolated cookies, robots respected.
    pub fn secure() -> Self {
        Self::default()
    }

    /// Local synthetic fixtures only: loopback admitted, everything else kept.
    pub fn test_loopback() -> Self {
        Self {
            allow_loopback: true,
            ..Self::default()
        }
    }
}

/// True when the client built for `policy` carries no cookie jar.
///
/// The reqwest dependency is compiled without the `cookies` feature, so no
/// `Cookie` header is ever stored or replayed; this helper records the flag
/// for the 14.4 matrix (`isolate_cookies`) and keeps the invariant explicit.
pub fn cookie_isolation_active(policy: &FetchPolicy) -> bool {
    policy.isolate_cookies
}

/// Error for `host` when it must not be fetched under `policy`.
///
/// Returns `loopback_forbidden` for loopback/localhost and
/// `private_host_forbidden` for other non-public literals (private ranges,
/// link-local incl. the metadata IP, reserved/multicast/documentation and
/// sensitive single-label suffixes). Public DNS names return `None` here:
/// binding a resolved address to this decision requires broker egress and is
/// pending the B9 lane.
fn host_block_error(host: &str) -> Option<&'static str> {
    let mut lower = host.trim().trim_end_matches('.').to_lowercase();
    // `Url::host_str` may retain IPv6 brackets; strip them for the literal.
    if lower.starts_with('[') && lower.ends_with(']') && lower.len() >= 2 {
        lower = lower[1..lower.len() - 1].to_owned();
    }
    if lower.is_empty() {
        return Some("unsupported_url");
    }
    if lower == "localhost" || lower.ends_with(".localhost") {
        return Some("loopback_forbidden");
    }
    if lower == "metadata.google.internal"
        || lower.ends_with(".metadata.google.internal")
        || lower == "instance-data"
    {
        return Some("private_host_forbidden");
    }
    if lower == METADATA_IP {
        return Some("private_host_forbidden");
    }
    for suffix in [".local", ".internal", ".lan", ".home", ".corp", ".intranet"] {
        if lower.ends_with(suffix) {
            return Some("private_host_forbidden");
        }
    }
    // Decimal-integer IPv4 (`http://2130706433/` == 127.0.0.1): canonicalize
    // before the literal check so obfuscated loopback cannot bypass it.
    if !lower.is_empty()
        && lower.bytes().all(|b| b.is_ascii_digit())
        && let Ok(n) = lower.parse::<u32>()
    {
        let ip = std::net::IpAddr::V4(std::net::Ipv4Addr::from(n));
        return ip_block_error(ip);
    }
    if let Some(hex) = lower
        .strip_prefix("0x")
        .or_else(|| lower.strip_prefix("0X"))
        && let Ok(n) = u32::from_str_radix(hex, 16)
    {
        let ip = std::net::IpAddr::V4(std::net::Ipv4Addr::from(n));
        return ip_block_error(ip);
    }
    if let Ok(ip) = lower.parse::<std::net::IpAddr>() {
        return ip_block_error(ip);
    }
    None
}

fn ip_block_error(ip: std::net::IpAddr) -> Option<&'static str> {
    let blocked = match ip {
        std::net::IpAddr::V4(v) => {
            let o = v.octets();
            v.is_loopback()
                || v.is_private()
                || v.is_link_local()
                || v.is_unspecified()
                || v.is_broadcast()
                || v.is_multicast()
                || v.is_documentation()
                || o == [169, 254, 169, 254]
                || (o[0] == 100 && (64..=127).contains(&o[1]))
                || (o[0] == 192 && o[1] == 0 && o[2] == 0)
                || (o[0] == 198 && (o[1] == 18 || o[1] == 19))
                || o[0] >= 240
                || o[0] == 0
        }
        std::net::IpAddr::V6(v) => {
            if let Some(mapped) = v.to_ipv4_mapped() {
                return ip_block_error(std::net::IpAddr::V4(mapped));
            }
            let s = v.segments();
            v.is_loopback()
                || v.is_unspecified()
                || v.is_multicast()
                || (s[0] & 0xfe00) == 0xfc00
                || (s[0] & 0xffc0) == 0xfe80
                || (s[0] == 0x2001 && s[1] == 0x0db8)
        }
    };
    if !blocked {
        return None;
    }
    if ip.is_loopback() {
        return Some("loopback_forbidden");
    }
    Some("private_host_forbidden")
}

/// True when `host` must not be fetched under the secure default.
pub fn host_is_blocked(host: &str) -> bool {
    host_block_error(host).is_some()
}

/// Validate `url` for fetching under `policy`.
///
/// Runs [`normalize_url`] (scheme/userinfo/length) plus the loopback,
/// private-range, metadata-IP and sensitive-suffix guard. With
/// `allow_loopback` only 127.0.0.0/8, ::1 and `localhost` are admitted; every
/// other non-public literal stays refused.
pub fn check_fetch_url(url: &str, policy: &FetchPolicy) -> Result<reqwest::Url, String> {
    let normalized = normalize_url(url)?;
    let parsed: reqwest::Url = normalized
        .parse()
        .map_err(|_| "unsupported_url".to_string())?;
    let host = parsed
        .host_str()
        .ok_or_else(|| "unsupported_url".to_string())?;
    if let Some(reason) = host_block_error(host) {
        if policy.allow_loopback && reason == "loopback_forbidden" {
            return Ok(parsed);
        }
        return Err(reason.into());
    }
    Ok(parsed)
}

/// True when following `next` from `current` would downgrade HTTPS to HTTP.
pub fn is_downgrade(current: &str, next: &str) -> bool {
    current.starts_with("https://") && next.starts_with("http://")
}

/// Minimal `robots.txt` allow check for one path (RFC 9309 subset).
///
/// Selects the group matching `user_agent` (exact, case-insensitive) or `*`,
/// then applies the longest matching `Allow`/`Disallow` rule; ties prefer
/// `Allow`. Empty/disallow-free bodies allow everything. This mirrors the
/// production `excalibur_search::robots` decision for single-page reads so
/// the browser crate can enforce a supplied body without a new dependency;
/// `robots.txt` retrieval and caching stay broker-owned (pending B9).
pub fn robots_allows(robots_txt: &str, user_agent: &str, path: &str) -> bool {
    let ua = user_agent.to_lowercase();
    type RobotsGroup = (Vec<String>, Vec<(String, bool)>);
    let mut groups: Vec<RobotsGroup> = Vec::new();
    let mut agents: Vec<String> = Vec::new();
    let mut rules: Vec<(String, bool)> = Vec::new();
    let mut in_group = false;
    for raw in robots_txt.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((k, v)) = line.split_once(':') else {
            continue;
        };
        let key = k.trim().to_lowercase();
        let val = v.trim();
        if key == "user-agent" {
            if in_group && !rules.is_empty() {
                groups.push((agents.clone(), rules.clone()));
                agents.clear();
                rules.clear();
            }
            agents.push(val.to_lowercase());
            in_group = true;
        } else if key == "disallow" {
            if val.is_empty() {
                continue;
            }
            rules.push((val.to_owned(), false));
        } else if key == "allow" {
            if val.is_empty() {
                continue;
            }
            rules.push((val.to_owned(), true));
        }
    }
    if in_group && !agents.is_empty() {
        groups.push((agents, rules));
    }
    let mut selected: Option<&Vec<(String, bool)>> = None;
    for (agents, rules) in &groups {
        if agents.iter().any(|a| a == &ua) {
            selected = Some(rules);
            break;
        }
    }
    if selected.is_none() {
        for (agents, rules) in &groups {
            if agents.iter().any(|a| a == "*") {
                selected = Some(rules);
                break;
            }
        }
    }
    let Some(rules) = selected else {
        return true;
    };
    let mut best: Option<(usize, bool)> = None;
    for (pattern, allow) in rules {
        if pattern.is_empty() {
            continue;
        }
        if path.starts_with(pattern.as_str()) {
            let len = pattern.len();
            match best {
                Some((best_len, best_allow)) => {
                    if len > best_len || (len == best_len && *allow && !best_allow) {
                        best = Some((len, *allow));
                    }
                }
                None => best = Some((len, *allow)),
            }
        }
    }
    best.map(|(_, allow)| allow).unwrap_or(true)
}

/// Index-enqueue intent for a page already known to be robots-checked.
///
/// Returns false when the policy disables enqueue, or — with
/// `respect_robots` — when `robots_allow` is false. Actual queue insertion is
/// broker-owned (pending B9); this hook keeps the read path honest about the
/// 14.4 `robots.txt`/index row.
pub fn should_enqueue_index(robots_allow: bool, policy: &FetchPolicy) -> bool {
    if !policy.enqueue_index {
        return false;
    }
    if policy.respect_robots && !robots_allow {
        return false;
    }
    true
}

/// Fetch an HTTP(S) URL with explicit timeouts and a 10 MiB body cap.
///
/// Returns `(status, final_url, text_body)`. Redirects are followed manually
/// (at most [`MAX_REDIRECTS`]) so every hop is re-validated by
/// [`check_fetch_url`] semantics plus an HTTPS-downgrade refusal. Bodies are
/// consumed in chunks with a running counter: past [`MAX_BODY_BYTES`] the
/// read aborts with `body_too_large` before the full body is buffered.
/// Cookies are never stored or replayed (no cookie jar); `robots.txt`
/// retrieval and broker egress stay pending the B9 lane.
pub async fn fetch_http(url: &str) -> Result<(u16, String, String), String> {
    fetch_http_with_policy(url, &FetchPolicy::secure()).await
}

/// Fetch under an explicit [`FetchPolicy`].
///
/// `FetchPolicy::test_loopback` admits loopback fixtures for local synthetic
/// tests; every other guard (private ranges, metadata IP, downgrade,
/// chunked cap, cookie isolation) still applies.
pub async fn fetch_http_with_policy(
    url: &str,
    policy: &FetchPolicy,
) -> Result<(u16, String, String), String> {
    let first = check_fetch_url(url, policy)?;
    let _ = cookie_isolation_active(policy);
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(std::time::Duration::from_secs(10))
        .timeout(std::time::Duration::from_secs(30))
        .user_agent(USER_AGENT)
        .build()
        .map_err(|e| e.to_string())?;
    let mut current = first.to_string();
    let mut hops = 0usize;
    loop {
        let response = client
            .get(current.clone())
            .send()
            .await
            .map_err(|e| e.to_string())?;
        let status = response.status();
        let final_now = response.url().to_string();
        check_fetch_url(&final_now, policy)?;
        if is_downgrade(&current, &final_now) || is_downgrade(url, &final_now) {
            return Err("redirect_downgrade".into());
        }
        if status.is_redirection() {
            if hops >= MAX_REDIRECTS {
                return Err("too_many_redirects".into());
            }
            let location = response
                .headers()
                .get(reqwest::header::LOCATION)
                .and_then(|v| v.to_str().ok())
                .ok_or_else(|| "redirect_no_location".to_string())?
                .to_owned();
            let base: reqwest::Url = current.parse().map_err(|_| "unsupported_url".to_string())?;
            let next = base
                .join(&location)
                .map_err(|_| "unsupported_url".to_string())?;
            let next_str = next.to_string();
            check_fetch_url(&next_str, policy)?;
            if is_downgrade(&current, &next_str) || is_downgrade(url, &next_str) {
                return Err("redirect_downgrade".into());
            }
            current = next_str;
            hops += 1;
            continue;
        }
        let status_u16 = status.as_u16();
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_owned();
        if !supported_type(&content_type) {
            return Err(format!("unsupported_type:{content_type}"));
        }
        let text = read_capped_body(response).await?;
        return Ok((status_u16, final_now, text));
    }
}

/// Consume `response` in chunks, aborting past [`MAX_BODY_BYTES`].
///
/// The running counter prevents a hostile server from forcing the full body
/// into memory before the cap is enforced (14.2 step 3).
async fn read_capped_body(response: reqwest::Response) -> Result<String, String> {
    let mut total: usize = 0;
    let mut buf: Vec<u8> = Vec::new();
    let mut response = response;
    loop {
        let chunk = response.chunk().await.map_err(|e| e.to_string())?;
        let Some(bytes) = chunk else {
            break;
        };
        total = total.saturating_add(bytes.len());
        if total > MAX_BODY_BYTES {
            return Err("body_too_large".into());
        }
        buf.extend_from_slice(&bytes);
    }
    Ok(String::from_utf8_lossy(&buf).into_owned())
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
