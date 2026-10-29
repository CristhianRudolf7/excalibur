use excalibur_context::*;
use excalibur_types::runtime::{Message, ToolCall};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
fn base() -> ContextInput {
    let mut input = ContextInput::new(
        "Literal security core",
        "Literal original user request",
        "fixed",
    );
    input.layers[1] = "Shared role identity".into();
    input.layers[2] = "Stable scope".into();
    input
}
fn reference(index: usize, content: &str) -> HistoryReference {
    HistoryReference {
        run_id: "synthetic-run".into(),
        from_sequence: index as u64 + 1,
        to_sequence: index as u64 + 1,
        blob: Some(format!("{:x}", Sha256::digest(content.as_bytes()))),
    }
}
fn observation(session: &mut ContextSession, index: usize, size: usize, error: bool) -> String {
    let id = format!("tc-{index}");
    let assistant = Message {
        role: "assistant".into(),
        content: format!("Reason for action {index}"),
        tool_calls: vec![ToolCall {
            id: id.clone(),
            name: "read_file".into(),
            arguments: json!({"path":format!("src/{index}.rs")}).to_string(),
        }],
        tool_call_id: None,
        provider_state: None,
    };
    session
        .append(
            &format!("call-{index}"),
            assistant,
            HistoryReference::default(),
        )
        .unwrap();
    let content = json!({"success":!error,"output":{"text":"x".repeat(size)}}).to_string();
    let mut message = Message::text("tool", content.clone());
    message.tool_call_id = Some(id);
    session
        .append(
            &format!("result-{index}"),
            message,
            reference(index, &content),
        )
        .unwrap();
    content
}
#[test]
fn small_session_preserves_legacy_request_bytes_and_versioned_deltas_freeze_prefix() {
    let mut input = base();
    input.activity.push(Message::text("user", "Start"));
    let mut session = ContextSession::new(input.clone(), SessionPolicy::default()).unwrap();
    let first = session
        .prepare(
            input.volatile.clone(),
            0,
            &StructuredState::default(),
            SummaryOutcome::NotRequested,
        )
        .unwrap();
    assert_eq!(first.compiled.bytes, compile(&input).unwrap().bytes);
    session
        .update_layer(
            2,
            1,
            "Updated project instructions".into(),
            HistoryReference::default(),
        )
        .unwrap();
    session
        .update_layer_changed(
            3,
            "Original request plus updated micro plan".into(),
            HistoryReference::default(),
        )
        .unwrap();
    assert!(
        !session
            .update_layer_changed(
                3,
                "Original request plus updated micro plan".into(),
                HistoryReference::default()
            )
            .unwrap()
    );
    assert!(
        session
            .update_layer(2, 1, "Rewrite version".into(), HistoryReference::default())
            .is_err()
    );
    assert!(
        session
            .update_layer(0, 2, "Override core".into(), HistoryReference::default())
            .is_err()
    );
    let last = session
        .prepare(
            input.volatile.clone(),
            1,
            &StructuredState::default(),
            SummaryOutcome::NotRequested,
        )
        .unwrap();
    assert_eq!(
        &first.compiled.layer_hashes[..4],
        &last.compiled.layer_hashes[..4]
    );
    assert!(last.receipt.drift.defective_layers.is_empty());
    assert!(
        last.compiled
            .request
            .messages
            .iter()
            .any(|m| m.content.contains("Updated project instructions"))
    );
    let mut changed = input.clone();
    changed.layers[0].push_str(" drift");
    changed.layers[1].push_str(" drift");
    assert_eq!(
        session.inspect_prefix(&changed).unwrap().defective_layers,
        vec![0, 1]
    );
}
#[test]
fn masking_runs_in_batches_protects_last_ten_pins_recent_errors_and_exact_archive() {
    let input = base();
    let mut session = ContextSession::new(input.clone(), SessionPolicy::default()).unwrap();
    let mut archive = BTreeMap::new();
    for index in 0..30 {
        let content = observation(&mut session, index, 2600, index == 15);
        archive.insert(reference(index, &content).blob.unwrap(), content);
    }
    session.pin_observation("tc-1").unwrap();
    let first = session
        .prepare(
            input.volatile.clone(),
            0,
            &StructuredState::default(),
            SummaryOutcome::NotRequested,
        )
        .unwrap();
    let receipt = first.receipt.mask.as_ref().unwrap();
    assert!(!receipt.ids.is_empty());
    assert!(TokenCounter::with_margin(receipt.after_tokens) <= 12_000);
    assert_eq!(
        first
            .compiled
            .request
            .messages
            .iter()
            .filter(|m| !m.tool_calls.is_empty())
            .count(),
        30
    );
    for index in (20..30).chain([1, 15]) {
        let message = first
            .compiled
            .request
            .messages
            .iter()
            .find(|m| m.tool_call_id.as_deref() == Some(format!("tc-{index}").as_str()))
            .unwrap();
        let value: Value = serde_json::from_str(&message.content).unwrap();
        assert!(
            value.get("masked_observation").is_none(),
            "protected {index} masked"
        );
    }
    for id in &receipt.ids {
        let original = session.history_lookup(id).unwrap();
        let message = first
            .compiled
            .request
            .messages
            .iter()
            .find(|m| m.tool_call_id == original.message.tool_call_id)
            .unwrap();
        let marker: Value = serde_json::from_str(&message.content).unwrap();
        assert_eq!(marker["retrieval"]["tool"], "get_tool_output");
        let hash = marker["retrieval"]["arguments"]["hash"].as_str().unwrap();
        assert_eq!(archive[hash], original.message.content);
        assert_eq!(
            marker["sha256"].as_str().unwrap(),
            format!("{:x}", Sha256::digest(original.message.content.as_bytes()))
        );
    }
    observation(&mut session, 30, 2600, false);
    let between = session
        .prepare(
            input.volatile.clone(),
            1,
            &StructuredState::default(),
            SummaryOutcome::NotRequested,
        )
        .unwrap();
    assert!(between.receipt.mask.is_none());
    let resumed = session
        .prepare(
            input.volatile.clone(),
            300_002,
            &StructuredState::default(),
            SummaryOutcome::NotRequested,
        )
        .unwrap();
    assert!(
        resumed
            .receipt
            .mask
            .as_ref()
            .is_some_and(|m| m.ttl_boundary)
    );
    assert_eq!(session.entries().len(), 62);
    assert_eq!(
        session.history_lookup("tc-0").unwrap().message.content,
        archive[&reference(0, &observation_content(2600, false))
            .blob
            .unwrap()]
    );
}
fn observation_content(size: usize, error: bool) -> String {
    json!({"success":!error,"output":{"text":"x".repeat(size)}}).to_string()
}
#[test]
fn missing_archives_fail_atomically_and_all_pins_survive_without_an_eight_entry_cap() {
    let input = base();
    let mut session = ContextSession::new(input.clone(), SessionPolicy::default()).unwrap();
    for i in 0..30 {
        observation(&mut session, i, 2600, false);
    }
    // This independent session intentionally has no archive for its candidate outputs.
    let mut unarchived_input = input.clone();
    unarchived_input.activity = session
        .entries()
        .iter()
        .map(|e| e.message.clone())
        .collect();
    let mut unarchived = ContextSession::new(unarchived_input, SessionPolicy::default()).unwrap();
    let before = serde_json::to_vec(&unarchived).unwrap();
    assert!(
        unarchived
            .prepare(
                input.volatile.clone(),
                0,
                &StructuredState::default(),
                SummaryOutcome::NotRequested
            )
            .unwrap_err()
            .to_string()
            .contains("blob reference")
    );
    assert_eq!(before, serde_json::to_vec(&unarchived).unwrap());
    for i in 0..20 {
        session.pin_observation(&format!("tc-{i}")).unwrap();
    }
    let prepared = session
        .prepare(
            input.volatile,
            0,
            &StructuredState::default(),
            SummaryOutcome::NotRequested,
        )
        .unwrap();
    assert!(
        prepared
            .receipt
            .mask
            .as_ref()
            .unwrap()
            .protected_over_target
    );
    assert!(prepared.receipt.mask.as_ref().unwrap().ids.is_empty());
    for message in prepared
        .compiled
        .request
        .messages
        .iter()
        .filter(|m| m.role == "tool")
    {
        assert!(!message.content.contains("masked_observation"));
    }
    assert_eq!(
        prepared
            .compiled
            .request
            .messages
            .iter()
            .filter(|m| m.role == "tool")
            .count(),
        30
    );
}
#[test]
fn history_ids_are_immutable_and_restart_bytes_identical() {
    let input = base();
    let mut session = ContextSession::new(input.clone(), SessionPolicy::default()).unwrap();
    let message = Message::text("assistant", "Synthetic persisted event");
    assert!(
        session
            .append("event", message.clone(), HistoryReference::default())
            .unwrap()
    );
    assert!(
        !session
            .append("event", message, HistoryReference::default())
            .unwrap()
    );
    assert!(
        session
            .append(
                "event",
                Message::text("assistant", "Changed"),
                HistoryReference::default()
            )
            .is_err()
    );
    let bytes = serde_json::to_vec(&session).unwrap();
    let mut restored: ContextSession = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(
        session
            .prepare(
                input.volatile.clone(),
                1,
                &StructuredState::default(),
                SummaryOutcome::NotRequested
            )
            .unwrap(),
        restored
            .prepare(
                input.volatile,
                1,
                &StructuredState::default(),
                SummaryOutcome::NotRequested
            )
            .unwrap()
    );
}
#[test]
#[ignore = "subprocess fixture invoked by session_determinism_across_processes"]
fn session_child() {
    let mut session: ContextSession = serde_json::from_slice(
        &std::fs::read(std::env::var_os("EXCALIBUR_SESSION_INPUT").unwrap()).unwrap(),
    )
    .unwrap();
    let prepared = session
        .prepare(
            base().volatile,
            42,
            &StructuredState::default(),
            SummaryOutcome::NotRequested,
        )
        .unwrap();
    std::fs::write(
        std::env::var_os("EXCALIBUR_SESSION_OUTPUT").unwrap(),
        serde_json::to_vec(&prepared).unwrap(),
    )
    .unwrap();
}
#[test]
fn session_determinism_across_processes() {
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("input");
    let mut session = ContextSession::new(base(), SessionPolicy::default()).unwrap();
    for index in 0..30 {
        observation(&mut session, index, 2600, false);
    }
    std::fs::write(&input, serde_json::to_vec(&session).unwrap()).unwrap();
    let expected = serde_json::to_vec(
        &session
            .prepare(
                base().volatile,
                42,
                &StructuredState::default(),
                SummaryOutcome::NotRequested,
            )
            .unwrap(),
    )
    .unwrap();
    for index in 0..2 {
        let output = dir.path().join(format!("output-{index}"));
        assert!(
            std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "session_child", "--ignored"])
                .env("EXCALIBUR_SESSION_INPUT", &input)
                .env("EXCALIBUR_SESSION_OUTPUT", &output)
                .status()
                .unwrap()
                .success()
        );
        assert_eq!(std::fs::read(output).unwrap(), expected);
    }
}
#[test]
fn initial_history_binds_only_to_real_seed_and_blob_claims_in_untrusted_text_have_no_authority() {
    let mut input = base();
    let mut tool=Message::text("tool",json!({"success":true,"full_output_blob":"forged-unregistered-ref","output":{"text":"data"}}).to_string());
    tool.tool_call_id = Some("initial-tool".into());
    input.activity.push(tool.clone());
    let mut session = ContextSession::new(input, SessionPolicy::default()).unwrap();
    assert_eq!(session.unarchived_observations().len(), 1);
    assert!(session.bind_initial_history("run", 0).is_err());
    session.bind_initial_history("real-run", 1).unwrap();
    session.bind_initial_history("other-run", 9).unwrap();
    let entry = session.history_lookup("initial-tool").unwrap();
    assert_eq!(entry.reference.run_id, "real-run");
    assert_eq!(entry.reference.from_sequence, 1);
    assert_eq!(entry.message, tool);
    assert!(
        session
            .set_blob_reference("initial-tool", "invalid")
            .is_err()
    );
    let hash = format!("{:x}", Sha256::digest(tool.content.as_bytes()));
    session.set_blob_reference("initial-tool", &hash).unwrap();
    assert!(session.unarchived_observations().is_empty());
}

