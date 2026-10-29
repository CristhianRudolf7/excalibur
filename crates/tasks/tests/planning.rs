use excalibur_journal::Journal;
use excalibur_store::{Database, DatabaseKind, ReadLane, StorageConfig};
use excalibur_tasks::{PromotionInput, TaskPolicy, TaskStore, WorkLevel};
use serde_json::json;
async fn fixture() -> (tempfile::TempDir, Database, TaskStore, Journal) {
    let t = tempfile::tempdir().unwrap();
    let db = Database::open(
        t.path().join("main.db"),
        DatabaseKind::Main,
        StorageConfig::default(),
    )
    .unwrap();
    let s = TaskStore::open(db.clone()).await.unwrap();
    let j = Journal::new(db.clone()).await.unwrap();
    (t, db, s, j)
}
async fn task(s: &TaskStore, id: &str) {
    s.batch(
        "general",
        &format!("create:{id}"),
        json!({"ops":[{"op":"create_task","id":id,"title":id}]}),
    )
    .await
    .unwrap();
}
#[tokio::test]
async fn atomic_dag_missing_reference_and_receipt_replay() {
    let (_t, db, s, _) = fixture().await;
    for id in ["a", "b", "c"] {
        task(&s, id).await;
    }
    let args = json!({"ops":[{"op":"add_dependency","from":"a","to":"b"},{"op":"add_dependency","from":"b","to":"c"}]});
    let first = s.batch("general", "dag", args.clone()).await.unwrap();
    assert_eq!(
        first,
        s.batch("general", "dag", args.clone()).await.unwrap()
    );
    assert_eq!(
        Some(first),
        s.receipt("general", "dag", &args).await.unwrap()
    );
    let bad = json!({"ops":[{"op":"create_task","id":"rollback","title":"rollback"},{"op":"add_dependency","from":"c","to":"a"}]});
    assert!(
        s.batch("general", "cycle", bad)
            .await
            .unwrap_err()
            .to_string()
            .contains("cycle")
    );
    assert!(s.task("general", "rollback").await.is_err());
    let bad = json!({"ops":[{"op":"create_task","id":"rollback2","title":"rollback"},{"op":"add_dependency","from":"a","to":"absent"}]});
    assert!(s.batch("general", "missing", bad).await.is_err());
    assert!(s.task("general", "rollback2").await.is_err());
    assert!(
        s.batch("general", "dag", json!({"ops":[{"op":"query"}]}))
            .await
            .is_err()
    );
    db.shutdown().await.unwrap();
}
#[tokio::test]
async fn optimistic_versions_preserve_concurrent_changes_and_scope() {
    let (_t, db, s, _) = fixture().await;
    task(&s, "t").await;
    let args = json!({"ops":[{"op":"update_task","id":"t","expected_version":1,"title":"new"}]});
    let (a, b) = tokio::join!(
        s.batch("general", "a", args.clone()),
        s.batch("general", "b", args)
    );
    assert_eq!(u8::from(a.is_ok()) + u8::from(b.is_ok()), 1);
    let err = a.err().or_else(|| b.err()).unwrap().to_string();
    assert!(err.contains("version conflict"));
    assert!(err.contains("current="));
    assert_eq!(s.task("general", "t").await.unwrap().version, 2);
    let restricted = s.clone().with_policy(TaskPolicy::episode("another"));
    assert!(restricted.task("general", "t").await.is_err());
    assert!(
        restricted
            .batch(
                "another",
                "escape",
                json!({"ops":[{"op":"query","scope":"general"}]})
            )
            .await
            .is_err()
    );
    assert!(
        restricted
            .batch(
                "another",
                "project",
                json!({"ops":[{"op":"create_project","data":{"name":"forbidden"}}]})
            )
            .await
            .is_err()
    );
    assert!(s.batch("general","status",json!({"ops":[{"op":"update_task","id":"t","expected_version":2,"data":{"derived_state":"completed"}}]})).await.is_err());
    db.shutdown().await.unwrap();
}
#[tokio::test]
async fn metadata_survives_creation_and_merges_without_losing_original_request() {
    let (_t, db, s, _) = fixture().await;
    s.host_batch("general","create",json!({"ops":[{"op":"create_task","id":"t","title":"task","data":{"metadata":{"workspace":"/synthetic","a":1},"request_card":{"literal_original":"Original user request"}}}]})).await.unwrap();
    let t = s.task("general", "t").await.unwrap();
    assert_eq!(t.metadata["workspace"], "/synthetic");
    assert_eq!(t.request_card["literal_original"], "Original user request");
    s.batch("general","update",json!({"ops":[{"op":"update_task","id":"t","expected_version":1,"data":{"metadata":{"b":2}}}]})).await.unwrap();
    let t = s.task("general", "t").await.unwrap();
    assert_eq!(t.metadata["workspace"], "/synthetic");
    assert_eq!(t.metadata["a"], 1);
    assert_eq!(t.metadata["b"], 2);
    db.shutdown().await.unwrap();
}
#[tokio::test]
async fn root_state_derives_and_reopening_preserves_previous_root() {
    let (_t, db, s, j) = fixture().await;
    task(&s, "t").await;
    let run = j.create_run("root", json!({})).await.unwrap();
    s.bind_root("t", &run).await.unwrap();
    assert_eq!(
        s.task("general", "t").await.unwrap().derived_state,
        "in_progress"
    );
    j.set_state(&run, "paused", Some("user".into()), "pause")
        .await
        .unwrap();
    assert_eq!(
        s.task("general", "t").await.unwrap().derived_state,
        "paused"
    );
    j.set_state(&run, "running", None, "resume").await.unwrap();
    j.set_state(&run, "completed", None, "done").await.unwrap();
    assert_eq!(
        s.task("general", "t").await.unwrap().derived_state,
        "completed"
    );
    let next = j.create_run("reopened", json!({})).await.unwrap();
    assert!(s.bind_root("t", &next).await.is_err());
    s.reopen("t", &next, "revised work").await.unwrap();
    assert_eq!(
        s.task("general", "t").await.unwrap().root_run_id.as_deref(),
        Some(next.as_str())
    );
    assert_eq!(j.state(&run).await.unwrap().0, "completed");
    let count = db
        .read(ReadLane::Interactive, |c| {
            Ok(
                c.query_row("SELECT count(*) FROM task_root_history", [], |r| {
                    r.get::<_, u64>(0)
                })?,
            )
        })
        .await
        .unwrap();
    assert_eq!(count, 2);
    db.shutdown().await.unwrap();
}
#[tokio::test]
async fn promotions_registered_projects_and_explicit_user_override() {
    let (_t, db, s, _) = fixture().await;
    task(&s, "t").await;
    let p=s.batch("general","project",json!({"ops":[{"op":"create_project","data":{"name":"Project","roots":["/synthetic/project"]}}]})).await.unwrap();
    let p = p["results"][0]["id"].as_str().unwrap().to_owned();
    let input = |steps, p, estimate, milestones| PromotionInput {
        touched_project: p,
        steps,
        estimated_seconds: estimate,
        milestone_count: milestones,
        user_requests_initiative: false,
        forced_level: None,
    };
    assert_eq!(
        s.promote("t", input(15, None, None, 0))
            .await
            .unwrap()
            .level,
        WorkLevel::N1
    );
    let n2 = s.promote("t", input(16, Some(p), None, 0)).await.unwrap();
    assert_eq!(n2.level, WorkLevel::N2);
    assert!(n2.macro_item_id.is_some());
    assert_eq!(
        s.promote("t", input(16, None, Some(86401), 0))
            .await
            .unwrap()
            .level,
        WorkLevel::N3
    );
    let mut force = input(100, None, None, 10);
    force.forced_level = Some(WorkLevel::N1);
    s.promote("t", force).await.unwrap();
    assert_eq!(
        s.promote("t", input(100, None, None, 10))
            .await
            .unwrap()
            .level,
        WorkLevel::N1
    );
    db.shutdown().await.unwrap();
}
#[tokio::test]
async fn bounded_projection_query_keeps_pagination_cursor_without_returning_id() {
    let (_t, db, s, _) = fixture().await;
    let ops = (0..25)
        .map(
            |i| json!({"op":"create_task","id":format!("task-{i:02}"),"title":format!("Task {i}")}),
        )
        .collect::<Vec<_>>();
    s.batch("general", "seed", json!({"ops":ops}))
        .await
        .unwrap();
    let result=s.batch("general","q",json!({"ops":[{"op":"query","entity":"task","fields":["title"],"where":{"level":["N1"]}}]})).await.unwrap();
    let page = &result["results"][0];
    assert_eq!(page["rows"].as_array().unwrap().len(), 20);
    assert_eq!(page["more"], true);
    assert!(page["next"].is_string());
    assert!(page.to_string().len() <= 8000);
    assert!(
        page["rows"]
            .as_array()
            .unwrap()
            .iter()
            .all(|v| v.as_object().unwrap().len() == 1)
    );
    let next = s
        .batch(
            "general",
            "q2",
            json!({"ops":[{"op":"query","entity":"task","fields":["title"],"after":page["next"]}]}),
        )
        .await
        .unwrap();
    assert_eq!(next["results"][0]["rows"].as_array().unwrap().len(), 5);
    db.shutdown().await.unwrap();
}
async fn engine_run(j: &Journal, key: &str) -> String {
    let spec = excalibur_engine::RunSpec {
        context: Default::default(),
        budget: Default::default(),
        criteria: vec![excalibur_engine::Criterion::FinalResponse { schema: None }],
    };
    j.create_run(key,json!({"spec":spec,"started_at":excalibur_store::unix_seconds(),"gateway_identity":"synthetic"})).await.unwrap()
}
#[tokio::test]
async fn pause_resume_exact_members_cancel_confirmation_and_reports_survive_restart() {
    let (_t, db, s, j) = fixture().await;
    task(&s, "t").await;
    let root = engine_run(&j, "root").await;
    let child = engine_run(&j, "child").await;
    let manual = engine_run(&j, "manual").await;
    s.bind_root("t", &root).await.unwrap();
    s.bind_child("t", &child, &root).await.unwrap();
    s.bind_child("t", &manual, &root).await.unwrap();
    j.set_state(&manual, "paused", Some("user".into()), "manual")
        .await
        .unwrap();
    let pause = s.pause_subtree("t", "pause").await.unwrap();
    assert_eq!(pause.runs.len(), 2);
    let reopened = TaskStore::open(db.clone()).await.unwrap();
    let resume = reopened.resume_subtree(&pause.id, "resume").await.unwrap();
    assert_eq!(resume.runs.len(), 2);
    assert_eq!(j.state(&root).await.unwrap().0, "ready");
    assert_eq!(j.state(&child).await.unwrap().0, "ready");
    assert_eq!(j.state(&manual).await.unwrap().0, "paused");
    let pending = s.cancel_subtree("t", "cancel", false).await.unwrap();
    assert!(pending.confirmation_required);
    assert_eq!(j.state(&root).await.unwrap().0, "ready");
    let cancelled = s.cancel_subtree("t", "cancel", true).await.unwrap();
    assert!(cancelled.applied);
    for run in [&root, &child, &manual] {
        assert_eq!(j.state(run).await.unwrap().0, "cancelled");
        let reports = j
            .replay(run, 0)
            .await
            .unwrap()
            .into_iter()
            .filter(|e| e.kind == "delivery_report")
            .collect::<Vec<_>>();
        assert!(!reports.is_empty());
    }
    assert!(
        s.resume_subtree(&pause.id, "too-late")
            .await
            .unwrap()
            .runs
            .is_empty()
    );
    db.shutdown().await.unwrap();
}
#[tokio::test]
async fn host_metadata_original_request_and_forced_level_are_not_model_writable() {
    let (_t, db, s, _) = fixture().await;
    let args = json!({"ops":[{"op":"create_task","id":"t","title":"Task","data":{"level":"N3","request_card":{"literal_original":"Do all requested work"}}},{"op":"create_requirement","id":"r","data":{"text":"Original requirement"}},{"op":"add_criterion","id":"c","requirement":"r","statement":"Respond","method":"inspection","metadata":{"implicit_final_response":true}}]});
    assert!(s.batch("general", "untrusted", args.clone()).await.is_err());
    s.host_batch("general", "host", args).await.unwrap();
    for patch in [
        json!({"level":"N1"}),
        json!({"forced_level":true}),
        json!({"request_card":{"literal_original":"Nothing"}}),
    ] {
        assert!(
            s.batch(
                "general",
                &patch.to_string(),
                json!({"ops":[{"op":"update_task","id":"t","expected_version":1,"data":patch}]})
            )
            .await
            .is_err()
        );
    }
    s.batch("general","mutate-condition",json!({"ops":[{"op":"update_criterion","id":"c","expected_version":1,"data":{"statement":"An entirely different custom claim"}}]})).await.unwrap();
    let flag=db.read(ReadLane::Interactive,|c|Ok(c.query_row("SELECT json_extract(metadata,'$.implicit_final_response') FROM criteria WHERE id='c'",[],|r|r.get::<_,Option<bool>>(0))?)).await.unwrap();
    assert_eq!(flag, None);
    db.shutdown().await.unwrap();
}
#[tokio::test]
async fn replanning_follows_requirement_linked_task_descendants_and_preserves_versions() {
    let (_t, db, s, j) = fixture().await;
    for id in ["a", "b", "c"] {
        task(&s, id).await;
    }
    let root = engine_run(&j, "root-b").await;
    s.bind_root("b", &root).await.unwrap();
    s.batch("general","plan",json!({"ops":[{"op":"create_plan","id":"p","data":{"kind":"macro","title":"Plan"}},{"op":"create_plan_item","id":"frozen","data":{"plan_id":"p","title":"Finished unaffected","state":"done"}},{"op":"create_plan_item","id":"ib","data":{"plan_id":"p","title":"Old title","task_id":"b"}},{"op":"create_requirement","id":"r","data":{"text":"Changed requirement"}},{"op":"add_criterion","id":"c1","requirement":"r","statement":"Observable","method":"test"},{"op":"link_requirement","task":"b","requirement":"r"},{"op":"add_dependency","from":"b","to":"c"}]})).await.unwrap();
    let graph = s
        .affected_subgraph("general", vec!["r".into()])
        .await
        .unwrap();
    assert!(graph["affected"].as_array().unwrap().contains(&json!("b")));
    assert!(graph["affected"].as_array().unwrap().contains(&json!("c")));
    assert!(
        graph["frozen"]
            .as_array()
            .unwrap()
            .contains(&json!("frozen"))
    );
    let change = json!({"op":"replan","id":"p","expected_version":1,"changed":["r"],"ops":[{"op":"update_plan_item","id":"ib","expected_version":1,"data":{"title":"New title"}}]});
    s.batch("general", "replan", json!({"ops":[change]}))
        .await
        .unwrap();
    let old = s.plan_version("general", "p", 1).await.unwrap();
    assert!(
        old["items"]
            .as_array()
            .unwrap()
            .iter()
            .any(|i| i["id"] == "ib" && i["title"] == "Old title")
    );
    let new = s.plan_version("general", "p", 2).await.unwrap();
    assert!(
        new["items"]
            .as_array()
            .unwrap()
            .iter()
            .any(|i| i["id"] == "ib" && i["title"] == "New title")
    );
    assert!(
        j.replay(&root, 0)
            .await
            .unwrap()
            .iter()
            .any(|e| e.kind == "directive_received")
    );
    let invalid = json!({"ops":[{"op":"replan","id":"p","expected_version":2,"changed":["r"],"ops":[{"op":"update_plan_item","id":"frozen","expected_version":1,"data":{"title":"Must not change"}}]}]});
    assert!(s.batch("general", "frozen-change", invalid).await.is_err());
    db.shutdown().await.unwrap();
}
