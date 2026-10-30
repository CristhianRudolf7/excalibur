//! B6 resource wiring for webd (architecture 13.1-13.6, 18.3).
//!
//! Thin enforcement layer over `excalibur-browser-resources`: admission with
//! the per-origin moving average, the ordered 1-7 eviction plan with LIVE
//! protection, disk reserve gates, low-disk cleanup sparing listening
//! origins, and harness telemetry fields. No browser lifecycle, broker or
//! host accounting lives here; every budget constant and decision rule stays
//! in the shared core.

use excalibur_browser_resources::admission::{AdmissionRequest, OriginEstimator, decide};
use excalibur_browser_resources::budgets::{DiskBudgets, MemoryBudgets, SearchProfile};
use excalibur_browser_resources::disk::{
    LowDiskAction, WebdDataUsage, low_disk_action, search_start_allowed,
};
use excalibur_browser_resources::error::{ResourceError, Result};
use excalibur_browser_resources::eviction::{EvictionStep, TabInfo, suspend_session};
use excalibur_browser_resources::telemetry::BrowserResourceSample;

pub use excalibur_browser_resources::admission::{
    AdmissionDecision, AdmissionReadings, DenyReason,
};

/// Wired resource gate held by the webd service.
///
/// Budgets come from the shared core defaults; the running service only
/// supplies live readings (working set, PSI, free disk, tab counts) and
/// applies the returned decisions.
#[derive(Debug)]
pub struct ResourceGate {
    estimator: OriginEstimator,
    sample: BrowserResourceSample,
    memory: MemoryBudgets,
    disk: DiskBudgets,
}

impl Default for ResourceGate {
    fn default() -> Self {
        Self::new()
    }
}

impl ResourceGate {
    /// Create a gate with architecture default budgets and an empty estimator.
    #[must_use]
    pub fn new() -> Self {
        Self {
            estimator: OriginEstimator::new(),
            sample: BrowserResourceSample::new(0, None, 0, 0),
            memory: MemoryBudgets::default(),
            disk: DiskBudgets::default(),
        }
    }

    /// Memory budgets in force.
    #[must_use]
    pub fn memory_budgets(&self) -> MemoryBudgets {
        self.memory
    }

    /// Disk budgets in force.
    #[must_use]
    pub fn disk_budgets(&self) -> DiskBudgets {
        self.disk
    }

    /// Decide admission for `origin` using the moving-average estimate.
    ///
    /// The soft budget always comes from the wired [`MemoryBudgets`]; callers
    /// supply the remaining live readings (no budget field to ignore).
    /// Over-budget outcomes return a 120 s wait so eviction (13.3 steps 1-6)
    /// can free room before the caller reports `capacity_exceeded` (see
    /// [`denial_code`]).
    #[must_use]
    pub fn admit(&self, origin: &str, readings: AdmissionReadings) -> AdmissionDecision {
        let request = AdmissionRequest::with_estimator(
            origin,
            &self.estimator,
            readings,
            self.memory.soft_global_bytes,
        );
        decide(&request)
    }

    /// Record a finished footprint sample for the moving average.
    pub fn observe_finished(&mut self, origin: &str, bytes: u64) {
        self.estimator.observe(origin, bytes);
    }

    /// Current estimate for `origin` (moving average or architecture default).
    #[must_use]
    pub fn estimate(&self, origin: &str) -> u64 {
        self.estimator.estimate(origin)
    }

    /// Plan eviction for the observed tabs in architecture 13.3 order.
    ///
    /// Steps stop as soon as the working set returns under budget; LIVE tabs
    /// only ever appear in step 7 as a quiet reload.
    #[must_use]
    pub fn eviction_plan(&self, tabs: &[TabInfo]) -> Vec<EvictionStep> {
        excalibur_browser_resources::eviction::plan_eviction(tabs)
    }

    /// Decision helper for suspension protection: listening sessions refuse
    /// without explicit user approval (`listener_cannot_suspend`).
    ///
    /// Enforcing layer: `sessions::Manager` enforces this at the lifecycle
    /// boundary; `eviction::suspend_session` and this helper only compute the
    /// decision so webd wiring and tests share the rule without duplicating it.
    pub fn suspend_session(is_listening: bool, user_approved: bool) -> Result<()> {
        suspend_session(is_listening, user_approved)
    }

