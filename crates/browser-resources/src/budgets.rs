//! Default budgets from architecture sections 13.2 and 13.5.
//!
//! All values are explicit constants so the main lane can reuse them without
//! re-reading the document. RAM limits never change with the disk profile.

/// One mebibyte in bytes.
pub const MIB: u64 = 1024 * 1024;
/// One gibibyte in bytes.
pub const GIB: u64 = 1024 * MIB;

/// Soft global browser working-set budget: 1.5 GiB.
pub const SOFT_GLOBAL_BYTES: u64 = 1536 * MIB;
/// Hard global limit: 2.0 GiB.
pub const HARD_GLOBAL_BYTES: u64 = 2 * GIB;
/// cgroup `memory.max` backing: 2.5 GiB.
pub const CGROUP_MAX_BYTES: u64 = 2560 * MIB;

/// Per listening-tab warning: 700 MiB.
pub const LISTEN_WARN_PER_TAB_BYTES: u64 = 700 * MIB;
/// Live tab reload threshold: 1 GiB held for [`LIVE_OVER_SECS`].
pub const LIVE_RELOAD_BYTES: u64 = GIB;
/// Duration above [`LIVE_RELOAD_BYTES`] before a quiet reload is scheduled.
pub const LIVE_OVER_SECS: u64 = 10 * 60;
/// Quiet window (no activity) required before the scheduled reload.
pub const LIVE_QUIET_SECS: u64 = 2 * 60;

/// Ephemeral per-tab warning: 500 MiB.
pub const EPHEMERAL_WARN_PER_TAB_BYTES: u64 = 500 * MIB;
/// Ephemeral per-tab hard close: 800 MiB.
pub const EPHEMERAL_CLOSE_PER_TAB_BYTES: u64 = 800 * MIB;
/// Ephemeral instance limits: at most 3 contexts and 3 active tabs.
pub const EPHEMERAL_MAX_CONTEXTS: u32 = 3;
/// Ephemeral instance limits: at most 3 active tabs.
pub const EPHEMERAL_MAX_TABS: u32 = 3;
/// Ephemeral instance stops after 10 minutes without contexts.
pub const EPHEMERAL_IDLE_STOP_SECS: u64 = 10 * 60;

/// Tab limits: 4 per execution, 12 globally.
pub const TABS_PER_EXECUTION: u32 = 4;
/// Tab limits: 12 globally.
pub const TABS_GLOBAL: u32 = 12;

/// Default per-origin footprint estimate: 200 MiB.
pub const ADMISSION_DEFAULT_ESTIMATE_BYTES: u64 = 200 * MIB;
/// Known heavy applications (WhatsApp/Teams class): 400 MiB.
pub const ADMISSION_HEAVY_ESTIMATE_BYTES: u64 = 400 * MIB;
/// Admission margin added to every estimate: 200 MiB.
pub const ADMISSION_MARGIN_BYTES: u64 = 200 * MIB;
/// Admission refuses when PSI `some avg10` reaches 20 %.
pub const ADMISSION_PSI_AVG10_LIMIT_PCT: f64 = 20.0;
/// Level D eviction trigger: PSI `some avg10` above 10 %.
pub const EVICTION_PSI_AVG10_TRIGGER_PCT: f64 = 10.0;
/// A denied operation waits up to 120 s before `capacity_exceeded`.
pub const ADMISSION_WAIT_TIMEOUT_SECS: u64 = 120;

/// webd RSS budget without an open index: 60 MiB.
pub const WEBD_MAX_RSS_BYTES: u64 = 60 * MIB;

/// Classification window: an agent operation keeps a tab ACTIVE for 60 s.
pub const ACTIVE_WINDOW_SECS: u64 = 60;
/// Idle TTL before IDLE tabs are evicted first: 5 minutes.
pub const IDLE_TTL_SECS: u64 = 5 * 60;

/// Chrome HTTP cache: 32 MiB ephemeral, 64 MiB persistent.
pub const CHROME_CACHE_EPHEMERAL_BYTES: u64 = 32 * MIB;
/// Chrome HTTP cache: 64 MiB persistent.
pub const CHROME_CACHE_PERSISTENT_BYTES: u64 = 64 * MIB;
/// Profile directory walk interval: every 10 minutes.
pub const PROFILE_WALK_SECS: u64 = 10 * 60;
/// Per-profile warning: 1 GiB.
pub const PROFILE_WARN_BYTES: u64 = GIB;
/// Per-profile cache clearing starts at 1.5 GiB; session data is never deleted.
pub const PROFILE_CLEAR_BYTES: u64 = 1536 * MIB;

