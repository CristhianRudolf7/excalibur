use excalibur_engine::{Budget, CompletionGate, GateInput, GateIssueKind};
use excalibur_journal::Journal;
use excalibur_store::{Database, DatabaseKind, ReadLane, StorageConfig};
use excalibur_tasks::{TaskStore, WorkLevel, checklist::*, evidence::*, verification::*};
use excalibur_tools::{Access, LocalTools, ToolPolicy};
use excalibur_types::runtime::*;
use serde_json::{Value, json};
use std::{
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};
struct Fixture {
    _temp: tempfile::TempDir,
    db: Database,
    store: TaskStore,
    root: PathBuf,
    run: String,
    tools: Arc<LocalTools>,
}
async fn fixture(method: &str, definition: Option<Value>, patterns: Value) -> Fixture {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("project");
    std::fs::create_dir(&root).unwrap();
    let db = Database::open(
        temp.path().join("data/main.db"),
        DatabaseKind::Main,
        StorageConfig::default(),
    )
    .unwrap();
    let store = TaskStore::open(db.clone()).await.unwrap();
    let journal = Journal::new(db.clone()).await.unwrap();
    let run = journal
        .create_run("root", json!({"test":true}))
        .await
        .unwrap();
    let mut ops = vec![
        json!({"op":"create_task","id":"task","title":"Synthetic work","data":{"request_card":{"original":"Check the synthetic source"},"metadata":{"workspace":root}}}),
        json!({"op":"create_requirement","id":"req","data":{"text":"Synthetic source is correct","state":"accepted"}}),
        json!({"op":"create_criterion","id":"criterion","data":{"requirement_id":"req","statement":"Synthetic check passes","method":method,"impact_area":patterns,"metadata":{"implicit_final_response":false}}}),
        json!({"op":"link_requirement","task":"task","requirement":"req"}),
    ];
    if let Some(definition) = definition {
        ops.push(json!({"op":"create_check","id":"check","data":{"criterion_id":"criterion","definition":definition}}));
    }
    store
        .host_batch("general", "seed", json!({"ops":ops}))
        .await
        .unwrap();
    store.bind_root("task", &run).await.unwrap();
    let tools = Arc::new(
        LocalTools::new(
            db.clone(),
            root.clone(),
            ToolPolicy {
                read: Access::Allow,
                write: Access::Allow,
                exec: Access::Allow,
                ..Default::default()
            },
        )
        .await
        .unwrap(),
    );
    Fixture {
        _temp: temp,
        db,
        store,
        root,
        run,
        tools,
    }
}
fn input(step: u64) -> GateInput {
    GateInput {
        step,
        instruction_revision: 0,
        final_text: "Synthetic result".into(),
        local_pending: vec![],
        budget_remaining: Budget::default(),
    }
}
#[tokio::test]
async fn file_assertion_is_executed_freshness_is_targeted_and_untracked_additions_invalidate() {
    let f = fixture(
        "test",
        Some(json!({"kind":"file_contains","path":"src/a","needle":"OK"})),
        json!(["src/**"]),
    )
    .await;
    std::fs::create_dir(f.root.join("src")).unwrap();
    std::fs::write(f.root.join("src/a"), "OK\n").unwrap();
    std::fs::write(f.root.join("outside"), "one").unwrap();
    let runner = CheckRunner::new(f.tools.clone(), f.root.clone());
    let e = runner
        .run_keyed(&f.store, "task", "check", &f.run, "first")
        .await
        .unwrap();
    assert_eq!(e.result, EvidenceResult::Pass);
    assert_eq!(e.tool_calls, 1);
    assert!(e.output_blob.is_some());
    let (_, criteria) = f.store.requirements_and_criteria("task").await.unwrap();
    assert_eq!(
        f.store
            .assess_criterion(&criteria[0], 0)
            .await
            .unwrap()
            .state,
        CriterionState::Satisfied
    );
    std::fs::write(f.root.join("outside"), "unrelated").unwrap();
    assert!(e.snapshot.fresh(0).await.unwrap());
    assert!(
        f.store
            .invalidate_paths("general", &["outside".into()])
            .await
            .unwrap()
            .is_empty()
    );
    std::fs::write(f.root.join("src/new"), "new untracked file").unwrap();
    assert!(!e.snapshot.fresh(0).await.unwrap());
    assert_eq!(
        f.store
            .assess_criterion(&criteria[0], 0)
            .await
            .unwrap()
            .state,
        CriterionState::Unverified
    );
    assert_eq!(
        f.store
            .invalidate_paths("general", &["src/new".into()])
            .await
            .unwrap(),
        vec!["criterion"]
    );
    let same = runner
        .run_keyed(&f.store, "task", "check", &f.run, "first")
        .await
        .unwrap();
    assert_eq!(same.id, e.id);
    assert_eq!(runner.tool_calls(), 1, "keyed operation must not rerun");
}
#[tokio::test]
async fn instruction_revision_invalidates_without_rewriting_old_evidence() {
    let f = fixture(
        "test",
        Some(json!({"kind":"file_contains","path":"source","needle":"OK"})),
        json!(["source"]),
    )
    .await;
    std::fs::write(f.root.join("source"), "OK").unwrap();
    let runner = CheckRunner::new(f.tools.clone(), f.root.clone());
    let e = runner.run(&f.store, "task", "check", &f.run).await.unwrap();
    f.store
        .invalidate_instruction_revision("general", "task", 1)
        .await
        .unwrap();
    let (_, criteria) = f.store.requirements_and_criteria("task").await.unwrap();
    assert_eq!(
        f.store
            .assess_criterion(&criteria[0], 1)
            .await
            .unwrap()
            .state,
        CriterionState::Unverified
    );
    assert_eq!(
        f.store
            .evidence(&e.id)
            .await
            .unwrap()
            .unwrap()
            .snapshot
            .instruction_revision,
        0
    );
}
#[tokio::test]
async fn judgments_cannot_satisfy_code_or_forge_backend_evidence() {
    let f = fixture("test", None, json!(["**"])).await;
    let snap = WorkspaceSnapshot::capture(f.root.clone(), vec!["**".into()], 0, vec![], vec![])
        .await
        .unwrap();
    assert!(
        f.store
            .record_judgment(
                "task",
                "criterion",
                &f.run,
                snap,
                true,
                "model says so",
                false
            )
            .await
            .is_err()
    );
    assert!(
        f.store
            .batch(
                "general",
                "forge",
                json!({"ops":[{"op":"record_evidence","check":"check","result":"pass"}]})
            )
            .await
            .is_err()
    );
    let gate = TaskCompletionGate::new(
        f.store.clone(),
        CheckRunner::new(f.tools.clone(), f.root.clone()),
    );
    let guard = gate.inspect(&f.run, &input(0)).await.unwrap();
    assert!(
        guard
            .report()
            .issues
            .iter()
            .any(|i| i.kind == GateIssueKind::Criterion)
    );
}
#[tokio::test]
async fn checklist_preserves_every_outside_byte_and_archives_user_inside_edits() {
    let f = fixture(
        "test",
        Some(json!({"kind":"file_contains","path":"source","needle":"OK"})),
        json!(["**"]),
    )
    .await;
    std::fs::write(f.root.join("source"), "OK").unwrap();
    std::fs::create_dir(f.root.join("tests")).unwrap();
    let path = f.root.join("tests/README.md");
    let original = format!(
        "outside\r\n{CHECKLIST_MARKER}\nuser inside edit\n{CHECKLIST_MARKER}\ntail without newline"
    );
    std::fs::write(&path, &original).unwrap();
    let runner = CheckRunner::new(f.tools.clone(), f.root.clone()).with_projection(path.clone());
    let e = runner.run(&f.store, "task", "check", &f.run).await.unwrap();
    let projection = ChecklistProjection::new(f.tools.clone(), "tests/README.md".into());
    let entries = vec![ChecklistEntry {
        check_id: "check".into(),
        label: "synthetic".into(),
        passed: true,
        fresh: true,
        recorded_at: Some(e.recorded_at),
        commit: None,
        evidence_id: Some(e.id),
    }];
    assert!(projection.update(&f.store, &f.run, &entries).await.unwrap());
    let actual = std::fs::read_to_string(&path).unwrap();
    assert!(actual.starts_with("outside\r\n"));
    assert!(actual.ends_with("\ntail without newline"));
    assert!(!actual.contains("user inside edit"));
    assert!(
        e.snapshot.fresh(0).await.unwrap(),
        "owned projection must not invalidate source proof"
    );
    let blobs =
        f.db.read(ReadLane::Interactive, |c| {
            let mut s = c.prepare(
                "SELECT preimage_blob FROM file_versions WHERE path LIKE '%/tests/README.md'",
            )?;
            Ok(s.query_map([], |r| r.get::<_, String>(0))?
                .collect::<std::result::Result<Vec<_>, _>>()?)
        })
        .await
        .unwrap();
    assert!(!blobs.is_empty());
    let bytes =
        f.db.read_blob(blobs[0].parse().unwrap(), 100000)
            .await
            .unwrap();
    assert_eq!(bytes, original.as_bytes());
    std::fs::write(&path, actual.replace("outside", "user changed outside")).unwrap();
    assert!(!e.snapshot.fresh(0).await.unwrap());
    assert!(!projection.update(&f.store, &f.run, &entries).await.unwrap());
}
#[tokio::test]
async fn new_generated_projection_and_eval_external_projection_do_not_change_task_fingerprints() {
    let f = fixture(
        "test",
        Some(json!({"kind":"file_contains","path":"source","needle":"OK"})),
        json!(["**"]),
    )
    .await;
    std::fs::write(f.root.join("source"), "OK").unwrap();
    let generated = f.root.join("tests/README.md");
    let runner =
        CheckRunner::new(f.tools.clone(), f.root.clone()).with_projection(generated.clone());
    let e = runner.run(&f.store, "task", "check", &f.run).await.unwrap();
    let entries = vec![ChecklistEntry {
        check_id: "check".into(),
        label: "synthetic check".into(),
        passed: true,
        fresh: true,
        recorded_at: Some(e.recorded_at),
        commit: None,
        evidence_id: Some(e.id.clone()),
    }];
    ChecklistProjection::new(f.tools.clone(), "tests/README.md".into())
        .update(&f.store, &f.run, &entries)
        .await
        .unwrap();
    assert!(e.snapshot.fresh(0).await.unwrap());
    let ephemeral = f._temp.path().join("ephemeral");
    std::fs::create_dir(&ephemeral).unwrap();
    let tools = Arc::new(
        LocalTools::new(
            f.db.clone(),
            ephemeral.clone(),
            ToolPolicy {
                read: Access::Allow,
                write: Access::Allow,
                ..Default::default()
            },
        )
        .await
        .unwrap(),
    );
    let projection = ChecklistProjection::new(tools, "README.md".into());
    assert_eq!(
        projection.absolute_path(&f.run).await.unwrap(),
        ephemeral.join("README.md")
    );
    projection.update(&f.store, &f.run, &entries).await.unwrap();
    assert!(ephemeral.join("README.md").exists());
    assert!(!f.root.join("README.md").exists());
}
#[tokio::test]
async fn final_gate_executes_all_checks_and_holds_task_mutation_lease_through_close() {
    let f = fixture(
        "test",
        Some(json!({"kind":"file_contains","path":"source","needle":"OK"})),
        json!(["source"]),
    )
    .await;
    std::fs::write(f.root.join("source"), "OK").unwrap();
    f.store.batch("general","second",json!({"ops":[{"op":"create_check","id":"second","data":{"criterion_id":"criterion","definition":{"kind":"file_contains","path":"source","needle":"MISSING"}}}]})).await.unwrap();
    let gate = TaskCompletionGate::new(
        f.store.clone(),
        CheckRunner::new(f.tools.clone(), f.root.clone()),
    );
    let guard = gate.inspect(&f.run, &input(0)).await.unwrap();
    assert!(guard.report().issues.iter().any(|i| i.id == "criterion"));
    let all = f.store.task_evidence("task").await.unwrap();
    assert_eq!(all.len(), 2);
    assert!(all.iter().any(|e| e.result == EvidenceResult::Fail));
    let store = f.store.clone();
    let wait = tokio::spawn(async move { store.begin_mutation().await });
    tokio::task::yield_now().await;
    assert!(
        !wait.is_finished(),
        "final gate lease must exclude mutations"
    );
    drop(guard);
    drop(wait.await.unwrap());
}
#[cfg(target_os = "linux")]
#[tokio::test]
async fn real_declared_command_checks_use_sandbox_and_capture_actual_exit_metric_and_failure() {
    let f=fixture("test",Some(json!({"kind":"command","command":"printf synthetic; exit 7","expected_exit":7,"timeout_ms":10000})),json!(["source"])).await;
    excalibur_tools::terminal::prepare_project_boundary(&f.root).unwrap();
    let runner = CheckRunner::new(f.tools.clone(), f.root.clone());
    let e = runner.run(&f.store, "task", "check", &f.run).await.unwrap();
    assert_eq!(e.result, EvidenceResult::Pass, "{}", evidence_message(&e));
    assert!(runner.tool_calls() >= 1);
    f.store.batch("general","metric",json!({"ops":[{"op":"create_check","id":"metric","data":{"criterion_id":"criterion","definition":{"kind":"metric","command":"printf '{\"score\":5}'","pointer":"/score","min":4.0,"max":6.0,"timeout_ms":10000}}}]})).await.unwrap();
    let metric = runner
        .run(&f.store, "task", "metric", &f.run)
        .await
        .unwrap();
    assert_eq!(
        metric.result,
        EvidenceResult::Pass,
        "{}",
        evidence_message(&metric)
    );
    f.tools.terminals().shutdown().await;
}
fn evidence_message(e: &EvidenceRecord) -> String {
    format!("{:?}: {}", e.result, e.explanation)
}