    /// True when a disk-growing operation keeps the configured reserve.
    ///
    /// The enforcing layer is the webd service (`serve` startup and any
    /// future per-operation gates): only the configured
    /// [`DiskBudgets::reserve_bytes`] (default 5 GiB from the shared core) is
    /// checked here. The core helper [`disk_admission_allowed`] encodes the
    /// same constant and is not checked twice.
    #[must_use]
    pub fn disk_growth_allowed(&self, free_bytes: u64, growth_bytes: u64) -> bool {
        free_bytes.saturating_sub(growth_bytes) >= self.disk.reserve_bytes
    }

    /// True when the search backend may start under `profile`.
    #[must_use]
    pub fn search_allowed(&self, free_bytes: u64, profile: SearchProfile) -> bool {
        search_start_allowed(free_bytes, profile)
    }

    /// Low-disk cleanup scope; never touches listening origins.
    #[must_use]
    pub fn low_disk_cleanup(
        &self,
        free_bytes: u64,
        webd_usage: WebdDataUsage,
    ) -> Option<LowDiskAction> {
        low_disk_action(free_bytes, webd_usage.total_exceeded())
    }

    /// Fold a fresh process-tree reading into the telemetry sample.
    pub fn note_sample(
        &mut self,
        working_set_bytes: u64,
        pss_bytes: Option<u64>,
        instances: u32,
        tabs: u32,
    ) {
        self.sample
            .observe(working_set_bytes, pss_bytes, instances, tabs);
    }

    /// Gather live admission readings for `data_dir` from the running machine.
    ///
    /// Working set prefers the private level-A cgroup (`memory.current -
    /// `inactive_file`) and falls back to the level-B `/proc` self-read when
    /// the cgroup is shared or lacks the memory controller; PSI comes from
    /// `/proc/pressure/memory` when exposed, free disk from `disk_available`.
    /// Tab counts come from the caller because only the service model knows
    /// them. No budget field is carried: `admit_live` always applies the
    /// wired [`MemoryBudgets`].
    #[must_use]
    pub fn live_readings(
        execution_tabs: u32,
        global_tabs: u32,
        growth_bytes: u64,
        data_dir: &std::path::Path,
    ) -> AdmissionReadings {
        let working_set_bytes = excalibur_browser_resources::sampling::cgroup_dir_from_self()
            .and_then(|dir| excalibur_browser_resources::sampling::read_cgroup_working_set(&dir))
            .or_else(excalibur_browser_resources::sampling::current_process_working_set)
            .unwrap_or(0);
        let psi_some_avg10_pct = excalibur_browser_resources::sampling::live_psi_some_avg10();
        let free_disk_bytes = crate::disk_available(data_dir).unwrap_or(0);
        AdmissionReadings {
            working_set_bytes,
            psi_some_avg10_pct,
            execution_tabs,
            global_tabs,
            free_disk_bytes,
            growth_bytes,
        }
    }

    /// Decide admission for `origin` using live machine readings.
    ///
    /// Per-operation memory admission in the serve path (13.3): the estimate
    /// comes from the shared-core moving average, the soft budget from the
    /// wired [`MemoryBudgets`], and every other reading from
    /// [`Self::live_readings`]. Over-budget outcomes return the 120 s
    /// `WaitWithTimeout` so eviction (steps 1-6) can run before the caller
    /// reports `capacity_exceeded`. Disk, tab-limit and PSI denials fail
    /// immediately without waiting.
    #[must_use]
    pub fn admit_live(
        &self,
        origin: &str,
        execution_tabs: u32,
        global_tabs: u32,
        growth_bytes: u64,
        data_dir: &std::path::Path,
    ) -> AdmissionDecision {
        let readings = Self::live_readings(execution_tabs, global_tabs, growth_bytes, data_dir);
        self.admit(origin, readings)
    }

