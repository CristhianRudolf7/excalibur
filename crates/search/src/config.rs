//! Profiles and configuration for the standalone search foundation.
//!
//! Budgets follow section 15.5 of the browser architecture: the `minimo`
//! profile is the default and the `ampliado` profile only changes thresholds.
//! RAM behaviour never changes with the profile; only on-disk quotas do.

use serde::{Deserialize, Serialize};

use crate::error::{Result, SearchError};

/// Named crawl/index profile.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum SearchProfile {
    /// Default profile: index at most 1.5 GiB, state at most 0.5 GiB.
    #[default]
    Minimo,
    /// Optional wider profile: index at most 6 GiB, state at most 1 GiB.
    Ampliado,
}

impl SearchProfile {
    /// Profile name as reported in `web_search` coverage.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Minimo => "minimo",
            Self::Ampliado => "ampliado",
        }
    }

    /// Maximum index directory size in bytes.
    #[must_use]
    pub fn index_budget_bytes(self) -> u64 {
        match self {
            Self::Minimo => 1_610_612_736,
            Self::Ampliado => 6_442_450_944,
        }
    }

    /// Maximum frontier/state size in bytes.
    #[must_use]
    pub fn state_budget_bytes(self) -> u64 {
        match self {
            Self::Minimo => 536_870_912,
            Self::Ampliado => 1_073_741_824,
        }
    }

    /// Maximum pages fetched per host per run window.
    #[must_use]
    pub fn host_page_budget(self) -> u32 {
        match self {
            Self::Minimo => 30,
            Self::Ampliado => 30,
        }
    }

    /// Global parallel fetch slots for the polite scheduler.
    #[must_use]
    pub fn max_inflight(self) -> usize {
        16
    }

    /// Minimum `incoming_links` for a CirrusSearch document to be ingested.
    #[must_use]
    pub fn wiki_incoming_links_threshold(self) -> i64 {
        match self {
            Self::Minimo => 10,
            Self::Ampliado => 2,
        }
    }
}

/// Static configuration for the search foundation.
///
/// The broker, resource accounting and `web_read` integration consume this
/// struct later; every field is explicit so the main lane can adapt it
/// without changing call sites inside this crate.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchConfig {
    /// Active profile.
    pub profile: SearchProfile,
    /// Operator contact URL advertised in the crawler user agent.
    /// Empty means the scheduler must refuse to start.
    pub contact_url: String,
    /// Crawler token, e.g. `ExcaliburBot/0.1`.
    pub user_agent: String,
    /// Directory holding `search.db` and the on-disk index.
    pub data_dir: String,
}

impl Default for SearchConfig {
    fn default() -> Self {
        Self {
            profile: SearchProfile::Minimo,
            contact_url: String::new(),
            user_agent: "ExcaliburBot/0.1".to_owned(),
            data_dir: "search-data".to_owned(),
        }
    }
}

impl SearchConfig {
    /// Reject configurations that must never crawl, mirroring 15.4:
    /// without an operator contact URL the scheduler does not start.
    pub fn validate_for_crawl(&self) -> Result<()> {
        if self.contact_url.trim().is_empty() {
            return Err(SearchError::MissingContactUrl);
        }
        Ok(())
    }

    /// Full user-agent header value carrying the contact URL.
    #[must_use]
    pub fn user_agent_header(&self) -> String {
        format!("{} (+{})", self.user_agent, self.contact_url)
    }
}
