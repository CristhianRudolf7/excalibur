use excalibur_eval::{ExecOptions, Execution};
use excalibur_journal::instructions::{
    InstructionOrigin, InstructionOriginKind, InstructionScope, InstructionStore,
};
use excalibur_models::{ModelError, ModelGateway};
use excalibur_tasks::VerificationInput;
use excalibur_types::runtime::{BoxFuture, Message, ModelRequest, ModelResponse, Usage};
use excalibur_vault::Redactor;
use serde_json::json;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
const GENERAL: &str = "  Exact general instruction.\nPreserva ñ 🦀.\n";
const TASK: &str = "Exact task instruction: use synthetic data only.\n";
const RETIRED: &str = "Retired registry instruction must never be delegated.";
const FOREIGN: &str = "Another task's private scoped instruction.";
struct RegistryReviewer {
    calls: AtomicUsize,
}
impl ModelGateway for RegistryReviewer {
    fn complete<'a>(
        &'a self,
        request: ModelRequest,
    ) -> BoxFuture<'a, Result<ModelResponse, ModelError>> {
        Box::pin(async move {
            let verifier = request.messages.first().is_some_and(|message| {
                message
                    .content
                    .contains("You are the independent task verifier")
            });
            if verifier {
                self.calls.fetch_add(1, Ordering::SeqCst);
                let payload: serde_json::Value =
                    serde_json::from_str(&request.messages[1].content).unwrap();
                let input: VerificationInput =
                    serde_json::from_value(payload["request"].clone()).unwrap();
                assert!(input.applicable_instructions.iter().any(|instruction|instruction.text==GENERAL&&instruction.revision==2));
                assert!(
                    input
                        .applicable_instructions
                        .iter()
                        .any(|instruction| instruction.text == TASK && instruction.revision == 1)
                );
                assert!(
                    !input
                        .applicable_instructions
                        .iter()
                        .any(|instruction| instruction.text == RETIRED
                            || instruction.text == FOREIGN)
                );
                let criteria=input.criteria.iter().map(|criterion|{
      let ids=payload["independent_evidence"].as_array().unwrap().iter().filter(|e|e["criterion_id"]==criterion.id).map(|e|e["id"].clone()).collect::<Vec<_>>();
      json!({"criterion_id":criterion.id,"passed":true,"explanation":"Independent evidence matches the literal instructions","evidence_ids":ids})
    }).collect::<Vec<_>>();
                return Ok(ModelResponse {
                    message: Message::text(
                        "assistant",
                        json!({"criteria":criteria,"uncovered_requests":[],"request_covered":true})
                            .to_string(),
                    ),
                    usage: Usage {
                        input_tokens: 11,
                        output_tokens: 7,
                        ..Default::default()
                    },
                });
            }
            Ok(ModelResponse {
                message: Message::text("assistant", "Synthetic reviewed answer."),
                usage: Usage {
                    input_tokens: 9,
                    output_tokens: 5,
                    ..Default::default()
                },
            })
        })
    }
}
fn origin() -> InstructionOrigin {
    InstructionOrigin {
        kind: InstructionOriginKind::Host,
        reference: "authenticated synthetic host".into(),
    }
}
#[tokio::test]
async fn actual_gateway_verifier_receives_initial_registry_literals_and_excludes_retired_or_foreign_scopes()
 {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("workspace");
    std::fs::create_dir(&root).unwrap();
    let model = Arc::new(RegistryReviewer {
        calls: AtomicUsize::new(0),
    });
    let execution = Execution::prepare(
        ExecOptions::new(
            "Return a reviewed answer with every applicable instruction",
            root,
            temp.path().join("logs"),
        ),
        model.clone(),
        Redactor::default(),
    )
    .await
    .unwrap();
    let store = InstructionStore::open(execution.tasks.database().clone(), Redactor::default())
        .await
        .unwrap();
    let general = store
        .register(
            "initial-general",
            None,
            InstructionScope::General,
            origin(),
            "Before host revision.",
        )
        .await
        .unwrap();
    store
        .revise("revise-general", None, &general.id, 1, origin(), GENERAL)
        .await
        .unwrap();
    store
        .register(
            "initial-task",
            None,
            InstructionScope::Task("episode-task".into()),
            origin(),
            TASK,
        )
        .await
        .unwrap();
    let retired = store
        .register(
            "initial-retired",
            None,
            InstructionScope::General,
            origin(),
            RETIRED,
        )
        .await
        .unwrap();
    store
        .retire("retire-before-first-context", None, &retired.id, 1)
        .await
        .unwrap();
    store
        .register(
            "foreign-task",
            None,
            InstructionScope::Task("other-task".into()),
            origin(),
            FOREIGN,
        )
        .await
        .unwrap();
    let task = execution
        .tasks
        .task("episode", "episode-task")
        .await
        .unwrap();
    execution.tasks.host_batch("episode","request-independent-review",json!({"ops":[{"op":"update_task","id":task.id,"expected_version":task.version,"data":{"metadata":{"user_requested_verification":true}}}]})).await.unwrap();
    let outcome = execution.run(None).await.unwrap();
    assert_eq!(outcome.status, "completed", "{outcome:?}");
    assert_eq!(model.calls.load(Ordering::SeqCst), 1);
}
