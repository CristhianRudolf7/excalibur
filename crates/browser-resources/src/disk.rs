//! Disk accounting from architecture section 13.5.
//!
//! The 5 GiB free reserve is unconditional: no profile, index, download or
//! staged upload is admitted when it would breach it. The search budgets are
//! explicit per profile and the crawler/indexer additionally require the free
//! space to cover budget plus reserve. Listening origins are never cleaned.

use crate::budgets::SearchProfile;
use crate::budgets::{
    CHROME_CACHE_EPHEMERAL_BYTES, CHROME_CACHE_PERSISTENT_BYTES, DISK_LOW_THRESHOLD_BYTES,
    DISK_RESERVE_BYTES, PROFILE_CLEAR_BYTES, PROFILE_WARN_BYTES, QUARANTINE_BUDGET_BYTES,
    READ_CACHE_BUDGET_BYTES, SEARCH_AMPLIADO_INDEX_BYTES, SEARCH_AMPLIADO_STATE_BYTES,
    SEARCH_MINIMO_INDEX_BYTES, SEARCH_MINIMO_STATE_BYTES, SNAPSHOT_BUDGET_BYTES,
    WEBD_DATA_TOTAL_BYTES,
};

/// Chrome HTTP cache size for the instance class (`--disk-cache-size`).
#[must_use]
pub fn chrome_cache_bytes(ephemeral: bool) -> u64 {
    if ephemeral {
        CHROME_CACHE_EPHEMERAL_BYTES
    } else {
        CHROME_CACHE_PERSISTENT_BYTES
    }
}

/// Profile-size action for a per-profile byte count.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProfileAction {
    /// Below 1 GiB: nothing to do.
    Ok,
    /// At or above 1 GiB: warn with the per-origin breakdown.
    Warn,
    /// At or above 1.5 GiB: empty only the allowed caches; never session data.
    ClearAllowedCaches,
}

/// Classify a profile directory size.
#[must_use]
pub fn profile_action(profile_bytes: u64) -> ProfileAction {
    if profile_bytes >= PROFILE_CLEAR_BYTES {
        ProfileAction::ClearAllowedCaches
    } else if profile_bytes >= PROFILE_WARN_BYTES {
        ProfileAction::Warn
    } else {
        ProfileAction::Ok
    }
}

/// True when a growing operation of `growth_bytes` keeps the reserve intact.
#[must_use]
pub fn disk_admission_allowed(free_bytes: u64, growth_bytes: u64) -> bool {
    free_bytes.saturating_sub(growth_bytes) >= DISK_RESERVE_BYTES
}

/// True when the search backend may start under `profile`.
///
/// The free space must cover the profile budget plus the 5 GiB reserve.
#[must_use]
pub fn search_start_allowed(free_bytes: u64, profile: SearchProfile) -> bool {
    let need = match profile {
        SearchProfile::Minimo => SEARCH_MINIMO_INDEX_BYTES + SEARCH_MINIMO_STATE_BYTES,
        SearchProfile::Ampliado => SEARCH_AMPLIADO_INDEX_BYTES + SEARCH_AMPLIADO_STATE_BYTES,
    };
    free_bytes >= need.saturating_add(DISK_RESERVE_BYTES)
}

/// Low-disk cleanup scope. Listening origins are never touched.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LowDiskAction {
    /// Empty all ephemeral caches.
    pub empty_ephemeral_caches: bool,
    /// Clear persistent `cache_storage` and `shader_cache` of non-listening
    /// origins only.
    pub clear_persistent_non_listener_caches: bool,
    /// Never clean listening origins.
    pub touches_listening_origins: bool,
}

/// Low-disk policy: active below 7 GiB free (2 GiB above the reserve) or when
/// the webd budget is exceeded.
#[must_use]
pub fn low_disk_action(free_bytes: u64, webd_budget_exceeded: bool) -> Option<LowDiskAction> {
    if free_bytes < DISK_LOW_THRESHOLD_BYTES || webd_budget_exceeded {
        Some(LowDiskAction {
            empty_ephemeral_caches: true,
            clear_persistent_non_listener_caches: true,
            touches_listening_origins: false,
        })
    } else {
        None
    }
}

/// webd data-budget state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WebdDataUsage {
    /// Snapshot blobs in bytes.
    pub snapshots_bytes: u64,
    /// Read cache in bytes.
    pub read_cache_bytes: u64,
    /// Quarantine plus staging in bytes.
    pub quarantine_bytes: u64,
}

impl WebdDataUsage {
    /// Combined total without the search index.
    #[must_use]
    pub fn total_bytes(self) -> u64 {
        self.snapshots_bytes
            .saturating_add(self.read_cache_bytes)
            .saturating_add(self.quarantine_bytes)
    }

    /// True when the 2.5 GiB webd total is exceeded.
    #[must_use]
    pub fn total_exceeded(self) -> bool {
        self.total_bytes() > WEBD_DATA_TOTAL_BYTES
    }

    /// True when snapshots exceed the 500 MiB LRU budget.
    #[must_use]
    pub fn snapshots_exceeded(self) -> bool {
        self.snapshots_bytes > SNAPSHOT_BUDGET_BYTES
    }

    /// True when the read cache exceeds 200 MiB.
    #[must_use]
    pub fn read_cache_exceeded(self) -> bool {
        self.read_cache_bytes > READ_CACHE_BUDGET_BYTES
    }

    /// True when quarantine plus staging exceed 300 MiB (reject new items).
    #[must_use]
    pub fn quarantine_full(self) -> bool {
        self.quarantine_bytes >= QUARANTINE_BUDGET_BYTES
    }
}

/// Least-recently-used byte budget: insert evicts oldest entries first.
#[derive(Debug, Clone, Default)]
pub struct LruBudget {
    /// Budgets in bytes.
    pub budget_bytes: u64,
    entries: Vec<LruEntry>,
}

/// One cached entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LruEntry {
    /// Entry key.
    pub key: String,
    /// Entry size in bytes.
    pub bytes: u64,
    /// Insertion order (lower is older).
    pub seq: u64,
}

impl LruBudget {
    /// Create a budget of `budget_bytes`.
    #[must_use]
    pub fn new(budget_bytes: u64) -> Self {
        Self {
            budget_bytes,
            entries: Vec::new(),
        }
    }

    /// Current total in bytes.
    #[must_use]
    pub fn total_bytes(&self) -> u64 {
        self.entries.iter().map(|e| e.bytes).sum()
    }

    /// Insert `entry`, evicting oldest entries while over budget.
    /// Returns the evicted keys in eviction order.
    pub fn insert(&mut self, entry: LruEntry) -> Vec<String> {
        self.entries.push(entry);
        self.entries.sort_by_key(|e| e.seq);
        let mut evicted = Vec::new();
        while self.total_bytes() > self.budget_bytes {
            if self.entries.is_empty() {
                break;
            }
            evicted.push(self.entries.remove(0).key.clone());
        }
        evicted
    }
}
