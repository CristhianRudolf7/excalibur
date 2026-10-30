//! End-to-end crawl wiring: frontier -> ingest -> index -> ranking -> web_search.
//!
//! Sections 15.4-15.9 over local synthetic fixtures only: sitemap/feed
//! ingestion enqueues bounded URL sets, the polite scheduler dispatches them,
//! synthetic page bodies flow through `record_outcome` / index insert /
//! `mark_indexed` (refresh intervals from 15.6), and ranking plus coverage
//! produce the `web_search` response shape from 15.9. No network, no live
//! crawls, no daemons.
//!
//! Refresh follows the 15.6 approximation with explicitly stored intervals:
//! first visit stores 7 d, a changed revisit halves the interval and an
//! unchanged revisit doubles it, clamped to 1-60 d; due `indexed` rows are
//! redispatched by `select_due`. Budget enforcement stops low-priority
//! sources (`web`, `news`, `hn`) at 95% of the index budget while
//! `wiki`/`docs`/`arxiv`/`read` remain admissible; the response reports
//! `coverage.budget_reached` and `insufficient` on raw BM25 and result
//! count, exactly like [`crate::search::SearchService`].

use std::collections::HashMap;

use crate::backend::IndexBackend;
use crate::error::Result;
use crate::frontier::{FetchOutcome, Frontier, Priority, Source};
use crate::index::{Doc, DocSource};
use crate::ingest::{parse_feed_links, parse_sitemap_locs_for_sitemap};
use crate::normalize::registrable_domain;
use crate::ranking::rank_candidates;
use crate::search::{Coverage, SearchOptions, SearchResponse};
use crate::wire::{BudgetProvider, FetchGate};

/// One synthetic fetched page (local fixture, never a network fetch).
#[derive(Debug, Clone)]
pub struct PipelinePage {
    /// Page title.
    pub title: String,
    /// Headings.
    pub headings: Vec<String>,
    /// Main text (also the snippet seed and dedup input).
    pub body: String,
    /// Document source for budget admission.
    pub source: DocSource,
    /// Language tag.
    pub lang: String,
    /// License tag.
    pub license: String,
    /// Publication time (unix seconds, 0 when unknown).
    pub published_at: i64,
    /// Fetch time (unix seconds).
    pub fetched_at: i64,
    /// Ranking prior.
    pub prior: f64,
    /// Whether the fetch saw `noindex` (skips indexing, keeps frontier state).
    pub noindex: bool,
    /// HTTP status to record (`None` means 200).
    pub status: Option<u16>,
    /// Whether the body changed since the last fetch (drives 15.6 intervals).
    pub content_changed: bool,
}

impl PipelinePage {
    /// Minimal synthetic page with default metadata.
    #[must_use]
    pub fn new(title: &str, body: &str) -> Self {
        Self {
            title: title.to_owned(),
            headings: Vec::new(),
            body: body.to_owned(),
            source: DocSource::Web,
            lang: "en".to_owned(),
            license: String::new(),
            published_at: 0,
            fetched_at: 1_700_000_000,
            prior: 0.2,
            noindex: false,
            status: Some(200),
            content_changed: true,
        }
    }
}

/// Per-run counters for the pipeline step.
#[derive(Debug, Clone, Default)]
pub struct RunStats {
    /// Dispatched fetch tasks.
    pub dispatched: usize,
    /// Documents stored in the index.
    pub indexed: usize,
    /// URLs skipped by robots or `noindex`.
    pub skipped: usize,
    /// Fetches denied by the [`FetchGate`].
    pub gate_denied: usize,
}

/// Frontier -> ingest -> index wiring over one index backend.
///
/// The pipeline owns no threads and spawns no daemons: callers drive
/// `seed_*` once and then `run_once` per clock tick with a synthetic page
/// table. Budgets come from [`BudgetProvider`]; per-dispatch admission comes
/// from [`FetchGate`]. The B9 main lane replaces the synthetic page table
/// with `web_read`/broker fetches without changing these states.
pub struct Pipeline<B> {
    frontier: Frontier,
    index: B,
    profile: String,
    index_budget_bytes: u64,
}

impl<B: IndexBackend> Pipeline<B> {
    /// Build a pipeline from an open frontier and index.
    pub fn new(frontier: Frontier, index: B, budgets: &impl BudgetProvider, profile: &str) -> Self {
        Self {
            frontier,
            index,
            profile: profile.to_owned(),
            index_budget_bytes: budgets.index_budget_bytes(),
        }
    }

    /// Access the frontier (scheduling assertions in tests).
    pub fn frontier(&self) -> &Frontier {
        &self.frontier
    }

    /// Mutable frontier access (clock-driven dispatch in tests).
    pub fn frontier_mut(&mut self) -> &mut Frontier {
        &mut self.frontier
    }

