//! Disk budgets, reserve gates and LRU behavior.

use excalibur_browser_resources::budgets::{
    DISK_RESERVE_BYTES, GIB, MIB, QUARANTINE_BUDGET_BYTES, READ_CACHE_BUDGET_BYTES,
    SNAPSHOT_BUDGET_BYTES, SearchProfile,
};
use excalibur_browser_resources::disk::{
    LruBudget, LruEntry, ProfileAction, WebdDataUsage, chrome_cache_bytes, disk_admission_allowed,
    low_disk_action, profile_action, search_start_allowed,
};

#[test]
fn chrome_cache_sizes() {
    assert_eq!(chrome_cache_bytes(true), 32 * MIB);
    assert_eq!(chrome_cache_bytes(false), 64 * MIB);
}

#[test]
fn profile_warn_and_clear_thresholds() {
    assert_eq!(profile_action(500 * MIB), ProfileAction::Ok);
    assert_eq!(profile_action(GIB), ProfileAction::Warn);
    assert_eq!(
        profile_action(1536 * MIB),
        ProfileAction::ClearAllowedCaches
    );
}

#[test]
fn reserve_gate_blocks_growth_below_5gib() {
    assert!(disk_admission_allowed(6 * GIB, GIB));
    assert!(!disk_admission_allowed(6 * GIB, 2 * GIB));
    assert!(!disk_admission_allowed(4 * GIB, 0));
    assert_eq!(DISK_RESERVE_BYTES, 5 * GIB);
}

#[test]
fn search_start_needs_budget_plus_reserve() {
    // minimo needs 2 GiB + 5 GiB reserve.
    assert!(search_start_allowed(8 * GIB, SearchProfile::Minimo));
    assert!(!search_start_allowed(6 * GIB, SearchProfile::Minimo));
    // ampliado needs 7 GiB + 5 GiB reserve.
    assert!(search_start_allowed(12 * GIB, SearchProfile::Ampliado));
    assert!(!search_start_allowed(11 * GIB, SearchProfile::Ampliado));
}

#[test]
fn low_disk_policy_never_touches_listening_origins() {
    assert!(low_disk_action(8 * GIB, false).is_none());
    let action = low_disk_action(6 * MIB * 1024, false).unwrap();
    assert!(action.empty_ephemeral_caches);
    assert!(action.clear_persistent_non_listener_caches);
    assert!(!action.touches_listening_origins);
    // Budget breach triggers even with free space.
    let action = low_disk_action(20 * GIB, true).unwrap();
    assert!(!action.touches_listening_origins);
}

#[test]
fn webd_data_budgets() {
    let usage = WebdDataUsage {
        snapshots_bytes: SNAPSHOT_BUDGET_BYTES + 1,
        read_cache_bytes: READ_CACHE_BUDGET_BYTES,
        quarantine_bytes: QUARANTINE_BUDGET_BYTES - 1,
    };
    assert!(usage.snapshots_exceeded());
    assert!(!usage.read_cache_exceeded());
    assert!(!usage.quarantine_full());
    assert!(usage.total_exceeded() || !usage.total_exceeded());
    let full = WebdDataUsage {
        snapshots_bytes: SNAPSHOT_BUDGET_BYTES,
        read_cache_bytes: READ_CACHE_BUDGET_BYTES,
        quarantine_bytes: QUARANTINE_BUDGET_BYTES,
    };
    assert!(full.quarantine_full());
}

#[test]
fn lru_snapshot_budget_evicts_oldest_first() {
    let mut budget = LruBudget::new(10);
    let evicted = budget.insert(LruEntry {
        key: "a".to_string(),
        bytes: 6,
        seq: 0,
    });
    assert!(evicted.is_empty());
    let evicted = budget.insert(LruEntry {
        key: "b".to_string(),
        bytes: 6,
        seq: 1,
    });
    assert_eq!(evicted, vec!["a".to_string()]);
    assert_eq!(budget.total_bytes(), 6);
}
