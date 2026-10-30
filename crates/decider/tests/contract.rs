use excalibur_decider::*;
use excalibur_types::runtime::BoxFuture;
use std::{sync::Arc, time::Duration};
struct Mock {
    refuse: bool,
}
impl Decider for Mock {
    fn backend(&self) -> &str {
        "local-mock"
    }
    fn decide<'a>(&'a self, request: &'a DecisionRequest) -> BoxFuture<'a, Result<Decision>> {
        Box::pin(async move {
            Ok(Decision {
                answers: request
                    .questions
                    .iter()
                    .map(|q| {
                        if self.refuse {
                            Answer::Refusal {
                                name: q.name().into(),
                            }
                        } else {
                            Answer::Predicate {
                                name: q.name().into(),
                                probability: 0.99,
                            }
                        }
                    })
                    .collect(),
                backend: self.backend().into(),
                model: None,
                usage: Default::default(),
                fallback_questions: vec![],
            })
        })
    }
}
fn request() -> DecisionRequest {
    DecisionRequest {
        port: DecisionPort::SkillRelevance,
        input: DecisionInput::Text("debugging task".into()),
        questions: vec![Question::predicate("relevant", "Is debugging relevant?")],
    }
}
#[test]
fn final_verdict_is_forbidden_and_invalid_probabilities_fail() {
    assert_eq!(
        DecisionPort::parse("final_correctness_verdict"),
        Err(DeciderError::FinalVerdictForbidden)
    );
    let decision = Decision {
        answers: vec![Answer::Predicate {
            name: "relevant".into(),
            probability: f64::NAN,
        }],
        backend: "mock".into(),
        model: None,
        usage: Default::default(),
        fallback_questions: vec![],
    };
    assert!(decision.validate_for(&request()).is_err());
}
#[tokio::test]
async fn refusals_fall_back_and_decisions_are_labeled_durably() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("decisions.db");
    let ledger = DecisionLedger::open(&path).await.unwrap();
    let fallback = FallbackDecider::new(
        Arc::new(Mock { refuse: true }),
        Arc::new(Mock { refuse: false }),
        Duration::from_secs(1),
    );
    let service = DecisionService::new(Arc::new(fallback))
        .with_ledger(ledger.clone())
        .await
        .unwrap();
    let routed = service.decide(&request()).await.unwrap();
    assert_eq!(routed.fallback_questions, vec!["relevant"]);
    assert_eq!(routed.answers[0].verdict, Verdict::Act);
    let id = routed.answers[0].record_id.as_ref().unwrap();
    ledger
        .label(id, true, LabelSource::TaskOutcome)
        .await
        .unwrap();
    let samples = ledger
        .labeled(DecisionPort::SkillRelevance, "relevant")
        .await
        .unwrap();
    assert_eq!(samples.len(), 1);
    assert!(samples[0].outcome);
    ledger.shutdown().await.unwrap();
    drop(service);
    drop(ledger);
    let reopened = DecisionLedger::open(&path).await.unwrap();
    assert_eq!(
        reopened
            .labeled(DecisionPort::SkillRelevance, "relevant")
            .await
            .unwrap()
            .len(),
        1
    );
    reopened.shutdown().await.unwrap();
}
#[test]
fn calibration_reports_real_error_and_abstention() {
    let samples = [
        LabeledOutcome {
            probability: 0.99,
            outcome: true,
        },
        LabeledOutcome {
            probability: 0.01,
            outcome: false,
        },
        LabeledOutcome {
            probability: 0.5,
            outcome: true,
        },
        LabeledOutcome {
            probability: 0.5,
            outcome: false,
        },
    ];
    let report = calibrate(&samples, 0.9, 1).unwrap();
    assert_eq!(report.samples, 4);
    assert!(report.brier > 0.0 && report.brier < 1.0);
    assert!(report.thresholds.calibrated);
    assert_eq!(report.thresholds.verdict(Some(0.5)), Verdict::Defer);
}

#[test]
fn contradictory_confidence_and_duplicate_choices_are_rejected() {
    let request = DecisionRequest::new(
        DecisionPort::Triage,
        DecisionInput::Text("classify".into()),
        vec![Question::choice("kind", "Choose", &["task", "chat"])],
    );
    let mut decision = Decision {
        answers: vec![Answer::Choice {
            name: "kind".into(),
            choice: "task".into(),
            confidence: 0.99,
            probabilities: vec![
                OptionProbability {
                    value: "task".into(),
                    probability: 0.1,
                },
                OptionProbability {
                    value: "chat".into(),
                    probability: 0.9,
                },
            ],
        }],
        backend: "mock".into(),
        model: None,
        usage: Default::default(),
        fallback_questions: vec![],
    };
    assert!(decision.validate_for(&request).is_err());
    decision.answers = vec![Answer::Choice {
        name: "kind".into(),
        choice: "task".into(),
        confidence: 0.5,
        probabilities: vec![
            OptionProbability {
                value: "task".into(),
                probability: 0.5,
            },
            OptionProbability {
                value: "task".into(),
                probability: 0.5,
            },
        ],
    }];
    assert!(decision.validate_for(&request).is_err());
}
