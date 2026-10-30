//! `web_search` query service over the disk index (sections 15.7-15.9).
//!
//! The response shape matches the architecture contract: ranked results,
//! coverage with profile/budget bookkeeping and an `insufficient` flag that
//! tells the agent to reformulate, read known sites or use `source_search`.
//! Latency calibration (p95 < 150 ms at one million documents) belongs to
//! the main-lane VPS measurement; this foundation reports per-query latency
//! so the measurement has a hook.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::error::Result;
use crate::index::DiskIndex;
use crate::ranking::{self, RankedResult};
use crate::tokenize;

/// Query options for `web_search`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SearchOptions {
    /// Force the freshness term on.
    #[serde(default)]
    pub freshness: bool,
    /// Restrict to these host/domain suffixes (also honours `site:` in query).
    #[serde(default)]
    pub domains: Vec<String>,
    /// Restrict to a language tag (`es`, `en`, ...). Empty means any.
    #[serde(default)]
    pub lang: Option<String>,
    /// Maximum results (capped at 50).
    #[serde(default)]
    pub k: usize,
    /// Caller clock (unix seconds) for freshness; 0 disables it.
    #[serde(default)]
    pub now_secs: i64,
}

/// Coverage block attached to every response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Coverage {
    /// Profile name (`minimo` / `ampliado`).
    pub profile: String,
    /// Indexed document count.
    pub documents: u64,
    /// Latest fetch timestamp, if any.
    pub last_crawl_at: Option<i64>,
    /// Documents per source.
    pub by_source: HashMap<String, u64>,
    /// True once low-priority ingestion stopped at 95% of budget.
    pub budget_reached: bool,
}

/// Full `web_search` response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchResponse {
    /// Ranked results (at most `k`, at most 2 per host).
    pub results: Vec<RankedResult>,
    /// Index coverage.
    pub coverage: Coverage,
    /// True when the agent should reformulate or read known sites.
    pub insufficient: bool,
    /// Query latency in milliseconds, for calibration hooks.
    pub latency_ms: u64,
}

/// Query service borrowing the disk index.
pub struct SearchService<'index> {
    index: &'index DiskIndex,
    profile: String,
}

impl<'index> SearchService<'index> {
    /// Bind a service to an open index with the given profile name.
    #[must_use]
    pub fn new(index: &'index DiskIndex, profile: &str) -> Self {
        Self {
            index,
            profile: profile.to_owned(),
        }
    }

    /// Run a query: conjunctive BM25 first, disjunctive fallback below 10
    /// candidates, then blending, diversity and the insufficient flag.
    pub fn search(&self, query: &str, options: &SearchOptions) -> Result<SearchResponse> {
        let started = std::time::Instant::now();
        let (text, site_domains) = split_site_filters(query);
        let mut domains = options.domains.clone();
        domains.extend(site_domains);
        let terms = tokenize(&text);
        let k = options.k.clamp(1, crate::ranking::MAX_K);
        let mut candidates = self.index.search_bm25(&terms, true, 200)?;
        if candidates.len() < 10 {
            candidates = self.index.search_bm25(&terms, false, 200)?;
        }
        let lang = options.lang.as_deref().filter(|value| !value.is_empty());
        let ranked = ranking::rank_candidates(
            &candidates,
            &text,
            options.freshness,
            lang,
            &domains,
            k,
            options.now_secs,
        );
        let results = ranking::dedupe_results(ranked);
        let max_score = results
            .iter()
            .map(|result| result.score)
            .fold(0.0_f64, f64::max);
        let insufficient = results.len() < 3 || max_score < 0.05;
        Ok(SearchResponse {
            results,
            coverage: Coverage {
                profile: self.profile.clone(),
                documents: self.index.len().unwrap_or(0),
                last_crawl_at: self.index.last_crawl_at().unwrap_or(None),
                by_source: self.index.by_source().unwrap_or_default(),
                budget_reached: self.index.budget_state().budget_reached,
            },
            insufficient,
            latency_ms: started.elapsed().as_millis() as u64,
        })
    }
}

/// Split `site:example.com` filters out of the query text.
fn split_site_filters(query: &str) -> (String, Vec<String>) {
    let mut domains = Vec::new();
    let mut kept = Vec::new();
    for token in query.split_whitespace() {
        if let Some(domain) = token.strip_prefix("site:") {
            let domain = domain.trim().to_ascii_lowercase();
            if !domain.is_empty() {
                domains.push(domain);
            }
        } else {
            kept.push(token);
        }
    }
    (kept.join(" "), domains)
}
