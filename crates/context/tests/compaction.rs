use excalibur_context::*;
use excalibur_types::runtime::{Message, ToolCall};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
fn base() -> ContextInput {
    ContextInput::new("Literal security core", "Literal original task", "fixed")
}
fn policy() -> SessionPolicy {
    SessionPolicy {
        compaction_cap: 30_000,
        ..Default::default()
    }
}
fn fill(session: &mut ContextSession, from: usize, to: usize) {
    for index in from..to {
        let id = format!("tc-{index}");
        let call = Message {
            role: "assistant".into(),
            content: format!("Reason {index}: {}", "reason ".repeat(170)),
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
                call,
                HistoryReference {
                    run_id: "run".into(),
                    from_sequence: index as u64 * 2 + 1,
                    to_sequence: index as u64 * 2 + 1,
                    ..Default::default()
                },
            )
            .unwrap();
        let content = json!({"success":true,"output":{"text":"result ".repeat(170)}}).to_string();
        let hash = format!("{:x}", Sha256::digest(content.as_bytes()));
        let mut result = Message::text("tool", content);
        result.tool_call_id = Some(id);
        session
            .append(
                &format!("result-{index}"),
                result,
                HistoryReference {
                    run_id: "run".into(),
                    from_sequence: index as u64 * 2 + 2,
                    to_sequence: index as u64 * 2 + 2,
                    blob: Some(hash),
                },
            )
            .unwrap();
    }
}
fn state() -> StructuredState {
    StructuredState {
        objective: "Implement verified task".into(),
        original_request: "Original literal request\r\nwith exact bytes".into(),
        pinned_instructions: vec![LiteralInstruction {
            id: "instruction".into(),
            scope: "project".into(),
            revision: 7,
            text: "NO TOQUES secretos.\r\nPreserva España y 日本語.".into(),
            source: HistoryReference {
                run_id: "run".into(),
                from_sequence: 7,
                to_sequence: 7,
                blob: None,
            },
        }],
        requirements: vec![ContextFact {
            id: "R1".into(),
            text: "Required deliverable".into(),
            state: "accepted".into(),
        }],
        criteria: vec![ContextFact {
            id: "C1".into(),
            text: "Observable pass".into(),
            state: "unverified".into(),
        }],
        plan: vec![ContextFact {
            id: "S1".into(),
            text: "Next action".into(),
            state: "pending".into(),
        }],
        next_steps: vec!["Run check".into()],
        artifacts: vec![ArtifactFact {
            path: "src/main.rs".into(),
            operation: "edit".into(),
            hash: "exact-postimage-sha256".into(),
        }],
        commands: vec![CommandFact {
            command: "synthetic-test --exact".into(),
            exit_code: Some(2),
        }],
        open_errors: vec![ContextFact {
            id: "E1".into(),
            text: "Exact failure: line 42\nExpected Ω".into(),
            state: "open".into(),
        }],
        children: vec![ContextFact {
            id: "child-1".into(),
            text: "Child task".into(),
            state: "blocked".into(),
        }],
        decisions: vec![ContextFact {
            id: "D1".into(),
            text: "Chosen storage because durable".into(),
            state: "accepted".into(),
        }],
        open_ids: ["additional-open".into()].into_iter().collect(),
    }
}
fn assert_pairs(prepared: &PreparedContext) {
    let mut pending = BTreeSet::new();
    for message in &prepared.compiled.request.messages {
        for call in &message.tool_calls {
            assert!(pending.insert(call.id.clone()));
        }
        if let Some(id) = &message.tool_call_id {
            assert!(pending.remove(id), "result without its call: {id}");
        }
    }
    assert!(pending.is_empty());
}
#[test]
fn structured_compaction_has_exact_artifacts_errors_ids_literal_instructions_and_fallback() {
    let input = base();
    let state = state();
    let mut session = ContextSession::new(input.clone(), policy()).unwrap();
    fill(&mut session, 0, 80);
    session.pin_observation("tc-0").unwrap();
    let request = session.summary_request(0, &state).unwrap().unwrap();
    assert!(!request.discarded.is_empty());
    let prepared = session
        .prepare(
            input.volatile.clone(),
            0,
            &state,
            SummaryOutcome::Unavailable {
                reason: "synthetic extractor failure".into(),
            },
        )
        .unwrap();
    let receipt = prepared.receipt.compaction.as_ref().unwrap();
    assert_eq!(receipt.id, request.id);
    assert_eq!(receipt.summary_status, "unavailable_summary_fallback");
    assert_eq!(receipt.target_activity_tokens, 20_000);
    assert!(receipt.recent_window_tokens <= 20_000);
    assert_eq!(
        receipt.protected_over_target,
        receipt.retained_activity_tokens > 20_000
    );
    assert!(!receipt.discarded.is_empty());
    assert_eq!(session.entries().len(), 161);
    let block = prepared
        .compiled
        .request
        .messages
        .iter()
        .find_map(|m| {
            serde_json::from_str::<Value>(&m.content)
                .ok()
                .filter(|v| v.get("context_compaction").is_some())
        })
        .unwrap();
    assert_eq!(
        block["context_compaction"]["structured_state"],
        serde_json::to_value(&state).unwrap()
    );
    for id in state.required_open_ids() {
        assert!(session.summary().open_ids.contains(&id));
    }
    assert!(
        prepared.compiled.request.messages[2]
            .content
            .contains(&state.pinned_instructions[0].text)
    );
    assert_pairs(&prepared);
    let pinned = prepared
        .compiled
        .request
        .messages
        .iter()
        .find(|m| m.tool_call_id.as_deref() == Some("tc-0"))
        .unwrap();
    assert!(!pinned.content.contains("masked_observation"));
    assert_eq!(session.history_search("Reason 1:", 0, 1)[0].id, "call-1");
    let restored: ContextSession =
        serde_json::from_slice(&serde_json::to_vec(&session).unwrap()).unwrap();
    assert_eq!(restored.entries(), session.entries());
}
#[test]
fn summaries_add_only_new_segments_repair_missing_open_ids_and_never_rewrite_previous_findings() {
    let input = base();
    let state = state();
    let mut session = ContextSession::new(input.clone(), policy()).unwrap();
    fill(&mut session, 0, 80);
    let first = session.summary_request(0, &state).unwrap().unwrap();
    let first_ids = first
        .discarded
        .iter()
        .map(|e| e.id.clone())
        .collect::<BTreeSet<_>>();
    session
        .prepare(
            input.volatile.clone(),
            0,
            &state,
            SummaryOutcome::Added {
                request_id: first.id,
                delta: IncrementalSummary {
                    findings: vec!["First finding retained".into()],
                    ..Default::default()
                },
            },
        )
        .unwrap();
    fill(&mut session, 80, 150);
    let second = session.summary_request(1, &state).unwrap().unwrap();
    assert_eq!(second.previous.findings, vec!["First finding retained"]);
    assert!(second.discarded.iter().all(|e| !first_ids.contains(&e.id)));
    let prepared = session
        .prepare(
            input.volatile,
            1,
            &state,
            SummaryOutcome::Added {
                request_id: second.id,
                delta: IncrementalSummary {
                    findings: vec!["Second finding appended".into()],
                    discarded_hypotheses: vec![RejectedHypothesis {
                        hypothesis: "Discarded hypothesis".into(),
                        reason: "Contradicted by actual check".into(),
                    }],
                    ..Default::default()
                },
            },
        )
        .unwrap();
    assert_eq!(
        session.summary().findings,
        vec!["First finding retained", "Second finding appended"]
    );
    for id in state.required_open_ids() {
        assert!(session.summary().open_ids.contains(&id));
    }
    assert_pairs(&prepared);
}
#[test]
fn wrong_summary_receipt_falls_back_and_instruction_bytes_survive_restart_and_delegation() {
    let input = base();
    let state = state();
    let mut session = ContextSession::new(input.clone(), policy()).unwrap();
    fill(&mut session, 0, 80);
    let prepared = session
        .prepare(
            input.volatile.clone(),
            0,
            &state,
            SummaryOutcome::Added {
                request_id: "wrong".into(),
                delta: IncrementalSummary {
                    findings: vec!["Do not merge wrong segment".into()],
                    ..Default::default()
                },
            },
        )
        .unwrap();
    assert_eq!(
        prepared.receipt.compaction.unwrap().summary_status,
        "invalid_summary_fallback"
    );
    assert!(session.summary().findings.is_empty());
    let mut restored: ContextSession =
        serde_json::from_slice(&serde_json::to_vec(&session).unwrap()).unwrap();
    let restored = restored
        .prepare(
            input.volatile.clone(),
            1,
            &state,
            SummaryOutcome::NotRequested,
        )
        .unwrap();
    let literal = state.pinned_instructions[0].text.as_bytes();
    assert!(restored.compiled.request.messages.iter().any(|m| {
        m.content
            .as_bytes()
            .windows(literal.len())
            .any(|w| w == literal)
    }));
    let mut child = ContextSession::new(input.clone(), SessionPolicy::default()).unwrap();
    let child = child
        .prepare(input.volatile, 0, &state, SummaryOutcome::NotRequested)
        .unwrap();
    assert!(child.compiled.request.messages.iter().any(|m| {
        m.content
            .as_bytes()
            .windows(literal.len())
            .any(|w| w == literal)
    }));
}
#[test]
fn conversation_condenses_at_24k_or_expired_ttl_and_omitted_history_remains_exact() {
    let input = base();
    let policy = SessionPolicy {
        conversation: true,
        ..Default::default()
    };
    let mut session = ContextSession::new(input.clone(), policy).unwrap();
    for i in 0..45 {
        session
            .append(
                &format!("turn-{i}"),
                Message::text("user", format!("Turn {i}: {}", "literal ".repeat(240))),
                HistoryReference {
                    run_id: "conversation".into(),
                    from_sequence: i + 1,
                    to_sequence: i + 1,
                    ..Default::default()
                },
            )
            .unwrap();
    }
    let state = StructuredState::default();
    let request = session.summary_request(0, &state).unwrap().unwrap();
    let discarded = request.discarded[0].clone();
    let first = session
        .prepare(
            input.volatile.clone(),
            0,
            &state,
            SummaryOutcome::Added {
                request_id: request.id,
                delta: IncrementalSummary {
                    findings: vec!["Prior conversation finding".into()],
                    ..Default::default()
                },
            },
        )
        .unwrap();
    let receipt = first.receipt.compaction.unwrap();
    assert!(receipt.conversation);
    assert!(receipt.retained_activity_tokens <= 12_000);

    for i in 45..48 {
        session
            .append(
                &format!("turn-{i}"),
                Message::text("user", format!("Turn {i}: {}", "literal ".repeat(100))),
                HistoryReference::default(),
            )
            .unwrap();
    }
    assert!(session.summary_request(1, &state).unwrap().is_none());
    session
        .prepare(
            input.volatile.clone(),
            1,
            &state,
            SummaryOutcome::NotRequested,
        )
        .unwrap();
    assert_eq!(
        session.summary().findings,
        vec!["Prior conversation finding"]
    );
    let next = session.summary_request(300_002, &state).unwrap().unwrap();
    assert_eq!(next.previous.findings, vec!["Prior conversation finding"]);
    let resumed = session
        .prepare(
            input.volatile,
            300_002,
            &state,
            SummaryOutcome::NotRequested,
        )
        .unwrap();
    assert!(resumed.receipt.compaction.unwrap().conversation);
    assert_eq!(session.history_lookup(&discarded.id).unwrap(), &discarded);
    assert_eq!(
        session.history_search(&discarded.message.content, 0, 1)[0]
            .message
            .content,
        discarded.message.content
    );
}
#[test]
fn impossible_protected_window_is_an_error_without_partial_session_mutation() {
    let mut input = base();
    input
        .activity
        .push(Message::text("assistant", "x".repeat(90_000)));
    let policy = SessionPolicy {
        model_window: 10_000,
        output_reserve: 1_000,
        ..Default::default()
    };
    let mut session = ContextSession::new(input.clone(), policy).unwrap();
    let before = session.clone();
    assert!(
        session
            .prepare(
                input.volatile,
                0,
                &StructuredState::default(),
                SummaryOutcome::NotRequested
            )
            .unwrap_err()
            .to_string()
            .contains("protected context")
    );
    assert_eq!(before, session);
}
#[test]
fn one_oversized_recent_tool_pair_stays_whole_and_reports_target_overflow() {
    let input = base();
    let mut session = ContextSession::new(input.clone(), policy()).unwrap();
    for index in 0..45 {
        session
            .append(
                &format!("old-{index}"),
                Message::text(
                    "assistant",
                    format!("Old reasoning {index}: {}", "old ".repeat(800)),
                ),
                HistoryReference::default(),
            )
            .unwrap();
    }
    let assistant = Message {
        role: "assistant".into(),
        content: "Current action and exact reasoning".into(),
        tool_calls: vec![ToolCall {
            id: "large-current".into(),
            name: "read_file".into(),
            arguments: json!({"path":"source"}).to_string(),
        }],
        tool_call_id: None,
        provider_state: Some(json!({"signature":"synthetic-provider-part"})),
    };
    session
        .append("large-call", assistant.clone(), HistoryReference::default())
        .unwrap();
    let mut result = Message::text(
        "tool",
        json!({"success":true,"output":{"text":"x".repeat(80_000)}}).to_string(),
    );
    result.tool_call_id = Some("large-current".into());
    let original = result.clone();
    session
        .append(
            "large-result",
            result,
            HistoryReference {
                run_id: "run".into(),
                from_sequence: 99,
                to_sequence: 99,
                blob: Some("a".repeat(64)),
            },
        )
        .unwrap();
    let prepared = session
        .prepare(
            input.volatile,
            0,
            &StructuredState::default(),
            SummaryOutcome::NotRequested,
        )
        .unwrap();
    let receipt = prepared.receipt.compaction.as_ref().unwrap();
    assert!(receipt.protected_over_target);
    assert!(receipt.recent_window_tokens > 20_000);
    assert!(receipt.retained_ids.contains(&"large-call".into()));
    assert!(receipt.retained_ids.contains(&"large-result".into()));
    assert_pairs(&prepared);
    let kept = prepared
        .compiled
        .request
        .messages
        .iter()
        .find(|m| m.tool_calls.iter().any(|c| c.id == "large-current"))
        .unwrap();
    assert_eq!(kept, &assistant);
    assert_eq!(
        session.history_lookup("large-result").unwrap().message,
        original
    );
}
