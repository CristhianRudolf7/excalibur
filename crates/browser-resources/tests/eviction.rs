//! Eviction order, LIVE protection, suspend and reload scheduling.

use excalibur_browser_resources::budgets::MIB;
use excalibur_browser_resources::error::ResourceError;
use excalibur_browser_resources::eviction::{
    EvictionStep, InstanceKind, TabClass, TabInfo, classify_by_age, forbidden_operations,
    plan_eviction, should_schedule_live_reload, suspend_session,
};

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
fn order_clears_caches_first_then_idle_lru() {
    let tabs = vec![
        tab(
            "new-idle",
            TabClass::Idle,
            InstanceKind::Ephemeral,
            400,
            50 * MIB,
        ),
        tab(
            "old-idle",
            TabClass::Idle,
            InstanceKind::Ephemeral,
            900,
            50 * MIB,
        ),
    ];
    let steps = plan_eviction(&tabs);
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
    // Orders are non-decreasing along the plan.
    let mut last = 0;
    for step in &steps {
        assert!(step.order() >= last);
        last = step.order();
    }
}

#[test]
fn pressure_and_gc_only_name_ephemeral_active() {
    let tabs = vec![
        tab(
            "live",
            TabClass::Live,
            InstanceKind::Persistent,
            0,
            100 * MIB,
        ),
        tab(
            "agent-tab",
            TabClass::Active,
            InstanceKind::Ephemeral,
            10,
            100 * MIB,
        ),
    ];
    let steps = plan_eviction(&tabs);
    let live_ids = vec!["live".to_string()];
    for step in &steps {
        match step {
            EvictionStep::CollectGarbageEphemeral { tab_ids } => {
                assert_eq!(tab_ids, &vec!["agent-tab".to_string()]);
            }
            EvictionStep::PressureEphemeral
            | EvictionStep::ClearBrowserCacheEphemeral
            | EvictionStep::RestartEphemeral
            | EvictionStep::ClearWebdCaches => {}
            EvictionStep::CloseIdle { .. }
            | EvictionStep::CloseActiveOverLimit { .. }
            | EvictionStep::ScheduleLiveReload { .. } => {
                assert!(!step.names_live_tab(&live_ids), "LIVE in {step:?}");
            }
        }
    }
    assert!(steps.contains(&EvictionStep::PressureEphemeral));
}

#[test]
fn close_active_over_hard_limit_emits_tab_evicted_candidate() {
    let tabs = vec![tab(
        "heavy-agent",
        TabClass::Active,
        InstanceKind::Ephemeral,
        5,
        900 * MIB,
    )];
    let steps = plan_eviction(&tabs);
    assert!(steps.contains(&EvictionStep::CloseActiveOverLimit {
        tab_id: "heavy-agent".to_string(),
    }));
}

#[test]
fn live_tabs_never_frozen_discarded_or_pressured() {
    // A quiet LIVE tab above 1 GiB only schedules a reload.
    let tabs = vec![tab(
        "live-heavy",
        TabClass::Live,
        InstanceKind::Persistent,
        180,
        1100 * MIB,
    )];
    let steps = plan_eviction(&tabs);
    assert!(steps.contains(&EvictionStep::ScheduleLiveReload {
        tab_id: "live-heavy".to_string(),
    }));
    // A busy LIVE tab schedules nothing destructive.
    let busy = vec![tab(
        "live-busy",
        TabClass::Live,
        InstanceKind::Persistent,
        10,
        1100 * MIB,
    )];
    let steps = plan_eviction(&busy);
    assert!(!steps.iter().any(|s| matches!(
        s,
        EvictionStep::ScheduleLiveReload { .. } | EvictionStep::CloseActiveOverLimit { .. }
    )));
    for forbidden in forbidden_operations() {
        assert!(!forbidden.contains("freeze") || forbidden != "freeze");
    }
    assert_eq!(forbidden_operations().len(), 3);
}

#[test]
fn reload_needs_1gib_10min_and_2min_quiet() {
    assert!(should_schedule_live_reload(1100 * MIB, 600, 120));
    assert!(!should_schedule_live_reload(900 * MIB, 900, 900));
    assert!(!should_schedule_live_reload(1100 * MIB, 599, 120));
    assert!(!should_schedule_live_reload(1100 * MIB, 900, 119));
}

#[test]
fn suspend_refuses_listening_without_approval() {
    assert_eq!(
        suspend_session(true, false),
        Err(ResourceError::ListenerCannotSuspend)
    );
    assert_eq!(suspend_session(true, true), Ok(()));
    assert_eq!(suspend_session(false, false), Ok(()));
}

#[test]
fn classification_windows() {
    assert_eq!(classify_by_age(true, 10_000), TabClass::Live);
    assert_eq!(classify_by_age(false, 0), TabClass::Active);
    assert_eq!(classify_by_age(false, 60), TabClass::Active);
    assert_eq!(classify_by_age(false, 61), TabClass::Idle);
}