    /// Access the index backend.
    pub fn index(&self) -> &B {
        &self.index
    }

    /// Configured index budget in bytes (B9 `browser-resources` wire point).
    #[must_use]
    pub fn budget_bytes(&self) -> u64 {
        self.index_budget_bytes
    }

    /// Mutable index access.
    pub fn index_mut(&mut self) -> &mut B {
        &mut self.index
    }

    /// Seed one host from a sitemap document fetched at `sitemap_url`.
    ///
    /// Targets are restricted to the sitemap host and directory prefix per
    /// the sitemaps protocol; cross-host links never seed new hosts.
    pub fn seed_with_sitemap(
        &self,
        host: &str,
        sitemap_url: &str,
        sitemap_xml: &str,
        prior: f64,
        now_secs: u64,
    ) -> Result<usize> {
        self.frontier.add_host(host, prior, None)?;
        let mut admitted = 0;
        for target in parse_sitemap_locs_for_sitemap(sitemap_xml, sitemap_url) {
            if self
                .frontier
                .enqueue(&target, 1, Priority::Normal, Source::Web, now_secs, 1)?
            {
                admitted += 1;
            }
        }
        Ok(admitted)
    }

    /// Seed one host from a feed document (RSS/Atom/JSON Feed).
    pub fn seed_with_feed(
        &self,
        host: &str,
        feed_xml: &str,
        prior: f64,
        now_secs: u64,
    ) -> Result<usize> {
        self.frontier.add_host(host, prior, None)?;
        let mut admitted = 0;
        for target in parse_feed_links(feed_xml) {
            // Feeds only enqueue same-host entries within depth 1; cross-host
            // links register candidates and are never crawled until seeded.
            if self
                .frontier
                .enqueue(&target, 1, Priority::Normal, Source::News, now_secs, 1)?
            {
                admitted += 1;
            }
        }
        Ok(admitted)
    }

    /// Dispatch due URLs and index their synthetic bodies.
    ///
    /// For each `select_due` task the gate is checked first (denied tasks
    /// stay `fetching` for the caller to requeue); otherwise the synthetic
    /// table supplies the page body, `record_outcome` moves the URL to
    /// `pending_commit` (or `skipped`/`gone`/`failed`), the document is
    /// inserted (budget-gated by source priority) and `mark_indexed`
    /// stores the 15.6 refresh interval.
    pub fn run_once(
        &mut self,
        pages: &HashMap<String, PipelinePage>,
        gate: &impl FetchGate,
        now_secs: u64,
    ) -> Result<RunStats> {
        let mut stats = RunStats::default();
        let tasks = self.frontier_mut().select_due(now_secs)?;
        stats.dispatched = tasks.len();
        for task in tasks {
            if !gate.allow_fetch(&task.host, &task.url) {
                stats.gate_denied += 1;
                // Leave the URL in `fetching`; the caller requeues it by
                // recording a retryable outcome on the next tick.
                self.frontier.record_outcome(
                    &task.url,
                    &FetchOutcome {
                        status: Some(503),
                        retry_after_secs: Some(60),
                        duration_ms: 0,
                        content_changed: false,
                        noindex: false,
                        nofollow: false,
                    },
                    now_secs,
                    None,
                    None,
                )?;
                continue;
            }
            let Some(page) = pages.get(&task.url) else {
                self.frontier.record_outcome(
                    &task.url,
                    &FetchOutcome {
                        status: Some(404),
                        retry_after_secs: None,
                        duration_ms: 10,
                        content_changed: false,
                        noindex: false,
                        nofollow: false,
                    },
                    now_secs,
                    None,
                    None,
                )?;
                continue;
            };
            let outcome = FetchOutcome {
                status: page.status.or(Some(200)),
                retry_after_secs: None,
                duration_ms: 400,
                content_changed: page.content_changed,
                noindex: page.noindex,
                nofollow: false,
            };
            let host = task.host.clone();
            let url = task.url.clone();
            if page.noindex {
                self.frontier
                    .record_outcome(&url, &outcome, now_secs, None, None)?;
                stats.skipped += 1;
                continue;
            }
            let doc = Doc {
                url: url.clone(),
                host: host.clone(),
                domain: registrable_domain(&host),
                source: page.source,
                lang: page.lang.clone(),
                license: page.license.clone(),
                title: page.title.clone(),
                headings: page.headings.clone(),
                body: page.body.clone(),
                published_at: page.published_at,
                fetched_at: page.fetched_at.max(i64::try_from(now_secs).unwrap_or(0)),
                prior: page.prior,
            };
            let content_hash = doc.content_hash();
            let simhash = doc.simhash();
            let state = self.frontier.record_outcome(
                &url,
                &outcome,
                now_secs,
                Some(&content_hash),
                Some(simhash),
            )?;
            // Only `pending_commit` URLs advance to the index. `gone`,
            // `skipped` and `failed` outcomes already moved the URL.
            if !matches!(state, crate::frontier::UrlState::PendingCommit) {
                continue;
            }
            match self.index.insert(&doc) {
                Ok(true) => {
                    self.frontier
                        .mark_indexed(&url, page.content_changed, now_secs)?;
                    stats.indexed += 1;
                }
                Ok(false) => {
                    // Exact/near duplicate: still mark indexed so refresh
                    // redispatch keeps working without index growth.
                    self.frontier.mark_indexed(&url, false, now_secs)?;
                }
                Err(crate::error::SearchError::BudgetReached) => {
                    // Low-priority admission stop at 95%: refresh the shared
                    // budget flag and leave the URL pending for a later
                    // window instead of losing it.
                    let _ = self.index.commit()?;
                    debug_assert!(self.index_budget_bytes > 0);
                    continue;
                }
                Err(other) => return Err(other),
            }
        }
        let _ = self.index.commit()?;
        Ok(stats)
    }

