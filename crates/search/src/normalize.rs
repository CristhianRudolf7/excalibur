//! URL normalization and tokenization (section 15.4).
//!
//! Parsing itself relies on the WHATWG `url` crate; on top of it the host is
//! lower-cased, fragments, default ports and tracking parameters are
//! removed, and `<link rel=canonical>` handling is left to the caller with
//! the registrable-domain helper below.

use serde::{Deserialize, Serialize};

use crate::error::{Result, SearchError};

/// Query parameters stripped during normalization.
const TRACKING_PARAMS: &[&str] = &[
    "utm_source",
    "utm_medium",
    "utm_campaign",
    "utm_term",
    "utm_content",
    "gclid",
    "fbclid",
    "mc_cid",
    "mc_eid",
];

/// Normalized URL with derived crawl fields.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NormalizedUrl {
    /// Canonical URL string used as index key material.
    pub canonical: String,
    /// Lower-cased host without port.
    pub host: String,
    /// Approximated registrable domain (last two labels).
    pub domain: String,
    /// Path used for robots decisions (path + optional query).
    pub robots_path: String,
    /// Search tokens derived from host, path and query.
    pub url_tokens: Vec<String>,
}

impl NormalizedUrl {
    /// Hash key for the frontier `urls` table.
    #[must_use]
    pub fn hash_key(&self) -> String {
        crate::dedup::sha256_hex(&self.canonical)
    }
}

/// Normalize `raw` into crawl fields.
pub fn normalize_url(raw: &str) -> Result<NormalizedUrl> {
    let mut parsed = url::Url::parse(raw)
        .map_err(|err| SearchError::InvalidUrl(raw.to_owned(), err.to_string()))?;
    match parsed.scheme() {
        "http" | "https" => {}
        other => {
            return Err(SearchError::InvalidUrl(
                raw.to_owned(),
                format!("unsupported scheme '{other}'"),
            ));
        }
    }
    parsed.set_fragment(None);
    let host = parsed
        .host_str()
        .ok_or_else(|| SearchError::InvalidUrl(raw.to_owned(), "missing host".to_owned()))?
        .to_ascii_lowercase();
    if parsed.port().is_some_and(|port| {
        (parsed.scheme() == "http" && port == 80) || (parsed.scheme() == "https" && port == 443)
    }) {
        let _ = parsed.set_port(None);
    }
    let kept: Vec<(String, String)> = parsed
        .query_pairs()
        .filter(|(key, _)| {
            let lower = key.to_ascii_lowercase();
            !TRACKING_PARAMS.contains(&lower.as_str()) && !lower.starts_with("utm_")
        })
        .map(|(key, value)| (key.into_owned(), value.into_owned()))
        .collect();
    if parsed.query_pairs().count() > 0 {
        parsed.query_pairs_mut().clear();
        for (key, value) in &kept {
            parsed.query_pairs_mut().append_pair(key, value);
        }
        if kept.is_empty() {
            parsed.set_query(None);
        }
    }
    let canonical = parsed.to_string();
    let robots_path = match parsed.query() {
        Some(query) => format!("{}?{query}", parsed.path()),
        None => parsed.path().to_owned(),
    };
    let url_tokens = tokenize_url_parts(&parsed);
    Ok(NormalizedUrl {
        canonical,
        domain: registrable_domain(&host),
        host,
        robots_path,
        url_tokens,
    })
}

/// Approximate the registrable domain as the last two DNS labels,
/// except for common two-level public suffixes (e.g. `co.uk`) where three
/// labels are kept. This is a local approximation, not a full PSL lookup.
#[must_use]
pub fn registrable_domain(host: &str) -> String {
    const TWO_LEVEL_SUFFIXES: &[&str] = &[
        "co.uk", "org.uk", "ac.uk", "gov.uk", "com.au", "net.au", "org.au", "co.jp", "com.br",
    ];
    let lower = host.to_ascii_lowercase();
    let labels: Vec<&str> = lower.split('.').filter(|part| !part.is_empty()).collect();
    if labels.len() <= 2 {
        return lower;
    }
    let last_two = format!("{}.{}", labels[labels.len() - 2], labels[labels.len() - 1]);
    if TWO_LEVEL_SUFFIXES.contains(&last_two.as_str()) && labels.len() >= 3 {
        return labels[labels.len() - 3..].join(".");
    }
    labels[labels.len() - 2..].join(".")
}

fn tokenize_url_parts(parsed: &url::Url) -> Vec<String> {
    let mut joined = String::new();
    joined.push_str(parsed.host_str().unwrap_or_default());
    joined.push(' ');
    joined.push_str(parsed.path());
    if let Some(query) = parsed.query() {
        joined.push(' ');
        joined.push_str(query);
    }
    crate::tokenize(&joined)
}
