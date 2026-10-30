//! Citation records backing `cite` (architecture 14.3).
//!
//! Pure helpers over extracted Markdown: normalized quote lookup,
//! stable content hashes, source identifiers and text-fragment URLs.
//! No network or Chrome I/O happens here.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::extract::Fragment;

/// A verifiable citation into a stored light-read document.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Citation {
    pub source_id: String,
    pub url: String,
    pub title: String,
    pub fragment_id: String,
    pub start: usize,
    pub end: usize,
    pub text_fragment_url: String,
    pub fetched_at: String,
    pub content_hash: String,
}

/// `sha256:<hex>` over normalized Markdown.
pub fn content_hash(markdown: &str) -> String {
    let digest = Sha256::digest(markdown.as_bytes());
    format!("sha256:{digest:x}")
}

/// Stable `w_<16 hex>` source identifier derived from the content hash.
pub fn source_id(content_hash: &str) -> String {
    let hex: String = content_hash
        .strip_prefix("sha256:")
        .unwrap_or(content_hash)
        .chars()
        .take(16)
        .collect();
    format!("w_{hex}")
}

/// Lowercase and collapse whitespace for tolerant quote matching.
pub fn normalize(text: &str) -> String {
    text.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

/// Locate `quote` inside `markdown`, returning byte offsets and fragment id.
///
/// Exact match wins; otherwise a whitespace/case-normalized search is used.
/// Returns `None` when the quote is absent.
pub fn locate(
    markdown: &str,
    fragments: &[Fragment],
    quote: &str,
) -> Option<(usize, usize, String)> {
    if quote.trim().is_empty() {
        return None;
    }
    if let Some(start) = markdown.find(quote) {
        let end = start + quote.len();
        return Some((start, end, fragment_at(fragments, start)));
    }
    let norm_md = normalize(markdown);
    let norm_q = normalize(quote);
    if norm_q.is_empty() {
        return None;
    }
    norm_md.find(&norm_q)?;
    // Map the normalized hit back to the raw markdown by scanning for the
    // first long token of the quote.
    let token = norm_q.split(' ').find(|t| t.len() >= 4).unwrap_or(&norm_q);
    let approx = markdown.to_lowercase().find(token).unwrap_or(0);
    let start = approx.min(markdown.len());
    let end = (start + quote.len()).min(markdown.len());
    Some((start, end, fragment_at(fragments, start)))
}

fn fragment_at(fragments: &[Fragment], offset: usize) -> String {
    for f in fragments {
        if offset >= f.start && offset < f.end.max(f.start + 1) {
            return f.id.clone();
        }
    }
    fragments
        .first()
        .map(|f| f.id.clone())
        .unwrap_or_else(|| "p1".into())
}

/// Build a `#:~:text=` link with the first words of the quote.
pub fn text_fragment_url(url: &str, quote: &str) -> String {
    let words: Vec<&str> = quote.split_whitespace().take(8).collect();
    let snippet = words.join(" ");
    let encoded = snippet
        .replace('%', "%25")
        .replace(' ', "%20")
        .replace('"', "%22")
        .replace('<', "%3C")
        .replace('>', "%3E")
        .replace('#', "%23");
    if encoded.is_empty() {
        return url.to_owned();
    }
    if url.contains("#:~:text=") {
        return url.to_owned();
    }
    if let Some(base) = url.split_once('#') {
        format!("{}#:~:text={}", base.0, encoded)
    } else {
        format!("{url}#:~:text={encoded}")
    }
}

/// Create a citation record for a located quote.
#[allow(clippy::too_many_arguments)]
pub fn cite(
    url: &str,
    title: &str,
    fetched_at: &str,
    content_hash: &str,
    markdown: &str,
    fragments: &[Fragment],
    quote: &str,
) -> Option<Citation> {
    let (start, end, fragment_id) = locate(markdown, fragments, quote)?;
    Some(Citation {
        source_id: source_id(content_hash),
        url: url.to_owned(),
        title: title.to_owned(),
        fragment_id,
        start,
        end,
        text_fragment_url: text_fragment_url(url, quote),
        fetched_at: fetched_at.to_owned(),
        content_hash: content_hash.to_owned(),
    })
}

/// Human-readable citation label, never inventing page numbers.
pub fn format_citation(c: &Citation, paragraph_no: usize) -> String {
    format!(
        "{} — {}, paragraph {paragraph_no} (read {})",
        c.title, c.url, c.fetched_at
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extract::extract_document;

    fn doc() -> (String, Vec<Fragment>) {
        let html = "<html><head><title>T</title></head><body><h1>A</h1><p>Alpha beta gamma.</p><p>Delta epsilon zeta.</p></body></html>";
        let d = extract_document(html);
        (d.markdown.clone(), d.fragments.clone())
    }

    #[test]
    fn exact_quote_wins() {
        let (md, frags) = doc();
        let hit = locate(&md, &frags, "Alpha beta gamma.").unwrap();
        assert_eq!(&md[hit.0..hit.1], "Alpha beta gamma.");
    }

    #[test]
    fn normalized_quote_matches() {
        let (md, frags) = doc();
        let hit = locate(&md, &frags, "  ALPHA   beta\nGAMMA. ").unwrap();
        assert!(!hit.2.is_empty());
    }

    #[test]
    fn missing_quote_is_none() {
        let (md, frags) = doc();
        assert!(locate(&md, &frags, "no such sentence here xyz").is_none());
    }
}
