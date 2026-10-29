use excalibur_engine::AdmissionState;
use excalibur_journal::Journal;
use excalibur_store::{Database, DatabaseKind, Priority, ReadLane, StorageConfig};
use excalibur_tasks::{PromotionInput, TaskPolicy, TaskStore, WorkLevel};
use serde_json::json;

async fn fixture() -> (tempfile::TempDir, Database, TaskStore, Journal) {
    let dir = tempfile::tempdir().unwrap();
    let db = Database::open(
        dir.path().join("main.db"),
        DatabaseKind::Main,
        StorageConfig::default(),
    )
    .unwrap();
    let store = TaskStore::open(db.clone()).await.unwrap();
    let journal = Journal::new(db.clone()).await.unwrap();
    (dir, db, store, journal)
}
async fn run(journal: &Journal, id: &str, created: bool) -> String {
    let spec = excalibur_engine::RunSpec {
        context: Default::default(),
        budget: Default::default(),
        criteria: vec![excalibur_engine::Criterion::FinalResponse { schema: None }],
    };
    journal.create_run(id,json!({"spec":spec,"started_at":excalibur_store::unix_seconds(),"gateway_identity":"synthetic","initial_state":if created {"created"}else{"running"}})).await.unwrap()
}
async fn create(store: &TaskStore, scope: &str, id: &str, level: &str) {
    store
        .batch(
            scope,
            &format!("create:{id}"),
            json!({"ops":[{"op":"create_task","id":id,"title":id,"data":{"level":level}}]}),
        )
        .await
        .unwrap();
}
async fn macro_item(db: &Database, task: &str) -> (String, String, u64) {
    let task = task.to_owned();
    db.read(ReadLane::Interactive,move|c|Ok(c.query_row("SELECT i.plan_id,i.id,p.version FROM tasks t JOIN plan_items i ON i.id=t.macro_item_id JOIN plans p ON p.id=i.plan_id WHERE t.id=?1",[task],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?)))?)).await.unwrap()
}
#[tokio::test]
async fn macro_approval_is_exact_current_and_cannot_acquire_host_privilege() {
    let (_dir, db, store, journal) = fixture().await;
    create(&store, "general", "task", "N3").await;
    let root = run(&journal, "root", false).await;
    store.bind_root("task", &root).await.unwrap();
    assert!(store.effect_admission(&root).await.is_err());
    let (plan, item, version) = macro_item(&db, "task").await;
    for op in [
        json!({"op":"update_plan","id":plan,"expected_version":version,"data":{"state":"active"}}),
        json!({"op":"create_plan","id":"fake","data":{"kind":"macro","state":"approved"}}),
    ] {
        assert!(
            store
                .batch("general", &op.to_string(), json!({"ops":[op]}))
                .await
                .is_err()
        );
    }
    let changes = json!([{"op":"update_plan_item","id":item,"expected_version":1,"data":{"title":"Approved work"}}]);
    let proposal = store
        .propose_replan(
            "general",
            &plan,
            version,
            "proposal",
            changes.clone(),
            vec!["task".into()],
            "User reviews plan",
        )
        .await
        .unwrap();
    assert!(store.batch("general","without-approval",json!({"ops":[{"op":"apply_plan","id":plan,"expected_version":version,"changed":["task"],"ops":changes}]})).await.is_err());
    store
        .approve_plan(proposal["id"].as_str().unwrap(), true, "synthetic-host")
        .await
        .unwrap();
    store.batch("general","apply",json!({"ops":[{"op":"apply_plan","id":plan,"expected_version":version,"approval":proposal["id"],"changed":["task"],"ops":changes}]})).await.unwrap();
    store.effect_admission(&root).await.unwrap();
    let direct = json!({"ops":[{"op":"update_plan_item","id":item,"expected_version":2,"data":{"title":"Unapproved change"}}]});
    assert!(
        store
            .batch("general", "direct", direct.clone())
            .await
            .is_err()
    );
    // Progress alone remains admissible; structural host edits create another plan version.
    store.batch("general","progress",json!({"ops":[{"op":"update_plan_item","id":item,"expected_version":2,"data":{"state":"in_progress"}}]})).await.unwrap();
    store.effect_admission(&root).await.unwrap();
    store.host_batch("general","host-semantic",json!({"ops":[{"op":"update_plan_item","id":item,"expected_version":3,"data":{"title":"New host scope"}}]})).await.unwrap();
    assert!(store.effect_admission(&root).await.is_err());
    let prior = store
        .plan_version("general", &plan, version + 1)
        .await
        .unwrap();
    assert_eq!(prior["items"][0]["title"], "Approved work");
    let (_, _, current) = macro_item(&db, "task").await;
    let task_version = store.task("general", "task").await.unwrap().version;
    let forged = json!([{"op":"update_task","id":"task","expected_version":task_version,"data":{"request_card":{"literal_original":"Erased request"}}}]);
    let proposal = store
        .propose_replan(
            "general",
            &plan,
            current,
            "malicious",
            forged.clone(),
            vec!["task".into()],
            "must not elevate",
        )
        .await
        .unwrap();
    store
        .approve_plan(proposal["id"].as_str().unwrap(), true, "synthetic-host")
        .await
        .unwrap();
    assert!(store.batch("general","forged",json!({"ops":[{"op":"apply_plan","id":plan,"expected_version":current,"approval":proposal["id"],"changed":["task"],"ops":forged}]})).await.is_err());
    db.shutdown().await.unwrap();
}
#[tokio::test]
async fn low_autonomy_requires_macro_approval_but_short_general_task_does_not() {
    let (_dir, db, store, journal) = fixture().await;
    let store = store.with_policy(TaskPolicy {
        autonomy: 1,
        ..Default::default()
    });
    create(&store, "general", "short", "N1").await;
    let short = run(&journal, "short", false).await;
    store.bind_root("short", &short).await.unwrap();
    store.effect_admission(&short).await.unwrap();
    create(&store, "general", "project-work", "N2").await;
    let root = run(&journal, "project-work", false).await;
    store.bind_root("project-work", &root).await.unwrap();
    assert!(store.effect_admission(&root).await.is_err());
    let (plan, item, version) = macro_item(&db, "project-work").await;
    let change = json!([{"op":"update_plan_item","id":item,"expected_version":1,"data":{"detail":"Reviewable scope"}}]);
    let proposed=store.batch("general","propose",json!({"ops":[{"op":"propose_plan","id":plan,"expected_version":version,"ops":change,"changed":["project-work"],"reason":"approve scope"}]})).await.unwrap();
    let proposal = &proposed["results"][0]["id"];
    store
        .approve_plan(proposal.as_str().unwrap(), true, "host")
        .await
        .unwrap();
    store.batch("general","apply",json!({"ops":[{"op":"apply_plan","id":plan,"expected_version":version,"ops":change,"approval":proposal,"changed":["project-work"]}]})).await.unwrap();
    store.effect_admission(&root).await.unwrap();
    db.shutdown().await.unwrap();
}
#[tokio::test]
async fn deferred_created_pause_is_durable_exact_and_old_admission_cannot_repause() {
    let (_dir, db, store, journal) = fixture().await;
    create(&store, "general", "task", "N1").await;
    let root = run(&journal, "root", false).await;
    let created = run(&journal, "created-child", true).await;
    store.bind_root("task", &root).await.unwrap();
    store.bind_child("task", &created, &root).await.unwrap();
    let pause = store.pause_subtree("task", "pause").await.unwrap();
    assert_eq!(pause.deferred_runs, vec![created.clone()]);
    assert_eq!(journal.state(&created).await.unwrap().0, "created");
    let reopened = TaskStore::open(db.clone()).await.unwrap();
    assert_eq!(
        reopened.admission_state(&created).await.unwrap(),
        AdmissionState::Paused(format!("control:{}", pause.id))
    );
    let resumed = reopened.resume_subtree(&pause.id, "resume").await.unwrap();
    assert_eq!(resumed.runs, vec![root.clone()]);
    assert_eq!(
        reopened.admission_state(&created).await.unwrap(),
        AdmissionState::Ready
    );
    // A stale admission collected before resume must not resurrect the pause fence.
    journal
        .set_state(&created, "ready", None, "ready")
        .await
        .unwrap();
    journal
        .set_state(
            &created,
            "paused",
            Some(format!("control:{}", pause.id)),
            "stale-admission",
        )
        .await
        .unwrap();
    let event = journal
        .replay(&created, 0)
        .await
        .unwrap()
        .into_iter()
        .last()
        .unwrap();
    reopened.observe_event(&created, &event).await.unwrap();
    assert_eq!(journal.state(&created).await.unwrap().0, "ready");
    let pause2 = reopened.pause_subtree("task", "pause2").await.unwrap();
    // Now ready, both are actually paused, and exact resume includes both.
    assert!(pause2.deferred_runs.is_empty());
    assert_eq!(
        reopened
            .resume_subtree(&pause2.id, "resume2")
            .await
            .unwrap()
            .runs
            .len(),
        2
    );
    db.shutdown().await.unwrap();
}
#[tokio::test]
async fn confirmation_cancels_current_subtree_and_paused_parent_rejects_new_binding() {
    let (_dir, db, store, journal) = fixture().await;
    create(&store, "general", "task", "N1").await;
    let root = run(&journal, "root", false).await;
    store.bind_root("task", &root).await.unwrap();
    let pending = store.cancel_subtree("task", "cancel", false).await.unwrap();
    assert!(pending.confirmation_required);
    let child = run(&journal, "child-after-request", true).await;
    store.bind_child("task", &child, &root).await.unwrap();
    let applied = store.cancel_subtree("task", "cancel", true).await.unwrap();
    assert_eq!(applied.runs.len(), 2);
    assert_eq!(journal.state(&child).await.unwrap().0, "cancelled");
    assert_eq!(journal.state(&root).await.unwrap().0, "cancelled");
    let another = run(&journal, "disallowed", true).await;
    assert!(store.bind_child("task", &another, &root).await.is_err());
    let next = run(&journal, "reopened", false).await;
    store.reopen("task", &next, "new work").await.unwrap();
    let pause = store.pause_subtree("task", "pause").await.unwrap();
    assert!(store.bind_child("task", &another, &next).await.is_err());
    store.resume_subtree(&pause.id, "resume").await.unwrap();
    let snapshot = store.snapshot("task").await.unwrap();
    assert_eq!(snapshot["task"]["root_run_id"], next);
    assert!(snapshot["latest_report"].is_object());
    db.shutdown().await.unwrap();
}
#[tokio::test]
async fn known_ids_do_not_bypass_scope_or_principal_for_controls_plans_and_bindings() {
    let (_dir, db, store, journal) = fixture().await;
    db.write(Priority::Normal, |c| {
        c.execute("INSERT INTO principals(id) VALUES('bob')", [])?;
        Ok(())
    })
    .await
    .unwrap();
    let bob = store.clone().with_policy(TaskPolicy {
        principal_id: Some("bob".into()),
        allowed_scopes: Some(["private".into()].into_iter().collect()),
        ..Default::default()
    });
    create(&bob, "private", "private-task", "N3").await;
    let root = run(&journal, "private-root", false).await;
    let lookup = root.clone();
    db.write(Priority::Journal, move |c| {
        c.execute(
            "UPDATE runs SET principal_id='bob',scope_id='private' WHERE id=?1",
            [lookup],
        )?;
        Ok(())
    })
    .await
    .unwrap();
    bob.bind_root("private-task", &root).await.unwrap();
    let pause = bob.pause_subtree("private-task", "pause").await.unwrap();
    let (plan, item, version) = macro_item(&db, "private-task").await;
    let proposal=bob.propose_replan("private",&plan,version,"proposal",json!([{"op":"update_plan_item","id":item,"expected_version":1,"data":{"title":"private"}}]),vec!["private-task".into()],"private").await.unwrap();
    for unauthorized in [
        store.clone(),
        bob.clone().with_policy(TaskPolicy {
            principal_id: Some("bob".into()),
            allowed_scopes: Some(["other".into()].into_iter().collect()),
            ..Default::default()
        }),
    ] {
        assert!(
            unauthorized
                .resume_subtree(&pause.id, "escape")
                .await
                .is_err()
        );
        assert!(
            unauthorized
                .approve_plan(proposal["id"].as_str().unwrap(), true, "host")
                .await
                .is_err()
        );
        assert!(unauthorized.bind_root("private-task", &root).await.is_err());
        assert!(unauthorized.snapshot("private-task").await.is_err());
    }
    assert_eq!(journal.state(&root).await.unwrap().0, "paused");
    bob.resume_subtree(&pause.id, "valid").await.unwrap();
    db.shutdown().await.unwrap();
}
#[tokio::test]
async fn automatic_promotion_without_project_creates_real_macro_link_and_replan_outbox_is_replay_safe()
 {
    let (_dir, db, store, journal) = fixture().await;
    create(&store, "general", "task", "N1").await;
    let root = run(&journal, "root", false).await;
    store.bind_root("task", &root).await.unwrap();
    store.batch("general","requirements",json!({"ops":[{"op":"create_requirement","id":"req","data":{"text":"Original","state":"accepted"}},{"op":"add_criterion","id":"crit","requirement":"req","statement":"Observable","method":"test"},{"op":"link_requirement","task":"task","requirement":"req"}]})).await.unwrap();
    let promoted = store
        .promote(
            "task",
            PromotionInput {
                touched_project: None,
                steps: 16,
                estimated_seconds: None,
                milestone_count: 0,
                user_requests_initiative: false,
                forced_level: None,
            },
        )
        .await
        .unwrap();
    assert_eq!(promoted.level, WorkLevel::N2);
    assert!(promoted.macro_item_id.is_some());
    store.batch("general","changed",json!({"ops":[{"op":"update_requirement","id":"req","expected_version":1,"data":{"text":"Changed"}}]})).await.unwrap();
    let requests = store.pending_replans("task").await.unwrap();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0]["reason"], "requirement changed");
    assert!(
        store
            .compact_micro_plan(&root)
            .await
            .unwrap()
            .unwrap()
            .contains("Pending replan")
    );
    journal
        .append(
            &root,
            "directive_applied",
            "applied",
            json!({"id":"external-user-instruction","text":"User asks replan"}),
        )
        .await
        .unwrap();
    let event = journal
        .replay(&root, 0)
        .await
        .unwrap()
        .into_iter()
        .last()
        .unwrap();
    store.observe_event(&root, &event).await.unwrap();
    let count = store.pending_replans("task").await.unwrap().len();
    assert!(count >= 2);
    store.observe_event(&root, &event).await.unwrap();
    assert_eq!(store.pending_replans("task").await.unwrap().len(), count);
    let snapshot = store.snapshot("task").await.unwrap();
    assert_eq!(snapshot["tree"][0]["run_id"], root);
    assert!(snapshot["progress"]["total"].as_u64().unwrap() > 0);
    db.shutdown().await.unwrap();
}
#[tokio::test]
async fn additive_initialization_preserves_canonical_and_legacy_untrusted_data() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("main.db");
    let db = Database::open(&path, DatabaseKind::Main, StorageConfig::default()).unwrap();
    db.write(Priority::Normal,|c|{c.execute("INSERT INTO tasks(id,code,scope_id) VALUES('existing','T0','general')",[])?;c.execute_batch("CREATE TABLE eval_episode_records(scope TEXT,id TEXT,entity TEXT,version INTEGER,data TEXT,PRIMARY KEY(scope,id));CREATE TABLE eval_task_receipts(scope TEXT,operation_key TEXT,request_hash TEXT,result TEXT);")?;c.execute("INSERT INTO eval_episode_records VALUES('general','legacy','criterion',7,?1)",[json!({"status":"satisfied","trusted":true}).to_string()])?;Ok(())}).await.unwrap();
    let store = TaskStore::open(db.clone()).await.unwrap();
    assert_eq!(store.task("general", "existing").await.unwrap().code, "T0");
    let legacy = store
        .batch(
            "general",
            "query",
            json!({"ops":[{"op":"query","entity":"criterion"}]}),
        )
        .await
        .unwrap();
    assert!(
        legacy["results"][0]["rows"]
            .as_array()
            .unwrap()
            .iter()
            .all(|row| row["trusted"] != true)
    );
    let counts = db
        .read(ReadLane::Interactive, |c| {
            Ok((
                c.query_row("SELECT count(*) FROM task_drafts", [], |r| {
                    r.get::<_, u64>(0)
                })?,
                c.query_row("SELECT count(*) FROM criteria", [], |r| r.get::<_, u64>(0))?,
            ))
        })
        .await
        .unwrap();
    assert_eq!(counts, (1, 0));
    drop(store);
    db.shutdown().await.unwrap();
    drop(db);
    let db = Database::open(&path, DatabaseKind::Main, StorageConfig::default()).unwrap();
    let store = TaskStore::open(db.clone()).await.unwrap();
    assert_eq!(store.task("general", "existing").await.unwrap().code, "T0");
    db.shutdown().await.unwrap();
}

