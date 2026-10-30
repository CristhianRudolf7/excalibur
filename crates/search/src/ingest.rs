//! Bounded feed, sitemap and CirrusSearch ingestion helpers (sections 15.3-15.5).
//!
//! These parsers are intentionally dependency-free and bounded: sitemaps stop
//! at 50 000 URLs and 50 MiB, feeds scan only link targets, and the
//! CirrusSearch dump is consumed as a line-pair stream so multi-gigabyte
//! dumps are never held in memory.

use serde::{Deserialize, Serialize};

/// Maximum URLs accepted from a single sitemap document.
pub const MAX_SITEMAP_URLS: usize = 50_000;
/// Maximum uncompressed sitemap size accepted (50 MiB).
pub const MAX_SITEMAP_BYTES: usize = 50 * 1024 * 1024;

/// Decode the five predefined XML entities used inside `<loc>` text.
/// Single pass so `&amp;lt;` decodes to `&lt;` instead of `<`.
fn decode_xml_entities(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while !rest.is_empty() {
        if let Some((decoded, len)) = decode_entity_at(rest) {
            out.push_str(decoded);
            rest = &rest[len..];
        } else if let Some(pos) = rest.find('&') {
            if pos == 0 {
                out.push('&');
                rest = &rest[1..];
            } else {
                out.push_str(&rest[..pos]);
                rest = &rest[pos..];
            }
        } else {
            out.push_str(rest);
            break;
        }
    }
    out
}

/// Match one of the five predefined XML entities at the start of `rest`.
/// Returns the decoded text and the byte length of the entity.
fn decode_entity_at(rest: &str) -> Option<(&'static str, usize)> {
    for (entity, decoded) in [
        ("&amp;", "&"),
        ("&lt;", "<"),
        ("&gt;", ">"),
        ("&quot;", "\""),
        ("&apos;", "'"),
    ] {
        if rest.starts_with(entity) {
            return Some((decoded, entity.len()));
        }
    }
    None
}

/// Whether `target` belongs to `host_suffix`: exact host or a subdomain.
fn host_matches_suffix(target: &str, host_suffix: &str) -> bool {
    let Ok(parsed) = url::Url::parse(target) else {
        return false;
    };
    if !matches!(parsed.scheme(), "http" | "https") {
        return false;
    }
    let Some(host) = parsed.host_str() else {
        return false;
    };
    let host = host.to_ascii_lowercase();
    let suffix = host_suffix.trim().to_ascii_lowercase();
    if suffix.is_empty() {
        return false;
    }
    host == suffix || host.ends_with(&format!(".{suffix}"))
}

/// Extract `<loc>` targets from a sitemap or sitemap index, bounded to
/// [`MAX_SITEMAP_URLS`]. Only `http(s)` targets whose host matches
/// `host_suffix` are kept; XML entities are decoded. Path restriction
/// against the sitemap URL needs [`parse_sitemap_locs_for_sitemap`].
#[must_use]
pub fn parse_sitemap_locs(xml: &str, host_suffix: &str) -> Vec<String> {
    let mut out = Vec::new();
    if xml.len() > MAX_SITEMAP_BYTES {
        return out;
    }
    let mut rest = xml;
    while let Some(start) = rest.find("<loc>") {
        if out.len() >= MAX_SITEMAP_URLS {
            break;
        }
        rest = &rest[start + 5..];
        let Some(end) = rest.find("</loc>") else {
            break;
        };
        let raw = rest[..end].trim();
        rest = &rest[end + 6..];
        let target = decode_xml_entities(raw);
        if host_matches_suffix(&target, host_suffix) {
            out.push(target);
        }
    }
    out
}

/// Extract `<loc>` targets restricted to the sitemap host and directory.
/// `sitemap_url` is the URL the `xml` was fetched from: targets must share
/// its host (or a subdomain when the sitemap host itself is a suffix) and
/// live under its directory prefix per the sitemaps protocol.
#[must_use]
pub fn parse_sitemap_locs_for_sitemap(xml: &str, sitemap_url: &str) -> Vec<String> {
    let Ok(base) = url::Url::parse(sitemap_url) else {
        return Vec::new();
    };
    if !matches!(base.scheme(), "http" | "https") {
        return Vec::new();
    }
    let Some(base_host) = base.host_str() else {
        return Vec::new();
    };
    let base_host = base_host.to_ascii_lowercase();
    let base_path = base.path();
    let dir_prefix = match base_path.rfind('/') {
        Some(idx) => base_path[..idx + 1].to_owned(),
        None => "/".to_owned(),
    };
    let mut out = Vec::new();
    if xml.len() > MAX_SITEMAP_BYTES {
        return out;
    }
    let mut rest = xml;
    while let Some(start) = rest.find("<loc>") {
        if out.len() >= MAX_SITEMAP_URLS {
            break;
        }
        rest = &rest[start + 5..];
        let Some(end) = rest.find("</loc>") else {
            break;
        };
        let raw = rest[..end].trim();
        rest = &rest[end + 6..];
        let target = decode_xml_entities(raw);
        let Ok(parsed) = url::Url::parse(&target) else {
            continue;
        };
        if !matches!(parsed.scheme(), "http" | "https") {
            continue;
        }
        let Some(host) = parsed.host_str() else {
            continue;
        };
        if host.to_ascii_lowercase() != base_host {
            continue;
        }
        if !parsed.path().starts_with(&dir_prefix) {
            continue;
        }
        out.push(target);
    }
    out
}

