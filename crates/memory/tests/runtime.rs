//! Host authority, durable keyed tools, cache blocks and the actual citation run tree.
mod common;
use common::*;
use excalibur_memory::*;
use excalibur_store::Priority;
use excalibur_types::runtime::*;
use serde_json::{Value, json};
use std::{collections::BTreeSet, sync::Arc};

struct EmptyTools;
impl ToolRuntime for EmptyTools {
    fn definitions(&self) -> Vec<ToolDefinition> {
        vec![]
    }
    fn prepare<'a>(
        &'a self,
        _: &'a str,
        _: ToolCall,
    ) -> BoxFuture<'a, std::result::Result<PreparedTool, String>> {
        Box::pin(async { Err("unknown tool".into()) })
    }
    fn execute<'a>(
        &'a self,
        _: &'a str,
        _: &'a str,
        _: &'a PreparedTool,
    ) -> BoxFuture<'a, ToolResult> {
        Box::pin(async { ToolResult::error("unknown tool") })
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
fn binding(run: &str) -> MemoryBinding {
    MemoryBinding {
        run: run.into(),
        principal: "owner".into(),
        scopes: BTreeSet::from(["general".into()]),
        primary_scope: "general".into(),
        conversation: Some("chat".into()),
        task: Some("task".into()),
        parent_run: None,
        role: BindingRole::Worker,
        timezone: chrono_tz::Europe::Berlin,
    }
}
fn call(name: &str, args: Value) -> ToolCall {
    ToolCall {
        id: "call".into(),
        name: name.into(),
        arguments: args.to_string(),
    }
}
fn write_args(text: &str) -> Value {
    json!({"action":"add","kind":"fact","statement":text,"sources":[{"kind":"message","id":"source"}]})
}

#[tokio::test]
async fn binding_survives_restart_without_changing_authority() {
    let f = fixture().await;
    let b = binding("worker");
    f.store.bind_run(&b, NOW).await.unwrap();
    let restarted = MemoryStore::open(f.db.clone()).await.unwrap();
    let recovered = restarted.run_binding("worker").await.unwrap().unwrap();
    assert_eq!(recovered.timezone, chrono_tz::Europe::Berlin);
    assert_eq!(recovered.conversation, b.conversation);
    assert_eq!(recovered.task, b.task);
    restarted.bind_run(&b, NOW + 1).await.unwrap();
    for field in [
        "principal",
        "scope",
        "conversation",
        "task",
        "parent",
        "role",
        "timezone",
    ] {
        let mut changed = b.clone();
        match field {
            "principal" => changed.principal = "other".into(),
            "scope" => {
                changed.scopes.insert("private".into());
            }
            "conversation" => changed.conversation = Some("other".into()),
            "task" => changed.task = Some("other".into()),
            "parent" => changed.parent_run = Some("other".into()),
            "role" => changed.role = BindingRole::Master,
            _ => changed.timezone = chrono_tz::UTC,
        }
        assert!(
            restarted.bind_run(&changed, NOW + 1).await.is_err(),
            "{field}"
        );
    }
}

