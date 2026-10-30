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
    skills: Mutex<Option<Arc<excalibur_daemon::skills::SkillsHost>>>,
    skill_file: std::path::PathBuf,
}
impl ModelGateway for Model {
    // The local synthetic provider has a finite per-call ceiling. Leaving it
    // unknown correctly reserves the entire remaining run on interruption.
    fn call_bound(&self, _: &ModelRequest) -> Option<excalibur_models::ModelCallBound> {
        Some(excalibur_models::ModelCallBound {
            max_tokens: 32768,
            max_cost_microusd: 0,
        })
    }
    fn complete(&self, request: ModelRequest) -> BoxFuture<'_, Result<ModelResponse, ModelError>> {
        Box::pin(async move {
            let step = self.calls.fetch_add(1, Ordering::SeqCst);
            if step == 2 {
                let loaded: serde_json::Value = serde_json::from_str(
                    &request
                        .messages
                        .iter()
                        .find(|m| m.tool_call_id.as_deref() == Some("step-1"))
                        .unwrap()
                        .content,
                )
                .unwrap();
                let tools = loaded["output"]["tools"].as_array().unwrap();
                assert!(tools.iter().any(|tool| tool["name"] == "load_skill"));
                assert!(
                    tools
                        .iter()
                        .any(|tool| tool["name"] == "run_history_search")
                );
            }
            if step == 6 {
                let memory = request
                    .messages
                    .iter()
                    .find(|m| m.tool_call_id.as_deref() == Some("step-5"))
                    .unwrap();
                let value: serde_json::Value = serde_json::from_str(&memory.content).unwrap();
                assert_eq!(value["success"], true);
                assert!(memory.content.contains("SCOPED MEMORY MESSAGE"));
            }
            if step == 7 {
                let history = request
                    .messages
                    .iter()
                    .find(|m| m.tool_call_id.as_deref() == Some("step-6"))
                    .unwrap();
                let value: serde_json::Value = serde_json::from_str(&history.content).unwrap();
                assert_eq!(value["success"], true);
                assert_eq!(value["output"]["scope"], "current_run");
            }
            self.requests.lock().unwrap().push(request);
            if step == 2 {
                std::fs::write(&self.skill_file, skill("CHANGED BODY")).unwrap();
                let skills = self.skills.lock().unwrap().clone().unwrap();
                skills.refresh("project:default").await.unwrap();
            }
            if step == 4 {
                std::future::pending::<()>().await;
            }
            let mut message = Message::text("assistant", "done");
            let action = match step {
                0 => Some(("search_skills", json!({"query":"reactor"}))),
                1 => Some(("load_skill", json!({"name":"reactor"}))),
                2 => Some((
                    "write_file",
                    json!({"path":"forbidden.txt","content":"must never exist"}),
                )),
                3 => Some(("read_file", json!({"path":"input.txt"}))),
                5 => Some((
                    "history_search",
                    json!({"text":"SCOPED MEMORY MESSAGE","include":"messages"}),
                )),
                6 => Some(("run_history_search", json!({"limit":16}))),
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
        "---\nname: reactor\ndescription: Read reactor files and investigate reactor bugs\nallowed-tools: search_skills load_skill read_file history_search run_history_search\nmetadata:\n  excalibur.group: debugging\n  excalibur.tools: read_file history_search run_history_search load_skill\n---\n{body}\n"
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
        skills: Mutex::new(None),
        skill_file: root.join(".agents/skills/reactor/SKILL.md"),
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
    *model.skills.lock().unwrap() = Some(host.skills.clone());
    let (run, agent_id, context) = runtimes.block_on(async {
        db.write(excalibur_store::Priority::Journal,|c|{
            c.execute("INSERT OR IGNORE INTO principals(id) VALUES('local')",[])?;
            c.execute("INSERT INTO conversations(id,channel,principal_id,active_scope,scope_id) VALUES('chat','test','local','project:default','project:default')",[])?;
            c.execute("INSERT INTO messages(id,conversation_id,role,text,scope_id,principal_id) VALUES('memory-message','chat','user','SCOPED MEMORY MESSAGE','project:default','local')",[])?;
            Ok(())
        }).await.unwrap();
        let parent = host
            .orchestrator
            .list_agents()
            .await
            .unwrap()
            .into_iter()
            .find(|a| a.depth == 0)
            .unwrap();
        let mut input=ContextInput::new("rules","task","now");
        input.token_budgets[0]=16_000; // Full local, memory and coordination schemas.
        let agent = host
            .orchestrator
            .spawn(
                &parent.id,
                Delegation {
                    operation: "skills-e2e".into(),
                    name: "SkillsWorker".into(),
                    profile: "coordinator".into(),
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
                        context: input,
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
                if model.calls.load(Ordering::SeqCst) >= 5 {
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
        host.orchestrator
            .control(&agent.id, excalibur_orchestrator::Control::Pause)
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
        let delta = host.skills.runtime.session_context(&run).await.unwrap();
        assert_eq!(delta.identity, context.identity);
        assert!(!delta.updates.is_empty());
        let state = host.orchestrator.engine().load(&run).await.unwrap();
        assert_eq!(
            state
                .context_session
                .as_ref()
                .unwrap()
                .history_search("Skill context update:", 0, 100)
                .len(),
            1
        );
        (run, agent.id, delta)
    });
    *model.skills.lock().unwrap() = None;
    runtimes.block_on(host.close()).unwrap();
    drop(db);
    let db = Database::open(path, DatabaseKind::Main, Default::default()).unwrap();
    let mut options = HostOptions::project(root);
    options.memory = Some(excalibur_daemon::MemoryOptions {
        maintenance_interval: None,
        ..Default::default()
    });
    let host = runtimes
        .block_on(DaemonHost::open(
            db,
            options,
            model.clone(),
            None,
            "synthetic-skills-host-token-00000000",
            Redactor::default(),
            runtimes.work(),
        ))
        .unwrap();
    *model.skills.lock().unwrap() = Some(host.skills.clone());
    runtimes.block_on(async {
        host.orchestrator
            .control(&agent_id, excalibur_orchestrator::Control::Resume)
            .await
            .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(30), async {
            loop {
                host.orchestrator.tick().await.unwrap();
                let status = host.orchestrator.status(&agent_id).await.unwrap();
                if status.state == "completed" {
                    break;
                }
                assert!(
                    !matches!(status.state.as_str(), "failed" | "blocked"),
                    "{status:?}"
                );
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap_or_else(|error| {
            panic!(
                "resume timed out: {error}; model calls {}",
                model.calls.load(Ordering::SeqCst)
            )
        });
        let state = host.orchestrator.engine().load(&run).await.unwrap();
        assert!(state.usage.estimated);
        assert!(state.usage.input_tokens + state.usage.output_tokens >= 32768);
        assert!(state.usage.input_tokens + state.usage.output_tokens < 100000);

        assert_eq!(
            state
                .context_session
                .as_ref()
                .unwrap()
                .history_search("Skill context update:", 0, 100)
                .len(),
            1
        );
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
    *model.skills.lock().unwrap() = None;
    runtimes.block_on(host.close()).unwrap();
    runtimes.shutdown(std::time::Duration::from_secs(3));
}