struct StageVerifier {
    store: TaskStore,
    workspace: std::path::PathBuf,
    tools: std::sync::Arc<excalibur_tools::LocalTools>,
}
impl excalibur_tasks::IndependentVerifier for StageVerifier {
    fn verify<'a>(
        &'a self,
        input: &'a excalibur_tasks::VerificationInput,
    ) -> excalibur_types::runtime::BoxFuture<'a, Result<excalibur_tasks::VerificationVerdict, String>>
    {
        Box::pin(async move {
            let journal = Journal::new(self.store.database().clone())
                .await
                .map_err(|e| e.to_string())?;
            let child = journal
                .create_run(&input.job_id, json!({"role":"verifier"}))
                .await
                .map_err(|e| e.to_string())?;
            self.store
                .register_verifier_run(&input.task_id, &child, &input.root_run_id)
                .await
                .map_err(|e| e.to_string())?;
            let runner =
                excalibur_tasks::CheckRunner::new(self.tools.clone(), self.workspace.clone());
            let mut verdicts = Vec::new();
            for criterion in &input.criteria {
                let mut evidence_ids = Vec::new();
                for check in &criterion.checks {
                    let evidence = runner
                        .run(&self.store, &input.task_id, &check.id, &child)
                        .await
                        .map_err(|e| e.to_string())?;
                    assert_eq!(evidence.result, excalibur_tasks::EvidenceResult::Pass);
                    evidence_ids.push(evidence.id);
                }
                verdicts.push(excalibur_tasks::CriterionVerdict {
                    criterion_id: criterion.id.clone(),
                    passed: true,
                    explanation: "Own synthetic file assertion".into(),
                    evidence_ids,
                });
            }
            journal
                .set_state(&child, "completed", None, "done")
                .await
                .map_err(|e| e.to_string())?;
            Ok(excalibur_tasks::VerificationVerdict {
                verifier_run_id: child,
                criteria: verdicts,
                uncovered_requests: vec![],
                request_covered: true,
            })
        })
    }
}
#[tokio::test]
async fn milestone_closure_requires_own_fresh_proof_or_versioned_host_exemption() {
    use excalibur_tasks::{CheckRunner, TaskCompletionGate};
    use excalibur_tools::{Access, LocalTools, ToolPolicy};
    use std::sync::Arc;
    let (dir, db, store, journal) = fixture().await;
    let workspace = dir.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    std::fs::write(workspace.join("source"), "OK").unwrap();
    store.host_batch("general","seed",json!({"ops":[
        {"op":"create_task","id":"task","title":"Synthetic task","data":{"metadata":{"workspace":workspace},"request_card":{"original":"Verify source"}}},
        {"op":"create_plan","id":"plan","data":{"kind":"macro","title":"Stages"}},
        {"op":"create_requirement","id":"req","data":{"text":"Source is OK","state":"accepted"}},
        {"op":"add_criterion","id":"criterion","requirement":"req","statement":"Source is OK","method":"test","impact_area":["source"]},
        {"op":"add_check","id":"check","criterion":"criterion","definition":{"kind":"file_contains","path":"source","needle":"OK"}},
        {"op":"link_requirement","task":"task","requirement":"req"},
        {"op":"create_milestone","id":"milestone","data":{"plan_id":"plan","title":"Verified stage","criteria":["criterion"]}}
    ]})).await.unwrap();
    let root = run(&journal, "root", false).await;
    store.bind_root("task", &root).await.unwrap();
    assert!(
        store
            .close_milestone("general", "milestone", 1, &root, "")
            .await
            .is_err()
    );
    assert!(store.batch("general","model-close",json!({"ops":[{"op":"update_milestone","id":"milestone","expected_version":1,"data":{"state":"done"}}]})).await.is_err());
    let tools = Arc::new(
        LocalTools::new(
            db.clone(),
            workspace.clone(),
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
    let verifier = Arc::new(StageVerifier {
        store: store.clone(),
        workspace: workspace.clone(),
        tools: tools.clone(),
    });
    let gate = TaskCompletionGate::new(store.clone(), CheckRunner::new(tools, workspace.clone()))
        .with_verifier(verifier);
    let input = excalibur_engine::GateInput {
        step: 0,
        instruction_revision: 0,
        final_text: "Source verified".into(),
        local_pending: vec![],
        budget_remaining: Default::default(),
    };
    let stage = gate
        .verify_task_stage(&root, &input, &["criterion".into()])
        .await
        .unwrap();
    let closed = store
        .close_milestone("general", "milestone", 1, &root, &stage.job_id)
        .await
        .unwrap();
    assert_eq!(closed["state"], "done");
    assert_eq!(closed["version"], 2);
    let replay = store
        .close_milestone("general", "milestone", 2, &root, &stage.job_id)
        .await
        .unwrap();
    assert_eq!(replay["version"], 2);
    std::fs::write(workspace.join("source"), "Changed").unwrap();
    assert!(
        store
            .close_milestone("general", "milestone", 2, &root, &stage.job_id)
            .await
            .is_err()
    );
    store
        .mark_not_applicable(
            "general",
            "criterion",
            1,
            "Host removed this code requirement",
            "synthetic-user",
        )
        .await
        .unwrap();
    let waived = store
        .close_milestone("general", "milestone", 2, &root, "")
        .await
        .unwrap();
    assert_eq!(waived["version"], 3);
    assert!(waived["verifier_job_id"].is_null());
    // The exemption is tied to the exact criterion version; semantic updates revoke it.
    store.batch("general","criterion-changed",json!({"ops":[{"op":"update_criterion","id":"criterion","expected_version":1,"data":{"statement":"New observable condition"}}]})).await.unwrap();
    assert!(
        store
            .close_milestone("general", "milestone", 3, &root, "")
            .await
            .is_err()
    );
    db.shutdown().await.unwrap();
}
#[tokio::test]
async fn proposal_operation_identity_binds_actor_scope_plan_version_and_diff() {
    let (_dir, db, store, _) = fixture().await;
    db.write(Priority::Normal, |c| {
        c.execute("INSERT INTO principals(id) VALUES('bob')", [])?;
        Ok(())
    })
    .await
    .unwrap();
    for (scope, id) in [
        ("general", "plan-a"),
        ("general", "plan-b"),
        ("other", "plan-c"),
    ] {
        store
            .batch(
                scope,
                &format!("create:{id}"),
                json!({"ops":[{"op":"create_plan","id":id,"data":{"kind":"macro","title":id}}]}),
            )
            .await
            .unwrap();
    }
    let bob = store.clone().with_policy(TaskPolicy {
        principal_id: Some("bob".into()),
        ..Default::default()
    });
    bob.batch(
        "general",
        "bob-plan",
        json!({"ops":[{"op":"create_plan","id":"plan-bob","data":{"kind":"macro","title":"Bob"}}]}),
    )
    .await
    .unwrap();
    let diff =
        json!([{"op":"create_notebook","id":"note","data":{"text":"same synthetic payload"}}]);
    let first = store
        .propose_replan(
            "general",
            "plan-a",
            1,
            "shared-key",
            diff.clone(),
            vec![],
            "same reason",
        )
        .await
        .unwrap();
    assert_eq!(
        first,
        store
            .propose_replan(
                "general",
                "plan-a",
                1,
                "shared-key",
                diff.clone(),
                vec![],
                "same reason"
            )
            .await
            .unwrap()
    );
    for (store, scope, plan, version) in [
        (&store, "general", "plan-b", 1),
        (&store, "other", "plan-c", 1),
        (&bob, "general", "plan-bob", 1),
        (&store, "general", "plan-a", 2),
    ] {
        let err = store
            .propose_replan(
                scope,
                plan,
                version,
                "shared-key",
                diff.clone(),
                vec![],
                "same reason",
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("proposal key reused"), "{err}");
        assert!(!err.to_string().contains(first["id"].as_str().unwrap()));
    }
    let changed = json!([{"op":"create_notebook","id":"note","data":{"text":"changed payload"}}]);
    assert!(
        store
            .propose_replan(
                "general",
                "plan-a",
                1,
                "shared-key",
                changed,
                vec![],
                "same reason"
            )
            .await
            .is_err()
    );
    // Exact retries survive later plan versions without reapplying or returning a different receipt.
    store.batch("general","later-version",json!({"ops":[{"op":"update_plan","id":"plan-a","expected_version":1,"data":{"title":"New version"}}]})).await.unwrap();
    assert_eq!(
        first,
        store
            .propose_replan(
                "general",
                "plan-a",
                1,
                "shared-key",
                diff,
                vec![],
                "same reason"
            )
            .await
            .unwrap()
    );
    db.shutdown().await.unwrap();
}

#[tokio::test]
async fn reopened_root_has_its_own_instruction_epoch_and_never_revives_old_proof_or_exemptions() {
    use excalibur_engine::{CompletionGate, GateInput};
    use excalibur_tasks::{CheckRunner, CriterionState, TaskCompletionGate};
    use excalibur_tools::{Access, LocalTools, ToolPolicy};
    use std::sync::Arc;
    let (dir, db, store, journal) = fixture().await;
    let workspace = dir.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    std::fs::write(workspace.join("source"), "OK").unwrap();
    store.host_batch("general","seed",json!({"ops":[
        {"op":"create_task","id":"task","title":"Reopen actual work","data":{"request_card":{"original":"Verify the synthetic source"},"metadata":{"workspace":workspace}}},
        {"op":"create_requirement","id":"req","data":{"text":"Source is correct","state":"accepted"}},
        {"op":"add_criterion","id":"criterion","requirement":"req","statement":"Source is OK","method":"test","impact_area":["source"]},
        {"op":"add_check","id":"check","criterion":"criterion","definition":{"kind":"file_contains","path":"source","needle":"OK"}},
        {"op":"add_criterion","id":"waived-zero","requirement":"req","statement":"Old zero-revision exemption","method":"inspection"},
        {"op":"add_criterion","id":"waived-three","requirement":"req","statement":"Old latest-revision exemption","method":"inspection"},
        {"op":"link_requirement","task":"task","requirement":"req"}
    ]})).await.unwrap();
    let old_root = run(&journal, "old-root", false).await;
    store.bind_root("task", &old_root).await.unwrap();
    store
        .mark_not_applicable(
            "general",
            "waived-zero",
            1,
            "Old host exemption at zero",
            "synthetic-host",
        )
        .await
        .unwrap();
    for revision in 1..=3 {
        journal
            .append(
                &old_root,
                "directive_applied",
                &format!("applied:{revision}"),
                json!({"id":format!("instruction:{revision}"),"text":"Synthetic instruction"}),
            )
            .await
            .unwrap();
        let event = journal
            .replay(&old_root, 0)
            .await
            .unwrap()
            .into_iter()
            .last()
            .unwrap();
        store.observe_event(&old_root, &event).await.unwrap();
    }
    store
        .mark_not_applicable(
            "general",
            "waived-three",
            1,
            "Old host exemption at three",
            "synthetic-host",
        )
        .await
        .unwrap();
    let tools = Arc::new(
        LocalTools::new(
            db.clone(),
            workspace.clone(),
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
    let runner = CheckRunner::new(tools, workspace);
    let old_proof = runner
        .run_keyed(&store, "task", "check", &old_root, "old-proof")
        .await
        .unwrap();
    assert_eq!(old_proof.snapshot.instruction_revision, 3);
    journal
        .set_state(&old_root, "completed", None, "old-finished")
        .await
        .unwrap();
    let new_root = run(&journal, "new-root", false).await;
    store
        .reopen("task", &new_root, "New user execution")
        .await
        .unwrap();
    let (_, criteria) = store.requirements_and_criteria("task").await.unwrap();
    for criterion in &criteria {
        assert_eq!(
            store.assess_criterion(criterion, 0).await.unwrap().state,
            CriterionState::Unverified,
            "{} retained old proof or exemption",
            criterion.id
        );
    }
    let current = db
        .read(ReadLane::Interactive, |c| {
            Ok((
                c.query_row(
                    "SELECT instruction_revision FROM criteria WHERE id='criterion'",
                    [],
                    |r| r.get::<_, u64>(0),
                )?,
                c.query_row(
                    "SELECT stale FROM evidence ORDER BY rowid LIMIT 1",
                    [],
                    |r| r.get::<_, bool>(0),
                )?,
            ))
        })
        .await
        .unwrap();
    assert_eq!(current, (0, true));
    let proof = runner
        .run_keyed(&store, "task", "check", &new_root, "new-proof")
        .await
        .unwrap();
    assert_eq!(proof.snapshot.instruction_revision, 0);
    assert_ne!(proof.id, old_proof.id);
    let criterion = criteria.iter().find(|c| c.id == "criterion").unwrap();
    assert_eq!(
        store.assess_criterion(criterion, 0).await.unwrap().state,
        CriterionState::Satisfied
    );
    for id in ["waived-zero", "waived-three"] {
        store
            .mark_not_applicable(
                "general",
                id,
                2,
                "New execution host exemption",
                "synthetic-host",
            )
            .await
            .unwrap();
    }
    // Retrying the binding cannot stale proof that belongs to this same new execution.
    store
        .reopen("task", &new_root, "Exact retry")
        .await
        .unwrap();
    assert_eq!(
        store.assess_criterion(criterion, 0).await.unwrap().state,
        CriterionState::Satisfied
    );
    let gate = TaskCompletionGate::new(store.clone(), runner);
    let input = GateInput {
        step: 0,
        instruction_revision: 0,
        final_text: "Verified in the reopened execution".into(),
        local_pending: vec![],
        budget_remaining: Default::default(),
    };
    let guard = gate.inspect(&new_root, &input).await.unwrap();
    assert!(
        guard.report().issues.is_empty(),
        "{:?}",
        guard.report().issues
    );
    drop(guard);
    let old_id = old_proof.id.clone();
    let (old_metadata_still_three, old_stale, old_version_exists, histories)=db.read(ReadLane::Interactive,move|c|Ok((
        c.query_row("SELECT json_extract(metadata,'$.snapshot.instruction_revision') FROM evidence WHERE id=?1",[&old_id],|r|r.get::<_,u64>(0))?,
        c.query_row("SELECT stale FROM evidence WHERE id=?1",[&old_id],|r|r.get::<_,bool>(0))?,
        c.query_row("SELECT count(*) FROM task_entity_versions WHERE entity='criterion' AND id='criterion' AND version=1",[],|r|r.get::<_,u64>(0))?,
        c.query_row("SELECT count(*) FROM task_root_history WHERE task_id='task'",[],|r|r.get::<_,u64>(0))?
    ))).await.unwrap();
    assert_eq!(
        (
            old_metadata_still_three,
            old_stale,
            old_version_exists,
            histories
        ),
        (3, true, 1, 2)
    );
    assert_eq!(journal.state(&old_root).await.unwrap().0, "completed");
    db.shutdown().await.unwrap();
}

#[tokio::test]
async fn durable_task_directives_wake_waiting_runs_without_overwriting_user_control() {
    let (_temp, db, store, journal) = fixture().await;
    store
        .batch(
            "general",
            "create-wake-task",
            json!({"ops":[{"op":"create_task","id":"wake-task","title":"Synthetic task"}]}),
        )
        .await
        .unwrap();
    let run = journal
        .create_run("wake-task-root", json!({}))
        .await
        .unwrap();
    store.bind_root("wake-task", &run).await.unwrap();
    journal
        .set_state_if(&run, &["running"], "waiting", Some("tool".into()), "wait")
        .await
        .unwrap();
    for (index, expected_state) in [
        (0, "running"),
        (1, "ready"),
        (2, "paused"),
        (3, "cancelled"),
    ] {
        if index == 1 {
            journal
                .set_state_if(
                    &run,
                    &["running"],
                    "blocked",
                    Some("input".into()),
                    "input-block",
                )
                .await
                .unwrap();
        }
        if index == 1 {
            store
                .request_replan(
                    "wake-task",
                    "blocking input still unavailable",
                    vec!["wake-task".into()],
                )
                .await
                .unwrap();
            assert_eq!(journal.state(&run).await.unwrap().0, "blocked");
        }
        if index == 2 {
            journal
                .set_state_if(
                    &run,
                    &["ready"],
                    "paused",
                    Some("user".into()),
                    "user-pause",
                )
                .await
                .unwrap();
        }
        if index == 3 {
            journal
                .set_state_if(
                    &run,
                    &["paused"],
                    "cancelled",
                    Some("user".into()),
                    "user-cancel",
                )
                .await
                .unwrap();
        }
        let task = store.task("general", "wake-task").await.unwrap();
        store.batch("general",&format!("modify-{index}"),json!({"ops":[{"op":"update_task","id":task.id,"expected_version":task.version,"title":format!("Updated task {index}")}]})).await.unwrap();
        assert_eq!(journal.state(&run).await.unwrap().0, expected_state);
    }
    let events = journal.replay(&run, 0).await.unwrap();
    assert_eq!(
        events
            .iter()
            .filter(|event| event.kind == "wake_event")
            .count(),
        2
    );
    db.shutdown().await.unwrap();
}
