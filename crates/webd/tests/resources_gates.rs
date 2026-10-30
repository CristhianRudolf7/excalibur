//! B6 resource wiring checks (architecture 13.2-13.6, synthetic only).
//!
//! Exercises the webd [`ResourceGate`] over the shared core: budget
//! constants, moving-average admission gates, the ordered 1-7 eviction plan
//! with LIVE protection, disk reserve gates, `listener_cannot_suspend` and
//! telemetry emission. No real Chrome, no network, no accounts.

use excalibur_browser_resources::admission::{AdmissionDecision, AdmissionReadings, DenyReason};
use excalibur_browser_resources::budgets::{
    ADMISSION_MARGIN_BYTES, DISK_RESERVE_BYTES, EPHEMERAL_CLOSE_PER_TAB_BYTES, GIB,
    HARD_GLOBAL_BYTES, MIB, SEARCH_AMPLIADO_INDEX_BYTES, SEARCH_AMPLIADO_STATE_BYTES,
    SOFT_GLOBAL_BYTES, SearchProfile,
};
use excalibur_browser_resources::disk::{ProfileAction, WebdDataUsage, profile_action};
use excalibur_browser_resources::error::ResourceError;
use excalibur_browser_resources::eviction::{EvictionStep, InstanceKind, TabClass, TabInfo};
use excalibur_webd::resources::{ResourceGate, denial_code, suspend_error_code, telemetry_fields};

fn readings(
    working_set_bytes: u64,
    psi_some_avg10_pct: Option<f64>,
    execution_tabs: u32,
    global_tabs: u32,
    free_disk_bytes: u64,
    growth_bytes: u64,
) -> AdmissionReadings {
    AdmissionReadings {
        working_set_bytes,
        psi_some_avg10_pct,
        execution_tabs,
        global_tabs,
        free_disk_bytes,
        growth_bytes,
    }
}

fn tab(id: &str, class: TabClass, instance: InstanceKind, idle: u64, bytes: u64) -> TabInfo {
    TabInfo {
        id: id.to_string(),
        class,
        instance,
        idle_secs: idle,
        working_set_bytes: bytes,
        in_listening_session: class == TabClass::Live,
    }
}

#[test]
fn budget_constants_match_architecture() {
    let gate = ResourceGate::new();
    assert_eq!(gate.memory_budgets().soft_global_bytes, SOFT_GLOBAL_BYTES);
    assert_eq!(gate.memory_budgets().soft_global_bytes, 1536 * MIB);
    assert_eq!(gate.memory_budgets().hard_global_bytes, HARD_GLOBAL_BYTES);
    assert_eq!(gate.memory_budgets().hard_global_bytes, 2 * GIB);
    assert_eq!(gate.disk_budgets().reserve_bytes, DISK_RESERVE_BYTES);
    assert_eq!(gate.disk_budgets().reserve_bytes, 5 * GIB);
    assert_eq!(EPHEMERAL_CLOSE_PER_TAB_BYTES, 800 * MIB);
    assert_eq!(
        SearchProfile::Ampliado.combined_bytes(),
        SEARCH_AMPLIADO_INDEX_BYTES + SEARCH_AMPLIADO_STATE_BYTES
    );
}

