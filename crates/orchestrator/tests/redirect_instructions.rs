mod support;
use excalibur_context::SessionPolicy;
use excalibur_engine::{Engine, Status};
use excalibur_journal::{Journal, instructions::InstructionStore};
use excalibur_orchestrator::{Control, Limits, Orchestrator};
use excalibur_store::{Database, DatabaseKind, Priority};
use excalibur_types::runtime::Message;
use std::sync::Arc;
use support::{delegation, model, no_tools};

#[tokio::test]
async fn redirect_revisions_are_literal_delegated_compacted_and_recovered_without_scope_broadcast()
{
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("main.db");
    let db = Database::open(&path, DatabaseKind::Main, Default::default()).unwrap();
    let engine = Arc::new(
        Engine::new(Journal::new(db.clone()).await.unwrap(), model(), no_tools())
            .with_context_policy(SessionPolicy {
                compaction_cap: 10_000,
                recent_activity_tokens: 1000,
                ..Default::default()
            }),
    );
    let host = Orchestrator::open(db.clone(), engine.clone(), Limits::default())
        .await
        .unwrap();
    let mut token = support::token();
    token
        .data_scopes
        .extend(["project:a".into(), "project:b".into()]);
    let master = host.register_master("Master", token).await.unwrap();
    let mut p = delegation("p", "project:a", 300);
    p.token.data_scopes.insert("project:a".into());
    let parent = host.spawn(&master.id, p).await.unwrap();
    let mut c = delegation("c", "project:a", 60);
    c.token.data_scopes.insert("project:a".into());
    for n in 0..100 {
        c.spec.context.activity.push(Message::text(
            "user",
            format!(
                "Synthetic prior context {n}: {}",
                "bounded history ".repeat(100)
            ),
        ));
    }
    let child = host.spawn(&parent.id, c).await.unwrap();
    let mut q = delegation("q", "project:b", 300);
    q.token.data_scopes.insert("project:b".into());
    let unrelated = host.spawn(&master.id, q).await.unwrap();
    host.control_keyed("pause-only-parent", &parent.id, Control::Interrupt)
        .await
        .unwrap();
    assert_eq!(host.status(&parent.id).await.unwrap().state, "paused");
    assert_eq!(host.status(&child.id).await.unwrap().state, "ready");
    let first = "  Preserve  both spaces.\nExport PDF — versión literal.\n";
    host.control_keyed(
        "redirect-first",
        &parent.id,
        Control::Redirect { text: first.into() },
    )
    .await
    .unwrap();
    assert_eq!(host.status(&parent.id).await.unwrap().state, "ready");
    let parent_run = parent.run_id.as_ref().unwrap();
    let child_run = child.run_id.as_ref().unwrap();
    let unrelated_run = unrelated.run_id.as_ref().unwrap();
    let initial = engine
        .journal()
        .context_instructions(parent_run)
        .await
        .unwrap()
        .task
        .into_iter()
        .find(|record| record.literal_text == first)
        .unwrap();
    assert_eq!(initial.revision, 1);
    let inherited = engine
        .journal()
        .context_instructions(child_run)
        .await
        .unwrap()
        .task
        .into_iter()
        .find(|record| record.id == initial.id)
        .unwrap();
    assert!(inherited.delegated);
    assert_eq!(inherited.literal_text, first);
    assert!(
        engine
            .journal()
            .context_instructions(unrelated_run)
            .await
            .unwrap()
            .records()
            .all(|record| record.id != initial.id)
    );
    let second = "  Preserve  both spaces.\nExport CSV, never PDF — ñ 🦀.\n";
    host.control_keyed(
        "redirect-second",
        &parent.id,
        Control::Redirect {
            text: second.into(),
        },
    )
    .await
    .unwrap();
    let registry = InstructionStore::open(db.clone(), Default::default())
        .await
        .unwrap();
    let latest = registry.get(None, &initial.id).await.unwrap();
    assert_eq!(latest.revision, 2);
    assert_eq!(latest.literal_text, second);
    // Replay a control whose effect was confirmed before its host acknowledgement.
    db.write(Priority::Interactive,|connection|{connection.execute("UPDATE orchestration_controls SET state='pending',report=NULL WHERE operation='redirect-second'",[])?;Ok(())}).await.unwrap();
    assert_eq!(host.recover_controls().await.unwrap(), 1);
    assert_eq!(registry.get(None, &initial.id).await.unwrap().revision, 2);
    assert_eq!(registry.history(None, &initial.id).await.unwrap().len(), 2);
    let child_current = engine
        .journal()
        .context_instructions(child_run)
        .await
        .unwrap()
        .task
        .into_iter()
        .find(|record| record.id == initial.id)
        .unwrap();
    assert_eq!(child_current.literal_text, second);
    assert_eq!(child_current.revision, 2);
    assert!(host.mailbox(&unrelated.id).await.unwrap().is_empty());
    assert_eq!(host.status(&unrelated.id).await.unwrap().state, "ready");
    let parent_id = parent.id.clone();
    let child_id = child.id.clone();
    let child_run = child_run.clone();
    let unrelated_run = unrelated_run.clone();
    let instruction_id = initial.id;
    drop(registry);
    drop(host);
    drop(engine);
    db.shutdown().await.unwrap();
    drop(db);
    let reopened = Database::open(&path, DatabaseKind::Main, Default::default()).unwrap();
    let journal = Journal::new(reopened.clone()).await.unwrap();
    let engine = Arc::new(Engine::new(journal.clone(), model(), no_tools()));
    let host = Orchestrator::open(reopened.clone(), engine.clone(), Limits::default())
        .await
        .unwrap();
    let selected = journal
        .context_instructions(&child_run)
        .await
        .unwrap()
        .task
        .into_iter()
        .find(|record| record.id == instruction_id)
        .unwrap();
    assert_eq!(selected.literal_text, second);
    assert_eq!(selected.revision, 2);
    assert!(
        journal
            .context_instructions(&unrelated_run)
            .await
            .unwrap()
            .records()
            .all(|record| record.id != instruction_id)
    );
    host.deliver_mailbox(&child_id).await.unwrap();
    let result = engine.run(&child_run).await.unwrap();
    assert_eq!(result.status, Status::Completed);
    let events = journal.replay(&child_run, 0).await.unwrap();
    let request = &events
        .iter()
        .find(|event| event.kind == "model_request")
        .unwrap()
        .payload["request"];
    let messages = request["messages"].as_array().unwrap();
    assert!(messages.iter().any(|message| {
        message["content"]
            .as_str()
            .is_some_and(|content| content.contains(second))
    }));
    let preparation = events
        .iter()
        .find(|event| event.kind == "context_prepared")
        .unwrap();
    assert!(preparation.payload["receipt"]["compaction"].is_object());
    let report = host
        .control_keyed(
            "redirect-second",
            &parent_id,
            Control::Redirect {
                text: second.into(),
            },
        )
        .await
        .unwrap();
    assert!(report.affected.contains(&parent_id));
    reopened.shutdown().await.unwrap();
}
