//! Production tantivy index implementing the section 15.5 document model.
//!
//! Compiled only with the `tantivy` feature so the default build stays light
//! and deterministic. The schema mirrors [`crate::index::DiskIndex`] exactly:
//!
//! | Field | Type | Notes |
//! |---|---|---|
//! | `url` | STRING, STORED | update key, `delete_term(url)` on rewrite |
//! | `host`, `domain`, `source`, `lang`, `license` | STRING, STORED | filters; `license` kept for Wikipedia attribution |
//! | `title` | TEXT (positions) + STORED | `excalibur` tokenizer, boost x3 |
//! | `headings` | TEXT (freqs only) | boost x1.5, no positions to save disk |
//! | `body` | TEXT (freqs only) | boost x1, no positions per 15.5 |
//! | `url_tokens` | TEXT | host + path + query tokens, boost x2 |
//! | `snippet` | STORED | first 1 KiB of the main text, never indexed |
//! | `published_at`, `fetched_at` | I64 FAST + STORED | unix seconds (DATE FAST in 15.5; unix seconds keep the wiring dependency-free) |
//! | `prior` | F64 FAST + STORED | harmonic-rank / popularity prior |
//! | `content_hash` | STRING STORED | exact-duplicate key (`sha256` hex) |
//! | `simhash` | U64 FAST + STORED | near-duplicate key, same-host Hamming <= 3 |
//!
//! Analyzer `excalibur` is `SimpleTokenizer + RemoveLongFilter(40) +
//! LowerCaser + AsciiFoldingFilter`, matching [`crate::tokenize`] (the
//! deterministic backend folds the same accents without tantivy). BM25 uses
//! tantivy defaults (`k1 = 1.2`, `b = 0.75`, fixed) with field boosts
//! applied as query-time boosts, identical to [`crate::index`] constants.
//!
//! Writer budget is 64 MiB (the documented per-thread minimum is 15 MiB).
//! Inserts stage in the writer; [`TantivyIndex::commit`] publishes them and
//! refreshes the 95%-of-budget state. Source-priority admission, dedup and
//! stable URL tie-breaks match `DiskIndex` so the main lane swaps backends
//! without changing query or ingest call sites. All checks use local
//! synthetic fixtures; no live crawls.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::backend::IndexBackend;
use crate::dedup::{SIMHASH_HAMMING_THRESHOLD, hamming_distance};
use crate::error::{Result, SearchError};
use crate::index::{
    BOOST_BODY, BOOST_HEADINGS, BOOST_TITLE, BOOST_URL, BudgetState, Doc, DocSource, ScoredDoc,
    StoredDoc,
};

/// Writer memory budget: 64 MiB per section 15.5.
const WRITER_BUDGET_BYTES: usize = 64_000_000;
/// Analyzer name registered on every tantivy index.
const ANALYZER_NAME: &str = "excalibur";

#[cfg(feature = "tantivy")]
use tantivy::collector::{Count, TopDocs};
#[cfg(feature = "tantivy")]
use tantivy::query::{AllQuery, BooleanQuery, BoostQuery, Occur, Query, TermQuery};
#[cfg(feature = "tantivy")]
use tantivy::schema::{
    FAST, Field, IndexRecordOption, STORED, STRING, Schema, TextFieldIndexing, TextOptions, Value,
};
#[cfg(feature = "tantivy")]
use tantivy::tokenizer::{
    AsciiFoldingFilter, LowerCaser, RemoveLongFilter, SimpleTokenizer, TextAnalyzer,
};
#[cfg(feature = "tantivy")]
use tantivy::{Index, IndexReader, IndexWriter, ReloadPolicy, TantivyDocument, Term};

