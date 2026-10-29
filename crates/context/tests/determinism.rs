use excalibur_context::{ContextError, ContextInput, compile, stable_prefix_changed};
use excalibur_types::runtime::{Message, ToolDefinition};
use serde_json::json;
fn fixture() -> ContextInput {
    let mut input = ContextInput::new(
        "Literal core safety rules.",
        "Literal user task.",
        "2026-10-29T00:00:00Z",
    );
    input.layers[1] = "Shared worker identity".into();
    input.layers[2] = "Project conventions and fixed instructions".into();
    for name in ["write_file", "read_file"] {
        input.tools.push(ToolDefinition{name:name.into(),description:format!("{name} description"),input_schema:json!({"type":"object","properties":{"path":{"type":"string"}},"required":["path"]})});
    }
    input.append_message(Message::text("user", "Start work"));
    input
}
#[test]
fn stable_layers_tools_and_append_only_deltas_are_deterministic() {
    let mut input = fixture();
    let first = compile(&input).unwrap();
    let original = input.clone();
    input.tools.reverse();
    assert_eq!(first, compile(&input).unwrap());
    for revision in 1..=100 {
        input.append_delta(revision, format!("requirement R{revision} updated"));
    }
    input.volatile.now_utc = "2026-10-29T01:00:00Z".into();
    let last = compile(&input).unwrap();
    assert!(!stable_prefix_changed(&first, &last));
    assert_eq!(first.layer_hashes[..4], last.layer_hashes[..4]);
    assert_ne!(first.layer_hashes[4], last.layer_hashes[4]);
    assert_ne!(first.layer_hashes[5], last.layer_hashes[5]);
    assert_eq!(input.layers, original.layers);
    assert_eq!(input.activity[0], original.activity[0]);
    assert_eq!(first.request.tools[0].name, "read_file");
    assert!(!first.request.messages[0].content.contains("2026-10-29"));
    let snapshot = serde_json::to_vec(&input).unwrap();
    let restored: ContextInput = serde_json::from_slice(&snapshot).unwrap();
    assert_eq!(last, compile(&restored).unwrap());
}
#[test]
fn layer_budgets_reject_excess_without_mutating_literal_instructions() {
    let mut input = fixture();
    input.token_budgets[2] = 1;
    let before = input.clone();
    assert!(matches!(
        compile(&input),
        Err(ContextError::Budget { layer: 2, .. })
    ));
    assert_eq!(input, before);
    input = fixture();
    input.suggested_skills = (0..8).map(|n| format!("skill{n}")).collect();
    let compiled = compile(&input).unwrap();
    assert!(compiled.request.messages[3].content.contains("skill2"));
    assert!(!compiled.request.messages[3].content.contains("skill3"));
    input.tools.push(input.tools[0].clone());
    assert!(matches!(compile(&input), Err(ContextError::ToolNames)));
    assert!(matches!(
        compile(&ContextInput::default()),
        Err(ContextError::MissingTime)
    ));
}
#[test]
#[ignore = "subprocess fixture invoked by determinism_across_processes_and_restart"]
fn context_child() {
    let input: ContextInput = serde_json::from_slice(
        &std::fs::read(std::env::var_os("EXCALIBUR_CONTEXT_INPUT").unwrap()).unwrap(),
    )
    .unwrap();
    std::fs::write(
        std::env::var_os("EXCALIBUR_CONTEXT_OUTPUT").unwrap(),
        serde_json::to_vec(&compile(&input).unwrap()).unwrap(),
    )
    .unwrap();
}
#[test]
fn determinism_across_processes_and_restart() {
    let root = std::env::temp_dir().join(format!("excalibur-context-{}", std::process::id()));
    std::fs::create_dir_all(&root).unwrap();
    let input = root.join("input.json");
    std::fs::write(&input, serde_json::to_vec(&fixture()).unwrap()).unwrap();
    let expected = serde_json::to_vec(&compile(&fixture()).unwrap()).unwrap();
    for n in 0..2 {
        let output = root.join(format!("output{n}.json"));
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "context_child", "--ignored"])
            .env("EXCALIBUR_CONTEXT_INPUT", &input)
            .env("EXCALIBUR_CONTEXT_OUTPUT", &output)
            .status()
            .unwrap();
        assert!(status.success());
        assert_eq!(std::fs::read(output).unwrap(), expected);
    }
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn current_micro_plan_changes_only_the_volatile_projection() {
    let mut context = excalibur_context::ContextInput::new("Core", "Literal request", "UTC");
    let before = excalibur_context::compile(&context).unwrap();
    context.volatile.current_plan = "micro@2 S1: verify [in_progress]".into();
    let after = excalibur_context::compile(&context).unwrap();
    assert_eq!(&before.layer_hashes[..5], &after.layer_hashes[..5]);
    assert_ne!(before.layer_hashes[5], after.layer_hashes[5]);
    assert!(
        after
            .request
            .messages
            .last()
            .unwrap()
            .content
            .contains("micro@2")
    );
    assert_eq!(
        after.bytes,
        excalibur_context::compile(&context).unwrap().bytes
    );
}
