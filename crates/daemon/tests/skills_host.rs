//! Real daemon -> authority -> engine -> skill/deferred/security stack, local mocks only.
use excalibur_context::ContextInput;
use excalibur_daemon::{DaemonHost, HostOptions, RuntimeSet};
use excalibur_engine::{Budget, Criterion, RunSpec};
use excalibur_models::{ModelError, ModelGateway};
use excalibur_orchestrator::Delegation;
use excalibur_store::{Database, DatabaseKind};
use excalibur_types::runtime::*;
use excalibur_vault::Redactor;
use serde_json::json;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};
struct Model {
    calls: AtomicUsize,
    requests: Mutex<Vec<ModelRequest>>,
}
impl ModelGateway for Model {
    fn complete(&self, request: ModelRequest) -> BoxFuture<'_, Result<ModelResponse, ModelError>> {
        Box::pin(async move {
            self.requests.lock().unwrap().push(request);
            let step = self.calls.fetch_add(1, Ordering::SeqCst);
            let mut message = Message::text("assistant", "done");
            let action = match step {
                0 => Some(("search_skills", json!({"query":"reactor"}))),
                1 => Some(("load_skill", json!({"name":"reactor"}))),
                2 => Some((
                    "write_file",
                    json!({"path":"forbidden.txt","content":"must never exist"}),
                )),
                3 => Some(("read_file", json!({"path":"input.txt"}))),
                _ => None,
            };
            if let Some((name, args)) = action {
                message.content.clear();
                message.tool_calls.push(ToolCall {
                    id: format!("step-{step}"),
                    name: name.into(),
                    arguments: args.to_string(),
                });
            }
            Ok(ModelResponse {
                message,
                usage: Usage {
                    input_tokens: 10,
                    output_tokens: 10,
                    ..Default::default()
                },
            })
        })
    }
}
fn skill(body: &str) -> String {
    format!(
        "---\nname: reactor\ndescription: Read reactor files and investigate reactor bugs\nallowed-tools: search_skills load_skill read_file\nmetadata:\n  excalibur.group: debugging\n  excalibur.tools: [read_file]\n---\n{body}\n"
    )
}
#[test]
fn daemon_loads_real_skill_in_stable_context_enforces_it_and_restores_session() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("project");
    std::fs::create_dir_all(root.join(".agents/skills/reactor")).unwrap();
    std::fs::write(
        root.join(".agents/skills/reactor/SKILL.md"),
        skill("ORIGINAL SKILL BODY"),
    )
    .unwrap();
    std::fs::write(root.join("input.txt"), "reactor evidence").unwrap();
    let path = temp.path().join("main.db");
    let db = Database::open(&path, DatabaseKind::Main, Default::default()).unwrap();
    let runtimes = RuntimeSet::new(2, 1).unwrap();
    let model = Arc::new(Model {
        calls: AtomicUsize::new(0),
        requests: Mutex::new(vec![]),
    });
    let mut options = HostOptions::project(root.clone());
    options.memory = Some(excalibur_daemon::MemoryOptions {
        maintenance_interval: None,
        ..Default::default()
    });
    let host = runtimes
        .block_on(DaemonHost::open(
            db.clone(),
            options,
            model.clone(),
            None,
            "synthetic-skills-host-token-00000000",
            Redactor::default(),
            runtimes.work(),
        ))
        .unwrap();
    let (run, context) = runtimes.block_on(async {
        let parent = host
            .orchestrator
            .list_agents()
            .await
            .unwrap()
            .into_iter()
            .find(|a| a.depth == 0)
            .unwrap();
        let agent = host
            .orchestrator
            .spawn(
                &parent.id,
                Delegation {
                    operation: "skills-e2e".into(),
                    name: "SkillsWorker".into(),
                    profile: "implementer".into(),
                    scope: "project:default".into(),
                    task: excalibur_types::TaskId::new().to_string(),
                    origin_conversation: "chat".into(),
                    goal: "Read reactor input and investigate reactor bugs".into(),
                    deliverable: "A report".into(),
                    inputs: vec![],
                    pinned_instructions: vec![],
                    suggested_skills: vec![],
                    fork_context: false,
                    token: parent.token,
                    spec: RunSpec {
                        context: ContextInput::new("rules", "task", "now"),
                        budget: Budget::default(),
                        criteria: vec![Criterion::FinalResponse { schema: None }],
                    },
                    priority: 1,
                },
            )
            .await
            .unwrap();
        let run = agent.run_id.unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(30), async {
            loop {
                host.orchestrator.tick().await.unwrap();
                let state = host.orchestrator.status(&agent.id).await.unwrap();
                if state.state == "completed" {
                    break;
                }
                assert!(
                    !matches!(state.state.as_str(), "blocked" | "failed"),
                    "{state:?}"
                );
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        let context = host.skills.runtime.session_context(&run).await.unwrap();
        assert!(context.identity.contains("reactor"));
        assert!(context.suggested_skills.len() <= 3);
        let calls = model.requests.lock().unwrap();
        assert!(calls.len() >= 5);
        for request in calls.iter() {
            assert_eq!(request.tools, calls[0].tools);
            assert!(
                request
                    .messages
                    .iter()
                    .any(|m| m.content.contains("Frozen skill catalog:"))
            );
        }
        let stable = |request: &ModelRequest| {
            request
                .messages
                .iter()
                .filter(|m| m.content.contains("Frozen skill catalog:"))
                .map(|m| m.content.clone())
                .collect::<Vec<_>>()
        };
        for request in calls.iter().skip(1) {
            assert_eq!(stable(request), stable(&calls[0]));
        }
        assert!(
            calls
                .iter()
                .flat_map(|r| &r.messages)
                .any(|m| m.content.contains("ORIGINAL SKILL BODY"))
        );
        assert!(
            calls
                .iter()
                .flat_map(|r| &r.messages)
                .any(|m| m.content.contains("allowed-tools"))
        );
        assert!(
            calls
                .iter()
                .flat_map(|r| &r.messages)
                .any(|m| m.content.contains("reactor evidence"))
        );
        drop(calls);
        assert!(!root.join("forbidden.txt").exists());
        std::fs::write(
            root.join(".agents/skills/reactor/SKILL.md"),
            skill("CHANGED BODY"),
        )
        .unwrap();
        host.skills.refresh("project:default").await.unwrap();
        let delta = host.skills.runtime.session_context(&run).await.unwrap();
        assert_eq!(delta.identity, context.identity);
        assert!(!delta.updates.is_empty());
        (run, delta)
    });
    runtimes.block_on(host.close()).unwrap();
    drop(db);
    let db = Database::open(path, DatabaseKind::Main, Default::default()).unwrap();
    let host = runtimes
        .block_on(DaemonHost::open(
            db,
            HostOptions::project(root),
            model,
            None,
            "synthetic-skills-host-token-00000000",
            Redactor::default(),
            runtimes.work(),
        ))
        .unwrap();
    runtimes.block_on(async {
        assert_eq!(
            host.skills.runtime.session_context(&run).await.unwrap(),
            context
        );
        let denied = host
            .skills
            .runtime
            .prepare(
                &run,
                ToolCall {
                    id: "denied-restart".into(),
                    name: "write_file".into(),
                    arguments: json!({"path":"forbidden.txt","content":"no"}).to_string(),
                },
            )
            .await;
        assert!(denied.is_err());
    });
    runtimes.block_on(host.close()).unwrap();
    runtimes.shutdown(std::time::Duration::from_secs(3));
}
