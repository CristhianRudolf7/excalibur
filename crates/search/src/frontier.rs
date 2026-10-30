//! SQLite-backed crawl frontier and polite scheduler (section 15.4).
//!
//! Tables mirror the `hosts`/`urls` schema from the architecture. All time
//! is unix seconds supplied by the caller so tests run on a controlled
//! clock. State transitions are atomic per call; crash recovery re-queues
//! `fetching` and `pending_commit` URLs that never committed.

use rusqlite::{OptionalExtension, params};

use crate::error::{Result, SearchError};
use crate::normalize::{self, NormalizedUrl, normalize_url};
use crate::robots::{self, RobotsFetch, RobotsRules};

/// URL lifecycle states.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UrlState {
    Queued,
    Fetching,
    PendingCommit,
    Indexed,
    Skipped,
    Failed,
    Gone,
}

impl UrlState {
    fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Fetching => "fetching",
            Self::PendingCommit => "pending_commit",
            Self::Indexed => "indexed",
            Self::Skipped => "skipped",
            Self::Failed => "failed",
            Self::Gone => "gone",
        }
    }

    fn parse(value: &str) -> Self {
        match value {
            "fetching" => Self::Fetching,
            "pending_commit" => Self::PendingCommit,
            "indexed" => Self::Indexed,
            "skipped" => Self::Skipped,
            "failed" => Self::Failed,
            "gone" => Self::Gone,
            _ => Self::Queued,
        }
    }
}

/// Crawl source of a URL, matching the architecture source list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    Web,
    Wiki,
    Docs,
    News,
    Arxiv,
    Hn,
    Read,
}

impl Source {
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
}

/// Lower numeric value means higher crawl priority.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Priority {
    High = 0,
    Normal = 10,
    Low = 20,
}

/// Outcome of a fetch used to reschedule the host and URL.
#[derive(Debug, Clone)]
pub struct FetchOutcome {
    /// HTTP status code, if the fetch reached the server.
    pub status: Option<u16>,
    /// `Retry-After` seconds advertised with 429/503, if any.
    pub retry_after_secs: Option<u64>,
    /// Download duration in milliseconds.
    pub duration_ms: u64,
    /// Whether the page content changed since the last fetch.
    pub content_changed: bool,
    /// `noindex` seen via `<meta name=robots>` or `X-Robots-Tag`.
    pub noindex: bool,
    /// `nofollow` seen via `<meta name=robots>` or `X-Robots-Tag`.
    pub nofollow: bool,
}

/// A URL selected for fetching.
#[derive(Debug, Clone)]
pub struct CrawlTask {
    /// Canonical URL to fetch.
    pub url: String,
    /// Lower-cased host.
    pub host: String,
    /// Crawl depth (seeds are depth 0).
    pub depth: u8,
}

/// Read-only host counters for admission and coverage.
#[derive(Debug, Clone, Default)]
pub struct FrontierStats {
    /// Known hosts.
    pub hosts: u64,
    /// URLs in `queued` state.
    pub queued: u64,
    /// URLs in `fetching` state.
    pub fetching: u64,
    /// URLs in `pending_commit` state.
    pub pending_commit: u64,
    /// URLs in `indexed` state.
    pub indexed: u64,
}

/// Polite crawl frontier persisted in `search.db`.
pub struct Frontier {
    conn: rusqlite::Connection,
    contact_url: String,
    host_page_budget: u32,
    max_inflight: usize,
}

impl Frontier {
    /// Open (or create) the frontier at `path`.
    ///
    /// Fails with [`SearchError::MissingContactUrl`] when `contact_url` is
    /// empty: without operator contact the scheduler must not start.
    pub fn open(
        path: &std::path::Path,
        contact_url: &str,
        host_page_budget: u32,
        max_inflight: usize,
    ) -> Result<Self> {
        if contact_url.trim().is_empty() {
            return Err(SearchError::MissingContactUrl);
        }
        let conn = rusqlite::Connection::open(path)?;
        let frontier = Self {
            conn,
            contact_url: contact_url.to_owned(),
            host_page_budget,
            max_inflight: max_inflight.max(1),
        };
        frontier.migrate()?;
        Ok(frontier)
    }

    /// Open an in-memory frontier for tests.
    pub fn open_memory(contact_url: &str) -> Result<Self> {
        if contact_url.trim().is_empty() {
            return Err(SearchError::MissingContactUrl);
        }
        let conn = rusqlite::Connection::open_in_memory()?;
        let frontier = Self {
            conn,
            contact_url: contact_url.to_owned(),
            host_page_budget: 30,
            max_inflight: 16,
        };
        frontier.migrate()?;
        Ok(frontier)
    }