/// Tantivy field handles for the 15.5 schema.
#[cfg(feature = "tantivy")]
#[derive(Debug, Clone, Copy)]
struct Fields {
    url: Field,
    host: Field,
    domain: Field,
    source: Field,
    lang: Field,
    license: Field,
    title: Field,
    headings: Field,
    body: Field,
    url_tokens: Field,
    snippet: Field,
    published_at: Field,
    fetched_at: Field,
    prior: Field,
    content_hash: Field,
    simhash: Field,
}

/// Production tantivy index (feature `tantivy`).
#[cfg(feature = "tantivy")]
pub struct TantivyIndex {
    index: Index,
    reader: IndexReader,
    writer: IndexWriter<TantivyDocument>,
    fields: Fields,
    path: Option<PathBuf>,
    budget_bytes: u64,
    budget_reached: bool,
    staged: u64,
}

#[cfg(feature = "tantivy")]
fn map_err(value: tantivy::TantivyError) -> SearchError {
    SearchError::Storage(value.to_string())
}

#[cfg(feature = "tantivy")]
fn build_schema() -> (Schema, Fields) {
    let mut builder = Schema::builder();
    let url = builder.add_text_field("url", STRING | STORED);
    let host = builder.add_text_field("host", STRING | STORED);
    let domain = builder.add_text_field("domain", STRING | STORED);
    let source = builder.add_text_field("source", STRING | STORED);
    let lang = builder.add_text_field("lang", STRING | STORED);
    let license = builder.add_text_field("license", STRING | STORED);
    let title_options: TextOptions = TextOptions::default()
        .set_indexing_options(
            TextFieldIndexing::default()
                .set_tokenizer(ANALYZER_NAME)
                .set_index_option(IndexRecordOption::WithFreqsAndPositions),
        )
        .set_stored();
    let title = builder.add_text_field("title", title_options);
    let freq_options = |stored: bool| {
        let mut options = TextOptions::default().set_indexing_options(
            TextFieldIndexing::default()
                .set_tokenizer(ANALYZER_NAME)
                .set_index_option(IndexRecordOption::WithFreqs),
        );
        if stored {
            options = options.set_stored();
        }
        options
    };
    let headings = builder.add_text_field("headings", freq_options(true));
    let body = builder.add_text_field("body", freq_options(false));
    let url_tokens = builder.add_text_field("url_tokens", freq_options(false));
    let snippet = builder.add_text_field("snippet", STORED);
    let published_at = builder.add_i64_field("published_at", FAST | STORED);
    let fetched_at = builder.add_i64_field("fetched_at", FAST | STORED);
    let prior = builder.add_f64_field("prior", FAST | STORED);
    let content_hash = builder.add_text_field("content_hash", STRING | STORED);
    let simhash = builder.add_u64_field("simhash", FAST | STORED);
    let schema = builder.build();
    (
        schema,
        Fields {
            url,
            host,
            domain,
            source,
            lang,
            license,
            title,
            headings,
            body,
            url_tokens,
            snippet,
            published_at,
            fetched_at,
            prior,
            content_hash,
            simhash,
        },
    )
}

#[cfg(feature = "tantivy")]
fn register_tokenizer(index: &Index) {
    let analyzer = TextAnalyzer::builder(SimpleTokenizer::default())
        .filter(RemoveLongFilter::limit(40))
        .filter(LowerCaser)
        .filter(AsciiFoldingFilter)
        .build();
    index.tokenizers().register(ANALYZER_NAME, analyzer);
}

#[cfg(feature = "tantivy")]
fn dir_size_bytes(path: &Path) -> u64 {
    let mut total = 0_u64;
    let mut stack = vec![path.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let entry_path = entry.path();
            if entry_path.is_dir() {
                stack.push(entry_path);
            } else if let Ok(meta) = entry.metadata() {
                total = total.saturating_add(meta.len());
            }
        }
    }
    total
}

#[cfg(feature = "tantivy")]
impl TantivyIndex {
    /// Access the underlying tantivy index (mapping-note surface).
    #[must_use]
    pub fn index(&self) -> &Index {
        &self.index
    }

