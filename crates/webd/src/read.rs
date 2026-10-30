//! `web_read` service path: validation, HTTP-first heuristic with browser
//! fallback, TTL result cache and citation storage.
//!
//! The browser fallback render is injected by the caller as already-rendered
//! HTML (ephemeral context + `observe.wait_ready` + `DOM.getOuterHTML`). This
//! keeps the service deterministic and testable without real Chrome while
//! preserving the 14.2 HTTP-first contract. No agent tool is registered here.

use std::collections::HashMap;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

use excalibur_browser::cite::{self, Citation};
use excalibur_browser::extract::{self, Fragment};
use excalibur_browser::read::{self, LightRead, ReadCache, Render, WaitGate};

/// Cache TTL for `web_read` results: 24 h.
pub const CACHE_TTL_SECS: u64 = 24 * 3600;
/// Extractor label recorded with every read.
pub const EXTRACTOR: &str = read::EXTRACTOR;

/// How the document was obtained.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RenderPath {
    Http,
    Browser,
}

/// Stored page backing fragment reads and citation checks.
#[derive(Clone, Debug)]
pub struct StoredPage {
    pub read: LightRead,
    pub full_markdown: String,
    pub fragments: Vec<Fragment>,
}

/// `web_read` service with a TTL cache and a citation backing store.
#[derive(Default)]
pub struct Service {
    cache: ReadCache,
    pages: Mutex<HashMap<String, StoredPage>>,
}

impl Service {
    pub fn new() -> Self {
        Self {
            cache: ReadCache::new(CACHE_TTL_SECS),
            pages: Mutex::new(HashMap::new()),
        }
    }

    /// Look up a cached read without fetching. Returns `None` on expiry.
    pub fn cached(&self, normalized_url: &str, now: u64) -> Option<LightRead> {
        self.cache.get(normalized_url, now)
    }

    /// Read from plain HTTP HTML after a valid wait.
    ///
    /// When `fresh` is false a non-expired cache entry is returned with its
    /// original fetch date. Otherwise the HTML is extracted; when the
    /// HTTP-first heuristic requires a browser, `rendered_html` (already
    /// waited and rendered) is used when provided, else `needs_browser` is
    /// returned.
    #[allow(clippy::too_many_arguments)]
    pub fn read_http(
        &self,
        url: &str,
        html: &str,
        http_status: u16,
        rendered_html: Option<&str>,
        fresh: bool,
        gate: &WaitGate,
        epoch: u64,
        now: u64,
    ) -> Result<LightRead, String> {
        let normalized = read::normalize_url(url)?;
        if !fresh && let Some(hit) = self.cache.get(&normalized, now) {
            return Ok(hit);
        }
        gate.check(epoch)?;
        let doc = extract::extract_document(html);
        let (active_doc, render) = match read::decide_render(&doc) {
            read::HttpDecision::Http => (doc, Render::Http),
            read::HttpDecision::Browser(reason) => {
                let rendered = rendered_html.ok_or_else(|| format!("needs_browser:{reason}"))?;
                (extract::extract_document(rendered), Render::Browser)
            }
        };
        let fetched_at = format_ts(now);
        let result = read::build_read(
            &normalized,
            &normalized,
            &active_doc.title,
            &active_doc.markdown,
            &active_doc.fragments,
            http_status,
            render,
            &fetched_at,
        );
        self.pages
            .lock()
            .map(|mut pages| {
                pages.insert(
                    result.content_hash.clone(),
                    StoredPage {
                        read: result.clone(),
                        full_markdown: active_doc.markdown.clone(),
                        fragments: active_doc.fragments.clone(),
                    },
                )
            })
            .ok();
        self.cache.put(
            &normalized,
            result.clone(),
            active_doc.markdown,
            active_doc.fragments,
            now,
        );
        Ok(result)
    }

    /// Read a fragment range from the stored full Markdown by content hash.
    pub fn read_fragments(&self, content_hash: &str, range: &str) -> Result<String, String> {
        let pages = self.pages.lock().map_err(|_| "storage_error".to_string())?;
        let page = pages
            .get(content_hash)
            .ok_or_else(|| "unknown_content".to_string())?;
        let metas: Vec<read::FragmentMeta> = page
            .fragments
            .iter()
            .map(read::FragmentMeta::from)
            .collect();
        read::slice_fragments(&page.full_markdown, &metas, range)
    }

    /// Validate that `quote` appears in the stored document (`search.cite`).
    pub fn cite(&self, content_hash: &str, quote: &str) -> Option<Citation> {
        let pages = self.pages.lock().ok()?;
        let page = pages.get(content_hash)?;
        cite::cite(
            &page.read.final_url,
            &page.read.title,
            &page.read.fetched_at,
            &page.read.content_hash,
            &page.full_markdown,
            &page.fragments,
            quote,
        )
    }
}

/// Minimal `YYYY-MM-DDTHH:MM:SSZ` rendering for a Unix timestamp.
pub fn format_ts(now_secs: u64) -> String {
    // Days-to-date conversion for the cache timestamp (no chrono dependency).
    let days = now_secs / 86400;
    let secs = now_secs % 86400;
    let (mut y, mut m, mut d) = (1970_i64, 1_i64, 1_i64);
    let mut remaining = days as i64;
    loop {
        let leap = (y % 4 == 0 && y % 100 != 0) || y % 400 == 0;
        let year_days = if leap { 366 } else { 365 };
        if remaining < year_days {
            break;
        }
        remaining -= year_days;
        y += 1;
    }
    let leap = (y % 4 == 0 && y % 100 != 0) || y % 400 == 0;
    let lens = [
        31,
        if leap { 29 } else { 28 },
        31,
        30,
        31,
        30,
        31,
        31,
        30,
        31,
        30,
        31,
    ];
    for (i, len) in lens.iter().enumerate() {
        if remaining < *len {
            m = i as i64 + 1;
            d += remaining;
            break;
        }
        remaining -= *len;
    }
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        secs / 3600,
        (secs % 3600) / 60,
        secs % 60
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn caches_with_original_date() {
        let svc = Service::new();
        let gate = WaitGate::ready(1);
        let html = "<html><head><title>C</title></head><body><p>Cacheable article text with enough words to avoid the SPA heuristic and pass as plain http content.</p></body></html>";
        let first = svc
            .read_http(
                "https://example.com/a",
                html,
                200,
                None,
                false,
                &gate,
                1,
                100,
            )
            .unwrap();
        let second = svc
            .read_http(
                "https://example.com/a",
                "<html><body>changed</body></html>",
                200,
                None,
                false,
                &gate,
                1,
                200,
            )
            .unwrap();
        assert_eq!(first.content_hash, second.content_hash);
        assert_eq!(first.fetched_at, second.fetched_at);
    }

    #[test]
    fn spa_needs_browser_without_render() {
        let svc = Service::new();
        let gate = WaitGate::ready(2);
        let shell = "<html><body><div id=\"root\"></div><script>a</script><script>b</script><script>c</script></body></html>";
        let err = svc
            .read_http(
                "https://example.com/app",
                shell,
                200,
                None,
                true,
                &gate,
                2,
                300,
            )
            .unwrap_err();
        assert!(err.starts_with("needs_browser:"));
    }
}
