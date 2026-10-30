//! Deterministic disk-backed document index (section 15.5).
//!
//! The architecture selects tantivy 0.27 as the production store. This
//! foundation ships a deterministic SQLite
//! backend with the same document model, BM25 parameters and budget
//! semantics, so the main lane can swap the backend without changing query
//! or ingest call sites. Every behaviour is covered by local synthetic
//! fixtures; no network or model access is involved.

use std::collections::{HashMap, HashSet};

use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};

use crate::dedup::{self, SIMHASH_HAMMING_THRESHOLD, hamming_distance};
use crate::error::{Result, SearchError};
use crate::tokenize;

/// BM25 constants shared with the architecture (not configurable).
pub const BM25_K1: f64 = 1.2;
/// BM25 length-normalization constant.
pub const BM25_B: f64 = 0.75;
/// Field boosts: title x3, headings x1.5, url tokens x2, body x1.
pub const BOOST_TITLE: f64 = 3.0;
/// Headings boost.
pub const BOOST_HEADINGS: f64 = 1.5;
/// URL token boost.
pub const BOOST_URL: f64 = 2.0;
/// Body boost.
pub const BOOST_BODY: f64 = 1.0;
/// Stored snippet length (1 KiB of the main text).
pub const SNIPPET_CHARS: usize = 1024;

/// Document source, serialized as the `web_search` source string.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DocSource {
    Web,
    Wiki,
    Docs,
    News,
    Arxiv,
    Hn,
    Read,
}

impl DocSource {
    fn as_str(self) -> &'static str {
        match self {
            Self::Web => "web",
            Self::Wiki => "wiki",
            Self::Docs => "docs",
            Self::News => "news",
            Self::Arxiv => "arxiv",
            Self::Hn => "hn",
            Self::Read => "read",
        }
    }

    fn parse(value: &str) -> Self {
        match value {
            "wiki" => Self::Wiki,
            "docs" => Self::Docs,
            "news" => Self::News,
            "arxiv" => Self::Arxiv,
            "hn" => Self::Hn,
            "read" => Self::Read,
            _ => Self::Web,
        }
    }
}

/// A document to index.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Doc {
    /// Canonical URL (unique key).
    pub url: String,
    /// Lower-cased host.
    pub host: String,
    /// Registrable domain approximation.
    pub domain: String,
    /// Document source.
    pub source: DocSource,
    /// Language tag (`es`, `en`, ...).
    pub lang: String,
    /// License tag (e.g. `CC-BY-SA` for Wikipedia).
    pub license: String,
    /// Title.
    pub title: String,
    /// Headings.
    pub headings: Vec<String>,
    /// Main text without positions stored.
    pub body: String,
    /// Publication time (unix seconds, 0 when unknown).
    pub published_at: i64,
    /// Fetch time (unix seconds).
    pub fetched_at: i64,
    /// Ranking prior (harmonic-rank or popularity based).
    pub prior: f64,
}

impl Doc {
    /// Snippet: first [`SNIPPET_CHARS`] characters of the main text.
    #[must_use]
    pub fn snippet(&self) -> String {
        self.body.chars().take(SNIPPET_CHARS).collect()
    }

    /// Exact-duplicate key over the normalized body.
    #[must_use]
    pub fn content_hash(&self) -> String {
        dedup::content_hash(&self.body)
    }

    /// Near-duplicate key over title + headings + body tokens.
    #[must_use]
    pub fn simhash(&self) -> u64 {
        let mut tokens = tokenize(&self.title);
        for heading in &self.headings {
            tokens.extend(tokenize(heading));
        }
        tokens.extend(tokenize(&self.body));
        dedup::simhash64(&tokens)
    }

    fn headings_joined(&self) -> String {
        self.headings.join("\n")
    }

    fn url_tokens(&self) -> Vec<String> {
        tokenize(&format!("{} {}", self.host, self.url))
    }
}

/// Scored candidate returned by the index backend.
#[derive(Debug, Clone)]
pub struct ScoredDoc {
    /// Stored document fields for response rendering.
    pub doc: StoredDoc,
    /// Raw BM25 score before prior/freshness blending.
    pub bm25: f64,
}

