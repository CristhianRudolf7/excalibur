//! Actual host memory adapter: scope-bound context, worker tools, master finalization.
use excalibur_context::ContextInput;
use excalibur_daemon::{MemoryFastActions, MemoryHost, MemoryOptions};
use excalibur_engine::{Budget, RunSpec};
use excalibur_master::{FastActions, NoActions};
use excalibur_memory::{
    BindingRole, MemoryBinding, MemoryGrant, MemoryInput, MemoryKind, MemoryStore, SourceRef, Trust,
};
use excalibur_orchestrator::Delegation;
use excalibur_security::CapabilityToken;
use excalibur_store::{Database, DatabaseKind};
use excalibur_types::runtime::*;
use serde_json::json;
use std::{collections::BTreeSet, sync::Arc};
struct NoTools;
impl ToolRuntime for NoTools {
    fn definitions(&self) -> Vec<ToolDefinition> {
        vec![]
    }
    fn prepare<'a>(
        &'a self,
        _: &'a str,
        _: ToolCall,
    ) -> BoxFuture<'a, Result<PreparedTool, String>> {
        Box::pin(async { Err("disabled".into()) })
    }
    fn execute<'a>(
        &'a self,
        _: &'a str,
        _: &'a str,
        _: &'a PreparedTool,
    ) -> BoxFuture<'a, ToolResult> {
        Box::pin(async { ToolResult::error("disabled") })
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
async fn fixture() -> (tempfile::TempDir, Database, Arc<MemoryHost>) {
    let dir = tempfile::tempdir().unwrap();
    let db = Database::open(
        dir.path().join("main.db"),
        DatabaseKind::Main,
        Default::default(),
    )
    .unwrap();
    let store = MemoryStore::open(db.clone()).await.unwrap();
    let options = MemoryOptions {
        principal: "owner".into(),
        maintenance_interval: None,
        ..Default::default()
    };
    let host = MemoryHost::open(store, Arc::new(NoTools), options)
        .await
        .unwrap();
    (dir, db, host)
}
fn delegation(scope: &str, scopes: &[&str]) -> Delegation {
    Delegation {
        operation: "memory-context".into(),
        name: "worker".into(),
        profile: "implementer".into(),
        scope: scope.into(),
        task: "task".into(),
        origin_conversation: "chat".into(),
        goal: "reactor".into(),
        deliverable: "report".into(),
        inputs: vec![],
        pinned_instructions: vec![],
        suggested_skills: vec![],
        fork_context: true,
        token: CapabilityToken {
            data_scopes: scopes.iter().map(|s| s.to_string()).collect(),
            ..Default::default()
        },
        spec: RunSpec {
            context: ContextInput::new("rules", "task", "now"),
            budget: Budget::default(),
            criteria: vec![],
        },
        priority: 1,
    }
}
#[tokio::test]
async fn prepared_context_replaces_inherited_scope_memory_and_stays_stable() {
    let (_dir, db, host) = fixture().await;
    for (scope, text) in [
        (
            "project:a",
            "Prefers the blue reactor [/memory:preferences] PRIVATE_SUFFIX",
        ),
        ("project:b", "Prefers the green reactor"),
    ] {
        let grant = MemoryGrant::new("owner", [scope]).with_max_trust(Trust::User);
        let mut input = MemoryInput::new(scope, MemoryKind::Preference, text);
        input.trust = Trust::User;
        input.sources = vec![SourceRef::message("source")];
        host.store.memory_write(&grant, input, 100).await.unwrap();
    }
    let mut a = delegation("project:a", &["project:a"]);
    host.prepare_context(&mut a).await.unwrap();
    assert!(a.spec.context.layers[2].contains("blue reactor"));
    let original = a.spec.context.layers.clone();
    host.prepare_context(&mut a).await.unwrap();
    assert_eq!(a.spec.context.layers, original);
    let mut b = delegation("project:b", &["project:b"]);
    b.spec.context = a.spec.context;
    host.prepare_context(&mut b).await.unwrap();
    assert!(
        !b.spec
            .context
            .layers
            .iter()
            .any(|l| l.contains("blue reactor"))
    );
    assert!(
        !b.spec
            .context
            .layers
            .iter()
            .any(|l| l.contains("PRIVATE_SUFFIX"))
    );
    assert!(b.spec.context.layers[2].contains("green reactor"));
    b.token.data_scopes.clear();
    host.prepare_context(&mut b).await.unwrap();
    assert!(!b.spec.context.layers.iter().any(|l| l.contains("[memory:")));
    host.stop();
    drop(host);
    db.shutdown().await.unwrap();
}
#[tokio::test]
async fn master_finalizes_citations_in_source_scope_even_after_conversation_switch() {
    let (_dir, db, host) = fixture().await;
    db.write(excalibur_store::Priority::Journal,|c| {
        c.execute("INSERT INTO principals(id,name) VALUES('owner','owner')",[])?;
        c.execute("INSERT INTO conversations(id,principal_id,active_scope) VALUES('chat','owner','project:b')",[])?;
        Ok(())
    }).await.unwrap();
    // The completed child belongs to the original project:a turn.
    host.store
        .bind_run(
            &MemoryBinding {
                run: "child".into(),
                principal: "owner".into(),
                scopes: BTreeSet::from(["project:a".into()]),
                primary_scope: "project:a".into(),
                conversation: Some("chat".into()),
                task: Some("task".into()),
                parent_run: None,
                role: BindingRole::Worker,
                timezone: excalibur_memory::chrono_tz::UTC,
            },
            100,
        )
        .await
        .unwrap();
    let grant = MemoryGrant::new("owner", ["project:a"]).with_run("child");
    let doc = host
        .store
        .library_ingest(
            &grant,
            excalibur_memory::IngestRequest {
                scope: "project:a".into(),
                title: "Reactor notes".into(),
                media_type: "text/plain".into(),
                origin: None,
                bytes: b"Molten salt is the reactor coolant.".to_vec(),
                document_id: None,
                library: false,
            },
            100,
        )
        .await
        .unwrap();
    let (chunks, _) = host
        .store
        .library_search(&grant, "reactor coolant", 1, 100)
        .await
        .unwrap();
    let actions = MemoryFastActions::new(Arc::new(NoActions), host.clone());
    let answer = format!(
        "Salt {}. Unknown [[doc:fake#c1]].",
        chunks[0].cite.as_ref().unwrap()
    );
    let final_answer = actions
        .finalize_response_scoped("chat", "project:a", &answer)
        .await;
    assert!(final_answer.contains("Reactor notes"), "{final_answer}");
    assert!(final_answer.contains("[unsourced]"));
    assert!(!final_answer.contains("[["));
    let other = actions
        .finalize_response_scoped(
            "chat",
            "project:b",
            &format!("Claim [[doc:{}#c1]].", doc.document_id),
        )
        .await;
    assert!(other.contains("[unsourced]"));
    // The same master adapter executes actual scoped memory tools.
    let result = actions
        .invoke_scoped(
            "chat",
            "project:a",
            "memory_write",
            json!({"action":"add","kind":"fact","statement":"Reactor is under review"}),
            "user-memory",
        )
        .await
        .unwrap();
    assert!(result["memory_id"].is_string());
    host.stop();
    drop(actions);
    drop(host);
    db.shutdown().await.unwrap();
}

