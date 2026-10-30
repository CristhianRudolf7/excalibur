//! Deterministic text helpers shared by admission, retrieval and chunking.

/// Fold case and the Latin diacritics used by Spanish and nearby languages.
pub(crate) fn fold_char(c: char) -> char {
    match c {
        'á' | 'à' | 'ä' | 'â' | 'ã' | 'å' | 'Á' | 'À' | 'Ä' | 'Â' | 'Ã' | 'Å' => 'a',
        'é' | 'è' | 'ë' | 'ê' | 'É' | 'È' | 'Ë' | 'Ê' => 'e',
        'í' | 'ì' | 'ï' | 'î' | 'Í' | 'Ì' | 'Ï' | 'Î' => 'i',
        'ó' | 'ò' | 'ö' | 'ô' | 'õ' | 'Ó' | 'Ò' | 'Ö' | 'Ô' | 'Õ' => 'o',
        'ú' | 'ù' | 'ü' | 'û' | 'Ú' | 'Ù' | 'Ü' | 'Û' => 'u',
        'ñ' | 'Ñ' => 'n',
        'ç' | 'Ç' => 'c',
        other => other.to_lowercase().next().unwrap_or(other),
    }
}

pub(crate) fn normalize(text: &str) -> String {
    let folded: String = text.chars().map(fold_char).collect();
    folded.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Alphanumeric word tokens after folding.
pub(crate) fn words(text: &str) -> Vec<String> {
    normalize(text)
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(str::to_string)
        .collect()
}

const STOP: &[&str] = &[
    "a", "an", "the", "of", "to", "in", "on", "at", "is", "are", "was", "were", "and", "or", "for",
    "what", "who", "when", "where", "how", "did", "do", "does", "i", "my", "me", "we", "our", "it",
    "el", "la", "los", "las", "un", "una", "de", "del", "en", "y", "o", "que", "es", "fue", "con",
    "por", "para", "mi", "mis", "se", "lo", "al", "su", "sus", "le", "les", "como", "cual",
    "quien",
];

/// Content terms used for the lexical query; stop words are dropped unless nothing remains.
pub(crate) fn query_terms(text: &str) -> Vec<String> {
    let all = words(text);
    let content: Vec<_> = all
        .iter()
        .filter(|w| !STOP.contains(&w.as_str()))
        .cloned()
        .collect();
    let mut out = if content.is_empty() { all } else { content };
    out.dedup();
    out.truncate(32);
    out
}

/// FTS5 OR-query of quoted terms; never passes raw user syntax to MATCH.
pub(crate) fn fts_or(terms: &[String]) -> Option<String> {
    let quoted: Vec<String> = terms
        .iter()
        .filter(|t| !t.is_empty())
        .map(|t| format!("\"{}\"", t.replace('"', "\"\"")))
        .collect();
    (!quoted.is_empty()).then(|| quoted.join(" OR "))
}

/// FTS5 phrase for the trigram index (requires at least three characters).
pub(crate) fn fts_phrase(text: &str) -> Option<String> {
    (text.chars().count() >= 3).then(|| format!("\"{}\"", text.replace('"', "\"\"")))
}

pub(crate) fn estimate_tokens(text: &str) -> usize {
    text.chars().count().div_ceil(4).max(1)
}

/// Whether `needle` appears in `haystack` on word boundaries after folding.
pub(crate) fn contains_words(haystack_normalized: &str, needle: &str) -> bool {
    let needle = normalize(needle);
    if needle.is_empty() {
        return false;
    }
    let mut start = 0;
    while let Some(pos) = haystack_normalized[start..].find(&needle) {
        let at = start + pos;
        let end = at + needle.len();
        let before = haystack_normalized[..at]
            .chars()
            .next_back()
            .is_none_or(|c| !c.is_alphanumeric());
        let after = haystack_normalized[end..]
            .chars()
            .next()
            .is_none_or(|c| !c.is_alphanumeric());
        if before && after {
            return true;
        }
        start = end;
        if start >= haystack_normalized.len() {
            break;
        }
    }
    false
}
