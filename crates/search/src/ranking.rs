//! Ranking: BM25 blending, priors, freshness, diversity and RRF (15.7).
//!
//! Final score: `bm25/max_bm25 + 0.3*prior + 0.2*freshness`, where freshness
//! (`exp(-age_days/30)`) applies only when explicitly requested or when the
//! query shows recency intent. Diversity caps hosts at two results in the
//! top-k; result fusion across lists uses RRF with k=60.

use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Serialize};

use crate::index::ScoredDoc;

/// Weight of the static prior in the final score.
pub const PRIOR_WEIGHT: f64 = 0.3;
/// Weight of freshness when it applies.
pub const FRESHNESS_WEIGHT: f64 = 0.2;
/// Maximum results per host inside the top-k.
pub const MAX_PER_HOST: usize = 2;
/// RRF constant from the original paper.
pub const RRF_K: f64 = 60.0;
/// Maximum `k` accepted by `web_search`.
pub const MAX_K: usize = 50;

/// Query terms that signal recency intent (Spanish and English).
const RECENCY_TERMS: &[&str] = &[
    "hoy",
    "ultimas",
    "últimas",
    "noticias",
    "nuevo",
    "nueva",
    "reciente",
    "actualidad",
    "latest",
    "news",
    "today",
    "recent",
];

/// Whether `query` asks for current information.
#[must_use]
pub fn has_recency_intent(query: &str) -> bool {
    let lower = query.to_ascii_lowercase();
    if RECENCY_TERMS.iter().any(|term| lower.contains(*term)) {
        return true;
    }
    lower
        .split(|char: char| !char.is_ascii_alphanumeric())
        .any(|token| {
            token.len() == 4
                && token.starts_with("202")
                && token[1..].chars().all(|char| char.is_ascii_digit())
        })
}

/// Static prior from a Common Crawl harmonic rank position.
#[must_use]
pub fn prior_for_harmonic_rank(position: u64, total: u64) -> f64 {
    if position == 0 || total <= 1 {
        return 0.2;
    }
    let position = position.min(total) as f64;
    let total = total as f64;
    (1.0 - position.ln() / total.ln()).clamp(0.0, 1.0)
}

/// Freshness decay: `exp(-age_days/30)`.
#[must_use]
pub fn freshness_score(now_secs: i64, doc_time_secs: i64) -> f64 {
    let age_days = (now_secs.saturating_sub(doc_time_secs) as f64 / 86_400.0).max(0.0);
    (-age_days / 30.0).exp()
}

/// Ranked result with the blended final score.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RankedResult {
    /// Canonical URL.
    pub url: String,
    /// Title.
    pub title: String,
    /// 1 KiB snippet.
    pub snippet: String,
    /// Publication time.
    pub published_at: i64,
    /// Fetch time.
    pub fetched_at: i64,
    /// Source string.
    pub source: String,
    /// Stable source id derived from the URL hash.
    pub source_id: String,
    /// Final blended score.
    pub score: f64,
    /// Raw BM25 before blending.
    pub bm25: f64,
}

