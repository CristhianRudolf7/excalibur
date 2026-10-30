//! Admission control from architecture sections 13.2 and 13.5.
//!
//! A new operation is admitted only when the origin estimate plus the fixed
//! margin fits under the soft global budget, tab limits hold, PSI pressure is
//! below 20 % and the free disk would stay above the 5 GiB reserve. The
//! estimate is a per-origin moving average starting at 200 MiB (400 MiB for
//! known heavy applications such as WhatsApp and Teams).

use std::collections::HashMap;

use crate::budgets::{
    ADMISSION_DEFAULT_ESTIMATE_BYTES, ADMISSION_HEAVY_ESTIMATE_BYTES, ADMISSION_MARGIN_BYTES,
    ADMISSION_PSI_AVG10_LIMIT_PCT, ADMISSION_WAIT_TIMEOUT_SECS, DISK_RESERVE_BYTES, TABS_GLOBAL,
    TABS_PER_EXECUTION,
};

/// Origins whose default estimate is 400 MiB (measured WhatsApp/Teams class).
const HEAVY_ORIGINS: &[&str] = &[
    "web.whatsapp.com",
    "teams.microsoft.com",
    "teams.live.com",
    "mail.google.com",
];

/// True for the known heavy-application class.
#[must_use]
pub fn is_heavy_origin(origin: &str) -> bool {
    HEAVY_ORIGINS.contains(&origin)
}

/// Default estimate for an origin without history.
#[must_use]
pub fn default_estimate_bytes(origin: &str) -> u64 {
    if is_heavy_origin(origin) {
        ADMISSION_HEAVY_ESTIMATE_BYTES
    } else {
        ADMISSION_DEFAULT_ESTIMATE_BYTES
    }
}

/// Per-origin moving-average footprint estimator.
#[derive(Debug, Clone, Default)]
pub struct OriginEstimator {
    means: HashMap<String, u64>,
    counts: HashMap<String, u64>,
}

impl OriginEstimator {
    /// Create an empty estimator.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a finished-session footprint sample for `origin`.
    pub fn observe(&mut self, origin: &str, bytes: u64) {
        let count = self.counts.entry(origin.to_string()).or_insert(0);
        let mean = self.means.entry(origin.to_string()).or_insert(0);
        *count += 1;
        let n = *count;
        // Integer cumulative mean: mean += (sample - mean) / n.
        let updated = (*mean as i64 + ((bytes as i64 - *mean as i64) / n as i64)) as u64;
        *mean = updated;
    }

    /// Current estimate: the moving average, or the architecture default.
    #[must_use]
    pub fn estimate(&self, origin: &str) -> u64 {
        self.means
            .get(origin)
            .copied()
            .unwrap_or_else(|| default_estimate_bytes(origin))
    }
}

/// Inputs to one admission decision.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AdmissionRequest<'a> {
    /// Origin about to be opened (bare host, e.g. `example.com`).
    pub origin: &'a str,
    /// Estimated footprint in bytes (from [`OriginEstimator`]).
    pub estimated_bytes: u64,
    /// Current global working set in bytes.
    pub working_set_bytes: u64,
    /// Soft global budget in bytes.
    pub soft_budget_bytes: u64,
    /// Current PSI `some avg10` in percent, when measured.
    pub psi_some_avg10_pct: Option<f64>,
    /// Tabs already open in this execution.
    pub execution_tabs: u32,
    /// Tabs already open globally.
    pub global_tabs: u32,
    /// Free disk in bytes before the operation.
    pub free_disk_bytes: u64,
    /// Disk the operation would newly consume (profile, download...).
    pub growth_bytes: u64,
}

/// Runtime readings for one admission check, wired by webd.
///
/// Bundling keeps constructor and gate signatures under the argument limit
/// while every threshold stays in the shared core.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AdmissionReadings {
    /// Current global working set in bytes.
    pub working_set_bytes: u64,
    /// Soft global budget in bytes.
    pub soft_budget_bytes: u64,
    /// Current PSI `some avg10` in percent, when measured.
    pub psi_some_avg10_pct: Option<f64>,
    /// Tabs already open in this execution.
    pub execution_tabs: u32,
    /// Tabs already open globally.
    pub global_tabs: u32,
    /// Free disk in bytes before the operation.
    pub free_disk_bytes: u64,
    /// Disk the operation would newly consume (profile, download...).
    pub growth_bytes: u64,
}