    /// Run a `web_search` query over the wired index (section 15.9).
    ///
    /// Conjunctive BM25 first with disjunctive fallback below 10 candidates,
    /// filters before truncation, blending/diversity via `ranking`, and the
    /// `insufficient` flag on raw BM25 and result count. Coverage mirrors
    /// `SearchService` exactly (`profile`, `documents`, `last_crawl_at`,
    /// `by_source`, `budget_reached`).
    pub fn search(&self, query: &str, options: &SearchOptions) -> Result<SearchResponse> {
        let started = std::time::Instant::now();
        let (text, mut site_domains) = split_site_filters(query);
        let mut domains = options.domains.clone();
        domains.append(&mut site_domains);
        let terms = crate::tokenize(&text);
        let k = if options.k == 0 {
            10
        } else {
            options.k.clamp(1, crate::ranking::MAX_K)
        };
        let lang = options.lang.as_deref().filter(|value| !value.is_empty());
        let mut candidates = self.index.search_bm25(&terms, true, 200, lang, &domains)?;
        if candidates.len() < 10 {
            candidates = self.index.search_bm25(&terms, false, 200, lang, &domains)?;
        }
        let ranked = rank_candidates(
            &candidates,
            &text,
            options.freshness,
            lang,
            &domains,
            k,
            options.now_secs,
        );
        let results = crate::ranking::dedupe_results(ranked);
        let max_bm25 = candidates
            .iter()
            .map(|candidate| candidate.bm25)
            .fold(0.0_f64, f64::max);
        let insufficient = results.len() < 3 || max_bm25 < 0.1;
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

    /// Coverage snapshot for the 15.11 report (documents, sources, budget).
    pub fn coverage_snapshot(&self) -> Result<Coverage> {
        Ok(Coverage {
            profile: self.profile.clone(),
            documents: self.index.len()?,
            last_crawl_at: self.index.last_crawl_at()?,
            by_source: self.index.by_source()?,
            budget_reached: self.index.budget_state().budget_reached,
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

/// Known-item recall over the wired index (sections 15.10-15.11).
///
/// Queries `queries` (each entry is the query text plus the expected URL)
/// with `k = 10` and returns `(success_at_10, mrr)`. Callers build the
/// synthetic corpus with distinctive 3-5 term titles; the deterministic and
/// tantivy backends must both reach success@10 >= 0.9 on that probe.
pub fn known_item_recall<B: IndexBackend>(
    pipeline: &Pipeline<B>,
    queries: &[(&str, &str)],
) -> Result<(f64, f64)> {
    if queries.is_empty() {
        return Ok((1.0, 1.0));
    }
    let mut hits = 0_usize;
    let mut reciprocal_sum = 0.0_f64;
    for (query, expected_url) in queries {
        let response = pipeline.search(
            query,
            &SearchOptions {
                k: 10,
                ..Default::default()
            },
        )?;
        if let Some(rank) = response
            .results
            .iter()
            .position(|result| result.url == *expected_url)
        {
            hits += 1;
            reciprocal_sum += 1.0 / (rank as f64 + 1.0);
        }
    }
    let total = queries.len() as f64;
    Ok((hits as f64 / total, reciprocal_sum / total))
}

/// Real on-disk bytes per document for the 15.11 size report.
///
/// `dir_bytes` is the measured index directory size after `commit`;
/// `documents` is the indexed document count. Returns `None` on empty
/// corpora so callers never divide by zero.
#[must_use]
pub fn bytes_per_doc(dir_bytes: u64, documents: u64) -> Option<f64> {
    if documents == 0 {
        return None;
    }
    Some(dir_bytes as f64 / documents as f64)
}
