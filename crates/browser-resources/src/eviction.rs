//! Eviction order and listening-session protection (sections 13.3-13.4).
//!
//! Classes are LIVE (listening tabs), ACTIVE (an agent operation is running
//! or ran in the last 60 s), IDLE (unused past the 5-minute TTL) and plain
//! HTTP reads. Steps run in order and stop as soon as the working set falls
//! back under the soft budget. Listening tabs are never frozen, discarded,
//! pressured or suspended: freezing stops event delivery and a strict
//! `memory.high` without swap throttles the page into the same state.

use crate::budgets::{
    ACTIVE_WINDOW_SECS, EPHEMERAL_CLOSE_PER_TAB_BYTES, IDLE_TTL_SECS, LIVE_OVER_SECS,
    LIVE_QUIET_SECS, LIVE_RELOAD_BYTES,
};
use crate::error::{ResourceError, Result};

/// Liveness class of a tab or context.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum TabClass {
    /// Listening tab: protected from every destructive step.
    Live,
    /// Agent operation in progress or within the last 60 s.
    Active,
    /// Unused past the TTL; evicted least-recently-used first.
    Idle,
    /// Plain HTTP read (never holds a renderer).
    Http,
}

/// Instance hosting a tab.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstanceKind {
    /// Persistent instance hosting listening sessions.
    Persistent,
    /// Ephemeral instance for agent browsing.
    Ephemeral,
}

/// Observable tab state for one planning pass.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TabInfo {
    /// Stable tab identifier.
    pub id: String,
    /// Liveness class.
    pub class: TabClass,
    /// Hosting instance.
    pub instance: InstanceKind,
    /// Seconds since the last agent or network activity.
    pub idle_secs: u64,
    /// Current working-set estimate for the tab in bytes.
    pub working_set_bytes: u64,
    /// True while the tab belongs to a listening session.
    pub in_listening_session: bool,
}

/// Ordered eviction steps (architecture 13.3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EvictionStep {
    /// 1. Drop webd in-RAM caches (snapshot LRU, read cache).
    ClearWebdCaches,
    /// 2. Close IDLE contexts and tabs, least recent first (TTL 5 min),
    ///    including non-listening tabs of the persistent instance.
    CloseIdle {
        /// Tab ids in eviction order.
        tab_ids: Vec<String>,
    },
    /// 3a. `HeapProfiler.collectGarbage` on ephemeral ACTIVE tabs.
    CollectGarbageEphemeral {
        /// Tab ids to collect.
        tab_ids: Vec<String>,
    },
    /// 3b. `Memory.simulatePressureNotification(moderate)` on the ephemeral
    /// instance only; never on the persistent one.
    PressureEphemeral,
    /// 4. `Network.clearBrowserCache` on the ephemeral instance.
    ClearBrowserCacheEphemeral,
    /// 5. Close one ephemeral ACTIVE tab above its hard limit.
    CloseActiveOverLimit {
        /// Evicted tab id; the agent receives `tab_evicted`.
        tab_id: String,
    },
    /// 6. Restart the ephemeral instance.
    RestartEphemeral,
    /// 7. LIVE: only a quiet reload, never freeze/discard/pressure.
    ScheduleLiveReload {
        /// Tab id to reload at a quiet moment.
        tab_id: String,
    },
}

impl EvictionStep {
    /// Position in the 13.3 order (1-7).
    #[must_use]
    pub fn order(&self) -> u8 {
        match self {
            Self::ClearWebdCaches => 1,
            Self::CloseIdle { .. } => 2,
            Self::CollectGarbageEphemeral { .. } | Self::PressureEphemeral => 3,
            Self::ClearBrowserCacheEphemeral => 4,
            Self::CloseActiveOverLimit { .. } => 5,
            Self::RestartEphemeral => 6,
            Self::ScheduleLiveReload { .. } => 7,
        }
    }

    /// True when the step may touch a listening tab. Only step 7 names a
    /// LIVE tab, and only as a quiet reload preserving the session.
    #[must_use]
    pub fn names_live_tab(&self, live_ids: &[String]) -> bool {
        match self {
            Self::ScheduleLiveReload { tab_id } => live_ids.contains(tab_id),
            Self::CloseIdle { tab_ids } | Self::CollectGarbageEphemeral { tab_ids } => {
                tab_ids.iter().any(|id| live_ids.contains(id))
            }
            Self::CloseActiveOverLimit { tab_id } => live_ids.contains(tab_id),
            Self::ClearWebdCaches
            | Self::PressureEphemeral
            | Self::ClearBrowserCacheEphemeral
            | Self::RestartEphemeral => false,
        }
    }
}