#[test]
fn admission_moving_average_gates() {
    let mut gate = ResourceGate::new();
    // Default estimate admits a fresh origin under budget.
    assert_eq!(
        gate.admit("example.com", readings(500 * MIB, None, 0, 0, 20 * GIB, 0)),
        AdmissionDecision::Admit
    );
    // Heavy origins start at 400 MiB.
    assert_eq!(gate.estimate("web.whatsapp.com"), 400 * MIB);
    assert_eq!(gate.estimate("example.com"), 200 * MIB);
    // Moving average tracks finished samples.
    gate.observe_finished("example.com", 300 * MIB);
    assert_eq!(gate.estimate("example.com"), 300 * MIB);
    gate.observe_finished("example.com", 500 * MIB);
    assert_eq!(gate.estimate("example.com"), 400 * MIB);
    // Over budget waits 120 s with a breakdown before `capacity_exceeded`.
    match gate.admit("example.com", readings(1400 * MIB, None, 0, 0, 20 * GIB, 0)) {
        AdmissionDecision::WaitWithTimeout {
            timeout_secs,
            breakdown,
        } => {
            assert_eq!(timeout_secs, 120);
            assert_eq!(breakdown.margin_bytes, ADMISSION_MARGIN_BYTES);
            assert!(breakdown.projected_bytes > SOFT_GLOBAL_BYTES);
        }
        other => panic!("expected wait, got {other:?}"),
    }
    // Tab limits, PSI and reserve deny immediately.
    assert!(matches!(
        gate.admit("example.com", readings(100 * MIB, None, 4, 0, 20 * GIB, 0)),
        AdmissionDecision::Deny(DenyReason::TabLimit { .. })
    ));
    assert!(matches!(
        gate.admit(
            "example.com",
            readings(100 * MIB, Some(20.0), 0, 0, 20 * GIB, 0)
        ),
        AdmissionDecision::Deny(DenyReason::Pressure { .. })
    ));
    assert!(matches!(
        gate.admit(
            "example.com",
            readings(100 * MIB, None, 0, 0, 6 * GIB, 2 * GIB)
        ),
        AdmissionDecision::Deny(DenyReason::DiskReserve { .. })
    ));
    assert_eq!(denial_code(&AdmissionDecision::Admit), "admit");
    assert_eq!(
        denial_code(&gate.admit("example.com", readings(1400 * MIB, None, 0, 0, 20 * GIB, 0))),
        "capacity_exceeded"
    );
}

#[test]
fn eviction_order_protects_live() {
    let gate = ResourceGate::new();
    let tabs = vec![
        tab(
            "live",
            TabClass::Live,
            InstanceKind::Persistent,
            0,
            100 * MIB,
        ),
        tab(
            "old-idle",
            TabClass::Idle,
            InstanceKind::Persistent,
            900,
            50 * MIB,
        ),
        tab(
            "new-idle",
            TabClass::Idle,
            InstanceKind::Ephemeral,
            400,
            50 * MIB,
        ),
        tab(
            "agent-heavy",
            TabClass::Active,
            InstanceKind::Ephemeral,
            5,
            900 * MIB,
        ),
    ];
    let steps = gate.eviction_plan(&tabs);
    // Step 1 clears webd caches first; step 2 evicts IDLE least-recent first.
    assert_eq!(steps[0], EvictionStep::ClearWebdCaches);
    match &steps[1] {
        EvictionStep::CloseIdle { tab_ids } => {
            assert_eq!(
                tab_ids,
                &vec!["old-idle".to_string(), "new-idle".to_string()]
            );
        }
        other => panic!("expected CloseIdle, got {other:?}"),
    }
    // Orders are non-decreasing along the 1-7 plan.
    let mut last = 0u8;
    for step in &steps {
        assert!(step.order() >= last, "out of order: {steps:?}");
        last = step.order();
    }
    // Full 1-7 coverage on this mix: GC + pressure + cache + over-limit
    // close + ephemeral restart are all present.
    assert!(steps.contains(&EvictionStep::PressureEphemeral));
    assert!(steps.contains(&EvictionStep::ClearBrowserCacheEphemeral));
    assert!(steps.contains(&EvictionStep::RestartEphemeral));
    assert!(steps.contains(&EvictionStep::CloseActiveOverLimit {
        tab_id: "agent-heavy".to_string(),
    }));
    // LIVE tabs only appear in step 7 quiet reloads, never in destructive steps.
    let live_ids = vec!["live".to_string()];
    for step in &steps {
        match step {
            EvictionStep::ClearWebdCaches
            | EvictionStep::PressureEphemeral
            | EvictionStep::ClearBrowserCacheEphemeral
            | EvictionStep::RestartEphemeral => {}
            EvictionStep::ScheduleLiveReload { .. } => {}
            EvictionStep::CloseIdle { .. }
            | EvictionStep::CloseActiveOverLimit { .. }
            | EvictionStep::CollectGarbageEphemeral { .. } => {
                assert!(!step.names_live_tab(&live_ids), "LIVE in {step:?}");
            }
        }
    }
    // A quiet LIVE tab above 1 GiB schedules only a reload.
    let live_heavy = vec![tab(
        "live-heavy",
        TabClass::Live,
        InstanceKind::Persistent,
        180,
        1100 * MIB,
    )];
    let live_steps = gate.eviction_plan(&live_heavy);
    assert!(live_steps.contains(&EvictionStep::ScheduleLiveReload {
        tab_id: "live-heavy".to_string(),
    }));
}

