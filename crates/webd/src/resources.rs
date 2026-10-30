//! B6 resource wiring for webd (architecture 13.1-13.6, 18.3).
//!
//! Thin enforcement layer over `excalibur-browser-resources`: admission with
//! the per-origin moving average, the ordered 1-7 eviction plan with LIVE
//! protection, disk reserve gates, low-disk cleanup sparing listening
//! origins, and harness telemetry fields. No browser lifecycle, broker or
//! host accounting lives here; every budget constant and decision rule stays
//! in the shared core.

use excalibur_browser_resources::admission::{
    AdmissionDecision, AdmissionReadings, AdmissionRequest, DenyReason, OriginEstimator, decide,
};
use excalibur_browser_resources::budgets::{DiskBudgets, MemoryBudgets, SearchProfile};
use excalibur_browser_resources::disk::{
    LowDiskAction, WebdDataUsage, disk_admission_allowed, low_disk_action, search_start_allowed,
};
use excalibur_browser_resources::error::{ResourceError, Result};
use excalibur_browser_resources::eviction::{EvictionStep, TabInfo, suspend_session};
use excalibur_browser_resources::telemetry::BrowserResourceSample;

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
    /// supply the remaining live readings. Over-budget outcomes return a
    /// 120 s wait so eviction (13.3 steps 1-6) can free room before the
    /// caller reports `capacity_exceeded`.
    #[must_use]
    pub fn admit(&self, origin: &str, readings: AdmissionReadings) -> AdmissionDecision {
        let request = AdmissionRequest::with_estimator(origin, &self.estimator, readings);
        let mut request = request;
        request.soft_budget_bytes = self.memory.soft_global_bytes;
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

    /// Enforce suspension protection: listening sessions refuse without
    /// explicit user approval (`listener_cannot_suspend`).
    pub fn suspend_session(is_listening: bool, user_approved: bool) -> Result<()> {
        suspend_session(is_listening, user_approved)
    }

    /// True when a disk-growing operation keeps the configured reserve.
    #[must_use]
    pub fn disk_growth_allowed(&self, free_bytes: u64, growth_bytes: u64) -> bool {
        free_bytes.saturating_sub(growth_bytes) >= self.disk.reserve_bytes
            && disk_admission_allowed(free_bytes, growth_bytes)
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
