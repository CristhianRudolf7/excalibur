//! Resource/broker trait surface (B9 wire points, no cross-crate edits).

use excalibur_search::index::DocSource;
use excalibur_search::wire::{
    AllowAllGate, BudgetProvider, DenyAllGate, FetchGate, HostProxyWire, is_low_priority_source,
};
use excalibur_search::{SearchConfig, SearchProfile};

#[test]
fn budgets_mirror_architecture_profiles() {
    assert_eq!(SearchProfile::Minimo.index_budget_bytes(), 1_610_612_736);
    assert_eq!(SearchProfile::Ampliado.index_budget_bytes(), 6_442_450_944);
    assert_eq!(SearchProfile::Minimo.state_budget_bytes(), 536_870_912);
    let config = SearchConfig {
        profile: SearchProfile::Ampliado,
        ..SearchConfig::default()
    };
    assert_eq!(config.index_budget_bytes(), 6_442_450_944);
}

#[test]
fn low_priority_sources_stop_first() {
    assert!(is_low_priority_source(DocSource::Web));
    assert!(is_low_priority_source(DocSource::News));
    assert!(is_low_priority_source(DocSource::Hn));
    assert!(!is_low_priority_source(DocSource::Wiki));
    assert!(!is_low_priority_source(DocSource::Docs));
    assert!(!is_low_priority_source(DocSource::Arxiv));
    assert!(!is_low_priority_source(DocSource::Read));
}

#[test]
fn fetch_gates_fail_closed() {
    assert!(AllowAllGate.allow_fetch("a.example", "https://a.example/p"));
    assert!(!DenyAllGate.allow_fetch("a.example", "https://a.example/p"));
    let empty = HostProxyWire::default();
    assert!(!empty.allow_fetch("a.example", "https://a.example/p"));
    let wire = HostProxyWire::new(&["docs.example"]);
    assert!(wire.allow_fetch("docs.example", "https://docs.example/p"));
    assert!(!wire.allow_fetch("other.example", "https://other.example/p"));
}
