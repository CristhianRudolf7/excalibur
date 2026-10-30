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
/// Exact match wins; otherwise a whitespace/case-normalized search is used
/// and the normalized hit is mapped back to the exact raw byte span covering
/// the matched words (char-boundary aligned). Returned offsets always index
/// the raw `markdown`; normalizing `&markdown[start..end]` reproduces the
/// normalized quote. Returns `None` when the quote is absent.
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
    let norm_q = normalize(quote);
    if norm_q.is_empty() {
        return None;
    }
    let (start, end) = find_normalized_span(markdown, &norm_q)?;
    Some((start, end, fragment_at(fragments, start)))
}

/// Map a normalized quote to its exact raw byte span in `markdown`.
///
/// Tokenizes the raw document into words with byte ranges and finds the
/// normalized token sequence as a contiguous window (case-insensitive,
/// whitespace-flexible). The returned span covers the first matched word
/// through the last one with original casing and spacing preserved, so the
/// offsets are exact rather than approximate.
fn find_normalized_span(markdown: &str, norm_q: &str) -> Option<(usize, usize)> {
    let tokens: Vec<&str> = norm_q.split(' ').filter(|t| !t.is_empty()).collect();
    if tokens.is_empty() {
        return None;
    }
    let mut words: Vec<(usize, usize, String)> = Vec::new();
    let mut word_start: Option<usize> = None;
    for (idx, ch) in markdown.char_indices() {
        if ch.is_whitespace() {
            if let Some(start) = word_start.take() {
                let word = &markdown[start..idx];
                words.push((start, idx, word.to_lowercase()));
            }
        } else if word_start.is_none() {
            word_start = Some(idx);
        }
    }
    if let Some(start) = word_start.take() {
        let word = &markdown[start..];
        words.push((start, markdown.len(), word.to_lowercase()));
    }
    if words.len() < tokens.len() {
        return None;
    }
    for window in 0..=words.len() - tokens.len() {
        let mut ok = true;
        for (offset, token) in tokens.iter().enumerate() {
            if words[window + offset].2 != *token {
                ok = false;
                break;
            }
        }
        if ok {
            let start = words[window].0;
            let end = words[window + tokens.len() - 1].1;
            if markdown.is_char_boundary(start) && markdown.is_char_boundary(end) && start < end {
                return Some((start, end));
            }
        }
    }
    None
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