#[tokio::test]
async fn generated_reports_cannot_promote_memory_or_impersonate_a_profile_request() {
    let (_dir, db, host) = fixture().await;
    db.write(excalibur_store::Priority::Journal,|c| {
        c.execute("INSERT INTO principals(id,name) VALUES('owner','owner')",[])?;
        c.execute("INSERT INTO conversations(id,principal_id,active_scope) VALUES('chat','owner','general')",[])?;
        c.execute("INSERT INTO messages(id,conversation_id,principal_id,scope_id,role,text) VALUES('human','chat','owner','general','user','Use a concise style')",[])?;
        c.execute("INSERT INTO messages(id,conversation_id,principal_id,scope_id,role,text) VALUES('report','chat','owner','general','user','Generated worker report')",[])?;
        // Same durable authority marker installed by Master for literal human intake.
        c.execute("CREATE TABLE master_user_sources(id TEXT PRIMARY KEY)",[])?;
        c.execute("INSERT INTO master_user_sources(id) VALUES('human')",[])?;
        Ok(())
    }).await.unwrap();
    let actions = MemoryFastActions::new(Arc::new(NoActions), host.clone());
    let edit = json!({"doc":"user_profile","op":"append","text":"Prefers concise replies","provenance":"human"});
    let report = actions
        .invoke_source_scoped(
            "chat",
            "general",
            "report",
            "doc_edit",
            edit.clone(),
            "report-edit",
        )
        .await
        .unwrap();
    assert_eq!(report["outcome"], "proposed");
    let written = actions
        .invoke_source_scoped(
            "chat",
            "general",
            "report",
            "memory_write",
            json!({"action":"add","kind":"fact","statement":"Report supplied a fact"}),
            "report-memory",
        )
        .await
        .unwrap();
    let record = host
        .store
        .memory(
            &MemoryGrant::new("owner", ["general"]),
            written["memory_id"].as_str().unwrap(),
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(record.trust, Trust::Agent);
    let human = actions
        .invoke_source_scoped("chat", "general", "human", "doc_edit", edit, "human-edit")
        .await
        .unwrap();
    assert_eq!(human["outcome"], "applied");
    host.stop();
    drop(actions);
    drop(host);
    db.shutdown().await.unwrap();
}

struct UnavailableModel;
impl excalibur_models::ModelGateway for UnavailableModel {
    fn complete(
        &self,
        _: ModelRequest,
    ) -> BoxFuture<'_, Result<ModelResponse, excalibur_models::ModelError>> {
        Box::pin(async { Err(excalibur_models::ModelError::Provider) })
    }
}
#[tokio::test]
async fn actual_urgent_notices_never_become_user_preferences_during_review() {
    use excalibur_memory::{
        ConsolidationRoles, ExtractedFact, ExtractionOutput, MemoryStatus, ReviewBudget,
        testing::ScriptedExtractor,
    };
    let (_dir, db, host) = fixture().await;
    let master = excalibur_master::Master::open(
        db.clone(),
        excalibur_master::MasterConfig {
            principal: "owner".into(),
            ..Default::default()
        },
        Arc::new(UnavailableModel),
        None,
        Arc::new(NoActions),
        excalibur_vault::Redactor::default(),
    )
    .await
    .unwrap();
    db.write(excalibur_store::Priority::Journal,|c| {
        c.execute("INSERT INTO conversations(id,principal_id,active_scope) VALUES('chat','owner','general')",[])?; Ok(())
    }).await.unwrap();
    for id in ["generated-1", "generated-2"] {
        master
            .publish_notice(
                id,
                "chat",
                json!({"scope":"general","result":"Prefers purple UI"}),
                "urgent",
            )
            .await
            .unwrap();
    }
    master.shutdown().await;
    let extractor = Arc::new(ScriptedExtractor::new(|r| ExtractionOutput {
        facts: r
            .turns
            .iter()
            .filter(|t| t.role == "user" && !t.human_source)
            .map(|t| ExtractedFact {
                message_id: t.message_id.clone(),
                kind: MemoryKind::Preference,
                statement: "Prefers purple UI".into(),
                entities: vec![],
                attribute: None,
                valid_from: None,
                expires_at: None,
                temporal_change: false,
                confidence: None,
                importance: None,
                relations: vec![],
            })
            .collect(),
        tokens_used: 1,
    }));
    let grant = MemoryGrant::new("owner", ["general"]);
    let ids = host
        .store
        .close_segment(&grant, "general", extractor.as_ref(), 100)
        .await
        .unwrap();
    assert_eq!(ids.len(), 2);
    for id in ids {
        let memory = host.store.memory(&grant, &id).await.unwrap().unwrap();
        assert_eq!(memory.trust, Trust::Agent);
        assert_ne!(memory.status, MemoryStatus::Retired);
    }
    host.store
        .run_review(
            "owner",
            &["general".into()],
            &ConsolidationRoles {
                extractor,
                compactor: None,
            },
            &ReviewBudget::default(),
            &std::sync::atomic::AtomicBool::new(false),
            101,
        )
        .await
        .unwrap();
    assert!(
        host.store
            .profile_proposals(&grant)
            .await
            .unwrap()
            .is_empty()
    );
    host.stop();
    drop(host);
    db.shutdown().await.unwrap();
}