#[test]
fn suspend_refuses_listening_without_approval() {
    assert_eq!(
        ResourceGate::suspend_session(true, false),
        Err(ResourceError::ListenerCannotSuspend)
    );
    assert_eq!(
        suspend_error_code(&ResourceError::ListenerCannotSuspend),
        "listener_cannot_suspend"
    );
    assert_eq!(ResourceGate::suspend_session(true, true), Ok(()));
    assert_eq!(ResourceGate::suspend_session(false, false), Ok(()));
}

#[test]
fn disk_gates_and_low_disk_cleanup() {
    let gate = ResourceGate::new();
    // Reserve gate: the 5 GiB reserve is unconditional.
    assert!(gate.disk_growth_allowed(6 * GIB, GIB));
    assert!(!gate.disk_growth_allowed(6 * GIB, 2 * GIB));
    assert!(!gate.disk_growth_allowed(4 * GIB, 0));
    // Search needs budget plus reserve.
    assert!(gate.search_allowed(8 * GIB, SearchProfile::Minimo));
    assert!(!gate.search_allowed(6 * GIB, SearchProfile::Minimo));
    // Profile thresholds warn at 1 GiB and clear allowed caches at 1.5 GiB.
    assert_eq!(profile_action(500 * MIB), ProfileAction::Ok);
    assert_eq!(profile_action(GIB), ProfileAction::Warn);
    assert_eq!(
        profile_action(1536 * MIB),
        ProfileAction::ClearAllowedCaches
    );
    // Low-disk cleanup spares listening origins.
    let healthy = WebdDataUsage {
        snapshots_bytes: 10 * MIB,
        read_cache_bytes: 10 * MIB,
        quarantine_bytes: 10 * MIB,
    };
    assert!(gate.low_disk_cleanup(20 * GIB, healthy).is_none());
    let action = gate.low_disk_cleanup(6 * GIB, healthy).unwrap();
    assert!(action.empty_ephemeral_caches);
    assert!(action.clear_persistent_non_listener_caches);
    assert!(!action.touches_listening_origins);
}

#[test]
fn telemetry_fields_report_peaks_separately() {
    let mut gate = ResourceGate::new();
    gate.note_sample(500 * MIB, Some(400 * MIB), 2, 3);
    gate.note_sample(700 * MIB, Some(300 * MIB), 1, 5);
    let sample = gate.telemetry();
    assert_eq!(sample.browser_working_set_bytes, 700 * MIB);
    assert_eq!(sample.peak_working_set_bytes, 700 * MIB);
    // Peaks never fall back when the instantaneous value drops.
    assert_eq!(sample.peak_pss_bytes, Some(400 * MIB));
    assert_eq!(sample.peak_instances, 2);
    assert_eq!(sample.peak_tabs, 5);
    let fields = gate.telemetry_json();
    assert_eq!(fields["browser_working_set_bytes"], 700 * MIB);
    assert_eq!(fields["peak_pss_bytes"], 400 * MIB);
    assert_eq!(telemetry_fields(&sample)["browser_tabs"], 5);
}
