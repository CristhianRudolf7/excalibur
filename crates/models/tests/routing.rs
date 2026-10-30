use excalibur_models::routing::*;
fn config() -> RoleConfig {
    RoleConfig {
        roles: Role::REQUIRED
            .into_iter()
            .map(|r| {
                (
                    r,
                    RoleBinding {
                        primary: ModelBinding {
                            provider: "mock".into(),
                            model: format!("{r:?}"),
                            family: "mock".into(),
                            reasoning_effort: None,
                        },
                        fallbacks: vec![],
                    },
                )
            })
            .collect(),
    }
}
#[test]
fn escalation_keeps_executor_stable_and_consumes_evidence() {
    let mut router = Router::new(config(), Role::Executor).unwrap();
    router.observe_test("regression", false);
    router.observe_test("regression", false);
    assert!(matches!(router.next_step(), RouteDecision::Default { .. }));
    router.observe_test("regression", false);
    let RouteDecision::ConsultExpert { model, signals, .. } = router.next_step() else {
        panic!("expected expert");
    };
    assert_eq!(model.model, "Executor");
    assert_eq!(signals.len(), 1);
    assert!(matches!(router.next_step(), RouteDecision::Default { .. }));
    router.observe_resolution();
    assert!(!router.escalated());
}
#[test]
fn compact_brief_bounds_json_escape_expansion_and_signals() {
    let huge = "\u{0001}\"\\".repeat(100_000);
    let brief = ExpertBrief {
        goal: huge.clone(),
        state: huge.clone(),
        exact_error: huge.clone(),
        attempts: vec![huge.clone(); 20],
        fragments: vec![
            BriefFragment {
                label: huge.clone(),
                content: huge.clone()
            };
            50
        ],
        signals: vec![
            EscalationSignal::PersistentTestFailure {
                test: huge,
                attempts: 50
            };
            100
        ],
    };
    assert!(brief.compact().to_string().len() <= MAX_BRIEF_BYTES);
    assert!(!brief.request().tools_enabled);
    assert_eq!(brief.request().messages.len(), 2);
}