#[tokio::test]
async fn tool_replay_is_keyed_and_preparations_cannot_change_owner_or_tool() {
    let f = fixture().await;
    let runtime = MemoryTools::new(Arc::new(EmptyTools), f.store.clone());
    assert!(
        runtime
            .run_definitions("unbound")
            .await
            .unwrap()
            .unwrap()
            .is_empty()
    );
    assert!(
        runtime
            .prepare("unbound", call("memory_write", write_args("x")))
            .await
            .is_err()
    );
    runtime.bind(binding("worker")).await.unwrap();
    let tool = runtime
        .prepare(
            "worker",
            call("memory_write", write_args("The reactor uses salt")),
        )
        .await
        .unwrap();
    let first = runtime.execute("worker", "key", &tool).await;
    assert!(first.success, "{first:?}");
    let restarted = MemoryTools::new(
        Arc::new(EmptyTools),
        MemoryStore::open(f.db.clone()).await.unwrap(),
    );
    let replay = restarted.execute("worker", "key", &tool).await;
    assert_eq!(replay.output, first.output);
    assert!(matches!(
        restarted.reconcile("worker", "key", &tool).await,
        Reconcile::Done(_)
    ));
    let changed = restarted
        .prepare(
            "worker",
            call("memory_write", write_args("A different fact")),
        )
        .await
        .unwrap();
    assert!(!restarted.execute("worker", "key", &changed).await.success);
    assert!(!restarted.execute("stranger", "key", &tool).await.success);
    let mut renamed = tool.clone();
    renamed.call.name = "memory_propose".into();
    assert!(
        !restarted
            .execute("worker", "renamed", &renamed)
            .await
            .success
    );
    assert!(matches!(
        restarted.reconcile("worker", "renamed", &renamed).await,
        Reconcile::Uncertain(_)
    ));
    let memory = f
        .store
        .memory(
            &grant(&["general"]),
            first.output["memory_id"].as_str().unwrap(),
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(memory.trust, Trust::Agent);
    let mut outside = write_args("Private fact");
    outside["scope"] = json!("private");
    let denied = restarted
        .prepare("worker", call("memory_write", outside))
        .await
        .unwrap();
    let denied = restarted.execute("worker", "outside", &denied).await;
    assert_eq!(denied.output["code"], "memory_scope_denied");
}

#[tokio::test]
async fn interrupted_receipt_never_retries_a_possibly_applied_effect() {
    let f = fixture().await;
    let runtime = MemoryTools::new(Arc::new(EmptyTools), f.store.clone());
    runtime.bind(binding("worker")).await.unwrap();
    let tool = runtime
        .prepare(
            "worker",
            call(
                "doc_edit",
                json!({"doc":"notebook","op":"append","scope":"general","text":"Keep this note"}),
            ),
        )
        .await
        .unwrap();
    assert!(runtime.execute("worker", "key", &tool).await.success);
    // Represents a crash after an effect, before its completed receipt was committed.
    f.db.write(Priority::Journal, |c| {
        c.execute("UPDATE memory_tool_receipts SET state='pending',result='null' WHERE run_id='worker' AND key='key'", [])?;
        Ok(())
    }).await.unwrap();
    let restarted = MemoryTools::new(
        Arc::new(EmptyTools),
        MemoryStore::open(f.db.clone()).await.unwrap(),
    );
    assert!(matches!(
        restarted.reconcile("worker", "key", &tool).await,
        Reconcile::Uncertain(_)
    ));
    assert!(!restarted.execute("worker", "key", &tool).await.success);
    let count: i64 =
        f.db.read(excalibur_store::ReadLane::Interactive, |c| {
            Ok(c.query_row(
                "SELECT count(*) FROM doc_fragments WHERE text='Keep this note'",
                [],
                |r| r.get(0),
            )?)
        })
        .await
        .unwrap();
    assert_eq!(count, 1);
}

#[tokio::test]
async fn citation_tree_follows_descendants_but_never_another_principal() {
    let f = fixture().await;
    let mut master = binding("master");
    master.role = BindingRole::Master;
    f.store.bind_run(&master, NOW).await.unwrap();
    let child = binding("child");
    f.store.bind_run(&child, NOW).await.unwrap();
    let mut grandchild = binding("grandchild");
    grandchild.parent_run = Some("child".into());
    grandchild.conversation = None;
    f.store.bind_run(&grandchild, NOW).await.unwrap();
    let mut foreign = binding("foreign");
    foreign.principal = "other".into();
    foreign.parent_run = Some("child".into());
    f.store.bind_run(&foreign, NOW).await.unwrap();
    let tree = BindingRunTree {
        store: f.store.clone(),
    };
    assert_eq!(
        tree.subtree("master").await.unwrap(),
        BTreeSet::from(["master".into(), "child".into(), "grandchild".into()])
    );
    let _doc = f
        .store
        .library_ingest(
            &grant(&["general"]),
            IngestRequest {
                scope: "general".into(),
                title: "Reactor".into(),
                media_type: "text/plain".into(),
                origin: None,
                bytes: b"The reactor coolant is molten salt.".to_vec(),
                document_id: None,
                library: false,
            },
            NOW,
        )
        .await
        .unwrap();
    let (chunks, _) = f
        .store
        .library_search(
            &grant(&["general"]).with_run("grandchild"),
            "reactor coolant",
            1,
            NOW,
        )
        .await
        .unwrap();
    let answer = format!("Salt {}.", chunks[0].cite.as_ref().unwrap());
    let checked = f
        .store
        .validate_citations(&master.grant(), "master", &tree, &answer, "en")
        .await
        .unwrap();
    assert_eq!(checked.citations.len(), 1);
    assert!(checked.text.contains("no pagination"));
    assert!(checked.text.contains("Reactor"));
}

#[tokio::test]
async fn worker_assertion_cannot_replace_a_user_memory_by_claiming_a_change() {
    let f = fixture().await;
    let g = grant(&["general"]);
    let mut original = input("general", MemoryKind::Fact, "I live in Berlin");
    original.entities = vec!["Owner".into()];
    original.attribute = Some("home_city".into());
    let first = f.store.memory_write(&g, original, NOW).await.unwrap();
    let mut proposed = input("general", MemoryKind::Fact, "Now I live in Paris");
    proposed.entities = vec!["Owner".into()];
    proposed.attribute = Some("home_city".into());
    proposed.temporal_change = true;
    let result = f
        .store
        .memory_write(&g.clone().with_max_trust(Trust::Agent), proposed, NOW + 1)
        .await
        .unwrap();
    assert_eq!(result.decision, AdmissionDecision::Deferred);
    assert_eq!(
        f.store
            .memory(&g, &first.memory_id)
            .await
            .unwrap()
            .unwrap()
            .status,
        MemoryStatus::Active
    );
}

struct InterruptedEmbedder {
    marker: std::path::PathBuf,
}
impl EmbeddingProvider for InterruptedEmbedder {
    fn model(&self) -> String {
        "interrupted-fixture".into()
    }
    fn source_dimensions(&self) -> u32 {
        32
    }
    fn location(&self) -> ProviderLocation {
        ProviderLocation::Remote
    }
    fn embed<'a>(
        &'a self,
        _: &'a [String],
    ) -> BoxFuture<'a, std::result::Result<Vec<Vec<f32>>, String>> {
        Box::pin(async move {
            std::fs::write(&self.marker, b"candidate stored; embedding in progress").unwrap();
            std::future::pending().await
        })
    }
}