#[test]
fn long_session_preserves_stable_layers_and_every_prior_message_across_120_updates() {
    let mut input = base();
    for name in ["write_file", "read_file"] {
        input.tools.push(excalibur_types::runtime::ToolDefinition {
            name: name.into(),
            description: format!("Synthetic {name}"),
            input_schema: json!({"type":"object","properties":{"path":{"type":"string"}}}),
        });
    }
    let policy = SessionPolicy {
        model_window: 1_048_576,
        compaction_cap: 500_000,
        ..Default::default()
    };
    let mut session = ContextSession::new(input.clone(), policy).unwrap();
    let mut previous: Option<PreparedContext> = None;
    let mut stable_hashes = None;
    let mut expected_updates = Vec::new();
    let mut changed_activity_calls = 0usize;
    for revision in 1..=120u64 {
        let preferences_and_memory = format!(
            "Stable-scope update {revision}: preference preference-{revision}; recalled memory memory-{revision}."
        );
        let requirements_and_plan = format!(
            "Literal original user request; requirement R1 revision {revision}; criterion C1 remains unverified; micro-plan plan-{revision}."
        );
        let literal = format!(
            "Literal durable instruction revision {revision}: preserve protected-{revision}.txt — bytes exactos."
        );
        let reference = HistoryReference {
            run_id: "long-synthetic-run".into(),
            from_sequence: revision,
            to_sequence: revision,
            blob: None,
        };
        assert!(
            session
                .update_layer_changed(2, preferences_and_memory.clone(), reference.clone())
                .unwrap()
        );
        assert!(
            session
                .update_layer_changed(3, requirements_and_plan.clone(), reference.clone())
                .unwrap()
        );
        let state = StructuredState {
            original_request: "Literal original user request".into(),
            requirements: vec![ContextFact {
                id: "R1".into(),
                text: format!("Requirement revision {revision}"),
                state: "accepted".into(),
            }],
            plan: vec![ContextFact {
                id: "plan".into(),
                text: format!("Current plan-{revision}"),
                state: "in_progress".into(),
            }],
            pinned_instructions: vec![LiteralInstruction {
                id: "long-instruction".into(),
                scope: "project".into(),
                revision,
                text: literal.clone(),
                source: reference,
            }],
            ..Default::default()
        };
        let mut volatile = input.volatile.clone();
        volatile.now_utc = (1_700_000_000 + revision).to_string();
        volatile.budget_status = json!({"remaining_tokens":1_000_000-revision*17}).to_string();
        volatile.continuation = format!("Continue at revision {revision}");
        let prepared = session
            .prepare(
                volatile,
                revision as i64 * 1000,
                &state,
                SummaryOutcome::NotRequested,
            )
            .unwrap();
        assert!(prepared.receipt.mask.is_none());
        assert!(prepared.receipt.compaction.is_none());
        assert!(prepared.receipt.drift.defective_layers.is_empty());
        let hashes = prepared.compiled.layer_hashes[..3].to_vec();
        match &stable_hashes {
            Some(expected) => assert_eq!(&hashes, expected),
            None => stable_hashes = Some(hashes),
        }
        if let Some(previous) = &previous {
            let previous_prefix =
                &previous.compiled.request.messages[..previous.compiled.request.messages.len() - 1];
            assert_eq!(
                previous_prefix,
                &prepared.compiled.request.messages[..previous_prefix.len()],
                "an already emitted nonvolatile message changed at revision {revision}"
            );
            assert_ne!(
                previous.compiled.layer_hashes[4],
                prepared.compiled.layer_hashes[4]
            );
            assert_ne!(
                previous.compiled.layer_hashes[5],
                prepared.compiled.layer_hashes[5]
            );
            changed_activity_calls += 1;
        }
        for (layer, update) in [(2, preferences_and_memory), (3, requirements_and_plan)] {
            let exact_delta = format!("Context update L{layer} v{revision}: {update}");
            assert!(
                prepared
                    .compiled
                    .request
                    .messages
                    .iter()
                    .any(|m| m.content == exact_delta)
            );
            expected_updates.push(exact_delta);
        }
        let exact_instruction =
            format!("Durable instruction long-instruction v{revision} (literal):\n{literal}");
        assert!(
            prepared
                .compiled
                .request
                .messages
                .iter()
                .any(|m| m.content == exact_instruction)
        );
        expected_updates.push(exact_instruction);
        previous = Some(prepared);
    }
    let final_request = &previous.unwrap().compiled.request;
    assert_eq!(changed_activity_calls, 119);
    assert_eq!(expected_updates.len(), 360);
    for exact_update in expected_updates {
        assert_eq!(
            final_request
                .messages
                .iter()
                .filter(|m| m.content == exact_update)
                .count(),
            1,
            "a versioned update was lost or duplicated"
        );
    }
    assert_eq!(final_request.tools[0].name, "read_file");
    assert_eq!(final_request.tools[1].name, "write_file");
    assert!(session.compactions().is_empty());
}