    fn migrate(&self) -> Result<()> {
        self.conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS hosts(
                host TEXT PRIMARY KEY,
                domain TEXT NOT NULL DEFAULT '',
                prior REAL NOT NULL DEFAULT 0.2,
                next_fetch_at INTEGER NOT NULL DEFAULT 0,
                delay_ms INTEGER NOT NULL DEFAULT 1000,
                crawl_delay_ms INTEGER,
                robots_blob TEXT NOT NULL DEFAULT '',
                robots_fetched_at INTEGER NOT NULL DEFAULT 0,
                robots_reachable INTEGER NOT NULL DEFAULT 1,
                err_count INTEGER NOT NULL DEFAULT 0,
                parked_until INTEGER NOT NULL DEFAULT 0,
                page_budget INTEGER NOT NULL DEFAULT 30,
                pages_fetched INTEGER NOT NULL DEFAULT 0
            );
            CREATE TABLE IF NOT EXISTS urls(
                url_hash TEXT PRIMARY KEY,
                url TEXT NOT NULL,
                host TEXT NOT NULL,
                depth INTEGER NOT NULL DEFAULT 0,
                priority INTEGER NOT NULL DEFAULT 10,
                state TEXT NOT NULL DEFAULT 'queued',
                source TEXT NOT NULL DEFAULT 'web',
                last_fetch INTEGER,
                next_fetch INTEGER NOT NULL DEFAULT 0,
                etag TEXT,
                last_modified TEXT,
                n_fetch INTEGER NOT NULL DEFAULT 0,
                n_change INTEGER NOT NULL DEFAULT 0,
                simhash INTEGER,
                content_hash TEXT,
                opstamp INTEGER
            );
            CREATE INDEX IF NOT EXISTS idx_urls_state_host ON urls(state, host, priority, next_fetch);
            CREATE INDEX IF NOT EXISTS idx_urls_host ON urls(host, state);",
        )?;
        Ok(())
    }

    /// Operator contact advertised to hosts.
    #[must_use]
    pub fn contact_url(&self) -> &str {
        &self.contact_url
    }

    /// Register a host seed. Idempotent.
    pub fn add_host(&self, host: &str, prior: f64, page_budget: Option<u32>) -> Result<()> {
        let host = host.to_ascii_lowercase();
        let domain = normalize::registrable_domain(&host);
        self.conn.execute(
            "INSERT INTO hosts(host, domain, prior, page_budget)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(host) DO UPDATE SET prior=excluded.prior",
            params![
                host,
                domain,
                prior,
                i64::from(page_budget.unwrap_or(self.host_page_budget))
            ],
        )?;
        Ok(())
    }

    /// Enqueue `raw_url` for crawling. Same-host links obey the depth and
    /// host page budget; unknown cross-host links only register a candidate
    /// host and are not crawled until seeded.
    pub fn enqueue(
        &self,
        raw_url: &str,
        depth: u8,
        priority: Priority,
        source: Source,
        now_secs: u64,
        max_depth: u8,
    ) -> Result<bool> {
        let normalized: NormalizedUrl = normalize_url(raw_url)?;
        let known: Option<i64> = self
            .conn
            .query_row(
                "SELECT 1 FROM hosts WHERE host = ?1",
                params![normalized.host],
                |row| row.get(0),
            )
            .optional()?;
        if known.is_none() {
            self.add_host(&normalized.host, 0.2, None)?;
            return Ok(false);
        }
        if depth > max_depth {
            return Ok(false);
        }
        let pages: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM urls WHERE host = ?1 AND state IN ('queued','fetching','pending_commit','indexed')",
            params![normalized.host],
            |row| row.get(0),
        )?;
        let budget: i64 = self.conn.query_row(
            "SELECT page_budget FROM hosts WHERE host = ?1",
            params![normalized.host],
            |row| row.get(0),
        )?;
        if pages >= budget {
            return Ok(false);
        }
        let hash = NormalizedUrl::hash_key(&normalized);
        let inserted = self.conn.execute(
            "INSERT INTO urls(url_hash, url, host, depth, priority, state, source, next_fetch)
             VALUES (?1, ?2, ?3, ?4, ?5, 'queued', ?6, ?7)
             ON CONFLICT(url_hash) DO NOTHING",
            params![
                hash,
                normalized.canonical,
                normalized.host,
                i64::from(depth),
                i64::from(priority as u8),
                source.as_str(),
                i64::try_from(now_secs).unwrap_or(i64::MAX),
            ],
        )?;
        Ok(inserted == 1)
    }

    /// Store a fetched `robots.txt` body for `host`.
    pub fn store_robots(
        &self,
        host: &str,
        body: &str,
        reachable: bool,
        now_secs: u64,
        crawl_delay_secs: Option<u64>,
    ) -> Result<()> {
        let host = host.to_ascii_lowercase();
        let capped: String = body.chars().take(robots::MAX_ROBOTS_BYTES).collect();
        self.conn.execute(
            "INSERT INTO hosts(host, robots_blob, robots_fetched_at, robots_reachable, crawl_delay_ms)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(host) DO UPDATE SET
               robots_blob=excluded.robots_blob,
               robots_fetched_at=excluded.robots_fetched_at,
               robots_reachable=excluded.robots_reachable,
               crawl_delay_ms=excluded.crawl_delay_ms",
            params![
                host,
                capped,
                i64::try_from(now_secs).unwrap_or(0),
                i64::from(reachable),
                crawl_delay_secs.map(|secs| i64::try_from(secs.min(robots::MAX_CRAWL_DELAY_SECS)).unwrap_or(30) * 1000),
            ],
        )?;
        Ok(())
    }

    /// Decide whether `raw_url` may be fetched under cached robots state.
    pub fn robots_allowed(&self, raw_url: &str, user_agent: &str, now_secs: u64) -> Result<bool> {
        let normalized: NormalizedUrl = normalize_url(raw_url)?;
        let row: Option<(String, i64, i64)> = self
            .conn
            .query_row(
                "SELECT robots_blob, robots_fetched_at, robots_reachable FROM hosts WHERE host = ?1",
                params![normalized.host],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?;
        let Some((blob, fetched_at, reachable)) = row else {
            return Ok(true);
        };
        let now = i64::try_from(now_secs).unwrap_or(0);
        if now - fetched_at > i64::try_from(robots::ROBOTS_TTL_SECS).unwrap_or(i64::MAX) {
            return Ok(true);
        }
        if reachable == 0 {
            return Ok(false);
        }
        if blob.is_empty() {
            return Ok(true);
        }
        let rules = RobotsRules::parse(&blob);
        let decision = robots::decide(
            RobotsFetch::Ok,
            Some(&rules),
            user_agent,
            &normalized.robots_path,
        );
        Ok(decision.allowed)
    }

    /// Select due URLs: at most one in flight per host and at most
    /// `max_inflight` overall, ordered by priority. Selected rows move to
    /// `fetching` atomically.
    pub fn select_due(&mut self, now_secs: u64) -> Result<Vec<CrawlTask>> {
        let now = i64::try_from(now_secs).unwrap_or(0);
        let tx = self.conn.transaction()?;
        let mut stmt = tx.prepare(
            "SELECT u.url_hash, u.url, u.host FROM urls u
             JOIN hosts h ON h.host = u.host
             WHERE u.state = 'queued' AND u.next_fetch <= ?1
               AND h.next_fetch_at <= ?1 AND h.parked_until <= ?1
             ORDER BY u.priority ASC, u.next_fetch ASC
             LIMIT ?2",
        )?;
        let candidates: Vec<(String, String, String)> = stmt
            .query_map(
                params![now, i64::try_from(self.max_inflight * 4).unwrap_or(64)],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        drop(stmt);
        let mut tasks = Vec::new();
        let mut hosts_used = std::collections::HashSet::new();
        let inflight: i64 = tx.query_row(
            "SELECT COUNT(*) FROM urls WHERE state = 'fetching'",
            [],
            |row| row.get(0),
        )?;
        let mut slots = (i64::try_from(self.max_inflight).unwrap_or(16) - inflight).max(0);
        for (hash, url, host) in candidates {
            if slots <= 0 {
                break;
            }
            if hosts_used.contains(&host) {
                continue;
            }
            let busy: i64 = tx.query_row(
                "SELECT COUNT(*) FROM urls WHERE host = ?1 AND state = 'fetching'",
                params![host],
                |row| row.get(0),
            )?;
            if busy > 0 {
                continue;
            }
            tx.execute(
                "UPDATE urls SET state = 'fetching', last_fetch = ?1 WHERE url_hash = ?2",
                params![now, hash],
            )?;
            let depth: u8 = tx.query_row(
                "SELECT depth FROM urls WHERE url_hash = ?1",
                params![hash],
                |row| row.get::<_, i64>(0),
            )? as u8;
            hosts_used.insert(host.clone());
            tasks.push(CrawlTask { url, host, depth });
            slots -= 1;
        }
        tx.commit()?;
        Ok(tasks)
    }

    /// Apply a fetch outcome: reschedule host politeness and move the URL
    /// to its next state.
    pub fn record_outcome(
        &self,
        raw_url: &str,
        outcome: &FetchOutcome,
        now_secs: u64,
        content_hash: Option<&str>,
        simhash: Option<u64>,
    ) -> Result<UrlState> {
        let normalized: NormalizedUrl = normalize_url(raw_url)?;
        let now = i64::try_from(now_secs).unwrap_or(0);
        let hash = NormalizedUrl::hash_key(&normalized);
        let status = outcome.status.unwrap_or(0);

        if outcome.noindex || status == 404 || status == 410 {
            let state = if status == 404 || status == 410 {
                UrlState::Gone
            } else {
                UrlState::Skipped
            };
            self.finish_url(&hash, state, content_hash, simhash, outcome.content_changed)?;
            self.reschedule_host(&normalized.host, outcome, now, true)?;
            return Ok(state);
        }

        if status == 429 || status == 503 {
            self.register_host_error(&normalized.host, outcome.retry_after_secs, now)?;
            self.conn.execute(
                "UPDATE urls SET state = 'queued', next_fetch = ?1 WHERE url_hash = ?2",
                params![next_after_backoff(&normalized.host, now), hash,],
            )?;
            return Ok(UrlState::Queued);
        }

        if !(200..300).contains(&status) {
            self.register_host_error(&normalized.host, None, now)?;
            self.finish_url(&hash, UrlState::Failed, content_hash, simhash, false)?;
            return Ok(UrlState::Failed);
        }

        self.clear_host_errors(&normalized.host)?;
        self.finish_url(
            &hash,
            UrlState::PendingCommit,
            content_hash,
            simhash,
            outcome.content_changed,
        )?;
        self.reschedule_host(&normalized.host, outcome, now, true)?;
        Ok(UrlState::PendingCommit)
    }

    /// Mark a `pending_commit` URL as indexed after the writer commits.
    /// `changed` drives the Cho/Garcia-Molina style refresh interval.
    pub fn mark_indexed(&self, raw_url: &str, changed: bool, now_secs: u64) -> Result<()> {
        let normalized: NormalizedUrl = normalize_url(raw_url)?;
        let hash = NormalizedUrl::hash_key(&normalized);
        let interval = refresh_interval_secs(&self.conn, &hash, changed)?;
        let next = i64::try_from(now_secs).unwrap_or(0) + interval;
        self.conn.execute(
            "UPDATE urls SET state = 'indexed', next_fetch = ?1,
               n_fetch = n_fetch + 1, n_change = n_change + ?2 WHERE url_hash = ?3",
            params![next, i64::from(changed), hash],
        )?;
        Ok(())
    }

    /// Re-queue `fetching` and `pending_commit` rows left behind by a crash
    /// so no URL is lost or duplicated. Returns the re-queued count.
    pub fn recover_interrupted(&self) -> Result<u64> {
        let count = self.conn.execute(
            "UPDATE urls SET state = 'queued' WHERE state IN ('fetching','pending_commit')",
            [],
        )?;
        Ok(count as u64)
    }

    /// Current counters for admission and coverage reporting.
    pub fn stats(&self) -> Result<FrontierStats> {
        let hosts: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM hosts", [], |row| row.get(0))?;
        let count = |state: &str| -> Result<i64> {
            Ok(self.conn.query_row(
                "SELECT COUNT(*) FROM urls WHERE state = ?1",
                params![state],
                |row| row.get(0),
            )?)
        };
        Ok(FrontierStats {
            hosts: hosts as u64,
            queued: count("queued")? as u64,
            fetching: count("fetching")? as u64,
            pending_commit: count("pending_commit")? as u64,
            indexed: count("indexed")? as u64,
        })
    }

    /// State of a single URL, if known.
    pub fn url_state(&self, raw_url: &str) -> Result<Option<UrlState>> {
        let normalized: NormalizedUrl = normalize_url(raw_url)?;
        let hash = NormalizedUrl::hash_key(&normalized);
        let state: Option<String> = self
            .conn
            .query_row(
                "SELECT state FROM urls WHERE url_hash = ?1",
                params![hash],
                |row| row.get(0),
            )
            .optional()?;
        Ok(state.map(|value| UrlState::parse(&value)))
    }

    /// Next-fetch timestamp of a host, for politeness tests.
    pub fn host_next_fetch(&self, host: &str) -> Result<Option<u64>> {
        let value: Option<i64> = self
            .conn
            .query_row(
                "SELECT next_fetch_at FROM hosts WHERE host = ?1",
                params![host.to_ascii_lowercase()],
                |row| row.get(0),
            )
            .optional()?;
        Ok(value.map(|secs| secs as u64))
    }

    fn finish_url(
        &self,
        hash: &str,
        state: UrlState,
        content_hash: Option<&str>,
        simhash: Option<u64>,
        changed: bool,
    ) -> Result<()> {
        self.conn.execute(
            "UPDATE urls SET state = ?1, content_hash = COALESCE(?2, content_hash),
               simhash = COALESCE(?3, simhash), n_change = n_change + ?4 WHERE url_hash = ?5",
            params![
                state.as_str(),
                content_hash,
                simhash.map(|value| value as i64),
                i64::from(changed),
                hash,
            ],
        )?;
        Ok(())
    }

    fn reschedule_host(
        &self,
        host: &str,
        outcome: &FetchOutcome,
        now: i64,
        success: bool,
    ) -> Result<()> {
        let row: Option<(i64, Option<i64>)> = self
            .conn
            .query_row(
                "SELECT delay_ms, crawl_delay_ms FROM hosts WHERE host = ?1",
                params![host],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let (mut delay_ms, crawl_delay_ms) = row.unwrap_or((1000_i64, None));
        if success {
            delay_ms = i64::from(outcome.duration_ms.saturating_mul(2).clamp(1000, 5000) as u32);
            delay_ms += deterministic_jitter(host, now) as i64;
            if let Some(crawl) = crawl_delay_ms {
                delay_ms = delay_ms.max(crawl);
            }
        }
        self.conn.execute(
            "UPDATE hosts SET next_fetch_at = ?1, delay_ms = ?2, pages_fetched = pages_fetched + 1 WHERE host = ?3",
            params![now + delay_ms / 1000, delay_ms, host],
        )?;
        Ok(())
    }

    fn register_host_error(&self, host: &str, retry_after: Option<u64>, now: i64) -> Result<()> {
        let err_count: i64 = self
            .conn
            .query_row(
                "SELECT err_count FROM hosts WHERE host = ?1",
                params![host],
                |row| row.get(0),
            )
            .unwrap_or(0);
        let next_err = err_count + 1;
        if next_err >= 5 {
            self.conn.execute(
                "UPDATE hosts SET err_count = ?1, parked_until = ?2 WHERE host = ?3",
                params![next_err, now + 24 * 3600, host],
            )?;
            return Ok(());
        }
        let backoff = retry_after
            .map(|secs| secs.min(3600))
            .unwrap_or_else(|| (2_u64.pow(next_err as u32)).min(120));
        self.conn.execute(
            "UPDATE hosts SET err_count = ?1, next_fetch_at = ?2 WHERE host = ?3",
            params![next_err, now + backoff as i64, host],
        )?;
        Ok(())
    }

    fn clear_host_errors(&self, host: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE hosts SET err_count = 0 WHERE host = ?1",
            params![host],
        )?;
        Ok(())
    }
}

fn next_after_backoff(_host: &str, now: i64) -> i64 {
    now + 60
}

/// Deterministic 0-150 ms jitter from host and timestamp.
fn deterministic_jitter(host: &str, now: i64) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in host.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    hash ^= now as u64;
    hash = hash.wrapping_mul(0x0100_0000_01b3);
    (hash >> 11) % 151
}

/// Refresh interval per 15.6: start at 7 days, halve on change (min 1 day),
/// double on stability (max 60 days).
fn refresh_interval_secs(conn: &rusqlite::Connection, hash: &str, changed: bool) -> Result<i64> {
    const DAY: i64 = 24 * 3600;
    let row: Option<(i64, i64)> = conn
        .query_row(
            "SELECT n_fetch, n_change FROM urls WHERE url_hash = ?1",
            params![hash],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    let (n_fetch, _) = row.unwrap_or((0, 0));
    if n_fetch == 0 {
        return Ok(7 * DAY);
    }
    let current: i64 = conn
        .query_row(
            "SELECT next_fetch - COALESCE(last_fetch, next_fetch) FROM urls WHERE url_hash = ?1",
            params![hash],
            |row| row.get(0),
        )
        .unwrap_or(7 * DAY);
    let baseline = current.clamp(DAY, 60 * DAY);
    if changed {
        Ok((baseline / 2).max(DAY))
    } else {
        Ok((baseline * 2).min(60 * DAY))
    }
}