/// Extract linked page targets from a feed document (RSS `<link>`, Atom
/// `<link href>`, JSON Feed `url` fields). Bounded to 5 000 targets.
#[must_use]
pub fn parse_feed_links(feed: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = feed;
    while out.len() < 5_000 {
        let atom = rest.find("<link");
        let rss = rest.find("<link>");
        let json = rest.find("\"url\"");
        let next = [atom, rss, json].into_iter().flatten().min();
        let Some(pos) = next else { break };
        if Some(pos) == json {
            rest = &rest[pos + 5..];
            if let Some(target) = take_json_string_value(rest)
                && (target.starts_with("http://") || target.starts_with("https://"))
            {
                out.push(target);
            }
            continue;
        }
        if Some(pos) == rss {
            rest = &rest[pos + 6..];
            let end = rest.find("</link>").unwrap_or(rest.len());
            let target = rest[..end].trim().to_owned();
            rest = if let Some(end) = rest.find("</link>") {
                &rest[end + 7..]
            } else {
                ""
            };
            if target.starts_with("http://") || target.starts_with("https://") {
                out.push(target);
            }
            continue;
        }
        rest = &rest[pos + 5..];
        let tag_end = rest.find('>').map(|index| index + 1).unwrap_or(rest.len());
        let tag = &rest[..tag_end.min(rest.len())];
        rest = &rest[tag_end.min(rest.len())..];
        if let Some(href) = take_attr(tag, "href")
            && (href.starts_with("http://") || href.starts_with("https://"))
        {
            out.push(href);
        }
    }
    out
}

fn take_json_string_value(rest: &str) -> Option<String> {
    let colon = rest.find(':')?;
    let after = rest[colon + 1..].trim_start();
    let quoted = after.strip_prefix('"')?;
    let end = quoted.find('"')?;
    Some(quoted[..end].to_owned())
}

fn take_attr(tag: &str, name: &str) -> Option<String> {
    let needle = format!("{name}=");
    let pos = tag.find(&needle)?;
    let after = tag[pos + needle.len()..].trim_start();
    let quote = after.chars().next()?;
    if quote != '"' && quote != '\'' {
        return None;
    }
    let end = after[1..].find(quote)?;
    Some(after[1..1 + end].to_owned())
}

fn null_to_default<'de, D>(deserializer: D) -> std::result::Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Ok(Option::<String>::deserialize(deserializer)?.unwrap_or_default())
}

/// Minimal CirrusSearch document projection used by the streaming ingest.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CirrusDoc {
    /// Article title.
    pub title: String,
    /// Plain-text opening used as body and snippet seed.
    /// Real dumps carry explicit `null`; it maps to an empty string.
    #[serde(default, deserialize_with = "null_to_default")]
    pub opening_text: String,
    /// Headings, if present.
    #[serde(default)]
    pub heading: Vec<String>,
    /// Incoming-link count used for threshold filtering.
    #[serde(default)]
    pub incoming_links: i64,
    /// Popularity signal used as ranking prior.
    #[serde(default)]
    pub popularity_score: f64,
    /// Source URL, when the dump carries it.
    #[serde(default)]
    pub url: String,
    /// Language tag, when the dump carries it.
    #[serde(default)]
    pub language: String,
}

/// Parse one CirrusSearch line pair (`{"index":{...}}` + document JSON).
/// Returns `None` for the index line or for malformed input.
#[must_use]
pub fn parse_cirrus_pair(index_line: &str, doc_line: &str) -> Option<CirrusDoc> {
    if !index_line.trim_start().starts_with("{\"index\"") {
        return None;
    }
    serde_json::from_str(doc_line).ok()
}

/// Whether a CirrusSearch document passes the profile threshold.
#[must_use]
pub fn cirrus_admitted(doc: &CirrusDoc, incoming_links_threshold: i64) -> bool {
    doc.incoming_links >= incoming_links_threshold && !doc.title.trim().is_empty()
}