    fn with_index(
        index: Index,
        path: Option<PathBuf>,
        budget_bytes: u64,
    ) -> std::result::Result<Self, tantivy::TantivyError> {
        register_tokenizer(&index);
        let reader = index
            .reader_builder()
            .reload_policy(ReloadPolicy::Manual)
            .try_into()?;
        let writer = index.writer(WRITER_BUDGET_BYTES)?;
        let fields = fields_from_schema(&index.schema());
        Ok(Self {
            index,
            reader,
            writer,
            fields,
            path,
            budget_bytes,
            budget_reached: false,
            staged: 0,
        })
    }

    /// Open or create a file-backed production index at `path`.
    pub fn open(path: &Path, budget_bytes: u64) -> Result<Self> {
        let (schema, _) = build_schema();
        let directory = tantivy::directory::MmapDirectory::open(path)
            .map_err(|err| SearchError::Storage(err.to_string()))?;
        let index = Index::open_or_create(directory, schema).map_err(map_err)?;
        Self::with_index(index, Some(path.to_path_buf()), budget_bytes).map_err(map_err)
    }

    /// Open an in-RAM index for tests (no real byte accounting).
    pub fn open_in_ram(budget_bytes: u64) -> Result<Self> {
        let (schema, _) = build_schema();
        let index = Index::create_in_ram(schema);
        Self::with_index(index, None, budget_bytes).map_err(map_err)
    }

    /// Whether `source` stops first at 95% of budget (mirrors `DiskIndex`).
    #[must_use]
    pub fn is_low_priority_source(source: DocSource) -> bool {
        matches!(source, DocSource::Web | DocSource::News | DocSource::Hn)
    }

    fn budget_state_inner(&self) -> BudgetState {
        let bytes = self.path.as_deref().map(dir_size_bytes).unwrap_or(0);
        let reached =
            self.budget_bytes > 0 && bytes as u128 * 100 >= self.budget_bytes as u128 * 95;
        BudgetState {
            bytes,
            budget_bytes: self.budget_bytes,
            budget_reached: reached || self.budget_reached,
        }
    }

    fn term_query(&self, field: Field, text: &str) -> Box<dyn Query> {
        Box::new(TermQuery::new(
            Term::from_field_text(field, text),
            IndexRecordOption::Basic,
        ))
    }

    fn count_term(&self, field: Field, text: &str) -> Result<usize> {
        let searcher = self.reader.searcher();
        let query = self.term_query(field, text);
        searcher.search(&*query, &Count).map_err(map_err)
    }

    fn find_by_url(&self, url: &str) -> Result<Option<StoredDoc>> {
        let searcher = self.reader.searcher();
        let query = self.term_query(self.fields.url, url);
        let top: Vec<(f32, tantivy::DocAddress)> = searcher
            .search(&*query, &TopDocs::with_limit(2).order_by_score())
            .map_err(map_err)?;
        for (_, address) in top {
            if let Some(stored) = self.load_at(&searcher, address)?
                && stored.url == url
            {
                return Ok(Some(stored));
            }
        }
        Ok(None)
    }

    fn load_at(
        &self,
        searcher: &tantivy::Searcher,
        address: tantivy::DocAddress,
    ) -> Result<Option<StoredDoc>> {
        let doc: TantivyDocument = searcher.doc(address).map_err(map_err)?;
        let text = |field: Field| {
            doc.get_first(field)
                .and_then(|value| value.as_str())
                .unwrap_or_default()
                .to_owned()
        };
        let url = text(self.fields.url);
        if url.is_empty() {
            return Ok(None);
        }
        let i64_value = |field: Field| {
            doc.get_first(field)
                .and_then(|value| value.as_i64())
                .unwrap_or(0)
        };
        let f64_value = |field: Field| {
            doc.get_first(field)
                .and_then(|value| value.as_f64())
                .unwrap_or(0.2)
        };
        let u64_value = |field: Field| {
            doc.get_first(field)
                .and_then(|value| value.as_u64())
                .unwrap_or(0)
        };
        Ok(Some(StoredDoc {
            url,
            host: text(self.fields.host),
            domain: text(self.fields.domain),
            source: text(self.fields.source),
            lang: text(self.fields.lang),
            title: text(self.fields.title),
            headings: text(self.fields.headings),
            snippet: text(self.fields.snippet),
            published_at: i64_value(self.fields.published_at),
            fetched_at: i64_value(self.fields.fetched_at),
            prior: f64_value(self.fields.prior),
            content_hash: text(self.fields.content_hash),
            simhash: u64_value(self.fields.simhash),
        }))
    }