/// Stored document projection read back from disk.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredDoc {
    /// Canonical URL.
    pub url: String,
    /// Host.
    pub host: String,
    /// Domain.
    pub domain: String,
    /// Source string.
    pub source: String,
    /// Language tag.
    pub lang: String,
    /// Title.
    pub title: String,
    /// Headings.
    pub headings: String,
    /// Stored 1 KiB snippet.
    pub snippet: String,
    /// Publication time.
    pub published_at: i64,
    /// Fetch time.
    pub fetched_at: i64,
    /// Ranking prior.
    pub prior: f64,
    /// Exact-duplicate key.
    pub content_hash: String,
    /// Near-duplicate key.
    pub simhash: u64,
}

/// Disk usage and admission state.
#[derive(Debug, Clone, Copy)]
pub struct BudgetState {
    /// Current index file size in bytes.
    pub bytes: u64,
    /// Configured budget in bytes.
    pub budget_bytes: u64,
    /// True once usage reached 95% of budget.
    pub budget_reached: bool,
}

/// Deterministic disk-backed index.
///
/// Storage layout is a single SQLite file with `docs`, `terms` and `meta`
/// tables. Commits are atomic; updates use delete-then-insert by URL.
/// Low-priority ingestion stops once usage reaches 95% of budget.
pub struct DiskIndex {
    conn: rusqlite::Connection,
    path: std::path::PathBuf,
    budget_bytes: u64,
    budget_reached: bool,
    staged: u64,
}

impl DiskIndex {
    /// Open or create the index file with the given disk budget.
    pub fn open(path: &std::path::Path, budget_bytes: u64) -> Result<Self> {
        let conn = rusqlite::Connection::open(path)?;
        let index = Self {
            conn,
            path: path.to_path_buf(),
            budget_bytes,
            budget_reached: false,
            staged: 0,
        };
        index.migrate()?;
        Ok(index)
    }

    /// Open an in-memory index for tests.
    pub fn open_memory(budget_bytes: u64) -> Result<Self> {
        let conn = rusqlite::Connection::open_in_memory()?;
        let index = Self {
            conn,
            path: std::path::PathBuf::from(":memory:"),
            budget_bytes,
            budget_reached: false,
            staged: 0,
        };
        index.migrate()?;
        Ok(index)
    }

