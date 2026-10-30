//! Budget defaults and admission decisions.

use excalibur_browser_resources::admission::{
    AdmissionDecision, AdmissionRequest, OriginEstimator, decide, default_estimate_bytes,
    is_heavy_origin,
};
use excalibur_browser_resources::budgets::{
    ADMISSION_MARGIN_BYTES, DISK_RESERVE_BYTES, DiskBudgets, EPHEMERAL_CLOSE_PER_TAB_BYTES,
    EPHEMERAL_MAX_CONTEXTS, EPHEMERAL_MAX_TABS, GIB, HARD_GLOBAL_BYTES, MIB, MemoryBudgets,
    SOFT_GLOBAL_BYTES, SearchProfile, TABS_GLOBAL, TABS_PER_EXECUTION, WEBD_MAX_RSS_BYTES,
};

fn base_request() -> AdmissionRequest {
    AdmissionRequest {
        origin: "example.com",
        estimated_bytes: 200 * MIB,
        working_set_bytes: 500 * MIB,
        soft_budget_bytes: SOFT_GLOBAL_BYTES,
        psi_some_avg10_pct: None,
        execution_tabs: 0,
        global_tabs: 0,
        free_disk_bytes: 20 * GIB,
        growth_bytes: 0,
    }
}

#[test]
fn ram_defaults_match_architecture() {
    assert_eq!(SOFT_GLOBAL_BYTES, 1536 * MIB);
    assert_eq!(HARD_GLOBAL_BYTES, 2 * GIB);
    assert_eq!(
        MemoryBudgets::default().soft_global_bytes,
        SOFT_GLOBAL_BYTES
    );
    assert_eq!(TABS_PER_EXECUTION, 4);
    assert_eq!(TABS_GLOBAL, 12);
    assert_eq!(EPHEMERAL_MAX_CONTEXTS, 3);
    assert_eq!(EPHEMERAL_MAX_TABS, 3);
    assert_eq!(EPHEMERAL_CLOSE_PER_TAB_BYTES, 800 * MIB);
    assert_eq!(WEBD_MAX_RSS_BYTES, 60 * MIB);
    assert_eq!(DiskBudgets::default().reserve_bytes, DISK_RESERVE_BYTES);
    assert_eq!(SearchProfile::Minimo.combined_bytes(), 2 * GIB);
    assert_eq!(SearchProfile::Ampliado.combined_bytes(), 7 * GIB);
}

#[test]
fn heavy_origins_use_400_mib_default() {
    assert!(is_heavy_origin("web.whatsapp.com"));
    assert!(is_heavy_origin("teams.microsoft.com"));
    assert!(!is_heavy_origin("example.com"));
    assert_eq!(default_estimate_bytes("web.whatsapp.com"), 400 * MIB);
    assert_eq!(default_estimate_bytes("example.com"), 200 * MIB);
}

#[test]
fn estimator_tracks_moving_average() {
    let mut est = OriginEstimator::new();
    assert_eq!(est.estimate("example.com"), 200 * MIB);
    est.observe("example.com", 300 * MIB);
    assert_eq!(est.estimate("example.com"), 300 * MIB);
    est.observe("example.com", 500 * MIB);
    // Cumulative mean of 300 and 500 is 400.
    assert_eq!(est.estimate("example.com"), 400 * MIB);
}

#[test]
fn admit_when_estimate_plus_margin_fits() {
    let req = base_request();
    assert_eq!(decide(&req), AdmissionDecision::Admit);
}

#[test]
fn over_budget_waits_120s_with_breakdown() {
    let req = AdmissionRequest {
        working_set_bytes: 1400 * MIB,
        ..base_request()
    };
    match decide(&req) {
        AdmissionDecision::WaitWithTimeout {
            timeout_secs,
            breakdown,
        } => {
            assert_eq!(timeout_secs, 120);
            assert_eq!(breakdown.margin_bytes, ADMISSION_MARGIN_BYTES);
            assert_eq!(
                breakdown.projected_bytes,
                1400 * MIB + 200 * MIB + ADMISSION_MARGIN_BYTES
            );
            assert!(breakdown.projected_bytes > SOFT_GLOBAL_BYTES);
        }
        other => panic!("expected wait, got {other:?}"),
    }
}

#[test]
fn tab_limits_deny_without_wait() {
    let req = AdmissionRequest {
        execution_tabs: 4,
        ..base_request()
    };
    assert!(matches!(
        decide(&req),
        AdmissionDecision::Deny(
            excalibur_browser_resources::admission::DenyReason::TabLimit { .. }
        )
    ));
    let req = AdmissionRequest {
        global_tabs: 12,
        ..base_request()
    };
    assert!(matches!(
        decide(&req),
        AdmissionDecision::Deny(
            excalibur_browser_resources::admission::DenyReason::TabLimit { .. }
        )
    ));
}

#[test]
fn psi_at_20_denies() {
    let req = AdmissionRequest {
        psi_some_avg10_pct: Some(20.0),
        ..base_request()
    };
    assert!(matches!(
        decide(&req),
        AdmissionDecision::Deny(
            excalibur_browser_resources::admission::DenyReason::Pressure { .. }
        )
    ));
    let req = AdmissionRequest {
        psi_some_avg10_pct: Some(19.9),
        ..base_request()
    };
    assert_eq!(decide(&req), AdmissionDecision::Admit);
}

#[test]
fn disk_reserve_denies_growth() {
    let req = AdmissionRequest {
        free_disk_bytes: 6 * GIB,
        growth_bytes: 2 * GIB,
        ..base_request()
    };
    assert!(matches!(
        decide(&req),
        AdmissionDecision::Deny(
            excalibur_browser_resources::admission::DenyReason::DiskReserve { .. }
        )
    ));
}