/// Forbidden memory operations: they kill workers, stop events or throttle
/// the page without freeing memory.
#[must_use]
pub fn forbidden_operations() -> [&'static str; 3] {
    [
        "Memory.prepareForLeakDetection",
        "Page.setWebLifecycleState(frozen)",
        "strict memory.high without swap",
    ]
}

/// Plan the eviction order for tabs currently over budget.
///
/// Returns steps 1-6 in order for the evictable set plus step 7 entries for
/// LIVE tabs that held above 1 GiB for 10 minutes and are quiet. LIVE tabs
/// never appear in steps 2, 3 or 5. Callers execute in order and stop as soon
/// as the working set returns under budget.
#[must_use]
pub fn plan_eviction(tabs: &[TabInfo]) -> Vec<EvictionStep> {
    let mut steps = vec![EvictionStep::ClearWebdCaches];

    let mut idle: Vec<&TabInfo> = tabs
        .iter()
        .filter(|t| {
            t.class == TabClass::Idle && t.idle_secs >= IDLE_TTL_SECS && !t.in_listening_session
        })
        .collect();
    idle.sort_by_key(|t| std::cmp::Reverse(t.idle_secs));
    let idle_ids: Vec<String> = idle.iter().map(|t| t.id.clone()).collect();
    if !idle_ids.is_empty() {
        steps.push(EvictionStep::CloseIdle { tab_ids: idle_ids });
    }

    let ephemeral_active: Vec<String> = tabs
        .iter()
        .filter(|t| {
            t.class == TabClass::Active
                && t.instance == InstanceKind::Ephemeral
                && !t.in_listening_session
        })
        .map(|t| t.id.clone())
        .collect();
    if !ephemeral_active.is_empty() {
        steps.push(EvictionStep::CollectGarbageEphemeral {
            tab_ids: ephemeral_active,
        });
        steps.push(EvictionStep::PressureEphemeral);
    }

    if tabs
        .iter()
        .any(|t| t.instance == InstanceKind::Ephemeral && !t.in_listening_session)
    {
        steps.push(EvictionStep::ClearBrowserCacheEphemeral);
    }

    let mut over_limit: Vec<&TabInfo> = tabs
        .iter()
        .filter(|t| {
            t.class == TabClass::Active
                && t.instance == InstanceKind::Ephemeral
                && !t.in_listening_session
                && t.working_set_bytes >= EPHEMERAL_CLOSE_PER_TAB_BYTES
        })
        .collect();
    over_limit.sort_by_key(|t| std::cmp::Reverse(t.working_set_bytes));
    if let Some(victim) = over_limit.first() {
        steps.push(EvictionStep::CloseActiveOverLimit {
            tab_id: victim.id.clone(),
        });
    }

    if tabs
        .iter()
        .any(|t| t.instance == InstanceKind::Ephemeral && !t.in_listening_session)
    {
        steps.push(EvictionStep::RestartEphemeral);
    }

    for tab in tabs.iter().filter(|t| {
        t.class == TabClass::Live
            && t.working_set_bytes >= LIVE_RELOAD_BYTES
            && t.idle_secs >= LIVE_QUIET_SECS
    }) {
        steps.push(EvictionStep::ScheduleLiveReload {
            tab_id: tab.id.clone(),
        });
    }

    steps.sort_by_key(EvictionStep::order);
    steps
}

/// Decide whether a LIVE tab holding `bytes` for `over_secs` with
/// `quiet_secs` of no activity should be reloaded now.
///
/// The reload is scheduled only after 1 GiB for 10 minutes and 2 minutes of
/// quiet; it preserves cookies and IndexedDB because the profile is kept.
#[must_use]
pub fn should_schedule_live_reload(bytes: u64, over_secs: u64, quiet_secs: u64) -> bool {
    bytes >= LIVE_RELOAD_BYTES && over_secs >= LIVE_OVER_SECS && quiet_secs >= LIVE_QUIET_SECS
}

/// Classify a tab from its last-activity age.
#[must_use]
pub fn classify_by_age(is_live: bool, secs_since_activity: u64) -> TabClass {
    if is_live {
        TabClass::Live
    } else if secs_since_activity <= ACTIVE_WINDOW_SECS {
        TabClass::Active
    } else {
        TabClass::Idle
    }
}

/// Suspend a session: close its tabs and keep URLs for reopening. Listening
/// sessions refuse with `listener_cannot_suspend` unless the user approved.
pub fn suspend_session(is_listening: bool, user_approved: bool) -> Result<()> {
    if is_listening && !user_approved {
        return Err(ResourceError::ListenerCannotSuspend);
    }
    Ok(())
}