    fn host_simhashes(&self, host: &str) -> Result<Vec<(String, u64)>> {
        let searcher = self.reader.searcher();
        let query = self.term_query(self.fields.host, &host.to_ascii_lowercase());
        let top: Vec<(f32, tantivy::DocAddress)> = searcher
            .search(&*query, &TopDocs::with_limit(5000).order_by_score())
            .map_err(map_err)?;
        let mut out = Vec::new();
        for (_, address) in top {
            if let Some(stored) = self.load_at(&searcher, address)?
                && stored.host == host.to_ascii_lowercase()
            {
                // Simhash is stored as u64; reload it from the doc.
                let doc: TantivyDocument = searcher.doc(address).map_err(map_err)?;
                let sim = doc
                    .get_first(self.fields.simhash)
                    .and_then(|value| value.as_u64())
                    .unwrap_or(0);
                out.push((stored.url, sim));
            }
        }
        Ok(out)
    }

    fn make_doc(&self, doc: &Doc, content_hash: &str, simhash: u64) -> TantivyDocument {
        let mut out = TantivyDocument::default();
        out.add_text(self.fields.url, &doc.url);
        out.add_text(self.fields.host, doc.host.to_ascii_lowercase());
        out.add_text(self.fields.domain, &doc.domain);
        let source = match doc.source {
            DocSource::Web => "web",
            DocSource::Wiki => "wiki",
            DocSource::Docs => "docs",
            DocSource::News => "news",
            DocSource::Arxiv => "arxiv",
            DocSource::Hn => "hn",
            DocSource::Read => "read",
        };
        out.add_text(self.fields.source, source);
        out.add_text(self.fields.lang, &doc.lang);
        out.add_text(self.fields.license, &doc.license);
        out.add_text(self.fields.title, &doc.title);
        out.add_text(self.fields.headings, doc.headings.join("\n"));
        out.add_text(self.fields.body, &doc.body);
        out.add_text(self.fields.url_tokens, format!("{} {}", doc.host, doc.url));
        out.add_text(self.fields.snippet, doc.snippet());
        out.add_i64(self.fields.published_at, doc.published_at);
        out.add_i64(self.fields.fetched_at, doc.fetched_at);
        out.add_f64(self.fields.prior, doc.prior);
        out.add_text(self.fields.content_hash, content_hash);
        out.add_u64(self.fields.simhash, simhash);
        out
    }

    fn boosted_term(&self, field: Field, term: &str, boost: f32) -> Box<dyn Query> {
        Box::new(BoostQuery::new(
            Box::new(TermQuery::new(
                Term::from_field_text(field, term),
                IndexRecordOption::WithFreqs,
            )),
            boost,
        ))
    }