struct ProofVerifier {
    store: TaskStore,
    root: PathBuf,
    tools: Arc<LocalTools>,
    calls: AtomicUsize,
    inputs: std::sync::Mutex<Vec<VerificationInput>>,
    mode: u8,
}
impl IndependentVerifier for ProofVerifier {
    fn verify<'a>(
        &'a self,
        input: &'a VerificationInput,
    ) -> BoxFuture<'a, Result<VerificationVerdict, String>> {
        Box::pin(async move {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.inputs.lock().unwrap().push(input.clone());
            let journal = Journal::new(self.store.database().clone())
                .await
                .map_err(|e| e.to_string())?;
            let run = journal
                .create_run(
                    &format!("verifier:{}:{}", input.job_id, input.round),
                    json!({"role":"verifier"}),
                )
                .await
                .map_err(|e| e.to_string())?;
            let parent = if self.mode == 4 {
                let parent = journal
                    .create_run(
                        &format!("other-parent:{}", input.job_id),
                        json!({"role":"worker"}),
                    )
                    .await
                    .map_err(|e| e.to_string())?;
                self.store
                    .register_verifier_run(&input.task_id, &parent, &input.root_run_id)
                    .await
                    .map_err(|e| e.to_string())?;
                journal
                    .set_state_if(&parent, &["running"], "completed", None, "parent-done")
                    .await
                    .map_err(|e| e.to_string())?;
                parent
            } else {
                input.root_run_id.clone()
            };
            self.store
                .register_verifier_run(&input.task_id, &run, &parent)
                .await
                .map_err(|e| e.to_string())?;
            assert!(
                self.tools
                    .prepare(
                        &run,
                        ToolCall {
                            id: "forbidden-write".into(),
                            name: "write_file".into(),
                            arguments: json!({"path":"source","content":"not allowed"}).to_string()
                        }
                    )
                    .await
                    .is_err()
            );
            let runner = CheckRunner::new(self.tools.clone(), self.root.clone());
            let mut verdicts = Vec::new();
            for criterion in &input.criteria {
                let mut ids = Vec::new();
                if self.mode != 1 {
                    for check in &criterion.checks {
                        let e = runner
                            .run(&self.store, &input.task_id, &check.id, &run)
                            .await
                            .map_err(|e| e.to_string())?;
                        ids.push(e.id);
                    }
                    if criterion.checks.is_empty() {
                        let call = ToolCall {
                            id: format!("observation-{}", criterion.id),
                            name: "read_file".into(),
                            arguments: json!({"path":"source"}).to_string(),
                        };
                        let prep = self.tools.prepare(&run, call.clone()).await?;
                        let result = self.tools.execute(&run, &call.id, &prep).await;
                        assert!(result.success);
                        let sequence = journal
                            .append(
                                &run,
                                "verifier_tool_result",
                                &call.id,
                                json!({"call":call,"result":result}),
                            )
                            .await
                            .map_err(|e| e.to_string())?;
                        ids.push(format!("journal:{run}:{sequence}"));
                    }
                } else {
                    ids.extend(input.evidence.iter().map(|e| e.id.clone()));
                }
                verdicts.push(CriterionVerdict {
                    criterion_id: criterion.id.clone(),
                    passed: self.mode != 2,
                    explanation: if self.mode == 2 {
                        "independent disagreement"
                    } else {
                        "independent actual proof"
                    }
                    .into(),
                    evidence_ids: ids,
                });
            }
            journal
                .set_state_if(&run, &["running"], "completed", None, "finished")
                .await
                .map_err(|e| e.to_string())?;
            Ok(VerificationVerdict {
                verifier_run_id: run,
                criteria: verdicts,
                uncovered_requests: vec![],
                request_covered: true,
            })
        })
    }
}
async fn verifier_for(f: &Fixture, mode: u8) -> Arc<ProofVerifier> {
    let tools = Arc::new(
        LocalTools::new(
            f.db.clone(),
            f.root.clone(),
            ToolPolicy {
                read: Access::Allow,
                write: Access::Deny,
                exec: Access::Deny,
                ..Default::default()
            },
        )
        .await
        .unwrap(),
    );
    Arc::new(ProofVerifier {
        store: f.store.clone(),
        root: f.root.clone(),
        tools,
        calls: AtomicUsize::new(0),
        inputs: std::sync::Mutex::new(vec![]),
        mode,
    })
}
async fn n2(f: &Fixture) {
    let version = f.store.task("general", "task").await.unwrap().version;
    f.store.host_batch("general","n2",json!({"ops":[{"op":"update_task","id":"task","expected_version":version,"data":{"level":"N2"}}]})).await.unwrap();
    assert_eq!(
        f.store.task("general", "task").await.unwrap().level,
        WorkLevel::N2
    );
}
#[tokio::test]
async fn n2_five_requirements_create_isolated_verifier_with_own_actual_proof_and_reuse_fresh_result()
 {
    let f = fixture(
        "test",
        Some(json!({"kind":"file_contains","path":"source","needle":"OK"})),
        json!(["source"]),
    )
    .await;
    std::fs::write(f.root.join("source"), "OK").unwrap();
    n2(&f).await;
    let mut ops = Vec::new();
    for n in 2..=5 {
        ops.push(json!({"op":"create_requirement","id":format!("req{n}"),"data":{"text":format!("Requirement {n}"),"state":"accepted"}}));
        ops.push(json!({"op":"create_criterion","id":format!("cr{n}"),"data":{"requirement_id":format!("req{n}"),"statement":"Actual proof","method":"test","impact_area":["source"]}}));
        ops.push(json!({"op":"create_check","id":format!("ck{n}"),"data":{"criterion_id":format!("cr{n}"),"definition":{"kind":"file_contains","path":"source","needle":"OK"}}}));
        ops.push(json!({"op":"link_requirement","task":"task","requirement":format!("req{n}")}));
    }
    f.store
        .batch("general", "more", json!({"ops":ops}))
        .await
        .unwrap();
    let journal = Journal::new(f.db.clone()).await.unwrap();
    journal
        .append(
            &f.run,
            "private_executor_reasoning",
            "not-for-verifier",
            json!({"reasoning":"executor-private-chain-marker"}),
        )
        .await
        .unwrap();
    let verifier = verifier_for(&f, 0).await;
    let gate = TaskCompletionGate::new(
        f.store.clone(),
        CheckRunner::new(f.tools.clone(), f.root.clone()),
    )
    .with_verifier(verifier.clone());
    let guard = gate.inspect(&f.run, &input(0)).await.unwrap();
    assert!(
        guard.report().issues.is_empty(),
        "{:?}",
        guard.report().issues
    );
    drop(guard);
    assert_eq!(verifier.calls.load(Ordering::SeqCst), 1);
    let captured = verifier.inputs.lock().unwrap().clone();
    assert_eq!(captured[0].requirements.len(), 5);
    assert_eq!(captured[0].literal_request, "Check the synthetic source");
    assert!(
        !serde_json::to_string(&captured)
            .unwrap()
            .contains("executor-private-chain-marker")
    );
    let guard = gate.inspect(&f.run, &input(1)).await.unwrap();
    assert!(
        guard.report().issues.is_empty(),
        "{:?}",
        guard.report().issues
    );
    drop(guard);
    assert_eq!(
        verifier.calls.load(Ordering::SeqCst),
        1,
        "fresh passing independent proof should be reused"
    );
    assert!(
        f.store
            .task_evidence("task")
            .await
            .unwrap()
            .iter()
            .any(|e| e.run_id != f.run && e.kind == EvidenceKind::Execution)
    );
}
#[tokio::test]
async fn inspection_n2_requires_actual_independent_observation_even_below_five_requirements() {
    let f = fixture("inspection", None, json!(["source"])).await;
    std::fs::write(f.root.join("source"), "OK").unwrap();
    n2(&f).await;
    let snap = WorkspaceSnapshot::capture(f.root.clone(), vec!["source".into()], 0, vec![], vec![])
        .await
        .unwrap();
    f.store
        .record_judgment(
            "task",
            "criterion",
            &f.run,
            snap,
            true,
            "executor inspected output",
            false,
        )
        .await
        .unwrap();
    let no_verifier = TaskCompletionGate::new(
        f.store.clone(),
        CheckRunner::new(f.tools.clone(), f.root.clone()),
    );
    let guard = no_verifier.inspect(&f.run, &input(0)).await.unwrap();
    assert!(
        guard
            .report()
            .issues
            .iter()
            .any(|i| i.kind == GateIssueKind::IndependentVerification)
    );
    drop(guard);
    let verifier = verifier_for(&f, 0).await;
    let gate = TaskCompletionGate::new(
        f.store.clone(),
        CheckRunner::new(f.tools.clone(), f.root.clone()),
    )
    .with_verifier(verifier.clone());
    let guard = gate.inspect(&f.run, &input(1)).await.unwrap();
    assert!(
        guard.report().issues.is_empty(),
        "{:?}",
        guard.report().issues
    );
    drop(guard);
    assert_eq!(verifier.calls.load(Ordering::SeqCst), 1);
    assert!(
        f.store
            .task_evidence("task")
            .await
            .unwrap()
            .iter()
            .any(|e| e.kind == EvidenceKind::IndependentVerification)
    );
}
#[tokio::test]
async fn verifier_cannot_claim_executor_proof_as_its_own_and_stops_after_two_rounds() {
    let f = fixture(
        "test",
        Some(json!({"kind":"file_contains","path":"source","needle":"OK"})),
        json!(["source"]),
    )
    .await;
    std::fs::write(f.root.join("source"), "OK").unwrap();
    let version = f.store.task("general", "task").await.unwrap().version;
    f.store.host_batch("general","explicit",json!({"ops":[{"op":"update_task","id":"task","expected_version":version,"data":{"metadata":{"independent_verification":true}}}]})).await.unwrap();
    let verifier = verifier_for(&f, 1).await;
    let gate = TaskCompletionGate::new(
        f.store.clone(),
        CheckRunner::new(f.tools.clone(), f.root.clone()),
    )
    .with_verifier(verifier.clone());
    for step in 0..3 {
        let guard = gate.inspect(&f.run, &input(step)).await.unwrap();
        assert!(
            guard
                .report()
                .issues
                .iter()
                .any(|i| i.kind == GateIssueKind::IndependentVerification)
        );
        if step == 2 {
            assert!(
                guard
                    .report()
                    .issues
                    .iter()
                    .any(|i| i.blocked && i.detail.contains("two rounds"))
            );
        }
        drop(guard);
    }
    assert_eq!(verifier.calls.load(Ordering::SeqCst), 2);
}
#[tokio::test]
async fn not_applicable_is_a_versioned_host_decision_and_instruction_changes_revoke_it() {
    let f = fixture("test", None, json!(["source"])).await;
    f.store
        .mark_not_applicable(
            "general",
            "criterion",
            1,
            "No source exists for this explicitly excluded adapter",
            "user",
        )
        .await
        .unwrap();
    let (_, criteria) = f.store.requirements_and_criteria("task").await.unwrap();
    assert_eq!(
        f.store
            .assess_criterion(&criteria[0], 0)
            .await
            .unwrap()
            .state,
        CriterionState::NotApplicable
    );
    let gate = TaskCompletionGate::new(
        f.store.clone(),
        CheckRunner::new(f.tools.clone(), f.root.clone()),
    );
    let guard = gate.inspect(&f.run, &input(0)).await.unwrap();
    assert!(
        guard.report().issues.is_empty(),
        "{:?}",
        guard.report().issues
    );
    drop(guard);
    f.store
        .invalidate_instruction_revision("general", "task", 1)
        .await
        .unwrap();
    assert_eq!(
        f.store
            .assess_criterion(&criteria[0], 1)
            .await
            .unwrap()
            .state,
        CriterionState::Unverified
    );
}
#[cfg(target_os = "linux")]
#[tokio::test]
async fn interrupted_check_child() {
    let Ok(db_path) = std::env::var("EXCALIBUR_CHECK_CRASH_DB") else {
        return;
    };
    let root = PathBuf::from(std::env::var("EXCALIBUR_CHECK_CRASH_ROOT").unwrap());
    let run = std::env::var("EXCALIBUR_CHECK_CRASH_RUN").unwrap();
    let db = Database::open(db_path, DatabaseKind::Main, StorageConfig::default()).unwrap();
    let store = TaskStore::open(db.clone()).await.unwrap();
    let tools = Arc::new(
        LocalTools::new(
            db,
            root.clone(),
            ToolPolicy {
                read: Access::Allow,
                write: Access::Allow,
                exec: Access::Allow,
                ..Default::default()
            },
        )
        .await
        .unwrap(),
    );
    let _ = CheckRunner::new(tools, root)
        .run_keyed(&store, "task", "check", &run, "interrupted-operation")
        .await;
}
#[cfg(target_os = "linux")]
#[tokio::test]
async fn sigkill_after_real_command_effect_never_repeats_unconfirmed_check() {
    let f=fixture("test",Some(json!({"kind":"command","command":"printf 'effect\\n' >> effect; printf ready > ready; sleep 60","expected_exit":0,"timeout_ms":300000})),json!(["source"])).await;
    excalibur_tools::terminal::prepare_project_boundary(&f.root).unwrap();
    let path = f.db.path().to_owned();
    let root = f.root.clone();
    let run = f.run.clone();
    f.tools.terminals().shutdown().await;
    f.db.shutdown().await.unwrap();
    let _temp = f._temp;
    drop(f.tools);
    drop(f.store);
    drop(f.db);
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "interrupted_check_child", "--nocapture"])
        .env("EXCALIBUR_CHECK_CRASH_DB", &path)
        .env("EXCALIBUR_CHECK_CRASH_ROOT", &root)
        .env("EXCALIBUR_CHECK_CRASH_RUN", &run)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::inherit())
        .spawn()
        .unwrap();
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(20);
    while !root.join("ready").exists() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "child never reached its synthetic file barrier"
        );
        assert!(
            child.try_wait().unwrap().is_none(),
            "child exited before effect"
        );
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    child.kill().unwrap();
    child.wait().unwrap();
    assert_eq!(
        std::fs::read_to_string(root.join("effect")).unwrap(),
        "effect\n"
    );
    let db = Database::open(path, DatabaseKind::Main, StorageConfig::default()).unwrap();
    let store = TaskStore::open(db.clone()).await.unwrap();
    let tools = Arc::new(
        LocalTools::new(
            db,
            root.clone(),
            ToolPolicy {
                read: Access::Allow,
                write: Access::Allow,
                exec: Access::Allow,
                ..Default::default()
            },
        )
        .await
        .unwrap(),
    );
    let runner = CheckRunner::new(tools.clone(), root.clone());
    let error = runner
        .run_keyed(&store, "task", "check", &run, "interrupted-operation")
        .await
        .unwrap_err();
    assert!(error.to_string().contains("uncertain"), "{error}");
    assert_eq!(runner.tool_calls(), 0);
    assert_eq!(
        std::fs::read_to_string(root.join("effect")).unwrap(),
        "effect\n"
    );
    tools.terminals().shutdown().await;
}
#[tokio::test]
async fn snapshot_prunes_unrelated_build_trees_and_records_commit_plus_dirty_content() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().to_path_buf();
    std::fs::write(root.join("source"), "one").unwrap();
    std::fs::create_dir(root.join("target")).unwrap();
    for n in 0..4100 {
        std::fs::create_dir(root.join(format!("target/{n}"))).unwrap();
    }
    std::fs::create_dir_all(root.join(".git/refs/heads")).unwrap();
    std::fs::write(root.join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
    std::fs::write(
        root.join(".git/refs/heads/main"),
        format!("{}\n", "a".repeat(40)),
    )
    .unwrap();
    let empty = WorkspaceSnapshot::capture(root.clone(), vec![], 0, vec![], vec![])
        .await
        .unwrap();
    assert!(empty.fingerprints.is_empty());
    let snapshot =
        WorkspaceSnapshot::capture(root.clone(), vec!["source".into()], 0, vec![], vec![])
            .await
            .unwrap();
    assert_eq!(snapshot.commit, Some("a".repeat(40)));
    assert_eq!(snapshot.fingerprints.len(), 1);
    std::fs::write(root.join("source"), "dirty content").unwrap();
    let dirty = WorkspaceSnapshot::capture(root, vec!["source".into()], 0, vec![], vec![])
        .await
        .unwrap();
    assert_eq!(dirty.commit, snapshot.commit);
    assert_ne!(dirty.tree_hash, snapshot.tree_hash);
    assert!(!snapshot.fresh(0).await.unwrap());
}
#[cfg(unix)]
#[tokio::test]
async fn fingerprint_capture_never_reads_cross_authority_hardlinks_and_keeps_internal_links() {
    let f = fixture(
        "test",
        Some(json!({"kind":"file_contains","path":"alias","needle":"private"})),
        json!(["alias"]),
    )
    .await;
    let private = f._temp.path().join("vault");
    std::fs::write(&private, "private-synthetic-vault-value").unwrap();
    std::fs::hard_link(&private, f.root.join("alias")).unwrap();
    let runner = CheckRunner::new(f.tools.clone(), f.root.clone()).with_exclusions(vec![private]);
    let error = runner
        .run(&f.store, "task", "check", &f.run)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("authority"), "{error}");
    assert_eq!(runner.tool_calls(), 0);
    assert!(f.store.task_evidence("task").await.unwrap().is_empty());
    std::fs::remove_file(f.root.join("alias")).unwrap();
    std::fs::write(f.root.join("a"), "shared legitimate source").unwrap();
    std::fs::hard_link(f.root.join("a"), f.root.join("b")).unwrap();
    let snapshot = WorkspaceSnapshot::capture(f.root.clone(), vec!["a".into()], 0, vec![], vec![])
        .await
        .unwrap();
    assert_eq!(snapshot.fingerprints.len(), 1);
    std::fs::write(f.root.join(".env"), "synthetic-private-credential").unwrap();
    std::fs::hard_link(f.root.join(".env"), f.root.join("masked-alias")).unwrap();
    let error = WorkspaceSnapshot::capture(
        f.root.clone(),
        vec!["masked-alias".into()],
        0,
        vec![],
        vec![],
    )
    .await
    .unwrap_err();
    assert!(error.to_string().contains("authority"));
}
#[tokio::test]
async fn user_confirmation_closes_only_the_pending_criterion_without_rerunning_work() {
    let f = fixture("user_confirmation", None, json!([])).await;
    let runner = CheckRunner::new(f.tools.clone(), f.root.clone());
    let gate = TaskCompletionGate::new(f.store.clone(), runner.clone());
    let guard = gate.inspect(&f.run, &input(0)).await.unwrap();
    assert!(
        guard
            .report()
            .issues
            .iter()
            .all(|i| i.kind == GateIssueKind::UserConfirmation)
    );
    drop(guard);
    let journal = Journal::new(f.db.clone()).await.unwrap();
    journal
        .set_state_if(
            &f.run,
            &["running"],
            "completed_with_pending",
            Some("pending user confirmation".into()),
            "pending",
        )
        .await
        .unwrap();
    let snapshot = runner.snapshot(vec![], 0).await.unwrap();
    f.store
        .record_judgment(
            "task",
            "criterion",
            &f.run,
            snapshot,
            true,
            "explicit synthetic user confirmation",
            true,
        )
        .await
        .unwrap();
    let (_, criteria) = f.store.requirements_and_criteria("task").await.unwrap();
    assert_eq!(
        f.store
            .assess_criterion(&criteria[0], 0)
            .await
            .unwrap()
            .state,
        CriterionState::Satisfied
    );
    assert_eq!(runner.tool_calls(), 0);
    assert_eq!(
        journal.state(&f.run).await.unwrap().0,
        "completed_with_pending",
        "terminal delivery history remains immutable"
    );
}
#[tokio::test]
async fn blocking_questions_and_indirect_active_children_prevent_finalization() {
    let f = fixture("test", None, json!([])).await;
    f.store
        .mark_not_applicable("general", "criterion", 1, "No code applies", "host")
        .await
        .unwrap();
    let version = f.store.task("general", "task").await.unwrap().version;
    f.store.host_batch("general","questions",json!({"ops":[{"op":"update_task","id":"task","expected_version":version,"data":{"request_card":{"original":"Synthetic request","assumptions":["synthetic assumption"],"open_questions":[{"question":"Mandatory input?","blocking":true}]}}}]})).await.unwrap();
    let journal = Journal::new(f.db.clone()).await.unwrap();
    let child = journal.create_run("child", json!({})).await.unwrap();
    let descendant = journal.create_run("descendant", json!({})).await.unwrap();
    f.store.bind_child("task", &child, &f.run).await.unwrap();
    f.store
        .bind_child("task", &descendant, &child)
        .await
        .unwrap();
    journal
        .set_state_if(&child, &["running"], "completed", None, "finish-child")
        .await
        .unwrap();
    let gate = TaskCompletionGate::new(
        f.store.clone(),
        CheckRunner::new(f.tools.clone(), f.root.clone()),
    );
    let guard = gate.inspect(&f.run, &input(0)).await.unwrap();
    let report = guard.report();
    assert!(
        report
            .issues
            .iter()
            .any(|i| i.id == descendant && i.kind == GateIssueKind::Child)
    );
    assert!(
        report
            .issues
            .iter()
            .any(|i| i.detail == "Mandatory input?" && i.blocked)
    );
    assert_eq!(report.assumptions, vec!["synthetic assumption"]);
    assert_eq!(report.open_questions, vec!["Mandatory input?"]);
}
#[tokio::test]
async fn direct_evidence_and_verification_apis_enforce_scope_in_addition_to_principal() {
    let f = fixture(
        "test",
        Some(json!({"kind":"file_contains","path":"source","needle":"OK"})),
        json!(["source"]),
    )
    .await;
    std::fs::write(f.root.join("source"), "OK").unwrap();
    let runner = CheckRunner::new(f.tools.clone(), f.root.clone());
    let e = runner.run(&f.store, "task", "check", &f.run).await.unwrap();
    let (_, criteria) = f.store.requirements_and_criteria("task").await.unwrap();
    let restricted = f
        .store
        .clone()
        .with_policy(excalibur_tasks::TaskPolicy::episode("other"));
    assert!(restricted.evidence(&e.id).await.is_err());
    assert!(restricted.task_evidence("task").await.is_err());
    assert!(restricted.requirements_and_criteria("task").await.is_err());
    assert!(restricted.assess_criterion(&criteria[0], 0).await.is_err());
    assert!(
        restricted
            .mark_not_applicable("general", "criterion", 1, "not permitted", "host")
            .await
            .is_err()
    );
    let before = runner.tool_calls();
    assert!(
        runner
            .run(&restricted, "task", "check", &f.run)
            .await
            .is_err()
    );
    assert_eq!(runner.tool_calls(), before);
}
#[cfg(target_os = "linux")]
#[tokio::test]
async fn justified_exemption_skips_an_actual_executable_check_without_side_effects() {
    let f=fixture("test",Some(json!({"kind":"command","command":"printf forbidden > should-not-exist; exit 1","timeout_ms":10000})),json!(["**"])).await;
    f.store
        .mark_not_applicable(
            "general",
            "criterion",
            1,
            "Host excludes this unavailable adapter",
            "user",
        )
        .await
        .unwrap();
    let runner = CheckRunner::new(f.tools.clone(), f.root.clone());
    let gate = TaskCompletionGate::new(f.store.clone(), runner.clone())
        .with_checklist(f.tools.clone(), "tests/README.md".into());
    let guard = gate.inspect(&f.run, &input(0)).await.unwrap();
    assert!(
        guard.report().issues.is_empty(),
        "{:?}",
        guard.report().issues
    );
    assert_eq!(runner.tool_calls(), 0);
    assert!(!f.root.join("should-not-exist").exists());
    assert!(f.store.task_evidence("task").await.unwrap().is_empty());
}
#[tokio::test]
async fn host_stage_verification_runs_own_checks_and_exposes_a_durable_job() {
    let f = fixture(
        "test",
        Some(json!({"kind":"file_contains","path":"source","needle":"OK"})),
        json!(["source"]),
    )
    .await;
    std::fs::write(f.root.join("source"), "OK").unwrap();
    let verifier = verifier_for(&f, 0).await;
    let gate = TaskCompletionGate::new(
        f.store.clone(),
        CheckRunner::new(f.tools.clone(), f.root.clone()),
    )
    .with_verifier(verifier.clone());
    let stage = gate
        .verify_task_stage(&f.run, &input(0), &["criterion".into()])
        .await
        .unwrap();
    assert!(!stage.job_id.is_empty());
    assert!(stage.verdict.criteria[0].passed);
    assert!(
        verifier.inputs.lock().unwrap()[0]
            .verification_scope
            .starts_with("stage:")
    );
    let id = stage.job_id.clone();
    let state =
        f.db.read(ReadLane::Interactive, move |c| {
            Ok(c.query_row(
                "SELECT state FROM task_verifier_jobs WHERE id=?1",
                [id],
                |r| r.get::<_, String>(0),
            )?)
        })
        .await
        .unwrap();
    assert_eq!(state, "completed");
    assert!(
        gate.verify_task_stage(&f.run, &input(1), &["other-scope-criterion".into()])
            .await
            .is_err()
    );
}
#[tokio::test]
async fn evidence_outputs_and_metadata_never_persist_registered_secret_text() {
    let f = fixture("inspection", None, json!([])).await;
    let redactor = excalibur_vault::Redactor::default();
    let secret = "synthetic-registered-private-value";
    redactor
        .register_secret(&excalibur_vault::SecretValue::new(secret.as_bytes().to_vec()).unwrap());
    let store = f.store.clone().with_redactor(redactor);
    let snapshot = WorkspaceSnapshot::capture(f.root.clone(), vec![], 0, vec![], vec![])
        .await
        .unwrap();
    let e = store
        .record_judgment(
            "task",
            "criterion",
            &f.run,
            snapshot.clone(),
            true,
            &format!("observed {secret}"),
            false,
        )
        .await
        .unwrap();
    assert!(!e.explanation.contains(secret));
    let bytes =
        f.db.read_blob(e.output_blob.unwrap().parse().unwrap(), 100000)
            .await
            .unwrap();
    assert!(!String::from_utf8(bytes).unwrap().contains(secret));
    let mut forbidden = snapshot;
    forbidden.fingerprints.insert(secret.into(), "hash".into());
    assert!(
        store
            .record_judgment(
                "task",
                "criterion",
                &f.run,
                forbidden,
                true,
                "synthetic observation",
                false
            )
            .await
            .is_err()
    );
    let rows =
        f.db.read(ReadLane::Interactive, |c| {
            let mut q = c.prepare("SELECT metadata FROM evidence")?;
            Ok(q.query_map([], |r| r.get::<_, String>(0))?
                .collect::<std::result::Result<Vec<_>, _>>()?)
        })
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert!(!rows[0].contains(secret));
}
#[tokio::test]
async fn execution_trace_preserves_old_definition_and_environment_when_a_check_changes() {
    let f = fixture(
        "test",
        Some(json!({"kind":"file_contains","path":"source","needle":"OK"})),
        json!(["source"]),
    )
    .await;
    std::fs::write(f.root.join("source"), "OK").unwrap();
    let runner = CheckRunner::new(f.tools.clone(), f.root.clone());
    let e = runner.run(&f.store, "task", "check", &f.run).await.unwrap();
    assert_eq!(e.producer_snapshot["check"]["definition"]["needle"], "OK");
    assert_eq!(e.producer_snapshot["check_version"], 1);
    assert_eq!(e.producer_snapshot["runtime"]["os"], std::env::consts::OS);
    f.store.batch("general","change-check",json!({"ops":[{"op":"update_check","id":"check","expected_version":1,"data":{"definition":{"kind":"file_contains","path":"source","needle":"changed"},"environment":"new synthetic environment"}}]})).await.unwrap();
    let old = f.store.evidence(&e.id).await.unwrap().unwrap();
    assert!(old.stale);
    assert_eq!(old.producer_snapshot["check"]["definition"]["needle"], "OK");
    let new = runner.run(&f.store, "task", "check", &f.run).await.unwrap();
    assert_eq!(new.result, EvidenceResult::Fail);
    assert_eq!(
        new.producer_snapshot["check"]["environment"],
        "new synthetic environment"
    );
    assert_eq!(new.producer_snapshot["check_version"], 2);
    let edges=f.db.read(ReadLane::Interactive,|c|Ok(c.query_row("SELECT count(*) FROM edges WHERE (source_type='check' AND destination_type='evidence') OR (source_type='evidence' AND destination_type='run')",[],|r|r.get::<_,u64>(0))?)).await.unwrap();
    assert_eq!(edges, 4);
}
struct PendingCheck {
    execution_guard: tokio::sync::Mutex<()>,
    started: tokio::sync::Notify,
    interrupted: AtomicUsize,
}
impl ToolRuntime for PendingCheck {
    fn definitions(&self) -> Vec<ToolDefinition> {
        vec![]
    }
    fn prepare<'a>(
        &'a self,
        _: &'a str,
        call: ToolCall,
    ) -> BoxFuture<'a, Result<PreparedTool, String>> {
        Box::pin(async move {
            Ok(PreparedTool {
                call,
                effect: Effect::NonIdempotent,
                resources: vec![],
                exclusive: true,
                data: json!({}),
            })
        })
    }
    fn execute<'a>(
        &'a self,
        _: &'a str,
        _: &'a str,
        _: &'a PreparedTool,
    ) -> BoxFuture<'a, ToolResult> {
        Box::pin(async move {
            let _guard = self.execution_guard.lock().await;
            self.started.notify_one();
            std::future::pending().await
        })
    }
    fn reconcile<'a>(
        &'a self,
        _: &'a str,
        _: &'a str,
        _: &'a PreparedTool,
    ) -> BoxFuture<'a, Reconcile> {
        Box::pin(async { Reconcile::Uncertain("pending synthetic call".into()) })
    }
    fn interrupt_execution<'a>(
        &'a self,
        _: &'a str,
        _: bool,
        _: Option<&'a PreparedTool>,
    ) -> BoxFuture<'a, Result<bool, String>> {
        Box::pin(async move {
            let _guard = self.execution_guard.lock().await;
            self.interrupted.fetch_add(1, Ordering::SeqCst);
            Ok(true)
        })
    }
}
#[tokio::test(start_paused = true)]
async fn declared_timeout_interrupts_a_hung_adapter_deterministically_and_preserves_uncertain_intent()
 {
    let f = fixture(
        "test",
        Some(json!({"kind":"command","command":"synthetic pending adapter","timeout_ms":250})),
        json!([]),
    )
    .await;
    let runtime = Arc::new(PendingCheck {
        execution_guard: tokio::sync::Mutex::new(()),
        started: tokio::sync::Notify::new(),
        interrupted: AtomicUsize::new(0),
    });
    let runner = CheckRunner::new(runtime.clone(), f.root.clone());
    let store = f.store.clone();
    let run = f.run.clone();
    let task = tokio::spawn(async move { runner.run(&store, "task", "check", &run).await });
    runtime.started.notified().await;
    tokio::time::advance(std::time::Duration::from_millis(250)).await;
    let error = tokio::time::timeout(std::time::Duration::from_secs(1), task)
        .await
        .expect("interruption must acquire the released execution guard")
        .unwrap()
        .unwrap_err();
    assert!(error.to_string().contains("timed out"));
    assert_eq!(runtime.interrupted.load(Ordering::SeqCst), 1);
    assert!(f.store.task_evidence("task").await.unwrap().is_empty());
}
#[cfg(target_os = "linux")]
#[tokio::test]
async fn active_declared_command_releases_task_lock_and_honors_real_pause() {
    let f=fixture("test",Some(json!({"kind":"command","command":"printf ready > control-ready; sleep 60","timeout_ms":300000})),json!([])).await;
    excalibur_tools::terminal::prepare_project_boundary(&f.root).unwrap();
    let runner = CheckRunner::new(f.tools.clone(), f.root.clone());
    let store = f.store.clone();
    let run = f.run.clone();
    let check = tokio::spawn(async move { runner.run(&store, "task", "check", &run).await });
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(30);
    while !f.root.join("control-ready").exists() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "command did not reach synthetic barrier"
        );
        assert!(!check.is_finished(), "check exited before control barrier");
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    let journal = Journal::new(f.db.clone()).await.unwrap();
    tokio::time::timeout(
        std::time::Duration::from_secs(30),
        journal.set_state_if(
            &f.run,
            &["running"],
            "paused",
            Some("synthetic control".into()),
            "pause-active-check",
        ),
    )
    .await
    .unwrap()
    .unwrap();
    let error = tokio::time::timeout(std::time::Duration::from_secs(30), check)
        .await
        .unwrap()
        .unwrap()
        .unwrap_err();
    assert!(error.to_string().contains("interrupted"), "{error}");
    assert!(f.store.task_evidence("task").await.unwrap().is_empty());
    f.tools.terminals().shutdown().await;
}
#[tokio::test]
async fn third_actual_failed_check_requests_a_durable_affected_replan() {
    let f = fixture(
        "test",
        Some(json!({"kind":"file_contains","path":"source","needle":"required"})),
        json!(["source"]),
    )
    .await;
    std::fs::write(f.root.join("source"), "missing predicate").unwrap();
    let runner = CheckRunner::new(f.tools.clone(), f.root.clone());
    for _ in 0..3 {
        assert_eq!(
            runner
                .run(&f.store, "task", "check", &f.run)
                .await
                .unwrap()
                .result,
            EvidenceResult::Fail
        );
    }
    assert!(!f.store.pending_replans("task").await.unwrap().is_empty());
}
#[tokio::test]
async fn no_registered_checks_does_not_create_a_readme_or_test_directory() {
    let f = fixture("inspection", None, json!([])).await;
    let projection = ChecklistProjection::new(f.tools.clone(), "tests/README.md".into());
    assert!(!projection.update(&f.store, &f.run, &[]).await.unwrap());
    assert!(!f.root.join("tests").exists());
}
#[tokio::test]
async fn final_gate_runs_registered_project_checks_without_importing_other_task_requirements() {
    let f = fixture(
        "test",
        Some(json!({"kind":"file_contains","path":"source","needle":"OK"})),
        json!(["source"]),
    )
    .await;
    std::fs::write(f.root.join("source"), "OK").unwrap();
    let version = f.store.task("general", "task").await.unwrap().version;
    f.store.host_batch("general","project-catalog",json!({"ops":[
        {"op":"create_project","id":"project","data":{"name":"Synthetic project","roots":[f.root]}},
        {"op":"update_task","id":"task","expected_version":version,"data":{"project_id":"project"}},
        {"op":"create_task","id":"historical-task","title":"Registered regression","data":{"project_id":"project","request_card":{"original":"Historical request"}}},
        {"op":"create_requirement","id":"historical-requirement","data":{"text":"Preserve historical behavior","state":"accepted","project_id":"project"}},
        {"op":"create_criterion","id":"historical-criterion","data":{"requirement_id":"historical-requirement","statement":"Registered regression passes","method":"test","impact_area":["source"]}},
        {"op":"create_check","id":"historical-check","data":{"criterion_id":"historical-criterion","definition":{"kind":"file_contains","path":"source","needle":"MISSING"}}},
        {"op":"link_requirement","task":"historical-task","requirement":"historical-requirement"}
    ]})).await.unwrap();
    let gate = TaskCompletionGate::new(
        f.store.clone(),
        CheckRunner::new(f.tools.clone(), f.root.clone()),
    )
    .with_checklist(f.tools.clone(), "tests/README.md".into());
    let guard = gate.inspect(&f.run, &input(0)).await.unwrap();
    assert!(
        guard
            .report()
            .issues
            .iter()
            .any(|i| i.kind == GateIssueKind::ProjectCheck)
    );
    drop(guard);
    let (reqs, _) = f.store.requirements_and_criteria("task").await.unwrap();
    assert_eq!(
        reqs.len(),
        1,
        "project checks must not become acceptance requirements of the current task"
    );
    let evidence = f.store.task_evidence("task").await.unwrap();
    assert!(
        evidence
            .iter()
            .any(|e| e.check_id == "historical-check" && e.result == EvidenceResult::Fail)
    );
    let readme = std::fs::read_to_string(f.root.join("tests/README.md")).unwrap();
    assert!(readme.contains("historical-check"));
    assert!(readme.contains("failed"));
}
#[tokio::test]
async fn structured_original_message_sources_are_preserved_without_executor_fields() {
    let f = fixture(
        "test",
        Some(json!({"kind":"file_contains","path":"source","needle":"OK"})),
        json!(["source"]),
    )
    .await;
    std::fs::write(f.root.join("source"), "OK").unwrap();
    let version = f.store.task("general", "task").await.unwrap().version;
    f.store.host_batch("general","source-messages",json!({"ops":[{"op":"update_task","id":"task","expected_version":version,"data":{"metadata":{"independent_verification":true},"request_card":{"original":[{"msg_id":"message-a","text":"First literal request","reasoning":"never expose executor chain"},{"msg_id":"message-b","text":"Second literal request"}]}}}]})).await.unwrap();
    let verifier = verifier_for(&f, 0).await;
    let gate = TaskCompletionGate::new(
        f.store.clone(),
        CheckRunner::new(f.tools.clone(), f.root.clone()),
    )
    .with_verifier(verifier.clone());
    let guard = gate.inspect(&f.run, &input(0)).await.unwrap();
    assert!(
        guard.report().issues.is_empty(),
        "{:?}",
        guard.report().issues
    );
    drop(guard);
    let request = verifier.inputs.lock().unwrap()[0].literal_request.clone();
    assert!(request.contains("First literal request") && request.contains("message-b"));
    assert!(!request.contains("never expose executor chain"));
}
#[tokio::test]
async fn independent_input_includes_latest_applied_instructions_and_preserves_original_request() {
    let f = fixture(
        "test",
        Some(json!({"kind":"file_contains","path":"source","needle":"OK"})),
        json!(["source"]),
    )
    .await;
    std::fs::write(f.root.join("source"), "OK").unwrap();
    let runner = CheckRunner::new(f.tools.clone(), f.root.clone());
    let old = runner.run(&f.store, "task", "check", &f.run).await.unwrap();
    let current = f.store.task("general", "task").await.unwrap();
    f.store.host_batch("general","review-requested",json!({"ops":[{"op":"update_task","id":"task","expected_version":current.version,"data":{"metadata":{"independent_verification":true}}}]})).await.unwrap();
    let journal = Journal::new(f.db.clone()).await.unwrap();
    journal
        .append(
            &f.run,
            "directive_applied",
            "current-directive",
            json!({"id":"instruction-new","text":"Also preserve the synthetic latest constraint"}),
        )
        .await
        .unwrap();
    f.store
        .invalidate_instruction_revision("general", "task", 1)
        .await
        .unwrap();
    assert!(!old.snapshot.fresh(1).await.unwrap());
    let verifier = verifier_for(&f, 0).await;
    let gate = TaskCompletionGate::new(f.store.clone(), runner).with_verifier(verifier.clone());
    let mut current_input = input(1);
    current_input.instruction_revision = 1;
    let guard = gate.inspect(&f.run, &current_input).await.unwrap();
    assert!(
        guard.report().issues.is_empty(),
        "{:?}",
        guard.report().issues
    );
    drop(guard);
    let captured = verifier.inputs.lock().unwrap();
    assert_eq!(captured[0].literal_request, "Check the synthetic source");
    assert!(
        captured[0]
            .applicable_instructions
            .iter()
            .any(|i| i.id == "instruction-new"
                && i.revision == 1
                && i.text.contains("latest constraint"))
    );
}
#[tokio::test]
async fn evidence_owns_one_blob_reference_and_deletion_releases_that_reference() {
    let f = fixture("inspection", None, json!([])).await;
    let snapshot = WorkspaceSnapshot::capture(f.root.clone(), vec![], 0, vec![], vec![])
        .await
        .unwrap();
    let e = f
        .store
        .record_judgment(
            "task",
            "criterion",
            &f.run,
            snapshot,
            true,
            "synthetic inspection",
            false,
        )
        .await
        .unwrap();
    let id = e.id.clone();
    let count =
        f.db.read(ReadLane::Interactive, move |c| {
            Ok(c.query_row(
                "SELECT count(*) FROM blob_refs WHERE owner_type='evidence' AND owner_id=?1",
                [id],
                |r| r.get::<_, u64>(0),
            )?)
        })
        .await
        .unwrap();
    assert_eq!(count, 1);
    let id = e.id.clone();
    f.db.write(excalibur_store::Priority::Normal, move |c| {
        c.execute(
            "DELETE FROM edges WHERE source_id=?1 OR destination_id=?1",
            [&id],
        )?;
        c.execute("DELETE FROM evidence WHERE id=?1", [id])?;
        Ok(())
    })
    .await
    .unwrap();
    let id = e.id;
    let count =
        f.db.read(ReadLane::Interactive, move |c| {
            Ok(c.query_row(
                "SELECT count(*) FROM blob_refs WHERE owner_type='evidence' AND owner_id=?1",
                [id],
                |r| r.get::<_, u64>(0),
            )?)
        })
        .await
        .unwrap();
    assert_eq!(count, 0);
}
#[tokio::test]
async fn verifier_binding_cannot_be_reparented_or_reuse_the_parent_itself() {
    let f = fixture("inspection", None, json!([])).await;
    let journal = Journal::new(f.db.clone()).await.unwrap();
    let child = journal.create_run("bound-child", json!({})).await.unwrap();
    let other = journal.create_run("other-parent", json!({})).await.unwrap();
    f.store
        .register_verifier_run("task", &child, &f.run)
        .await
        .unwrap();
    f.store
        .register_verifier_run("task", &other, &f.run)
        .await
        .unwrap();
    assert!(
        f.store
            .register_verifier_run("task", &child, &other)
            .await
            .is_err()
    );
    assert!(
        f.store
            .register_verifier_run("task", &f.run, &f.run)
            .await
            .is_err()
    );
}
#[tokio::test]
async fn independent_verdict_requires_its_declared_parent_even_with_real_own_proof() {
    let f = fixture(
        "test",
        Some(json!({"kind":"file_contains","path":"source","needle":"OK"})),
        json!(["source"]),
    )
    .await;
    std::fs::write(f.root.join("source"), "OK").unwrap();
    let version = f.store.task("general", "task").await.unwrap().version;
    f.store.host_batch("general","review",json!({"ops":[{"op":"update_task","id":"task","expected_version":version,"data":{"metadata":{"independent_verification":true}}}]})).await.unwrap();
    let verifier = verifier_for(&f, 4).await;
    let gate = TaskCompletionGate::new(
        f.store.clone(),
        CheckRunner::new(f.tools.clone(), f.root.clone()),
    )
    .with_verifier(verifier);
    let guard = gate.inspect(&f.run, &input(0)).await.unwrap();
    assert!(
        guard
            .report()
            .issues
            .iter()
            .any(|i| i.kind == GateIssueKind::IndependentVerification
                && i.detail.contains("different parent")),
        "{:?}",
        guard.report().issues
    );
}