impl<'a> AdmissionRequest<'a> {
    /// Build a request for a runtime `origin`, filling the estimate from
    /// `estimator`. This keeps per-origin moving-average logic in the shared
    /// core so webd wiring never hard-codes footprint defaults.
    #[must_use]
    pub fn with_estimator(
        origin: &'a str,
        estimator: &OriginEstimator,
        readings: AdmissionReadings,
    ) -> Self {
        Self {
            origin,
            estimated_bytes: estimator.estimate(origin),
            working_set_bytes: readings.working_set_bytes,
            soft_budget_bytes: readings.soft_budget_bytes,
            psi_some_avg10_pct: readings.psi_some_avg10_pct,
            execution_tabs: readings.execution_tabs,
            global_tabs: readings.global_tabs,
            free_disk_bytes: readings.free_disk_bytes,
            growth_bytes: readings.growth_bytes,
        }
    }
}

/// Memory breakdown attached to waits and denials.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AdmissionBreakdown {
    /// Current working set.
    pub working_set_bytes: u64,
    /// Soft budget.
    pub soft_budget_bytes: u64,
    /// Origin estimate.
    pub estimated_bytes: u64,
    /// Fixed margin.
    pub margin_bytes: u64,
    /// Projected total (`working + estimate + margin`).
    pub projected_bytes: u64,
}

/// Admission outcome.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum AdmissionDecision {
    /// Open the tab or context.
    Admit,
    /// Over budget but eviction may free room: wait up to 120 s first.
    WaitWithTimeout {
        /// Seconds to wait before returning `capacity_exceeded`.
        timeout_secs: u64,
        /// Memory figures for the wait log.
        breakdown: AdmissionBreakdown,
    },
    /// Refused without waiting (limits, pressure or disk reserve).
    Deny(DenyReason),
}

/// Refusal cause.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum DenyReason {
    /// Per-execution or global tab limit reached.
    TabLimit {
        /// Tabs in this execution.
        execution_tabs: u32,
        /// Tabs globally.
        global_tabs: u32,
    },
    /// PSI pressure at or above 20 %.
    Pressure {
        /// Observed PSI `some avg10`.
        psi_some_avg10_pct: f64,
    },
    /// Free disk would fall below the 5 GiB reserve.
    DiskReserve {
        /// Free disk before the operation.
        free_disk_bytes: u64,
        /// Disk the operation would consume.
        growth_bytes: u64,
    },
    /// Over budget with no evictable room left.
    OverBudget(AdmissionBreakdown),
}

/// Decide one admission request.
///
/// Tab limits, PSI pressure and the disk reserve deny immediately. Memory
/// over budget returns a 120 s wait so eviction (13.3 steps 1-6) can free
/// room; the caller reports `capacity_exceeded` with the breakdown when the
/// wait expires.
#[must_use]
pub fn decide(request: &AdmissionRequest<'_>) -> AdmissionDecision {
    if request.execution_tabs >= TABS_PER_EXECUTION || request.global_tabs >= TABS_GLOBAL {
        return AdmissionDecision::Deny(DenyReason::TabLimit {
            execution_tabs: request.execution_tabs,
            global_tabs: request.global_tabs,
        });
    }
    if let Some(psi) = request.psi_some_avg10_pct
        && psi >= ADMISSION_PSI_AVG10_LIMIT_PCT
    {
        return AdmissionDecision::Deny(DenyReason::Pressure {
            psi_some_avg10_pct: psi,
        });
    }
    if request.free_disk_bytes.saturating_sub(request.growth_bytes) < DISK_RESERVE_BYTES {
        return AdmissionDecision::Deny(DenyReason::DiskReserve {
            free_disk_bytes: request.free_disk_bytes,
            growth_bytes: request.growth_bytes,
        });
    }
    let projected = request
        .working_set_bytes
        .saturating_add(request.estimated_bytes)
        .saturating_add(ADMISSION_MARGIN_BYTES);
    if projected <= request.soft_budget_bytes {
        AdmissionDecision::Admit
    } else {
        AdmissionDecision::WaitWithTimeout {
            timeout_secs: ADMISSION_WAIT_TIMEOUT_SECS,
            breakdown: AdmissionBreakdown {
                working_set_bytes: request.working_set_bytes,
                soft_budget_bytes: request.soft_budget_bytes,
                estimated_bytes: request.estimated_bytes,
                margin_bytes: ADMISSION_MARGIN_BYTES,
                projected_bytes: projected,
            },
        }
    }
}
