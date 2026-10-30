use excalibur_context::TokenCounter;
use excalibur_skills::{
    groups::{CatalogScope, TaskKind},
    index::{SkillIndex, SkillRoot},
    runtime::SkillRuntime,
};
use excalibur_store::{Database, DatabaseKind, Priority};
use excalibur_types::runtime::*;
use serde_json::json;
use std::{
    collections::BTreeMap,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};
#[derive(Default)]
struct Tools(AtomicUsize);
impl ToolRuntime for Tools {
    fn definitions(&self) -> Vec<ToolDefinition> {
        ["read_file", "write_file", "forbidden"]
            .into_iter()
            .map(|name| ToolDefinition {
                name: name.into(),
                description: name.into(),
                input_schema: json!({"type":"object"}),
            })
            .collect()
    }
    fn deferred_definitions<'a>(
        &'a self,
        _: &'a str,
    ) -> BoxFuture<'a, Result<Vec<ToolDefinition>, String>> {
        Box::pin(async {
            Ok(vec![ToolDefinition {
                name: "domain_read".into(),
                description: "domain".into(),
                input_schema: json!({"type":"object"}),
            }])
        })
    }
    fn prepare<'a>(
        &'a self,
        _: &'a str,
        call: ToolCall,
    ) -> BoxFuture<'a, Result<PreparedTool, String>> {
        Box::pin(async move {
            if call.name == "forbidden" {
                return Err("host denied".into());
            }
            Ok(PreparedTool {
                call,
                effect: Effect::Pure,
                resources: vec![],
                exclusive: false,
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
            self.0.fetch_add(1, Ordering::SeqCst);
            ToolResult::ok(json!({"done":true}))
        })
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
fn call(name: &str, args: serde_json::Value) -> ToolCall {
    ToolCall {
        id: name.into(),
        name: name.into(),
        arguments: args.to_string(),
    }
}
fn write(root: &std::path::Path, name: &str, allowed: &str, tools: &str, body: &str) {
    std::fs::create_dir_all(root.join(name)).unwrap();
    std::fs::write(root.join(name).join("SKILL.md"),format!("---\nname: {name}\ndescription: Read and debug reactor files\nallowed-tools: {allowed}\nmetadata:\n  excalibur.group: debugging\n  excalibur.tools: [{tools}]\n---\n{body}\n")).unwrap();
}
async fn index(root: &std::path::Path) -> Arc<SkillIndex> {
    let index = Arc::new(SkillIndex::new(vec![SkillRoot::user(root)]));
    index.refresh_background().await.unwrap();
    index
}
#[tokio::test]
async fn durable_frozen_scope_hash_and_all_execution_paths_restrict_authority() {
    let dir = tempfile::tempdir().unwrap();
    let a = dir.path().join("a");
    let b = dir.path().join("b");
    write(
        &a,
        "reactor",
        "search_skills load_skill read_file domain_read",
        "domain_read",
        "original body",
    );
    write(&b, "private-b", "read_file", "", "PRIVATE B");
    let indexes = BTreeMap::from([("a".into(), index(&a).await), ("b".into(), index(&b).await)]);
    let db = Database::open(
        dir.path().join("db"),
        DatabaseKind::Main,
        Default::default(),
    )
    .unwrap();
    let tools = Arc::new(Tools::default());
    let rt = SkillRuntime::new(
        db.clone(),
        tools.clone(),
        indexes.clone(),
        10000,
        TokenCounter::for_model("gpt-4o"),
    )
    .await
    .unwrap();
    let scope = || CatalogScope::new(TaskKind::Programming, "implementer");
    rt.bind("run-a", "a", scope(), "reactor").await.unwrap();
    rt.bind("other-a", "a", scope(), "reactor").await.unwrap();
    assert!(rt.bind("run-a", "b", scope(), "private").await.is_err());
    let frozen = rt.session_context("run-a").await.unwrap();
    assert!(frozen.suggested_skills.len() <= 3);
    assert!(frozen.identity.contains("reactor"));
    assert!(!frozen.identity.contains("private-b"));
    assert!(
        TokenCounter::for_model("gpt-4o").count_text(
            frozen
                .identity
                .split("Frozen skill catalog:\n")
                .nth(1)
                .unwrap()
        ) <= 200
    );
    let prepared = rt
        .prepare("run-a", call("write_file", json!({})))
        .await
        .unwrap();
    let load = rt
        .prepare("run-a", call("load_skill", json!({"name":"reactor"})))
        .await
        .unwrap();
    let result = rt.execute("run-a", "load-1", &load).await;
    assert!(result.success, "{result:?}");
    assert_eq!(result.output["tools"][0]["name"], "domain_read");
    assert!(
        rt.prepare(
            "run-a",
            call("call_tool", json!({"name":"domain_read","args":{}}))
        )
        .await
        .is_ok()
    );
    assert!(
        !rt.execute("run-a", "write-after-load", &prepared)
            .await
            .success
    );
    assert_eq!(tools.0.load(Ordering::SeqCst), 0);
    assert!(
        rt.prepare("run-a", call("write_file", json!({})))
            .await
            .is_err()
    );
    assert!(
        rt.prepare(
            "run-a",
            call("call_tool", json!({"name":"write_file","args":{}}))
        )
        .await
        .is_err()
    );
    assert!(
        rt.audit_builtin("run-a", &call("status", json!({})), None)
            .await
            .is_err()
    );
    assert!(
        rt.prepare("other-a", call("write_file", json!({})))
            .await
            .is_ok()
    );
    let replay = rt.execute("run-a", "load-1", &load).await;
    assert_eq!(replay, result);
    write(&a, "reactor", "read_file", "", "changed body");
    indexes["a"].refresh_background().await.unwrap();
    let changed = rt.session_context("run-a").await.unwrap();
    assert_eq!(changed.identity, frozen.identity);
    assert_eq!(changed.updates.len(), 1);
    drop(rt);
    let rt = SkillRuntime::new(
        db.clone(),
        tools,
        indexes,
        10000,
        TokenCounter::for_model("gpt-4o"),
    )
    .await
    .unwrap();
    assert_eq!(rt.session_context("run-a").await.unwrap(), changed);
    assert_eq!(rt.execute("run-a", "load-1", &load).await, result);
    assert!(
        rt.prepare("run-a", call("write_file", json!({})))
            .await
            .is_err()
    );
    let invalid = rt
        .prepare("run-a", call("load_skill", json!({"name":"reactor"})))
        .await
        .unwrap();
    assert!(!rt.execute("run-a", "changed-load", &invalid).await.success);
    db.write(Priority::Journal, |c| {
        c.execute(
            "UPDATE skill_run_sessions SET body=body||' ' WHERE run='run-a'",
            [],
        )?;
        Ok(())
    })
    .await
    .unwrap();
    assert!(rt.session_context("run-a").await.is_err());
}
#[tokio::test]
async fn schema_failure_never_activates_and_skill_cannot_grant_host_denial() {
    let dir = tempfile::tempdir().unwrap();
    write(
        dir.path(),
        "missing",
        "read_file",
        "unknown_tool",
        "invalid schema",
    );
    write(
        dir.path(),
        "forbidden-skill",
        "forbidden",
        "forbidden",
        "host forbidden",
    );
    let indexes = BTreeMap::from([("a".into(), index(dir.path()).await)]);
    let db = Database::open(
        dir.path().join("db"),
        DatabaseKind::Main,
        Default::default(),
    )
    .unwrap();
    let rt = SkillRuntime::new(
        db,
        Arc::new(Tools::default()),
        indexes,
        100000,
        TokenCounter::default(),
    )
    .await
    .unwrap();
    rt.bind(
        "r",
        "a",
        CatalogScope::new(TaskKind::Programming, "implementer"),
        "debug",
    )
    .await
    .unwrap();
    let load = rt
        .prepare("r", call("load_skill", json!({"name":"missing"})))
        .await
        .unwrap();
    assert!(!rt.execute("r", "missing", &load).await.success);
    assert!(rt.prepare("r", call("write_file", json!({}))).await.is_ok());
    let load = rt
        .prepare("r", call("load_skill", json!({"name":"forbidden-skill"})))
        .await
        .unwrap();
    assert!(rt.execute("r", "forbidden", &load).await.success);
    assert!(
        rt.prepare("r", call("forbidden", json!({})))
            .await
            .unwrap_err()
            .contains("host denied")
    );
}
