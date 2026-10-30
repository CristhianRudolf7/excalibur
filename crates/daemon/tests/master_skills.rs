//! Real master context/tool path and durable source-scoped skill policy.
use excalibur_daemon::{
    DaemonHost, HostOptions, MemoryFastActions, MemoryOptions, RuntimeSet,
    master_skills::MasterSkillFastActions,
};
use excalibur_master::{FastActions, Incoming, MasterContextRequest};
use excalibur_memory::{MemoryGrant, MemoryInput, MemoryKind, SourceRef, Trust};
use excalibur_models::{CacheSettings, CacheStrategy, GatewayMetadata, ModelError, ModelGateway};
use excalibur_store::{Database, DatabaseKind, ReadLane};
use excalibur_types::runtime::*;
use excalibur_vault::Redactor;
use serde_json::{Value, json};
use std::{sync::Arc, time::Duration};
use tokio::sync::{mpsc, oneshot};
type Call = (ModelRequest, oneshot::Sender<ModelResponse>);
struct Model(mpsc::UnboundedSender<Call>);
impl ModelGateway for Model {
    fn metadata(&self) -> GatewayMetadata {
        GatewayMetadata {
            model: Some("gpt-4o".into()),
            ..Default::default()
        }
    }
    fn cache_settings(&self) -> CacheSettings {
        CacheSettings {
            strategy: CacheStrategy::Automatic,
            namespace_key_supported: true,
            ..Default::default()
        }
    }
    fn complete(&self, request: ModelRequest) -> BoxFuture<'_, Result<ModelResponse, ModelError>> {
        Box::pin(async move {
            let (tx, rx) = oneshot::channel();
            self.0
                .send((request, tx))
                .map_err(|_| ModelError::Provider)?;
            rx.await.map_err(|_| ModelError::Provider)
        })
    }
}
fn reply(name: Option<&str>, args: Value, id: &str) -> ModelResponse {
    let mut message = Message::text(
        "assistant",
        if name.is_some() {
            ""
        } else {
            "skill fixture complete"
        },
    );
    if let Some(name) = name {
        message.tool_calls.push(ToolCall {
            id: id.into(),
            name: name.into(),
            arguments: args.to_string(),
        });
    }
    ModelResponse {
        message,
        usage: Usage::default(),
    }
}
async fn next(rx: &mut mpsc::UnboundedReceiver<Call>) -> Call {
    tokio::time::timeout(Duration::from_secs(30), rx.recv())
        .await
        .unwrap()
        .unwrap()
}
fn incoming(id: &str, scope: &str) -> Incoming {
    Incoming {
        conversation: "skills-chat".into(),
        client_id: "fixture".into(),
        client_msg_id: id.into(),
        text: "reactor literal requirement: retain EXACT_SOURCE".into(),
        scope: Some(scope.into()),
        timezone: "UTC".into(),
        channel: "test".into(),
        reply_to: None,
        attachments: vec![],
    }
}
fn write(root: &std::path::Path, name: &str, tools: &str, body: &str) {
    let dir = root.join(".agents/skills").join(name);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("SKILL.md"),format!("---\nname: {name}\ndescription: reactor communication {name}\nallowed-tools: search_skills load_skill history_search\nmetadata:\n  excalibur.group: communication\n  excalibur.tools: {tools}\n---\n{body}\n")).unwrap();
}
async fn done(db: &Database, source: &str) {
    tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            let source = source.to_owned();
            let state: String = db
                .read(ReadLane::Interactive, move |c| {
                    Ok(c.query_row(
                        "SELECT state FROM master_inbox WHERE message_id=?1",
                        [source],
                        |r| r.get(0),
                    )?)
                })
                .await
                .unwrap();
            if matches!(state.as_str(), "done" | "consumed") {
                break;
            }
            assert!(
                !matches!(state.as_str(), "failed" | "context_blocked"),
                "{state}"
            );
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}
struct Core;
impl FastActions for Core {
    fn definitions(&self) -> Vec<ToolDefinition> {
        vec![ToolDefinition {
            name: "status".into(),
            description: "status".into(),
            input_schema: json!({"type":"object"}),
        }]
    }
    fn execute<'a>(
        &'a self,
        _: &'a str,
        _: &'a str,
        _: Option<&'a str>,
        _: &'a str,
    ) -> BoxFuture<'a, Result<Value, String>> {
        Box::pin(async { Ok(json!({"command":true})) })
    }
    fn invoke_source_scoped<'a>(
        &'a self,
        c: &'a str,
        s: &'a str,
        source: &'a str,
        _: &'a str,
        _: Value,
        key: &'a str,
    ) -> BoxFuture<'a, Result<Value, String>> {
        Box::pin(async move { Ok(json!({"conversation":c,"scope":s,"source":source,"key":key})) })
    }
    fn confirm_cancel<'a>(
        &'a self,
        _: &'a str,
        _: &'a str,
        _: &'a str,
        _: &'a str,
    ) -> BoxFuture<'a, Result<Value, String>> {
        Box::pin(async { Ok(json!({"human_confirmation_port":true})) })
    }
}
#[test]
fn master_skills_keep_frozen_prefix_original_scope_and_turn_policy_after_reopening() {
    let dir = tempfile::tempdir().unwrap();
    let a = dir.path().join("a");
    let b = dir.path().join("b");
    write(&a, "reactor", "history_search", "ORIGINAL_MASTER_SKILL");
    write(&a, "worker-only", "terminal", "NO_WORKER_SCHEMA");
    write(&a, "core-only", "run_history_search", "NO_ENGINE_SCHEMA");
    write(&b, "private-b", "status", "B_PRIVATE_BODY");
    let make_options = || {
        let mut options = HostOptions::project(a.clone());
        let mut project = options.projects[0].clone();
        project.id = "b".into();
        project.name = "B".into();
        project.roots = vec![b.clone()];
        options.projects.push(project);
        options.memory = Some(MemoryOptions {
            maintenance_interval: None,
            ..Default::default()
        });
        options
    };
    let path = dir.path().join("main.db");
    let db = Database::open(&path, DatabaseKind::Main, Default::default()).unwrap();
    let runtimes = RuntimeSet::new(2, 1).unwrap();
    let (tx, mut rx) = mpsc::unbounded_channel();
    let model = Arc::new(Model(tx));
    let host = runtimes
        .block_on(DaemonHost::open(
            db.clone(),
            make_options(),
            model.clone(),
            None,
            "synthetic-master-skills-token-000000",
            Redactor::default(),
            runtimes.work(),
        ))
        .unwrap();
    let (source, load_key, original_identity, durable_delta) = runtimes.block_on(async {
        let memory = &host.memory.as_ref().unwrap().store;
        for (scope, text) in [
            ("project:default", "A_MEMORY_MARKER"),
            ("project:b", "B_MEMORY_MARKER"),
        ] {
            let grant = MemoryGrant::new("local", [scope]).with_max_trust(Trust::User);
            let mut input = MemoryInput::new(scope, MemoryKind::Preference, text);
            input.trust = Trust::User;
            input.sources = vec![SourceRef::message("fixture-memory-source")];
            memory
                .memory_write(&grant, input, excalibur_store::unix_seconds())
                .await
                .unwrap();
        }
        let receipt = host
            .master
            .receive(incoming("first", "project:default"))
            .await
            .unwrap();
        let (first, answer) = next(&mut rx).await;
        assert!(first.messages[1].content.contains("reactor"));
        assert!(!first.messages[1].content.contains("private-b"));
        assert!(first.messages[2].content.contains("A_MEMORY_MARKER"));
        assert!(!first.messages[2].content.contains("B_MEMORY_MARKER"));
        assert!(first.messages[3].content.contains("EXACT_SOURCE"));
        assert!(first.tools.iter().any(|d| d.name == "load_skill"));
        assert!(!first.tools.iter().any(|d| matches!(
            d.name.as_str(),
            "terminal" | "read_file" | "run_history_search" | "confirm_cancel"
        )));
        answer
            .send(reply(
                Some("load_skill"),
                json!({"name":"reactor"}),
                "load-call",
            ))
            .unwrap();
        let (second, answer) = next(&mut rx).await;
        assert_eq!(first.messages[..4], second.messages[..4]);
        assert_eq!(
            first.cache.as_ref().unwrap().stable_prefix_hash,
            second.cache.as_ref().unwrap().stable_prefix_hash
        );
        assert!(
            second
                .messages
                .iter()
                .any(|m| m.content.contains("ORIGINAL_MASTER_SKILL"))
        );
        answer
            .send(reply(Some("status"), json!({}), "denied-status"))
            .unwrap();
        let (third, answer) = next(&mut rx).await;
        assert!(
            third
                .messages
                .iter()
                .any(|m| m.role == "tool" && m.content.contains("active skill allowed-tools"))
        );
        write(&a, "reactor", "history_search", "CHANGED_MASTER_SKILL");
        host.skills.refresh("project:default").await.unwrap();
        answer
            .send(reply(
                Some("history_search"),
                json!({"text":"fixture","include":"messages"}),
                "history-call",
            ))
            .unwrap();
        let (fourth, answer) = next(&mut rx).await;
        assert_eq!(first.messages[1], fourth.messages[1]);
        assert!(fourth.messages[2].content.contains("A_MEMORY_MARKER"));
        assert!(!fourth.messages[2].content.contains("B_MEMORY_MARKER"));
        assert!(
            fourth
                .messages
                .iter()
                .any(|m| m.content.contains("master-context-update:")
                    && m.content.contains("Skill context update:"))
        );
        let delta = fourth
            .messages
            .iter()
            .find(|m| m.content.contains("master-context-update:"))
            .unwrap()
            .clone();
        answer
            .send(reply(
                Some("history_search"),
                json!({"text":"fixture","include":"messages"}),
                "history-again",
            ))
            .unwrap();
        let (fifth, answer) = next(&mut rx).await;
        assert_eq!(first.messages[..4], fifth.messages[..4]);
        assert_eq!(
            fourth.cache.as_ref().unwrap().stable_prefix_hash,
            fifth.cache.as_ref().unwrap().stable_prefix_hash
        );
        assert_eq!(
            fifth
                .messages
                .iter()
                .filter(|m| m.content.contains("master-context-update:"))
                .collect::<Vec<_>>(),
            vec![&delta]
        );
        answer.send(reply(None, json!({}), "")).unwrap();
        done(&db, &receipt.message_id).await;
        // New sources in A start with no loaded restriction, but the original catalog.
        host.master
            .set_scope("skills-chat", "project:default")
            .await
            .unwrap();
        let next_source = host
            .master
            .receive(incoming("second", "project:default"))
            .await
            .unwrap();
        let (request, answer) = next(&mut rx).await;
        assert_eq!(request.messages[1], first.messages[1]);
        answer
            .send(reply(Some("status"), json!({}), "fresh-status"))
            .unwrap();
        let (request, answer) = next(&mut rx).await;
        assert!(
            !request
                .messages
                .iter()
                .any(|m| m.tool_call_id.as_deref() == Some("fresh-status")
                    && m.content.contains("active skill allowed-tools"))
        );
        answer.send(reply(None, json!({}), "")).unwrap();
        done(&db, &next_source.message_id).await;
        for (scope, private) in [("project:b", true), ("general", false)] {
            host.master.set_scope("skills-chat", scope).await.unwrap();
            let receipt = host.master.receive(incoming(scope, scope)).await.unwrap();
            let (request, answer) = next(&mut rx).await;
            assert!(!request.messages[1].content.contains("reactor"));
            assert_eq!(request.messages[1].content.contains("private-b"), private);
            assert!(!request.messages[2].content.contains("A_MEMORY_MARKER"));
            answer.send(reply(None, json!({}), "")).unwrap();
            done(&db, &receipt.message_id).await;
        }
        (
            receipt.message_id.clone(),
            format!("master:skills-chat:{}:load-call", receipt.message_id),
            first.messages[1].clone(),
            delta,
        )
    });
    runtimes.block_on(host.close()).unwrap();
    drop(db);
    let db = Database::open(&path, DatabaseKind::Main, Default::default()).unwrap();
    let host = runtimes
        .block_on(DaemonHost::open(
            db.clone(),
            make_options(),
            model,
            None,
            "synthetic-master-skills-token-000000",
            Redactor::default(),
            runtimes.work(),
        ))
        .unwrap();
    runtimes.block_on(async {
        let inner = Arc::new(MemoryFastActions::new(
            Arc::new(Core),
            host.memory.as_ref().unwrap().clone(),
        ));
        let actions = MasterSkillFastActions::new(
            db.clone(),
            inner,
            &host.skills,
            "local".into(),
            Redactor::default(),
        )
        .await
        .unwrap();
        let context = actions
            .session_context(MasterContextRequest {
                principal: "local",
                conversation: "skills-chat",
                original_scope: "project:default",
                source_message_id: &source,
            })
            .await
            .unwrap();
        assert!(original_identity.content.contains(context.identity.trim()));
        assert_eq!(context.updates.len(), 1);
        let row: (u64, String, String) = db
            .read(ReadLane::Interactive, |c| {
                Ok(c.query_row(
                    "SELECT sequence,update_id,body FROM master_context_updates",
                    [],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                )?)
            })
            .await
            .unwrap();
        assert_eq!(context.updates[0], (row.1.clone(), row.2.clone()));
        assert!(durable_delta.content.contains(&format!(
            "master-context-update:{} [{}] {}",
            row.0, row.1, row.2
        )));
        assert!(context.suggested_skills.len() <= 3);
        assert!(
            actions
                .invoke_source_scoped(
                    "skills-chat",
                    "project:default",
                    &source,
                    "status",
                    json!({}),
                    "after-reopen"
                )
                .await
                .unwrap_err()
                .contains("active skill allowed-tools")
        );
        let replay = actions
            .invoke_source_scoped(
                "skills-chat",
                "project:default",
                &source,
                "load_skill",
                json!({"name":"reactor"}),
                &load_key,
            )
            .await
            .unwrap();
        assert!(replay.to_string().contains("ORIGINAL_MASTER_SKILL"));
        assert!(
            actions
                .invoke_source_scoped(
                    "skills-chat",
                    "project:default",
                    &source,
                    "load_skill",
                    json!({"name":"worker-only"}),
                    &load_key
                )
                .await
                .unwrap_err()
                .contains("key reused")
        );
        for (name, key) in [
            ("worker-only", "worker-schema"),
            ("core-only", "core-schema"),
        ] {
            assert!(
                actions
                    .invoke_source_scoped(
                        "skills-chat",
                        "project:default",
                        &source,
                        "load_skill",
                        json!({"name":name}),
                        key
                    )
                    .await
                    .unwrap_err()
                    .contains("schema unavailable")
            );
        }
        assert!(
            actions
                .invoke_source_scoped(
                    "skills-chat",
                    "project:b",
                    &source,
                    "status",
                    json!({}),
                    "wrong-scope"
                )
                .await
                .is_err()
        );
        assert!(
            actions
                .session_context(MasterContextRequest {
                    principal: "foreign",
                    conversation: "skills-chat",
                    original_scope: "project:default",
                    source_message_id: &source
                })
                .await
                .is_err()
        );
        for name in [
            "terminal",
            "read_file",
            "run_history_search",
            "confirm_cancel",
        ] {
            assert!(
                actions
                    .invoke_source_scoped(
                        "skills-chat",
                        "project:default",
                        &source,
                        name,
                        json!({}),
                        name
                    )
                    .await
                    .is_err()
            );
        }
        assert_eq!(
            actions
                .confirm_cancel("skills-chat", "project:default", "target", "human-key")
                .await
                .unwrap()["human_confirmation_port"],
            true
        );
        let updates: u64 = db
            .read(ReadLane::Interactive, |c| {
                Ok(
                    c.query_row("SELECT count(*) FROM master_context_updates", [], |r| {
                        r.get(0)
                    })?,
                )
            })
            .await
            .unwrap();
        assert_eq!(updates, 1);
    });
    runtimes.block_on(host.close()).unwrap();
}

