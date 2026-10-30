use excalibur_decider::*;
use excalibur_models::{GatewayMetadata, ModelError, ModelGateway};
use excalibur_types::runtime::{BoxFuture, Message, ModelRequest, ModelResponse, Usage};
use serde_json::{Value, json};
use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
    time::Duration,
};
struct Gateway {
    model: String,
    backend: String,
    outputs: Mutex<VecDeque<String>>,
    calls: Mutex<Vec<ModelRequest>>,
}
impl Gateway {
    fn new(model: &str, outputs: Vec<Value>) -> Arc<Self> {
        Arc::new(Self {
            model: model.into(),
            backend: "fixture-config".into(),
            outputs: Mutex::new(outputs.into_iter().map(|v| v.to_string()).collect()),
            calls: Mutex::new(vec![]),
        })
    }
}
impl ModelGateway for Gateway {
    fn metadata(&self) -> GatewayMetadata {
        GatewayMetadata {
            model: Some(self.model.clone()),
            configuration_hash: self.backend.clone(),
            ..Default::default()
        }
    }
    fn complete<'a>(
        &'a self,
        request: ModelRequest,
    ) -> BoxFuture<'a, std::result::Result<ModelResponse, ModelError>> {
        Box::pin(async move {
            self.calls.lock().unwrap().push(request);
            let output = self
                .outputs
                .lock()
                .unwrap()
                .pop_front()
                .expect("unexpected gateway call");
            Ok(ModelResponse {
                message: Message::text("assistant", output),
                usage: Usage {
                    input_tokens: 3,
                    output_tokens: 2,
                    cost_microusd: 0,
                    estimated: false,
                },
            })
        })
    }
}
fn request() -> DecisionRequest {
    DecisionRequest::new(
        DecisionPort::SkillRelevance,
        DecisionInput::Text("local fixture".into()),
        vec![Question::predicate("relevant", "Is this relevant?")],
    )
}
fn answer(name: &str, probability: f64) -> Value {
    json!({"name":name,"refused":false,"distribution":[{"value":"true","probability":probability},{"value":"false","probability":1.0-probability}]})
}
fn output(answer: Value) -> Value {
    json!({"answers":[answer]})
}
#[tokio::test]
async fn malformed_llm_values_retry_once_then_fail() {
    let mut cases = vec![];
    for probability in [Value::Null, json!("bad"), json!(-1), json!(2)] {
        let mut bad = answer("relevant", 0.5);
        bad["distribution"][0]["probability"] = probability;
        cases.push(bad);
    }
    let mut missing = answer("relevant", 0.5);
    missing["distribution"][0]
        .as_object_mut()
        .unwrap()
        .remove("probability");
    cases.push(missing);
    let mut sum = answer("relevant", 0.5);
    sum["distribution"][1]["probability"] = json!(0.1);
    cases.push(sum);
    let mut refused = answer("relevant", 0.5);
    refused["refused"] = json!("false");
    cases.push(refused);
    let mut missing_refused = answer("relevant", 0.5);
    missing_refused.as_object_mut().unwrap().remove("refused");
    cases.push(missing_refused);
    let mut duplicate = answer("relevant", 0.5);
    duplicate["distribution"][1]["value"] = json!("true");
    cases.push(duplicate);
    for bad in cases {
        let gateway = Gateway::new("model-a", vec![output(bad.clone()), output(bad)]);
        assert!(
            LlmDecider::new(gateway.clone())
                .decide(&request())
                .await
                .is_err()
        );
        let calls = gateway.calls.lock().unwrap();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[1].messages.len(), 4);
        assert!(calls.iter().all(|c| !c.tools_enabled));
    }
}
#[tokio::test]
async fn retry_accepts_corrected_output_and_failure_uses_real_fallback() {
    let bad = json!({"name":"relevant","refused":false,"distribution":[]});
    let gateway = Gateway::new(
        "model-a",
        vec![output(bad.clone()), output(answer("relevant", 0.7))],
    );
    let result = LlmDecider::new(gateway.clone())
        .decide(&request())
        .await
        .unwrap();
    assert_eq!(result.answers[0].probability(), Some(0.7));
    assert_eq!(result.usage.input_tokens, 6);
    let primary = Gateway::new("model-a", vec![output(bad.clone()), output(bad)]);
    let backup = Gateway::new("model-b", vec![output(answer("relevant", 0.8))]);
    let decider = FallbackDecider::new(
        Arc::new(LlmDecider::new(primary.clone())),
        Arc::new(LlmDecider::new(backup.clone())),
        Duration::from_secs(1),
    );
    let result = decider.decide(&request()).await.unwrap();
    assert_eq!(result.model.as_deref(), Some("model-b"));
    assert_eq!(primary.calls.lock().unwrap().len(), 2);
    assert_eq!(backup.calls.lock().unwrap().len(), 1);
}
#[tokio::test]
async fn partial_refusal_preserves_each_answer_origin_in_the_ledger() {
    let mut request = request();
    request
        .questions
        .push(Question::predicate("other", "Is the other relevant?"));
    let primary = Gateway::new(
        "model-a",
        vec![
            json!({"answers":[answer("relevant", 0.7), {"name":"other","refused":true,"distribution":[]}]}),
        ],
    );
    let backup = Gateway::new("model-b", vec![output(answer("other", 0.8))]);
    let decider = FallbackDecider::new(
        Arc::new(LlmDecider::new(primary)),
        Arc::new(LlmDecider::new(backup)),
        Duration::from_secs(1),
    );
    let root = tempfile::tempdir().unwrap();
    let ledger = DecisionLedger::open(root.path().join("decisions.db"))
        .await
        .unwrap();
    let service = DecisionService::new(Arc::new(decider))
        .with_ledger(ledger.clone())
        .await
        .unwrap();
    let result = service.decide(&request).await.unwrap();
    assert_eq!(result.answers[0].origin.model.as_deref(), Some("model-a"));
    assert_eq!(result.answers[1].origin.model.as_deref(), Some("model-b"));
    let rows = ledger.records(request.port).await.unwrap();
    assert!(
        rows.iter().any(
            |r| r.calibration_key.as_ref().unwrap().origin.model.as_deref() == Some("model-b")
        )
    );
    ledger.shutdown().await.unwrap();
}
#[tokio::test]
async fn thresholds_do_not_cross_model_or_question_semantics() {
    let root = tempfile::tempdir().unwrap();
    let ledger = DecisionLedger::open(root.path().join("decisions.db"))
        .await
        .unwrap();
    let gateway = Gateway::new(
        "model-a",
        vec![
            output(answer("relevant", 0.7)),
            output(answer("relevant", 0.7)),
            output(answer("relevant", 0.7)),
        ],
    );
    let service = DecisionService::new(Arc::new(LlmDecider::new(gateway)))
        .with_ledger(ledger.clone())
        .await
        .unwrap();
    let first = service.decide(&request()).await.unwrap();
    let key = first.answers[0].calibration_key.clone();
    ledger
        .label(
            first.answers[0].record_id.as_ref().unwrap(),
            true,
            LabelSource::Review,
        )
        .await
        .unwrap();
    service.calibrate(&key, 0.9, 1).await.unwrap();
    assert_eq!(
        service.decide(&request()).await.unwrap().answers[0].verdict,
        Verdict::Act
    );
    let mut changed = request();
    changed.questions[0] =
        Question::predicate("relevant", "Is this irrelevant? Different semantics.");
    let changed = service.decide(&changed).await.unwrap();
    assert!(!changed.answers[0].thresholds.calibrated);
    assert_eq!(changed.answers[0].verdict, Verdict::Defer);
    let other = DecisionService::new(Arc::new(LlmDecider::new(Gateway::new(
        "model-b",
        vec![output(answer("relevant", 0.7))],
    ))))
    .with_ledger(ledger.clone())
    .await
    .unwrap();
    let other = other.decide(&request()).await.unwrap();
    assert!(!other.answers[0].thresholds.calibrated);
    assert_eq!(other.answers[0].verdict, Verdict::Defer);
    let mut gateway = Gateway::new("model-a", vec![output(answer("relevant", 0.7))]);
    Arc::get_mut(&mut gateway).unwrap().backend = "other-backend".into();
    let other_backend = DecisionService::new(Arc::new(LlmDecider::new(gateway)))
        .with_ledger(ledger.clone())
        .await
        .unwrap();
    let other_backend = other_backend.decide(&request()).await.unwrap();
    assert!(!other_backend.answers[0].thresholds.calibrated);
    assert_eq!(other_backend.answers[0].verdict, Verdict::Defer);
    ledger.shutdown().await.unwrap();
}
#[test]
fn descriptions_and_total_request_size_are_bounded() {
    let mut request = request();
    request.questions = vec![Question::choice("kind", "choose", &["a", "b"])];
    if let Question::Choice { choices, .. } = &mut request.questions[0] {
        choices[0].description = Some("x".repeat(MAX_INSTRUCTIONS_BYTES + 1));
    }
    assert!(request.validate().is_err());
    request.questions = vec![Question::score("risk", "score", &["low", "high"])];
    if let Question::Score { levels, .. } = &mut request.questions[0] {
        levels[0].description = Some("x".repeat(MAX_INSTRUCTIONS_BYTES + 1));
    }
    assert!(request.validate().is_err());
}
