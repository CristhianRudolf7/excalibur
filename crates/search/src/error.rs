//! Error type shared by the search foundation crates.

use thiserror::Error;

/// Errors returned by the standalone search foundation.
#[derive(Debug, Error)]
pub enum SearchError {
    /// The operator contact URL is missing; the crawl scheduler refuses to start.
    #[error("contact_url is required before the crawler may start")]
    MissingContactUrl,
    /// A supplied URL cannot be parsed or uses an unsupported scheme.
    #[error("invalid url '{0}': {1}")]
    InvalidUrl(String, String),
    /// SQLite persistence failed.
    #[error("storage error: {0}")]
    Storage(String),
    /// A document violates an ingestion bound.
    #[error("ingestion rejected: {0}")]
    Rejected(String),
    /// The configured disk budget is exhausted for the requested priority.
    #[error("disk budget reached; ingestion of low-priority sources is stopped")]
    BudgetReached,
}

impl From<rusqlite::Error> for SearchError {
    fn from(value: rusqlite::Error) -> Self {
        Self::Storage(value.to_string())
    }
}

impl From<serde_json::Error> for SearchError {
    fn from(value: serde_json::Error) -> Self {
        Self::Storage(value.to_string())
    }
}

/// Convenience alias for fallible search operations.
pub type Result<T> = std::result::Result<T, SearchError>;
