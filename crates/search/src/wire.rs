//! Resource and broker integration surface (section 15.2/15.4-15.5).
//!
//! This module defines **trait boundaries only**. It never imports
//! `browser-resources`, the network broker or `web_read`: the main lane
//! (B9) provides the real implementations. All checks in this crate run on
//! local synthetic fixtures with the [`AllowAllGate`] / [`DenyAllGate`]
//! test gates.
//!
//! Pending B9 wire points (not implemented here):
//! - budgets from `browser-resources` admission (`BrowserBudget`): the main
//!   lane maps `index_budget_bytes` / `state_budget_bytes` to
//!   [`BudgetProvider`] before opening the frontier and index;
//! - fetches via the `HostProxy` allowlist: the main lane maps proxy
//!   admission to [`FetchGate`] before every `select_due` dispatch;
//! - `web_read` full-page extraction feeds pipeline pages through the same
//!   `frontier -> ingest -> index` states (no new states).

use crate::config::{SearchConfig, SearchProfile};
use crate::index::DocSource;

/// Disk/index budgets supplied by host resource accounting.
///
/// The production provider lives in `browser-resources`; this crate only
/// needs the two byte quotas. [`SearchProfile`] and [`SearchConfig`] already
/// implement this trait so the main lane can adapt constructors without
/// changing pipeline call sites.
pub trait BudgetProvider {
    /// Maximum index directory size in bytes.
    fn index_budget_bytes(&self) -> u64;
    /// Maximum frontier/state size in bytes.
    fn state_budget_bytes(&self) -> u64;
}

impl BudgetProvider for SearchProfile {
    fn index_budget_bytes(&self) -> u64 {
        SearchProfile::index_budget_bytes(*self)
    }
    fn state_budget_bytes(&self) -> u64 {
        SearchProfile::state_budget_bytes(*self)
    }
}

impl BudgetProvider for SearchConfig {
    fn index_budget_bytes(&self) -> u64 {
        self.profile.index_budget_bytes()
    }
    fn state_budget_bytes(&self) -> u64 {
        self.profile.state_budget_bytes()
    }
}

/// Whether `source` stops first at 95% of the index budget (section 15.5).
///
/// General web, news and Hacker News rotative ingestion stops first; wiki,
/// docs, arXiv and read pages remain admissible. This mirrors
/// [`crate::index::DiskIndex::is_low_priority_source`] as a free function so
/// broker code can gate without opening an index.
#[must_use]
pub fn is_low_priority_source(source: DocSource) -> bool {
    matches!(source, DocSource::Web | DocSource::News | DocSource::Hn)
}

/// Fetch admission for one host/URL dispatch.
///
/// The production gate lives behind the `HostProxy` allowlist in the broker:
/// only hosts admitted by the proxy may be fetched, everything else fails
/// closed. The pipeline checks this gate after the frontier `select_due`
/// decision and before any synthetic or real fetch.
pub trait FetchGate {
    /// Whether `url` on `host` may be fetched now.
    fn allow_fetch(&self, host: &str, url: &str) -> bool;
}

/// Test gate that admits every fetch (local synthetic fixtures only).
#[derive(Debug, Clone, Copy, Default)]
pub struct AllowAllGate;

impl FetchGate for AllowAllGate {
    fn allow_fetch(&self, _host: &str, _url: &str) -> bool {
        true
    }
}

/// Test gate that denies every fetch (fail-closed probing).
#[derive(Debug, Clone, Copy, Default)]
pub struct DenyAllGate;

impl FetchGate for DenyAllGate {
    fn allow_fetch(&self, _host: &str, _url: &str) -> bool {
        false
    }
}

/// Pending `HostProxy` allowlist surface (B9 integration).
///
/// The broker owns the real allowlist; this struct records the wire shape so
/// the main lane can move the check without changing pipeline call sites:
/// an empty `allowed_hosts` denies everything (fail closed), otherwise only
/// listed hosts (exact or subdomain via leading-dot semantics handled by the
/// caller) are admitted. Suffix matching here is intentionally exact-or-
/// subdomain to mirror [`crate::normalize::registrable_domain`] policy.
#[derive(Debug, Clone, Default)]
pub struct HostProxyWire {
    /// Admitted hosts, lower-cased. Empty means deny-all.
    pub allowed_hosts: Vec<String>,
}

impl HostProxyWire {
    /// Build a wire gate from an allowlist (hosts are lower-cased).
    #[must_use]
    pub fn new(allowed: &[&str]) -> Self {
        Self {
            allowed_hosts: allowed
                .iter()
                .map(|host| host.to_ascii_lowercase())
                .collect(),
        }
    }
}

impl FetchGate for HostProxyWire {
    fn allow_fetch(&self, host: &str, _url: &str) -> bool {
        let host = host.to_ascii_lowercase();
        self.allowed_hosts
            .iter()
            .any(|allowed| host == *allowed || host.ends_with(&format!(".{allowed}")))
    }
}
