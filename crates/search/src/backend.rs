//! Index backend trait shared by the deterministic and production stores.
//!
//! [`DiskIndex`] is the deterministic SQLite backend used by the isolated
//! test suite. The tantivy production store (`tantivy_index::TantivyIndex`,
//! behind the `tantivy` feature) implements the same document model, BM25
//! parameters, 1 KiB snippets, delete-by-URL updates and 95%-of-budget
//! source-priority admission, so the main lane can swap backends without
//! changing query or ingest call sites.

use std::collections::HashMap;

use crate::error::Result;
use crate::index::{BudgetState, DiskIndex, Doc, ScoredDoc};

/// Minimal index surface used by the pipeline and `web_search` wiring.
///
/// Both backends enforce the same contract: exact duplicates by `content_hash`
/// and same-host SimHash merges return `Ok(false)`, low-priority sources
/// (`web`, `news`, `hn`) return [`crate::error::SearchError::BudgetReached`]
/// once usage reaches 95% of budget, and `search_bm25` applies `lang` and
/// `domains` filters before truncating to `limit` with stable URL tie-breaks.
pub trait IndexBackend {
    /// Insert or replace `doc`; `Ok(false)` means an exact/near duplicate.
    fn insert(&mut self, doc: &Doc) -> Result<bool>;
    /// Remove a document by URL (404/410 and `noindex` handling).
    fn remove(&mut self, url: &str) -> Result<bool>;
    /// Flush staged writes and refresh the disk-budget state.
    fn commit(&mut self) -> Result<BudgetState>;
    /// Current disk usage and budget state.
    fn budget_state(&self) -> BudgetState;
    /// Number of indexed documents.
    fn len(&self) -> Result<u64>;
    /// Whether the index holds no documents.
    fn is_empty(&self) -> Result<bool>;
    /// Documents per source for coverage reporting.
    fn by_source(&self) -> Result<HashMap<String, u64>>;
    /// Latest fetch timestamp across documents, if any.
    fn last_crawl_at(&self) -> Result<Option<i64>>;
    /// Whether `url` is indexed.
    fn contains(&self, url: &str) -> Result<bool>;
    /// BM25 candidate retrieval with hard filters applied before truncation.
    fn search_bm25(
        &self,
        query_terms: &[String],
        conjunctive: bool,
        limit: usize,
        lang: Option<&str>,
        domains: &[String],
    ) -> Result<Vec<ScoredDoc>>;
}

impl IndexBackend for DiskIndex {
    fn insert(&mut self, doc: &Doc) -> Result<bool> {
        DiskIndex::insert(self, doc)
    }
    fn remove(&mut self, url: &str) -> Result<bool> {
        DiskIndex::remove(self, url)
    }
    fn commit(&mut self) -> Result<BudgetState> {
        DiskIndex::commit(self)
    }
    fn budget_state(&self) -> BudgetState {
        DiskIndex::budget_state(self)
    }
    fn len(&self) -> Result<u64> {
        DiskIndex::len(self)
    }
    fn is_empty(&self) -> Result<bool> {
        DiskIndex::is_empty(self)
    }
    fn by_source(&self) -> Result<HashMap<String, u64>> {
        DiskIndex::by_source(self)
    }
    fn last_crawl_at(&self) -> Result<Option<i64>> {
        DiskIndex::last_crawl_at(self)
    }
    fn contains(&self, url: &str) -> Result<bool> {
        DiskIndex::contains(self, url)
    }
    fn search_bm25(
        &self,
        query_terms: &[String],
        conjunctive: bool,
        limit: usize,
        lang: Option<&str>,
        domains: &[String],
    ) -> Result<Vec<ScoredDoc>> {
        DiskIndex::search_bm25(self, query_terms, conjunctive, limit, lang, domains)
    }
}
