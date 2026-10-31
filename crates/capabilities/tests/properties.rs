//! Deterministic property checks for capability contracts.
//!
//! Closed-form unit assertions live in `lifecycle.rs`; these tests sweep
//! bounded deterministic inputs (fixed-seed splitmix64, no randomness
//! source, no network) over identifier stability, backoff shape, manifest
//! bounds, disclosure deny-by-default and application deltas/gates.

use excalibur_capabilities::{
    AcceptanceCheck, AppSnapshot, PollPolicy, app_state,
    disclosure::{Category, Commitment, DisclosurePolicy, Segment, filter_outbound},
    manifest::{CapabilityManifest, EffectClass, OperationSpec, ResourceLimits, Risk},
    tasks::{TaskState, task_id},
};
use serde_json::json;
use std::collections::{BTreeMap, BTreeSet};

fn splitmix64(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9E3779B97F4A7C15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D049BB133111EB);
    z ^ (z >> 31)
}

#[test]
fn task_id_is_stable_scoped_and_bounded() {
    let mut seed = 0x1234_5678_9ABC_DEF0;
    for _ in 0..256 {
        seed = splitmix64(&mut seed);
        let scope = format!("principal:p{}/project:q{}", seed % 97, seed % 13);
        let key = format!("op-{}", seed % 101);
        let first = task_id(&scope, &key);
        let second = task_id(&scope, &key);
        assert_eq!(first, second, "durable id is deterministic");
        assert!(first.starts_with("cap_"), "prefixed id");
        assert_eq!(first.len(), 36, "cap_ + 32 hex chars");
        assert!(first[4..].chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(task_id(&scope, &format!("{key}-other")), first);
        assert_ne!(task_id(&format!("{scope}-other"), &key), first);
    }
}

#[test]
fn backoff_is_monotone_capped_and_closed_form() {
    let policy = PollPolicy {
        default_interval_ms: 1_000,
        min_interval_ms: 100,
        max_interval_ms: 60_000,
        backoff_base_ms: 1_000,
        backoff_max_ms: 16_000,
        max_transient_failures: 4,
        in_flight_lease_ms: 30_000,
    };
    assert_eq!(policy.backoff(1), 1_000);
    assert_eq!(policy.backoff(2), 2_000);
    assert_eq!(policy.backoff(3), 4_000);
    assert_eq!(policy.backoff(4), 8_000);
    assert_eq!(policy.backoff(5), 16_000);
    let mut prev = 0;
    for n in 1..=64 {
        let delay = policy.backoff(n);
        assert!(delay >= prev, "backoff never decreases");
        assert!(delay <= 16_000, "backoff is capped");
        prev = delay;
    }
    assert_eq!(policy.backoff(60), 16_000);
}

#[test]
fn manifest_rejects_effects_without_idempotency_keys() {
    let operation = |effect: EffectClass, idempotent: bool| OperationSpec {
        name: "op".into(),
        input_schema: json!({"type": "object"}),
        output_schema: json!({"type": "object"}),
        effect,
        idempotent,
        risk: Risk::Low,
        long_running: true,
    };
    let base = || CapabilityManifest {
        id: "cap".into(),
        version: "1.0.0".into(),
        operations: vec![operation(EffectClass::ReadOnly, false)],
        platforms: vec!["linux".into()],
        deployment_profiles: vec!["local".into()],
        resources: ResourceLimits {
            approx_ram_mb: 64,
            max_sessions: 1,
        },
        max_concurrency: 1,
        states: vec![TaskState::Working, TaskState::Completed],
    };
    assert!(base().validate().is_ok());
    let mut write_without_key = base();
    write_without_key.operations = vec![operation(EffectClass::Write, false)];
    assert!(write_without_key.validate().is_err());
    let mut external_without_key = base();
    external_without_key.operations = vec![operation(EffectClass::External, false)];
    assert!(external_without_key.validate().is_err());
    let mut write_with_key = base();
    write_with_key.operations = vec![operation(EffectClass::Write, true)];
    assert!(write_with_key.validate().is_ok());
}

#[test]
fn disclosure_denies_by_default_and_never_shares_credentials() {
    let mut seed = 0xDEAD_BEEF_1234_5678;
    let all = [
        Category::General,
        Category::Availability,
        Category::Preferences,
        Category::ContactDetails,
        Category::Location,
        Category::PrivateData,
        Category::Memory,
        Category::SensitiveDocument,
        Category::Credential,
    ];
    for _ in 0..128 {
        let allowed: BTreeSet<Category> = all
            .iter()
            .copied()
            .filter(|_| splitmix64(&mut seed).is_multiple_of(2))
            .collect();
        let policies = vec![DisclosurePolicy {
            contact: "juan".into(),
            purpose: "plan".into(),
            allowed: allowed.clone(),
            authorized_commitments: BTreeSet::new(),
        }];
        let segments: Vec<Segment> = all
            .iter()
            .copied()
            .map(|category| Segment {
                category,
                text: "x".into(),
                commitment: None,
            })
            .collect();
        let decision = filter_outbound(&policies, "juan", "plan", segments);
        for sent in &decision.send {
            assert!(
                allowed.contains(&sent.category),
                "only allowed categories are sent"
            );
            assert_ne!(
                sent.category,
                Category::Credential,
                "credentials never leave"
            );
        }
        let expects_permission = all
            .iter()
            .any(|c| *c != Category::Credential && !allowed.contains(c));
        assert_eq!(decision.requires_permission, expects_permission);
        // Unknown contact/purpose withholds everything except credentials flag rule.
        let denied = filter_outbound(
            &policies,
            "unknown",
            "unknown",
            vec![Segment {
                category: Category::General,
                text: "hi".into(),
                commitment: None,
            }],
        );
        assert!(denied.send.is_empty());
        assert!(denied.requires_permission);
    }
    // Commitments need explicit authorization even when the category is allowed.
    let policies = vec![DisclosurePolicy {
        contact: "juan".into(),
        purpose: "plan".into(),
        allowed: BTreeSet::from([Category::General]),
        authorized_commitments: BTreeSet::new(),
    }];
    let decision = filter_outbound(
        &policies,
        "juan",
        "plan",
        vec![Segment {
            category: Category::General,
            text: "pay".into(),
            commitment: Some(Commitment::Payment),
        }],
    );
    assert!(decision.send.is_empty());
    assert!(decision.requires_permission);
    let authorized = vec![DisclosurePolicy {
        contact: "juan".into(),
        purpose: "plan".into(),
        allowed: BTreeSet::from([Category::General]),
        authorized_commitments: BTreeSet::from([Commitment::Payment]),
    }];
    let decision = filter_outbound(
        &authorized,
        "juan",
        "plan",
        vec![Segment {
            category: Category::General,
            text: "pay".into(),
            commitment: Some(Commitment::Payment),
        }],
    );
    assert_eq!(decision.send.len(), 1);
    assert!(!decision.requires_permission);
}

#[test]
fn app_delta_round_trip_and_gate_properties() {
    // Empty delta for identical snapshots; changed/removed keys are exact.
    let mut seed = 0xCAFE_F00D_0000_0001;
    for i in 0..64u64 {
        let before =
            AppSnapshot::initial("sheet-1", json!({"a": i, "b": "keep", "c": seed % 5})).unwrap();
        let same = before
            .next(json!({"a": i, "b": "keep", "c": seed % 5}))
            .unwrap();
        let empty = app_state::delta(&before, &same).unwrap();
        assert!(empty.is_empty(), "identical content has an empty delta");
        seed = splitmix64(&mut seed);

        let after = before
            .next(json!({"a": i + 1, "b": "keep", "extra": true}))
            .unwrap();
        let delta = app_state::delta(&before, &after).unwrap();
        assert!(!delta.is_empty());
        assert_eq!(delta.from_rev, before.rev);
        assert_eq!(delta.to_rev, after.rev);
        let mut expected_changed = BTreeMap::new();
        expected_changed.insert("a".to_string(), json!(i + 1));
        expected_changed.insert("extra".to_string(), json!(true));
        assert_eq!(delta.changed, expected_changed);
        assert_eq!(delta.removed, vec!["c".to_string()]);
        let summary = delta.summary();
        assert_eq!(summary["instance"], "sheet-1");
        assert_eq!(summary["changed_keys"], json!(["a", "extra"]));
        assert_eq!(summary["removed_keys"], json!(["c"]));
    }
    // Revision is monotone and cross-instance deltas are rejected.
    let first = AppSnapshot::initial("app", json!({})).unwrap();
    let second = first.next(json!({"x": 1})).unwrap();
    assert!(second.rev > first.rev);
    let other = AppSnapshot::initial("other", json!({})).unwrap();
    assert!(app_state::delta(&first, &other).is_err());
    assert!(
        app_state::delta(&second, &first).is_err(),
        "no backwards deltas"
    );
    assert!(AppSnapshot::initial("", json!({})).is_err());
    assert!(AppSnapshot::initial("app", json!([1, 2])).is_err());

    // Acceptance gate: optional failures never block, required ones do.
    let checks = vec![
        AcceptanceCheck::new("quiz-all-correct", "every answer correct", true, false).unwrap(),
        AcceptanceCheck::new("style", "optional polish", false, false).unwrap(),
    ];
    assert!(!app_state::gate_allows(&checks));
    assert_eq!(
        app_state::gate_blockers(&checks),
        vec!["quiz-all-correct".to_string()]
    );
    let passing = vec![
        AcceptanceCheck::new("quiz-all-correct", "every answer correct", true, true).unwrap(),
        AcceptanceCheck::new("style", "optional polish", false, false).unwrap(),
    ];
    assert!(app_state::gate_allows(&passing));
    assert!(app_state::gate_blockers(&passing).is_empty());
    assert_eq!(
        app_state::required_ids(&passing),
        vec!["quiz-all-correct".to_string()]
    );
}