/// Blend BM25 candidates into ranked results.
///
/// `freshness_hint` forces the freshness term on; otherwise it applies only
/// under recency intent. `now_secs` is the caller clock; pass 0 to disable
/// freshness entirely.
#[must_use]
#[allow(clippy::too_many_arguments)]
pub fn rank_candidates(
    candidates: &[ScoredDoc],
    query: &str,
    freshness_hint: bool,
    lang: Option<&str>,
    domains: &[String],
    k: usize,
    now_secs: i64,
) -> Vec<RankedResult> {
    let freshness_on = freshness_hint || has_recency_intent(query);
    let max_bm25 = candidates
        .iter()
        .map(|candidate| candidate.bm25)
        .fold(0.0_f64, f64::max);
    let mut ranked: Vec<RankedResult> = candidates
        .iter()
        .filter(|candidate| lang.is_none_or(|want| candidate.doc.lang.eq_ignore_ascii_case(want)))
        .filter(|candidate| {
            domains.is_empty()
                || domains.iter().any(|domain| {
                    candidate.doc.host == *domain
                        || candidate.doc.host.ends_with(&format!(".{domain}"))
                })
        })
        .map(|candidate| {
            let freshness = if freshness_on && now_secs > 0 {
                let stamp = candidate.doc.published_at.max(candidate.doc.fetched_at);
                freshness_score(now_secs, stamp)
            } else {
                0.0
            };
            let score = if max_bm25 > 0.0 {
                candidate.bm25 / max_bm25
                    + PRIOR_WEIGHT * candidate.doc.prior
                    + FRESHNESS_WEIGHT * freshness
            } else {
                0.0
            };
            RankedResult {
                url: candidate.doc.url.clone(),
                title: candidate.doc.title.clone(),
                snippet: candidate.doc.snippet.clone(),
                published_at: candidate.doc.published_at,
                fetched_at: candidate.doc.fetched_at,
                source: candidate.doc.source.clone(),
                source_id: format!(
                    "w_{}",
                    &candidate.doc.content_hash[..12.min(candidate.doc.content_hash.len())]
                ),
                score,
                bm25: candidate.bm25,
            }
        })
        .collect();
    ranked.sort_by(|left, right| {
        right
            .score
            .partial_cmp(&left.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    diversify(ranked, k.clamp(1, MAX_K))
}

fn diversify(mut ranked: Vec<RankedResult>, k: usize) -> Vec<RankedResult> {
    let mut per_host: HashMap<String, usize> = HashMap::new();
    let mut out = Vec::new();
    for result in ranked.drain(..) {
        let host = host_of(&result.url);
        let used = per_host.get(&host).copied().unwrap_or(0);
        if used < MAX_PER_HOST {
            per_host.insert(host, used + 1);
            out.push(result);
        }
        if out.len() >= k {
            break;
        }
    }
    out
}

fn host_of(url: &str) -> String {
    url::Url::parse(url)
        .ok()
        .and_then(|parsed| parsed.host_str().map(str::to_owned))
        .unwrap_or_default()
}

/// Fuse two ranked lists with Reciprocal Rank Fusion (k=60).
#[must_use]
pub fn fuse_rrf(first: &[RankedResult], second: &[RankedResult], k: usize) -> Vec<RankedResult> {
    let mut scores: HashMap<&str, f64> = HashMap::new();
    let mut by_url: HashMap<&str, &RankedResult> = HashMap::new();
    for (rank, result) in first.iter().enumerate() {
        *scores.entry(&result.url).or_default() += 1.0 / (RRF_K + 1.0 + rank as f64);
        by_url.entry(&result.url).or_insert(result);
    }
    for (rank, result) in second.iter().enumerate() {
        *scores.entry(&result.url).or_default() += 1.0 / (RRF_K + 1.0 + rank as f64);
        by_url.entry(&result.url).or_insert(result);
    }
    let mut fused: Vec<(&RankedResult, f64)> = scores
        .into_iter()
        .filter_map(|(url, score)| by_url.get(url).map(|result| (*result, score)))
        .collect();
    fused.sort_by(|left, right| {
        right
            .1
            .partial_cmp(&left.1)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    fused
        .into_iter()
        .take(k.clamp(1, MAX_K))
        .map(|(result, score)| {
            let mut merged = result.clone();
            merged.score = score;
            merged
        })
        .collect()
}

/// Deduplicate results that share a `content_hash` prefix or identical URL.
#[must_use]
pub fn dedupe_results(results: Vec<RankedResult>) -> Vec<RankedResult> {
    let mut seen_urls = HashSet::new();
    let mut seen_ids = HashSet::new();
    results
        .into_iter()
        .filter(|result| {
            seen_urls.insert(result.url.clone()) && seen_ids.insert(result.source_id.clone())
        })
        .collect()
}