    /// Fold one live working-set reading into the telemetry sample.
    ///
    /// Prefers level-A cgroup state and falls back to the level-B self-read;
    /// instance/tab counts come from the caller. Used at serve startup and
    /// shutdown so the emitted telemetry reflects the real machine instead
    /// of a zero placeholder.
    pub fn note_live_sample(&mut self, instances: u32, tabs: u32) {
        let working = excalibur_browser_resources::sampling::cgroup_dir_from_self()
            .and_then(|dir| excalibur_browser_resources::sampling::read_cgroup_working_set(&dir))
            .or_else(excalibur_browser_resources::sampling::current_process_working_set)
            .unwrap_or(0);
        self.note_sample(working, None, instances, tabs);
    }

    /// Current telemetry sample with peaks.
    #[must_use]
    pub fn telemetry(&self) -> BrowserResourceSample {
        self.sample
    }

    /// Telemetry fields for harness `ResourceSample` reporting, kept
    /// separate from harness and container RAM (architecture 13.1).
    #[must_use]
    pub fn telemetry_json(&self) -> serde_json::Value {
        telemetry_fields(&self.sample)
    }
}

/// Harness telemetry fields for one browser sample with peaks.
#[must_use]
pub fn telemetry_fields(sample: &BrowserResourceSample) -> serde_json::Value {
    serde_json::json!({
        "browser_working_set_bytes": sample.browser_working_set_bytes,
        "browser_pss_bytes": sample.browser_pss_bytes,
        "browser_instances": sample.browser_instances,
        "browser_tabs": sample.browser_tabs,
        "peak_working_set_bytes": sample.peak_working_set_bytes,
        "peak_pss_bytes": sample.peak_pss_bytes,
        "peak_instances": sample.peak_instances,
        "peak_tabs": sample.peak_tabs,
    })
}

/// Human-readable `capacity_exceeded` breakdown for wait expiry.
#[must_use]
pub fn capacity_exceeded_message(
    working_set_bytes: u64,
    soft_budget_bytes: u64,
    estimated_bytes: u64,
    margin_bytes: u64,
    projected_bytes: u64,
) -> String {
    format!(
        "capacity_exceeded: working_set={working_set_bytes} soft_budget={soft_budget_bytes} \
         estimate={estimated_bytes} margin={margin_bytes} projected={projected_bytes}"
    )
}

/// Map an admission denial to its wire code.
///
/// A `WaitWithTimeout` maps to `capacity_exceeded`: the caller waits up to
/// 120 s for eviction (13.3 steps 1-6) and reports `capacity_exceeded` with
/// the memory breakdown only when the wait expires. Immediate denials map to
/// `tab_limit`, `pressure`, `disk_reserve`, or `capacity_exceeded` for
/// `OverBudget` with no evictable room.
#[must_use]
pub fn denial_code(decision: &AdmissionDecision) -> &'static str {
    match decision {
        AdmissionDecision::Admit => "admit",
        AdmissionDecision::WaitWithTimeout { .. } => "capacity_exceeded",
        AdmissionDecision::Deny(DenyReason::TabLimit { .. }) => "tab_limit",
        AdmissionDecision::Deny(DenyReason::Pressure { .. }) => "pressure",
        AdmissionDecision::Deny(DenyReason::DiskReserve { .. }) => "disk_reserve",
        AdmissionDecision::Deny(DenyReason::OverBudget(_)) => "capacity_exceeded",
    }
}

/// Reserve gate for a configured reserve: growing by `growth_bytes` must
/// leave at least `reserve_bytes` free.
#[must_use]
pub fn growth_allowed_with_reserve(available: u64, growth: u64, reserve: u64) -> bool {
    crate::admits_growth(available, growth, reserve)
}

/// Convert a listener-suspension failure to its wire code.
#[must_use]
pub fn suspend_error_code(error: &ResourceError) -> &'static str {
    match error {
        ResourceError::ListenerCannotSuspend => "listener_cannot_suspend",
        ResourceError::BudgetExceeded(_)
        | ResourceError::CapacityExceeded(_)
        | ResourceError::DiskReserve(_)
        | ResourceError::InvalidInput(_) => "budget_exceeded",
    }
}