    /// Insert or replace `doc` with the same dedup and budget contract as `DiskIndex`.
    pub fn insert_inner(&mut self, doc: &Doc) -> Result<bool> {
        if doc.url.trim().is_empty() || doc.title.trim().is_empty() {
            return Err(crate::error::SearchError::Rejected(
                "documents require url and title".to_owned(),
            ));
        }
        if Self::is_low_priority_source(doc.source) && self.budget_state_inner().budget_reached {
            return Err(crate::error::SearchError::BudgetReached);
        }
        self.reader.reload().map_err(map_err)?;
        let content_hash = doc.content_hash();
        let simhash = doc.simhash();
        // Exact duplicates by content hash (any host, different URL).
        let searcher = self.reader.searcher();
        let hash_query = self.term_query(self.fields.content_hash, &content_hash);
        let hash_hits: Vec<(f32, tantivy::DocAddress)> = searcher
            .search(&*hash_query, &TopDocs::with_limit(5).order_by_score())
            .map_err(map_err)?;
        for (_, address) in hash_hits {
            if let Some(stored) = self.load_at(&searcher, address)?
                && stored.url != doc.url
            {
                return Ok(false);
            }
        }
        drop(searcher);
        // Near duplicates inside one host (Hamming <= 3).
        for (other_url, other_sim) in self.host_simhashes(&doc.host)? {
            if other_url != doc.url
                && hamming_distance(other_sim, simhash) <= SIMHASH_HAMMING_THRESHOLD
            {
                return Ok(false);
            }
        }
        self.writer
            .delete_term(Term::from_field_text(self.fields.url, &doc.url));
        let prepared = self.make_doc(doc, &content_hash, simhash);
        self.writer.add_document(prepared).map_err(map_err)?;
        self.staged += 1;
        Ok(true)
    }
}

#[cfg(feature = "tantivy")]
fn fields_from_schema(schema: &Schema) -> Fields {
    let field = |name: &str| schema.get_field(name).expect("tantivy schema field");
    Fields {
        url: field("url"),
        host: field("host"),
        domain: field("domain"),
        source: field("source"),
        lang: field("lang"),
        license: field("license"),
        title: field("title"),
        headings: field("headings"),
        body: field("body"),
        url_tokens: field("url_tokens"),
        snippet: field("snippet"),
        published_at: field("published_at"),
        fetched_at: field("fetched_at"),
        prior: field("prior"),
        content_hash: field("content_hash"),
        simhash: field("simhash"),
    }
}

#[cfg(feature = "tantivy")]
impl IndexBackend for TantivyIndex {
    fn insert(&mut self, doc: &Doc) -> Result<bool> {
        self.insert_inner(doc)
    }

    fn remove(&mut self, url: &str) -> Result<bool> {
        self.reader.reload().map_err(map_err)?;
        let existed = self.find_by_url(url)?.is_some();
        self.writer
            .delete_term(Term::from_field_text(self.fields.url, url));
        self.staged += 1;
        Ok(existed)
    }

    fn commit(&mut self) -> Result<BudgetState> {
        if self.staged > 0 {
            self.writer.commit().map_err(map_err)?;
            self.staged = 0;
        }
        self.reader.reload().map_err(map_err)?;
        let state = self.budget_state_inner();
        if state.budget_reached {
            self.budget_reached = true;
        }
        Ok(self.budget_state_inner())
    }

    fn budget_state(&self) -> BudgetState {
        self.budget_state_inner()
    }

    fn len(&self) -> Result<u64> {
        Ok(self.reader.searcher().num_docs())
    }

    fn is_empty(&self) -> Result<bool> {
        Ok(self.reader.searcher().num_docs() == 0)
    }

    fn by_source(&self) -> Result<HashMap<String, u64>> {
        let mut out = HashMap::new();
        for source in ["web", "wiki", "docs", "news", "arxiv", "hn", "read"] {
            let count = self.count_term(self.fields.source, source)?;
            if count > 0 {
                out.insert(source.to_owned(), count as u64);
            }
        }
        Ok(out)
    }

    fn last_crawl_at(&self) -> Result<Option<i64>> {
        let searcher = self.reader.searcher();
        let top: Vec<(f32, tantivy::DocAddress)> = searcher
            .search(&AllQuery, &TopDocs::with_limit(100_000).order_by_score())
            .map_err(map_err)?;
        let mut latest: Option<i64> = None;
        for (_, address) in top {
            if let Some(stored) = self.load_at(&searcher, address)? {
                latest = Some(latest.unwrap_or(stored.fetched_at).max(stored.fetched_at));
            }
        }
        Ok(latest)
    }