    fn migrate(&self) -> Result<()> {
        self.conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS docs(
                url TEXT PRIMARY KEY,
                host TEXT NOT NULL,
                domain TEXT NOT NULL DEFAULT '',
                source TEXT NOT NULL DEFAULT 'web',
                lang TEXT NOT NULL DEFAULT '',
                license TEXT NOT NULL DEFAULT '',
                title TEXT NOT NULL DEFAULT '',
                headings TEXT NOT NULL DEFAULT '',
                snippet TEXT NOT NULL DEFAULT '',
                published_at INTEGER NOT NULL DEFAULT 0,
                fetched_at INTEGER NOT NULL DEFAULT 0,
                prior REAL NOT NULL DEFAULT 0.2,
                content_hash TEXT NOT NULL DEFAULT '',
                simhash INTEGER NOT NULL DEFAULT 0,
                title_len INTEGER NOT NULL DEFAULT 0,
                headings_len INTEGER NOT NULL DEFAULT 0,
                url_len INTEGER NOT NULL DEFAULT 0,
                body_len INTEGER NOT NULL DEFAULT 0
            );
            CREATE TABLE IF NOT EXISTS terms(
                term TEXT NOT NULL,
                url TEXT NOT NULL,
                tf_title INTEGER NOT NULL DEFAULT 0,
                tf_headings INTEGER NOT NULL DEFAULT 0,
                tf_url INTEGER NOT NULL DEFAULT 0,
                tf_body INTEGER NOT NULL DEFAULT 0,
                PRIMARY KEY(term, url)
            );
            CREATE INDEX IF NOT EXISTS idx_terms_term ON terms(term);
            CREATE INDEX IF NOT EXISTS idx_docs_host ON docs(host);
            CREATE TABLE IF NOT EXISTS meta(
                key TEXT PRIMARY KEY,
                value TEXT NOT NULL DEFAULT ''
            );",
        )?;
        Ok(())
    }

    /// Whether `source` is low priority for budget admission.
    /// General web, news and HN rotative ingestion stops first at 95% of
    /// budget; wiki, docs, arXiv and read pages remain admissible.
    #[must_use]
    pub fn is_low_priority_source(source: DocSource) -> bool {
        matches!(source, DocSource::Web | DocSource::News | DocSource::Hn)
    }

    /// Insert or replace `doc`, enforcing exact and near-duplicate rules and
    /// the 95%-of-budget admission policy.
    /// Returns `true` when the document was stored.
    /// Returns [`SearchError::BudgetReached`] when usage already reached 95%
    /// and `doc` carries a low-priority source.
    pub fn insert(&mut self, doc: &Doc) -> Result<bool> {
        if doc.url.trim().is_empty() || doc.title.trim().is_empty() {
            return Err(SearchError::Rejected(
                "documents require url and title".to_owned(),
            ));
        }
        if Self::is_low_priority_source(doc.source) && self.budget_state().budget_reached {
            return Err(SearchError::BudgetReached);
        }
        let content_hash = doc.content_hash();
        let simhash = doc.simhash();
        let duplicate: Option<String> = self
            .conn
            .query_row(
                "SELECT url FROM docs WHERE content_hash = ?1 LIMIT 1",
                params![content_hash],
                |row| row.get(0),
            )
            .optional()?;
        if duplicate.is_some_and(|other| other != doc.url) {
            return Ok(false);
        }
        let headings_text = doc.headings_joined();
        let title_tokens = tokenize(&doc.title);
        let heading_tokens = tokenize(&headings_text);
        let url_tokens = doc.url_tokens();
        let body_tokens = tokenize(&doc.body);
        let near: Vec<(String, i64)> = self
            .conn
            .prepare("SELECT url, simhash FROM docs WHERE host = ?1")?
            .query_map(params![doc.host], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        for (other_url, other_sim) in near {
            if other_url != doc.url
                && hamming_distance(other_sim as u64, simhash) <= SIMHASH_HAMMING_THRESHOLD
            {
                return Ok(false);
            }
        }
        let tx = self.conn.transaction()?;
        tx.execute("DELETE FROM terms WHERE url = ?1", params![doc.url])?;
        tx.execute("DELETE FROM docs WHERE url = ?1", params![doc.url])?;
        tx.execute(
            "INSERT INTO docs(url, host, domain, source, lang, license, title, headings, snippet,
                              published_at, fetched_at, prior, content_hash, simhash,
                              title_len, headings_len, url_len, body_len)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18)",
            params![
                doc.url,
                doc.host,
                doc.domain,
                doc.source.as_str(),
                doc.lang,
                doc.license,
                doc.title,
                headings_text,
                doc.snippet(),
                doc.published_at,
                doc.fetched_at,
                doc.prior,
                content_hash,
                simhash as i64,
                title_tokens.len() as i64,
                heading_tokens.len() as i64,
                url_tokens.len() as i64,
                body_tokens.len() as i64,
            ],
        )?;
        let mut counts: HashMap<&str, [i64; 4]> = HashMap::new();
        for token in &title_tokens {
            counts.entry(token.as_str()).or_default()[0] += 1;
        }
        for token in &heading_tokens {
            counts.entry(token.as_str()).or_default()[1] += 1;
        }
        for token in &url_tokens {
            counts.entry(token.as_str()).or_default()[2] += 1;
        }
        for token in &body_tokens {
            counts.entry(token.as_str()).or_default()[3] += 1;
        }
        for (term, tf) in counts {
            tx.execute(
                "INSERT INTO terms(term, url, tf_title, tf_headings, tf_url, tf_body)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![term, doc.url, tf[0], tf[1], tf[2], tf[3]],
            )?;
        }
        tx.commit()?;
        self.staged += 1;
        Ok(true)
    }

    /// Remove a document by URL (404/410 and `noindex` handling).
    pub fn remove(&mut self, url: &str) -> Result<bool> {
        let tx = self.conn.transaction()?;
        tx.execute("DELETE FROM terms WHERE url = ?1", params![url])?;
        let removed = tx.execute("DELETE FROM docs WHERE url = ?1", params![url])?;
        tx.commit()?;
        Ok(removed > 0)
    }

    /// Flush staged writes and refresh the disk-budget state.
    pub fn commit(&mut self) -> Result<BudgetState> {
        self.staged = 0;
        Ok(self.budget_state())
    }

    /// Current disk usage and budget state.
    #[must_use]
    pub fn budget_state(&self) -> BudgetState {
        let bytes = std::fs::metadata(&self.path)
            .map(|meta| meta.len())
            .unwrap_or(0);
        let budget_reached =
            self.budget_bytes > 0 && bytes as u128 * 100 >= self.budget_bytes as u128 * 95;
        BudgetState {
            bytes,
            budget_bytes: self.budget_bytes,
            budget_reached: budget_reached || self.budget_reached,
        }
    }

    /// Force the budget-reached flag (used when the frontier stops
    /// low-priority sources after measuring real growth).
    pub fn set_budget_reached(&mut self, reached: bool) {
        self.budget_reached = reached;
    }

    /// Number of indexed documents.
    pub fn len(&self) -> Result<u64> {
        Ok(self
            .conn
            .query_row("SELECT COUNT(*) FROM docs", [], |row| row.get::<_, i64>(0))?
            as u64)
    }

    /// Whether the index holds no documents.
    pub fn is_empty(&self) -> Result<bool> {
        Ok(self.len()? == 0)
    }

    /// Documents per source for coverage reporting.
    pub fn by_source(&self) -> Result<HashMap<String, u64>> {
        let mut out = HashMap::new();
        let mut stmt = self
            .conn
            .prepare("SELECT source, COUNT(*) FROM docs GROUP BY source")?;
        for row in stmt.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
        })? {
            let (source, count) = row?;
            out.insert(source, count as u64);
        }
        Ok(out)
    }

    /// Latest fetch timestamp across documents.
    pub fn last_crawl_at(&self) -> Result<Option<i64>> {
        Ok(self
            .conn
            .query_row("SELECT MAX(fetched_at) FROM docs", [], |row| {
                row.get::<_, Option<i64>>(0)
            })?)
    }

    /// BM25 candidate retrieval over the four weighted fields.
    ///
    /// When `conjunctive` is set, only documents containing every query term
    /// are returned; callers fall back to disjunctive retrieval when fewer
    /// than 10 candidates match.
    /// `lang` and `domains` are hard filters applied before truncation so
    /// `site:`/language restrictions survive deep result sets.
    pub fn search_bm25(
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
        let total: f64 = self.len()? as f64;
        if total < 1.0 {
            return Ok(Vec::new());
        }
        let avgs = self.avg_lengths()?;
        let mut scores: HashMap<String, f64> = HashMap::new();
        let mut matched: HashMap<String, usize> = HashMap::new();
        for term in query_terms {
            let mut stmt = self.conn.prepare(
                "SELECT t.url, t.tf_title, t.tf_headings, t.tf_url, t.tf_body,
                        d.title_len, d.headings_len, d.url_len, d.body_len
                 FROM terms t JOIN docs d ON d.url = t.url WHERE t.term = ?1",
            )?;
            let rows = stmt.query_map(params![term], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, i64>(4)?,
                    row.get::<_, i64>(5)?,
                    row.get::<_, i64>(6)?,
                    row.get::<_, i64>(7)?,
                    row.get::<_, i64>(8)?,
                ))
            })?;
            let postings: Vec<_> = rows.collect::<std::result::Result<Vec<_>, _>>()?;
            if postings.is_empty() {
                continue;
            }
            let df = postings.len() as f64;
            let idf = (1.0 + (total - df + 0.5) / (df + 0.5)).ln();
            for (url, tf_title, tf_head, tf_url, tf_body, title_len, head_len, url_len, body_len) in
                postings
            {
                let mut score = 0.0;
                score += BOOST_TITLE * field_score(tf_title as f64, title_len as f64, avgs.0, idf);
                score += BOOST_HEADINGS * field_score(tf_head as f64, head_len as f64, avgs.1, idf);
                score += BOOST_URL * field_score(tf_url as f64, url_len as f64, avgs.2, idf);
                score += BOOST_BODY * field_score(tf_body as f64, body_len as f64, avgs.3, idf);
                *scores.entry(url.clone()).or_default() += score;
                *matched.entry(url).or_default() += 1;
            }
        }
        let mut urls: Vec<(String, f64)> = scores.into_iter().collect();
        if conjunctive {
            urls.retain(|(url, _)| matched.get(url).copied().unwrap_or(0) >= query_terms.len());
        }
        // Hard filters before truncation: keep only docs matching lang/domains.
        if lang.is_some() || !domains.is_empty() {
            let mut filtered = Vec::with_capacity(urls.len());
            for (url, score) in urls {
                if let Some(doc) = self.load(&url)? {
                    if lang.is_some_and(|want| !doc.lang.eq_ignore_ascii_case(want)) {
                        continue;
                    }
                    if !domains.is_empty()
                        && !domains.iter().any(|domain| {
                            doc.host == *domain || doc.host.ends_with(&format!(".{domain}"))
                        })
                    {
                        continue;
                    }
                    filtered.push((url, score));
                }
            }
            urls = filtered;
        }
        urls.sort_by(|left, right| {
            right
                .1
                .partial_cmp(&left.1)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| left.0.cmp(&right.0))
        });
        urls.truncate(limit.max(1));
        let mut out = Vec::with_capacity(urls.len());
        for (url, bm25) in urls {
            if let Some(doc) = self.load(&url)? {
                out.push(ScoredDoc { doc, bm25 });
            }
        }
        Ok(out)
    }

    fn avg_lengths(&self) -> Result<(f64, f64, f64, f64)> {
        let row: (Option<f64>, Option<f64>, Option<f64>, Option<f64>) = self.conn.query_row(
            "SELECT AVG(title_len), AVG(headings_len), AVG(url_len), AVG(body_len) FROM docs",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )?;
        Ok((
            row.0.unwrap_or(1.0).max(1.0),
            row.1.unwrap_or(1.0).max(1.0),
            row.2.unwrap_or(1.0).max(1.0),
            row.3.unwrap_or(1.0).max(1.0),
        ))
    }

    fn load(&self, url: &str) -> Result<Option<StoredDoc>> {
        let row: Option<StoredDoc> = self
            .conn
            .query_row(
                "SELECT url, host, domain, source, lang, title, headings, snippet,
                        published_at, fetched_at, prior, content_hash, simhash
                 FROM docs WHERE url = ?1",
                params![url],
                |row| {
                    Ok(StoredDoc {
                        url: row.get(0)?,
                        host: row.get(1)?,
                        domain: row.get(2)?,
                        source: row.get(3)?,
                        lang: row.get(4)?,
                        title: row.get(5)?,
                        headings: row.get(6)?,
                        snippet: row.get(7)?,
                        published_at: row.get(8)?,
                        fetched_at: row.get(9)?,
                        prior: row.get(10)?,
                        content_hash: row.get(11)?,
                        simhash: row.get::<_, i64>(12)? as u64,
                    })
                },
            )
            .optional()?;
        Ok(row)
    }

    /// Check whether `url` is indexed.
    pub fn contains(&self, url: &str) -> Result<bool> {
        Ok(self
            .conn
            .query_row("SELECT 1 FROM docs WHERE url = ?1", params![url], |row| {
                row.get::<_, i64>(0)
            })
            .optional()?
            .is_some())
    }

    /// Test helper: all indexed URLs on `host`.
    pub fn urls_on_host(&self, host: &str) -> Result<Vec<String>> {
        Ok(self
            .conn
            .prepare("SELECT url FROM docs WHERE host = ?1 ORDER BY url")?
            .query_map(params![host], |row| row.get(0))?
            .collect::<std::result::Result<Vec<String>, _>>()?)
    }

    /// Test helper: suppress dead-code warnings for set inspection.
    pub fn doc_sources_seen(&self) -> HashSet<DocSource> {
        let mut seen = HashSet::new();
        if let Ok(map) = self.by_source() {
            for key in map.keys() {
                seen.insert(DocSource::parse(key));
            }
        }
        seen
    }
}

fn field_score(tf: f64, doc_len: f64, avg_len: f64, idf: f64) -> f64 {
    if tf <= 0.0 {
        return 0.0;
    }
    let norm = tf * (BM25_K1 + 1.0)
        / (tf + BM25_K1 * (1.0 - BM25_B + BM25_B * doc_len / avg_len.max(1.0)));
    norm * idf
}