/// Always keep 5 GiB free; growing operations are refused below it.
pub const DISK_RESERVE_BYTES: u64 = 5 * GIB;
/// Low-disk policy starts at 7 GiB free (2 GiB above the reserve).
pub const DISK_LOW_THRESHOLD_BYTES: u64 = 7 * GIB;
/// webd data without the index: 2.5 GiB total.
pub const WEBD_DATA_TOTAL_BYTES: u64 = 2560 * MIB;
/// Snapshots in blobs: 500 MiB LRU.
pub const SNAPSHOT_BUDGET_BYTES: u64 = 500 * MIB;
/// Snapshot retention: 24 hours.
pub const SNAPSHOT_RETENTION_SECS: u64 = 24 * 3600;
/// Read cache: 200 MiB LRU.
pub const READ_CACHE_BUDGET_BYTES: u64 = 200 * MIB;
/// Quarantine plus staging: 300 MiB, new downloads/uploads rejected above it.
pub const QUARANTINE_BUDGET_BYTES: u64 = 300 * MIB;

/// Search `minimo` profile: 1.5 GiB index plus 0.5 GiB state.
pub const SEARCH_MINIMO_INDEX_BYTES: u64 = 1536 * MIB;
/// Search `minimo` profile state: 0.5 GiB.
pub const SEARCH_MINIMO_STATE_BYTES: u64 = 512 * MIB;
/// Search `ampliado` profile: 6 GiB index plus 1 GiB state.
pub const SEARCH_AMPLIADO_INDEX_BYTES: u64 = 6 * GIB;
/// Search `ampliado` profile state: 1 GiB.
pub const SEARCH_AMPLIADO_STATE_BYTES: u64 = GIB;
/// Report and clean the crate target when free disk drops below 10 GiB.
pub const TARGET_LOW_FREE_BYTES: u64 = 10 * GIB;

/// RAM budgets (architecture 13.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MemoryBudgets {
    /// Soft working-set budget for all webd plus Chrome processes.
    pub soft_global_bytes: u64,
    /// Hard global limit.
    pub hard_global_bytes: u64,
    /// cgroup `memory.max` backing when a private scope exists.
    pub cgroup_max_bytes: u64,
}

impl Default for MemoryBudgets {
    fn default() -> Self {
        Self {
            soft_global_bytes: SOFT_GLOBAL_BYTES,
            hard_global_bytes: HARD_GLOBAL_BYTES,
            cgroup_max_bytes: CGROUP_MAX_BYTES,
        }
    }
}

/// Disk budgets (architecture 13.5).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DiskBudgets {
    /// Free reserve that webd never grows into.
    pub reserve_bytes: u64,
    /// Low-disk policy threshold.
    pub low_threshold_bytes: u64,
    /// webd data without the index.
    pub webd_data_total_bytes: u64,
    /// Snapshot blob budget (LRU, 24 h).
    pub snapshot_bytes: u64,
    /// Read cache budget (LRU).
    pub read_cache_bytes: u64,
    /// Quarantine plus staging budget (reject above).
    pub quarantine_bytes: u64,
    /// Per-profile warning threshold.
    pub profile_warn_bytes: u64,
    /// Per-profile threshold where allowed caches are emptied.
    pub profile_clear_bytes: u64,
}

impl Default for DiskBudgets {
    fn default() -> Self {
        Self {
            reserve_bytes: DISK_RESERVE_BYTES,
            low_threshold_bytes: DISK_LOW_THRESHOLD_BYTES,
            webd_data_total_bytes: WEBD_DATA_TOTAL_BYTES,
            snapshot_bytes: SNAPSHOT_BUDGET_BYTES,
            read_cache_bytes: READ_CACHE_BUDGET_BYTES,
            quarantine_bytes: QUARANTINE_BUDGET_BYTES,
            profile_warn_bytes: PROFILE_WARN_BYTES,
            profile_clear_bytes: PROFILE_CLEAR_BYTES,
        }
    }
}

/// Disk profile selecting the search budgets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SearchProfile {
    /// Default: 1.5 GiB index plus 0.5 GiB state.
    #[default]
    Minimo,
    /// Optional: 6 GiB index plus 1 GiB state.
    Ampliado,
}

impl SearchProfile {
    /// Combined (index plus state) budget for the profile.
    #[must_use]
    pub fn combined_bytes(self) -> u64 {
        match self {
            Self::Minimo => SEARCH_MINIMO_INDEX_BYTES + SEARCH_MINIMO_STATE_BYTES,
            Self::Ampliado => SEARCH_AMPLIADO_INDEX_BYTES + SEARCH_AMPLIADO_STATE_BYTES,
        }
    }
}