struct QuickRuntime;
impl ToolRuntime for QuickRuntime {
    fn definitions(&self) -> Vec<ToolDefinition> {
        ["quick_lookup", "other_lookup", "slow_research"]
            .into_iter()
            .map(|name| ToolDefinition {
                name: name.into(),
                description: format!("reactor {name}"),
                input_schema: json!({"type":"object","additionalProperties":false}),
            })
            .collect()
    }
    fn prepare<'a>(
        &'a self,
        run: &'a str,
        call: ToolCall,
    ) -> BoxFuture<'a, Result<PreparedTool, String>> {
        Box::pin(async move {
            if run != "authorized-fast-master" {
                return Err("missing run capability".into());
            }
            Ok(PreparedTool {
                call,
                effect: Effect::Pure,
                resources: vec![],
                exclusive: false,
                data: Value::Null,
            })
        })
    }
    fn execute<'a>(
        &'a self,
        _: &'a str,
        _: &'a str,
        tool: &'a PreparedTool,
    ) -> BoxFuture<'a, ToolResult> {
        Box::pin(async move { ToolResult::ok(json!({"actual_tool":tool.call.name})) })
    }
    fn reconcile<'a>(
        &'a self,
        _: &'a str,
        _: &'a str,
        _: &'a PreparedTool,
    ) -> BoxFuture<'a, Reconcile> {
        Box::pin(async { Reconcile::Retry })
    }
}
#[test]
fn master_loads_authorized_deferred_fast_schema_through_memory_without_granting_other_targets() {
    use excalibur_daemon::MasterToolOptions;
    use excalibur_tools::deferred::{CatalogTool, ToolCatalog};
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("project");
    write(&root, "quick-skill", "quick_lookup", "QUICK_SKILL_BODY");
    let path = root.join(".agents/skills/quick-skill/SKILL.md");
    let body = std::fs::read_to_string(&path).unwrap().replace(
        "allowed-tools: search_skills load_skill history_search",
        "allowed-tools: search_skills load_skill search_tools quick_lookup",
    );
    std::fs::write(path, body).unwrap();
    let runtime = Arc::new(QuickRuntime);
    let defs = runtime.definitions();
    let tools = defs
        .iter()
        .enumerate()
        .map(|(i, d)| {
            let mut tool = CatalogTool::from_definition(d.clone());
            if i < 2 {
                tool.fast = true;
                tool.timeout_ms = 1000;
            }
            tool
        })
        .collect();
    let mut options = HostOptions::project(root);
    options.memory = Some(MemoryOptions {
        maintenance_interval: None,
        ..Default::default()
    });
    options.master_tools = Some(MasterToolOptions {
        secured_delegate: runtime,
        catalog: Arc::new(ToolCatalog::new(tools).unwrap()),
        authorized_run: "authorized-fast-master".into(),
        scopes: ["project:default".into()].into(),
    });
    let db = Database::open(
        dir.path().join("main.db"),
        DatabaseKind::Main,
        Default::default(),
    )
    .unwrap();
    let runtimes = RuntimeSet::new(2, 1).unwrap();
    let (tx, mut rx) = mpsc::unbounded_channel();
    let host = runtimes
        .block_on(DaemonHost::open(
            db.clone(),
            options,
            Arc::new(Model(tx)),
            None,
            "synthetic-master-skills-token-000000",
            Redactor::default(),
            runtimes.work(),
        ))
        .unwrap();
    runtimes.block_on(async {
        let source = host
            .master
            .receive(incoming("deferred-fast", "project:default"))
            .await
            .unwrap();
        let (first, answer) = next(&mut rx).await;
        assert!(!first.tools.iter().any(|d| d.name == "quick_lookup"));
        assert!(!first.messages[0].content.contains("quick_lookup"));
        answer
            .send(reply(
                Some("load_skill"),
                json!({"name":"quick-skill"}),
                "load-quick",
            ))
            .unwrap();
        let (second, answer) = next(&mut rx).await;
        let loaded: Value = serde_json::from_str(
            &second
                .messages
                .iter()
                .find(|m| m.tool_call_id.as_deref() == Some("load-quick"))
                .unwrap()
                .content,
        )
        .unwrap();
        assert!(loaded.to_string().contains("QUICK_SKILL_BODY"), "{loaded}");
        assert!(
            loaded
                .to_string()
                .contains(&serde_json::to_string(&defs[0].input_schema).unwrap()),
            "{loaded}"
        );
        answer
            .send(reply(
                Some("search_tools"),
                json!({"query":"reactor lookup research"}),
                "search-fast",
            ))
            .unwrap();
        let (third, answer) = next(&mut rx).await;
        let search = &third
            .messages
            .iter()
            .find(|m| m.tool_call_id.as_deref() == Some("search-fast"))
            .unwrap()
            .content;
        assert!(search.contains("quick_lookup"));
        assert!(!search.contains("slow_research"));
        answer
            .send(reply(
                Some("call_tool"),
                json!({"name":"quick_lookup","args":{}}),
                "execute-quick",
            ))
            .unwrap();
        let (fourth, answer) = next(&mut rx).await;
        let executed = &fourth
            .messages
            .iter()
            .find(|m| m.tool_call_id.as_deref() == Some("execute-quick"))
            .unwrap()
            .content;
        assert!(executed.contains("actual_tool"), "{executed}");
        assert!(executed.contains("quick_lookup"));
        answer
            .send(reply(
                Some("call_tool"),
                json!({"name":"other_lookup","args":{}}),
                "deny-other",
            ))
            .unwrap();
        let (fifth, answer) = next(&mut rx).await;
        let denied = &fifth
            .messages
            .iter()
            .find(|m| m.tool_call_id.as_deref() == Some("deny-other"))
            .unwrap()
            .content;
        assert!(denied.contains("active skill allowed-tools"), "{denied}");
        assert_eq!(first.messages[1], fifth.messages[1]);
        assert!(!fifth.tools.iter().any(|d| d.name == "quick_lookup"));
        answer.send(reply(None, json!({}), "")).unwrap();
        done(&db, &source.message_id).await;
    });
    runtimes.block_on(host.close()).unwrap();
}
