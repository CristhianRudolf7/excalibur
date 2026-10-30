//! Standalone search foundation for milestone B8 (`Browser search foundation`).
//!
//! This crate implements the parts of sections 15.4-15.5 and 15.7-15.9 that
//! do not need the browser lifecycle, the network broker or host resource
//! accounting: RFC 9309 robots parsing, URL normalization, the SQLite
//! frontier with a polite scheduler, exact/near-duplicate detection, bounded
//! feed/sitemap/CirrusSearch ingestion, a deterministic disk-backed index
//! with BM25, and the `web_search` ranking service with coverage and the
//! insufficient flag.
//!
//! The on-disk backend is an explicit [`index::DiskIndex`] behind plain
//! method boundaries; the production tantivy store plugs into the same
//! document model and query flow in the main lane. Budgets, schemas and
//! response shapes already follow the architecture so integration only
//! adapts constructors, never query call sites.

pub mod config;
pub mod dedup;
pub mod error;
pub mod frontier;
pub mod index;
pub mod ingest;
pub mod normalize;
pub mod ranking;
pub mod robots;
pub mod search;

pub use config::{SearchConfig, SearchProfile};
pub use error::{Result, SearchError};
pub use frontier::{CrawlTask, FetchOutcome, Frontier, Priority, Source, UrlState};
pub use index::{BudgetState, DiskIndex, Doc, DocSource, ScoredDoc, StoredDoc};
pub use ingest::{
    CirrusDoc, cirrus_admitted, parse_cirrus_pair, parse_feed_links, parse_sitemap_locs,
};
pub use normalize::{NormalizedUrl, normalize_url, registrable_domain};
pub use ranking::{
    RankedResult, fuse_rrf, has_recency_intent, prior_for_harmonic_rank, rank_candidates,
};
pub use robots::{RobotsFetch, RobotsRules, decide, parse_robots_directives};
pub use search::{Coverage, SearchOptions, SearchResponse, SearchService};

/// Lower-case ASCII folding tokenizer shared by the index and ranking.
///
/// Splits on non-alphanumeric characters, folds common Spanish/French
/// accents to ASCII (tantivy uses `AsciiFoldingFilter` for the same
/// reason), drops tokens longer than 40 characters and keeps everything
/// else verbatim. Deterministic with no configuration.
#[must_use]
pub fn tokenize(text: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    for ch in text.chars() {
        let folded = fold_char(ch);
        if folded.is_alphanumeric() {
            current.push(folded.to_ascii_lowercase());
        } else if !current.is_empty() {
            if current.len() <= 40 {
                tokens.push(std::mem::take(&mut current));
            } else {
                current.clear();
            }
        }
    }
    if !current.is_empty() && current.len() <= 40 {
        tokens.push(current);
    }
    tokens
}

fn fold_char(ch: char) -> char {
    match ch {
        'á' | 'à' | 'â' | 'ä' | 'ã' | 'å' | 'Á' | 'À' | 'Â' | 'Ä' | 'Ã' | 'Å' => 'a',
        'é' | 'è' | 'ê' | 'ë' | 'É' | 'È' | 'Ê' | 'Ë' => 'e',
        'í' | 'ì' | 'î' | 'ï' | 'Í' | 'Ì' | 'Î' | 'Ï' => 'i',
        'ó' | 'ò' | 'ô' | 'ö' | 'õ' | 'Ó' | 'Ò' | 'Ô' | 'Ö' | 'Õ' => 'o',
        'ú' | 'ù' | 'û' | 'ü' | 'Ú' | 'Ù' | 'Û' | 'Ü' => 'u',
        'ñ' | 'Ñ' => 'n',
        'ç' | 'Ç' => 'c',
        'ý' | 'ÿ' | 'Ý' => 'y',
        _ => ch,
    }
}
