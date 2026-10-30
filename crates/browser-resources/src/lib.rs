//! Standalone browser resource foundation for milestone B6.
//!
//! This crate implements the parts of architecture section 13 (plus the 18.3
//! measurement procedure) that do not need the live browser lifecycle, the
//! network broker or host accounting: tiered memory sampling with process
//! attribution, default RAM and disk budgets, admission control, the ordered
//! eviction plan with listening-session protection, disk admission gates,
//! harness telemetry fields and the reproducible measurement summary.
//!
//! The main lane wires these types into `webd` later without changing their
//! shapes: budgets, response codes (`capacity_exceeded`,
//! `listener_cannot_suspend`, `tab_evicted`) and telemetry fields already
//! follow the architecture. Every check runs on local synthetic fixtures;
//! no model access, no live crawl and no real user sessions are involved.

pub mod admission;
pub mod budgets;
pub mod disk;
pub mod error;
pub mod eviction;
pub mod measure;
pub mod sampling;
pub mod telemetry;

pub use admission::{
    AdmissionBreakdown, AdmissionDecision, AdmissionReadings, AdmissionRequest, DenyReason,
    OriginEstimator, decide, default_estimate_bytes, is_heavy_origin,
};
pub use budgets::{
    ADMISSION_DEFAULT_ESTIMATE_BYTES, ADMISSION_HEAVY_ESTIMATE_BYTES, ADMISSION_MARGIN_BYTES,
    ADMISSION_PSI_AVG10_LIMIT_PCT, ADMISSION_WAIT_TIMEOUT_SECS, CGROUP_MAX_BYTES,
    CHROME_CACHE_EPHEMERAL_BYTES, CHROME_CACHE_PERSISTENT_BYTES, DISK_RESERVE_BYTES, DiskBudgets,
    EPHEMERAL_CLOSE_PER_TAB_BYTES, EPHEMERAL_MAX_CONTEXTS, EPHEMERAL_MAX_TABS, HARD_GLOBAL_BYTES,
    LIVE_OVER_SECS, LIVE_QUIET_SECS, LIVE_RELOAD_BYTES, MemoryBudgets, SOFT_GLOBAL_BYTES,
    SearchProfile, TABS_GLOBAL, TABS_PER_EXECUTION, WEBD_MAX_RSS_BYTES,
};
pub use disk::{
    LowDiskAction, LruBudget, LruEntry, ProfileAction, WebdDataUsage, chrome_cache_bytes,
    disk_admission_allowed, low_disk_action, profile_action, search_start_allowed,
};
pub use error::{ResourceError, Result};
pub use eviction::{
    EvictionStep, InstanceKind, TabClass, TabInfo, classify_by_age, forbidden_operations,
    plan_eviction, should_schedule_live_reload, suspend_session,
};
pub use measure::{
    HEAVY_TABS_PER_CASE, LevelReadings, MeasureEnvironment, MeasuredCase, REPETITIONS, SETTLE_SECS,
    median_of_three, summarize_triplet,
};
pub use sampling::{
    InstanceClass as SamplingInstanceClass, MemoryEvents, ProcEntry, ProcessType, Sample,
    SampleLevel, classify_cmdline, current_process_working_set, desired_oom_score_adj,
    enumerate_proc_root, oom_raise_allowed, parse_byte_counter, parse_memory_events,
    parse_memory_stat_inactive_file, parse_psi_some_avg10, parse_smaps_pss,
    parse_status_working_set, psi_triggers_eviction, working_set_a,
};
pub use telemetry::BrowserResourceSample;