/// Launched only by the parent test; never uses a remote provider.
#[test]
#[ignore]
fn memory_receipt_crash_child() {
    let Some(root) = std::env::var_os("EXCALIBUR_MEMORY_CRASH_FIXTURE") else {
        return;
    };
    let root = std::path::PathBuf::from(root);
    tokio::runtime::Runtime::new().unwrap().block_on(async {
        let db = excalibur_store::Database::open(
            root.join("main.db"),
            excalibur_store::DatabaseKind::Main,
            Default::default(),
        )
        .unwrap();
        let store = MemoryStore::open(db).await.unwrap();
        store
            .embeddings()
            .attach(
                Arc::new(InterruptedEmbedder {
                    marker: root.join("effect-started"),
                }),
                512,
                NOW,
            )
            .await
            .unwrap();
        let runtime = MemoryTools::new(Arc::new(EmptyTools), store);
        runtime.bind(binding("worker")).await.unwrap();
        let tool = runtime
            .prepare(
                "worker",
                call(
                    "memory_write",
                    write_args("Stored before process interruption"),
                ),
            )
            .await
            .unwrap();
        runtime.execute("worker", "crash-key", &tool).await;
    });
}

#[test]
fn process_interruption_leaves_uncertain_effect_without_duplicating_it() {
    let dir = tempfile::tempdir().unwrap();
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "memory_receipt_crash_child",
            "--ignored",
            "--nocapture",
        ])
        .env("EXCALIBUR_MEMORY_CRASH_FIXTURE", dir.path())
        .stdout(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    while !dir.path().join("effect-started").exists() {
        if let Some(status) = child.try_wait().unwrap() {
            panic!("crash fixture exited early: {status}");
        }
        if std::time::Instant::now() >= deadline {
            child.kill().unwrap();
            child.wait().unwrap();
            panic!("crash fixture did not reach its durable effect");
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    child.kill().unwrap();
    child.wait().unwrap();
    tokio::runtime::Runtime::new().unwrap().block_on(async {
        let db = excalibur_store::Database::open(dir.path().join("main.db"), excalibur_store::DatabaseKind::Main, Default::default()).unwrap();
        let store = MemoryStore::open(db.clone()).await.unwrap();
        let runtime = MemoryTools::new(Arc::new(EmptyTools), store);
        let tool = runtime.prepare("worker", call("memory_write", write_args("Stored before process interruption"))).await.unwrap();
        assert!(matches!(runtime.reconcile("worker", "crash-key", &tool).await, Reconcile::Uncertain(_)));
        assert!(!runtime.execute("worker", "crash-key", &tool).await.success);
        let count: i64 = db.read(excalibur_store::ReadLane::Interactive, |c| Ok(c.query_row("SELECT count(*) FROM memories WHERE statement='Stored before process interruption'",[],|r|r.get(0))?)).await.unwrap();
        assert_eq!(count,1);
        drop(runtime); db.shutdown().await.unwrap();
    });
}

#[tokio::test]
async fn pinned_preferences_respect_validity_and_expiry_before_review() {
    let f = fixture().await;
    let g = grant(&["general"]);
    for (text, from, to, expires) in [
        ("Current preference", Some(NOW - DAY), None, None),
        ("Future preference", Some(NOW + DAY), None, None),
        ("Past preference", None, Some(NOW), None),
        ("Expired preference", None, None, Some(NOW)),
    ] {
        let mut preference = input("general", MemoryKind::Preference, text);
        preference.valid_from = from;
        preference.valid_to = to;
        preference.expires_at = expires;
        f.store.memory_write(&g, preference, NOW).await.unwrap();
    }
    let pinned = f.store.pinned_preferences(&g, 16, NOW).await.unwrap();
    assert_eq!(
        pinned
            .iter()
            .map(|(_, _, text)| text.as_str())
            .collect::<Vec<_>>(),
        ["Current preference"]
    );
}

#[tokio::test]
async fn repeated_agent_profile_suggestions_never_become_user_evidence() {
    let f = fixture().await;
    let worker = grant(&["general"]).with_max_trust(Trust::Agent);
    let mut proposal = None;
    for source in ["old-human-a", "old-human-b"] {
        let outcome = f
            .store
            .doc_edit(
                &worker,
                &DocName::UserProfile,
                DocEdit::Append {
                    heading_path: vec!["Style".into()],
                    scope: None,
                    text: "Always use the suggested style".into(),
                },
                EditContext {
                    actor: "worker".into(),
                    provenance: Some(source.into()),
                    explicit_user_request: false,
                },
                NOW,
            )
            .await
            .unwrap();
        if let EditOutcome::Proposed { proposal_id, .. } = outcome {
            proposal = Some(proposal_id);
        } else {
            panic!("expected a proposal");
        }
    }
    assert!(
        f.store
            .confirm_profile_proposals(&grant(&["general"]), None, NOW)
            .await
            .unwrap()
            .is_empty()
    );
    let proposal = proposal.unwrap();
    assert!(
        f.store
            .confirm_profile_proposals(&worker, Some(&proposal), NOW)
            .await
            .is_err()
    );
    assert_eq!(
        f.store
            .confirm_profile_proposals(&grant(&["general"]), Some(&proposal), NOW)
            .await
            .unwrap(),
        [proposal]
    );
}