    fn contains(&self, url: &str) -> Result<bool> {
        Ok(self.find_by_url(url)?.is_some())
    }

    fn search_bm25(
        &self,
        query_terms: &[String],
        conjunctive: bool,
        limit: usize,
        lang: Option<&str>,
        domains: &[String],
    ) -> Result<Vec<ScoredDoc>> {
        if query_terms.is_empty() {
            return Ok(Vec::new());
        }
        let fetch_limit = if lang.is_some() || !domains.is_empty() {
            (limit * 10).clamp(500, 5000)
        } else {
            limit.max(1)
        };
        let mut term_clauses: Vec<(Occur, Box<dyn Query>)> = Vec::new();
        for term in query_terms {
            let per_field = vec![
                (
                    Occur::Should,
                    self.boosted_term(self.fields.title, term, BOOST_TITLE as f32),
                ),
                (
                    Occur::Should,
                    self.boosted_term(self.fields.headings, term, BOOST_HEADINGS as f32),
                ),
                (
                    Occur::Should,
                    self.boosted_term(self.fields.url_tokens, term, BOOST_URL as f32),
                ),
                (
                    Occur::Should,
                    self.boosted_term(self.fields.body, term, BOOST_BODY as f32),
                ),
            ];
            term_clauses.push((
                if conjunctive {
                    Occur::Must
                } else {
                    Occur::Should
                },
                Box::new(BooleanQuery::new(per_field)),
            ));
        }
        let query = BooleanQuery::new(term_clauses);
        let searcher = self.reader.searcher();
        let top: Vec<(f32, tantivy::DocAddress)> = searcher
            .search(&query, &TopDocs::with_limit(fetch_limit).order_by_score())
            .map_err(map_err)?;
        let mut scored: Vec<(String, f64)> = Vec::new();
        for (score, address) in top {
            if let Some(stored) = self.load_at(&searcher, address)? {
                scored.push((stored.url, f64::from(score)));
            }
        }
        // Hard filters before truncation (BSF-09): keep only matching docs.
        let mut filtered: Vec<(String, f64)> = Vec::new();
        for (url, score) in scored {
            let Some(stored) = self.load_at_by_url(&searcher, &url)? else {
                continue;
            };
            if lang.is_some_and(|want| !stored.lang.eq_ignore_ascii_case(want)) {
                continue;
            }
            if !domains.is_empty()
                && !domains.iter().any(|domain| {
                    stored.host == *domain || stored.host.ends_with(&format!(".{domain}"))
                })
            {
                continue;
            }
            filtered.push((url, score));
        }
        filtered.sort_by(|left, right| {
            right
                .1
                .partial_cmp(&left.1)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| left.0.cmp(&right.0))
        });
        filtered.truncate(limit.max(1));
        let mut out = Vec::with_capacity(filtered.len());
        for (url, bm25) in filtered {
            if let Some(stored) = self.load_at_by_url(&searcher, &url)? {
                // Rebuild the scored doc from stored fields; tantivy already
                // applied BM25 with k1=1.2/b=0.75 and field boosts.
                out.push(ScoredDoc { doc: stored, bm25 });
            }
        }
        Ok(out)
    }
}

#[cfg(feature = "tantivy")]
impl TantivyIndex {
    fn load_at_by_url(&self, searcher: &tantivy::Searcher, url: &str) -> Result<Option<StoredDoc>> {
        let query = self.term_query(self.fields.url, url);
        let top: Vec<(f32, tantivy::DocAddress)> = searcher
            .search(&*query, &TopDocs::with_limit(2).order_by_score())
            .map_err(map_err)?;
        for (_, address) in top {
            if let Some(stored) = self.load_at(searcher, address)?
                && stored.url == url
            {
                return Ok(Some(stored));
            }
        }
        Ok(None)
    }
}
