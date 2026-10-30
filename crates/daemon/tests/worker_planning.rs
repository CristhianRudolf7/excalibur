use excalibur_daemon::{ScopedPlanning, WorkerTools};
use excalibur_engine::Budget;
use excalibur_journal::Journal;
use excalibur_orchestrator::{Agent, Project};
use excalibur_security::{Autonomy, CapabilityToken, Root, RunSecurity, SecuredTools, Security};
use excalibur_store::{Database, DatabaseKind};
use excalibur_tasks::TaskStore;
use excalibur_tools::{Access, LocalTools, ToolPolicy, terminal::SandboxConfig};
use excalibur_types::runtime::*;
use excalibur_vault::Redactor;
use serde_json::{Value, json};
use std::{path::PathBuf, sync::Arc};
struct Fixture {
    _temp: tempfile::TempDir,
    db: Database,
    journal: Journal,
    tasks: TaskStore,
    workers: Arc<WorkerTools>,
    security: Security,
    planning: ScopedPlanning,
    a: PathBuf,
    b: PathBuf,
}
async fn fixture() -> Fixture {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("work");
    let a = root.join("a");
    let b = root.join("b");
    std::fs::create_dir_all(&a).unwrap();
    std::fs::create_dir_all(&b).unwrap();
    let db = Database::open(
        temp.path().join("data/main.db"),
        DatabaseKind::Main,
        Default::default(),
    )
    .unwrap();
    let journal = Journal::new(db.clone()).await.unwrap();
    let tasks = TaskStore::open(db.clone()).await.unwrap();
    let redactor = Redactor::default();
    let policy = ToolPolicy {
        read: Access::Allow,
        write: Access::Allow,
        exec: Access::Allow,
        sandbox: Some(SandboxConfig::project(root.clone())),
        redactor: redactor.clone(),
        ..Default::default()
    };
    let local = Arc::new(
        LocalTools::new(db.clone(), root.clone(), policy.clone())
            .await
            .unwrap(),
    );
    let projects = [("a", &a), ("b", &b)]
        .into_iter()
        .map(|(id, path)| Project {
            id: id.into(),
            name: id.into(),
            aliases: vec![],
            description: String::new(),
            roots: vec![path.clone()],
            status: "active".into(),
            vcs: None,
            settings: Value::Null,
        })
        .collect();
    let workers = Arc::new(
        WorkerTools::new(db.clone(), local, policy, projects, None)
            .await
            .unwrap(),
    );
    let ceiling = CapabilityToken {
        roots: vec![Root {
            path: root.canonicalize().unwrap(),
            write: true,
        }],
        processes: true,
        sandbox_profile: Some("project-restricted".into()),
        data_scopes: ["project:a".into(), "project:b".into()].into(),
        budget: 1000,
        ..Default::default()
    };
    let security = Security::new(db.clone(), ceiling, redactor.clone())
        .await
        .unwrap();
    let secured: Arc<dyn ToolRuntime> =
        Arc::new(SecuredTools::new(workers.clone(), security.clone()));
    let planning = ScopedPlanning::new(secured, db.clone(), redactor)
        .await
        .unwrap();
    Fixture {
        _temp: temp,
        db,
        journal,
        tasks,
        workers,
        security,
        planning,
        a,
        b,
    }
}
async fn bind(
    f: &Fixture,
    name: &str,
    scope: &str,
    profile: &str,
    write: bool,
    processes: bool,
) -> Agent {
    let root = if scope == "project:a" { &f.a } else { &f.b };
    let run = f.journal.create_run(name, json!({})).await.unwrap();
    let task = excalibur_types::TaskId::new().to_string();
    f.tasks
        .host_batch(
            scope,
            &format!("task:{name}"),
            json!({"ops":[{"op":"create_task","id":task,"title":name}]}),
        )
        .await
        .unwrap();
    f.tasks.bind_root(&task, &run).await.unwrap();
    let agent = Agent {
        id: excalibur_types::AgentId::new().to_string(),
        parent_id: None,
        run_id: Some(run.clone()),
        depth: 1,
        name: name.into(),
        profile: profile.into(),
        profile_id: None,
        scope: scope.into(),
        task,
        origin_conversation: String::new(),
        token: CapabilityToken {
            roots: vec![Root {
                path: root.canonicalize().unwrap(),
                write,
            }],
            processes,
            sandbox_profile: Some("project-restricted".into()),
            data_scopes: [scope.to_owned()].into(),
            budget: 100,
            ..Default::default()
        },
        budget: Budget::default(),
        delegation: None,
    };
    let binding = f.workers.bind_agent(&agent, None).await.unwrap();
    f.security
        .register_run(
            &run,
            RunSecurity {
                parent: None,
                project: scope.into(),
                workspace: binding.workspace.clone(),
                token: binding.token,
                autonomy: vec![Autonomy::Autonomous],
                full_acknowledged: false,
                user_requested: true,
            },
            excalibur_store::unix_seconds(),
        )
        .await
        .unwrap();
    f.planning
        .bind_agent(&agent, binding.workspace)
        .await
        .unwrap();
    agent
}
fn call(name: &str, id: &str, args: Value) -> ToolCall {
    ToolCall {
        id: id.into(),
        name: name.into(),
        arguments: args.to_string(),
    }
}
async fn invoke(f: &Fixture, agent: &Agent, key: &str, args: Value) -> ToolResult {
    let run = agent.run_id.as_deref().unwrap();
    let prepared = f
        .planning
        .prepare(run, call("task_db", key, args))
        .await
        .unwrap();
    f.planning.execute(run, key, &prepared).await
}
#[tokio::test]
async fn worker_planning_is_scoped_keyed_and_rejects_unbound_or_rebound_contracts() {
    let f = fixture().await;
    let a = bind(&f, "a", "project:a", "coordinator", true, true).await;
    let b = bind(&f, "b", "project:b", "coordinator", true, true).await;
    let args = json!({"ops":[{"op":"create_task","id":"extra-a","title":"Only project A"}]});
    let first = invoke(&f, &a, "create-extra", args.clone()).await;
    assert!(first.success, "{first:?}");
    let second = invoke(&f, &a, "create-extra", args).await;
    assert_eq!(first, second);
    let query = json!({"ops":[{"op":"query","entity":"task","fields":["id","title"],"limit":20}]});
    let rows = invoke(&f, &b, "query-b", query.clone()).await;
    assert!(rows.success);
    assert!(!rows.output.to_string().contains("extra-a"));
    let run = a.run_id.as_deref().unwrap();
    assert!(
        f.planning
            .prepare(
                run,
                call(
                    "task_db",
                    "escape",
                    json!({"ops":[{"op":"query","entity":"task","scope":"project:b"}]})
                )
            )
            .await
            .is_err()
    );
    assert!(
        f.planning
            .prepare(
                "unbound",
                call(
                    "task_db",
                    "unbound",
                    json!({"ops":[{"op":"create_task","id":"unbound-task","title":"Forbidden"}]})
                )
            )
            .await
            .is_err()
    );
    assert!(f.tasks.task("project:a", "unbound-task").await.is_err());
    let prepared = f
        .planning
        .prepare(run, call("task_db", "query-a", query))
        .await
        .unwrap();
    assert!(
        !f.planning
            .execute(b.run_id.as_deref().unwrap(), "forged-scope", &prepared)
            .await
            .success
    );
    assert!(matches!(f.planning.reconcile(run,"create-extra",&f.planning.prepare(run,call("task_db","create-extra",json!({"ops":[{"op":"create_task","id":"extra-a","title":"Only project A"}]}))).await.unwrap()).await,Reconcile::Done(_)));
    let catalog = f.planning.run_definitions(run).await.unwrap().unwrap();
    assert!(
        catalog
            .iter()
            .any(|definition| definition.name == "task_db")
    );
    assert!(catalog.iter().all(|definition| definition.name != "status"));
    let mut forged = a.clone();
    forged.scope = "project:b".into();
    assert!(f.planning.bind_agent(&forged, f.b.clone()).await.is_err());
    f.workers.shutdown().await;
    f.db.shutdown().await.unwrap();
}
#[tokio::test]
async fn readonly_roles_cannot_mutate_metadata_while_planners_can_plan_without_writing_files() {
    let f = fixture().await;
    for profile in [
        "explorer",
        "researcher",
        "reviewer",
        "security-auditor",
        "verifier",
    ] {
        let agent = bind(
            &f,
            profile,
            "project:a",
            profile,
            false,
            profile == "verifier",
        )
        .await;
        let run = agent.run_id.as_deref().unwrap();
        assert!(f.planning.prepare(run,call("task_db","mutation",json!({"ops":[{"op":"create_task","id":format!("forbidden-{profile}"),"title":"Forbidden"}]}))).await.is_err());
        let result = invoke(
            &f,
            &agent,
            &format!("query:{profile}"),
            json!({"ops":[{"op":"query","entity":"task"}]}),
        )
        .await;
        assert!(result.success);
        let catalog = f.planning.run_definitions(run).await.unwrap().unwrap();
        let schema = &catalog
            .iter()
            .find(|definition| definition.name == "task_db")
            .unwrap()
            .input_schema;
        assert!(
            !schema["properties"]["ops"]["items"]["properties"]["op"]["enum"]
                .as_array()
                .unwrap()
                .contains(&json!("create_task"))
        );
    }
    let planner = bind(&f, "planner", "project:a", "planner", false, false).await;
    let result=invoke(&f,&planner,"plan",json!({"ops":[{"op":"create_plan","id":"micro-plan","data":{"kind":"micro","owner_task_id":planner.task}}]})).await;
    assert!(result.success, "{result:?}");
    assert!(
        f.planning
            .prepare(
                planner.run_id.as_deref().unwrap(),
                call(
                    "write_file",
                    "forbidden-file",
                    json!({"path":"forbidden.txt","content":"no"})
                )
            )
            .await
            .is_err()
    );
    assert!(!f.a.join("forbidden.txt").exists());
    f.workers.shutdown().await;
    f.db.shutdown().await.unwrap();
}
#[cfg(target_os = "linux")]
#[tokio::test]
async fn declared_worker_checks_use_the_bound_workspace_and_real_security_sandbox() {
    let f = fixture().await;
    let agent = bind(&f, "checker", "project:a", "verifier", false, true).await;
    f.tasks.host_batch("project:a","declare-check",json!({"ops":[{"op":"create_requirement","id":"cwd-requirement","data":{"text":"Check the actual working directory","state":"accepted"}},{"op":"create_criterion","id":"cwd-criterion","data":{"requirement_id":"cwd-requirement","statement":"Command runs in project A","method":"test","impact_area":["**"]}},{"op":"create_check","id":"cwd-check","data":{"criterion_id":"cwd-criterion","definition":{"kind":"command","command":"pwd","timeout_ms":5000,"expected_exit":0}}},{"op":"link_requirement","task":agent.task,"requirement":"cwd-requirement"}]})).await.unwrap();
    let result = invoke(
        &f,
        &agent,
        "run-declared-check",
        json!({"ops":[{"op":"run_check","check":"cwd-check"}]}),
    )
    .await;
    assert!(result.success, "{result:?}");
    assert_eq!(result.output["evidence"]["result"], "pass");
    assert_eq!(result.output["evidence"]["trusted"], true);
    let hash = result.output["evidence"]["output_blob"].as_str().unwrap();
    let bytes =
        f.db.read_blob(hash.parse().unwrap(), 1024 * 1024)
            .await
            .unwrap();
    let output: Value = serde_json::from_slice(&bytes).unwrap();
    assert!(
        output["output"]["output"]
            .as_str()
            .unwrap()
            .contains(f.a.to_str().unwrap())
    );
    let forbidden = f
        .planning
        .prepare(
            agent.run_id.as_deref().unwrap(),
            call(
                "task_db",
                "arbitrary",
                json!({"ops":[{"op":"run_check","check":"not-declared"}]}),
            ),
        )
        .await;
    assert!(
        forbidden.is_err()
            || !f
                .planning
                .execute(
                    agent.run_id.as_deref().unwrap(),
                    "arbitrary",
                    &forbidden.unwrap()
                )
                .await
                .success
    );
    f.workers.shutdown().await;
    f.db.shutdown().await.unwrap();
}
