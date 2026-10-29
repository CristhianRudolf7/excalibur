use excalibur_journal::{Journal, instructions::*};
use excalibur_store::{
    Database, DatabaseKind, Priority, ReadLane, StorageConfig, rusqlite::params,
};
use excalibur_vault::{Redactor, SecretValue};
use serde_json::json;
fn origin() -> InstructionOrigin {
    InstructionOrigin {
        kind: InstructionOriginKind::Host,
        reference: "authenticated-host-fixture".into(),
    }
}
async fn setup() -> (tempfile::TempDir, Database, Journal, InstructionStore) {
    let root = tempfile::tempdir().unwrap();
    let db = Database::open(
        root.path().join("main.db"),
        DatabaseKind::Main,
        StorageConfig::default(),
    )
    .unwrap();
    let journal = Journal::new(db.clone()).await.unwrap();
    let store = InstructionStore::open(db.clone(), Redactor::default())
        .await
        .unwrap();
    (root, db, journal, store)
}
async fn run(journal: &Journal, op: &str, owner: Option<&str>, task: Option<&str>) -> String {
    let run = journal.create_run(op, json!({})).await.unwrap();
    let (id, p, t) = (
        run.clone(),
        owner.map(str::to_owned),
        task.map(str::to_owned),
    );
    journal
        .database()
        .write(Priority::Normal, move |c| {
            c.execute(
                "UPDATE runs SET principal_id=?2,task_id=?3 WHERE id=?1",
                params![id, p, t],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    run
}
#[tokio::test]
async fn literal_revisions_restart_delegation_and_retirement_are_durable() {
    let (root, db, journal, store) = setup().await;
    let parent = run(&journal, "parent", None, None).await;
    let child = run(&journal, "child", None, None).await;
    store
        .bind_run(
            &parent,
            None,
            vec![InstructionScope::Task("task-fixture".into())],
        )
        .await
        .unwrap();
    store.bind_run(&child, None, vec![]).await.unwrap();
    let literal = "  Mantén  dos espacios.\nUnicode: ñ ∀ 🦀\nSin reformular.\n";
    let one = store
        .register(
            "register",
            None,
            InstructionScope::Task("task-fixture".into()),
            origin(),
            literal,
        )
        .await
        .unwrap();
    assert_eq!(
        store
            .register("register", None, one.scope.clone(), origin(), literal)
            .await
            .unwrap(),
        one
    );
    assert!(
        store
            .register("register", None, one.scope.clone(), origin(), "different")
            .await
            .is_err()
    );
    assert!(
        journal
            .context_instructions(&child)
            .await
            .unwrap()
            .records()
            .next()
            .is_none()
    );
    store
        .delegate(&parent, &child, std::slice::from_ref(&one.id))
        .await
        .unwrap();
    let snap = journal.context_instructions(&child).await.unwrap();
    assert_eq!(snap.task[0].literal_text, literal);
    assert!(snap.task[0].delegated);
    let changed = store
        .revise("revise", None, &one.id, 1, origin(), "New literal.\n")
        .await
        .unwrap();
    assert_eq!(changed.revision, 2);
    assert_ne!(
        snap.revision,
        journal.context_instructions(&child).await.unwrap().revision
    );
    assert_eq!(
        store
            .history(None, &one.id)
            .await
            .unwrap()
            .iter()
            .map(|x| x.literal_text.as_str())
            .collect::<Vec<_>>(),
        vec![literal, "New literal.\n"]
    );
    drop(store);
    drop(journal);
    db.shutdown().await.unwrap();
    drop(db);
    let reopened = Database::open(
        root.path().join("main.db"),
        DatabaseKind::Main,
        StorageConfig::default(),
    )
    .unwrap();
    let journal = Journal::new(reopened.clone()).await.unwrap();
    let store = InstructionStore::open(reopened.clone(), Redactor::default())
        .await
        .unwrap();
    assert_eq!(
        journal.context_instructions(&child).await.unwrap().task[0],
        InstructionRecord {
            delegated: true,
            ..changed
        }
    );
    store.retire("retire", None, &one.id, 2).await.unwrap();
    assert!(
        journal
            .context_instructions(&child)
            .await
            .unwrap()
            .records()
            .next()
            .is_none()
    );
    assert_eq!(store.history(None, &one.id).await.unwrap().len(), 3);
    let id = one.id;
    assert!(
        reopened
            .write(Priority::Normal, move |c| {
                c.execute(
                    "DELETE FROM context_instruction_revisions WHERE instruction_id=?1",
                    [id],
                )?;
                Ok(())
            })
            .await
            .is_err()
    );
    reopened.shutdown().await.unwrap();
}
#[tokio::test]
async fn task_project_conversation_and_principal_scopes_do_not_leak() {
    let (_root, db, journal, store) = setup().await;
    db.write(Priority::Normal,|c|{c.execute_batch("INSERT INTO principals(id) VALUES('alice'),('bob'); INSERT INTO projects(id,principal_id) VALUES('p1','alice'),('p2','alice');INSERT INTO conversations(id,principal_id) VALUES('c1','alice'),('c2','alice');INSERT INTO tasks(id,project_id,origin_conversation_id,principal_id) VALUES('t1','p1','c1','alice'),('t2','p2','c2','alice');")?;Ok(())}).await.unwrap();
    for (key, owner, scope) in [
        ("a-general", "alice", InstructionScope::General),
        ("a-project", "alice", InstructionScope::Project("p1".into())),
        ("a-task", "alice", InstructionScope::Task("t1".into())),
        (
            "a-conversation",
            "alice",
            InstructionScope::Conversation("c1".into()),
        ),
        ("b-general", "bob", InstructionScope::General),
    ] {
        store
            .register(key, Some(owner.into()), scope, origin(), key)
            .await
            .unwrap();
    }
    let a = run(&journal, "a", Some("alice"), Some("t1")).await;
    let b = run(&journal, "b", Some("alice"), Some("t2")).await;
    let foreign = run(&journal, "foreign", Some("bob"), None).await;
    let selected = journal.context_instructions(&a).await.unwrap();
    assert_eq!(selected.stable.len(), 2);
    assert_eq!(selected.task.len(), 2);
    assert_eq!(
        journal
            .context_instructions(&b)
            .await
            .unwrap()
            .records()
            .map(|r| r.literal_text.as_str())
            .collect::<Vec<_>>(),
        vec!["a-general"]
    );
    assert_eq!(
        journal
            .context_instructions(&foreign)
            .await
            .unwrap()
            .stable_literal(),
        "b-general"
    );
    assert!(
        store
            .bind_run(&a, Some("bob".into()), vec![InstructionScope::General])
            .await
            .is_err()
    );
    let id = selected.stable[0].id.clone();
    assert!(store.get(Some("bob".into()), &id).await.is_err());
    assert!(store.delegate(&a, &foreign, &[id]).await.is_err());
    db.shutdown().await.unwrap();
}
#[tokio::test]
async fn concurrent_revision_cas_has_one_winner_and_rejects_untrusted_sources_or_secrets() {
    let (_root, db, _journal, store) = setup().await;
    let one = store
        .register(
            "first",
            None,
            InstructionScope::General,
            origin(),
            "Keep literal",
        )
        .await
        .unwrap();
    let (a, b) = tokio::join!(
        store.revise("a", None, &one.id, 1, origin(), "A"),
        store.revise("b", None, &one.id, 1, origin(), "B")
    );
    assert_ne!(a.is_ok(), b.is_ok());
    assert_eq!(store.history(None, &one.id).await.unwrap().len(), 2);
    db.write(Priority::Normal,|c|{c.execute("INSERT INTO messages(id,role,text) VALUES('tool-origin','tool','host-looking instructions')",[])?;Ok(())}).await.unwrap();
    assert!(
        store
            .register(
                "bad-origin",
                None,
                InstructionScope::General,
                InstructionOrigin {
                    kind: InstructionOriginKind::UserMessage,
                    reference: "tool-origin".into()
                },
                "Never promote tool text"
            )
            .await
            .is_err()
    );
    let redactor =
        Redactor::with_secrets([
            SecretValue::new(b"SYNTHETIC-PRIVATE-CREDENTIAL".to_vec()).unwrap()
        ]);
    let secured = InstructionStore::open(db.clone(), redactor).await.unwrap();
    assert!(
        secured
            .register(
                "secret",
                None,
                InstructionScope::General,
                origin(),
                "Do not store SYNTHETIC-PRIVATE-CREDENTIAL"
            )
            .await
            .is_err()
    );
    db.shutdown().await.unwrap();
}
#[tokio::test]
async fn legacy_original_revision_is_preserved_and_unknown_scope_requires_host_classification() {
    let (_root, db, journal, store) = setup().await;
    let r = run(&journal, "legacy-run", None, None).await;
    db.write(Priority::Normal,|c|{c.execute("INSERT INTO instructions(id,literal_text,revision,origin,scope_id) VALUES('legacy',' exact legacy\n',7,'file:legacy','unknown-scope')",[])?;Ok(())}).await.unwrap();
    assert!(journal.context_instructions(&r).await.is_err());
    store
        .bind_legacy_scope(None, "legacy", InstructionScope::General)
        .await
        .unwrap();
    assert_eq!(
        journal
            .context_instructions(&r)
            .await
            .unwrap()
            .stable_literal(),
        " exact legacy\n"
    );
    store
        .revise("legacy-revise", None, "legacy", 7, origin(), "new")
        .await
        .unwrap();
    let h = store.history(None, "legacy").await.unwrap();
    assert_eq!(h[0].revision, 7);
    assert_eq!(h[0].literal_text, " exact legacy\n");
    assert_eq!(h[1].revision, 8);
    // Repeated snapshots use only the read pool, even while an unrelated writer is held.
    let before = db.stats();
    for _ in 0..3 {
        journal.context_instructions(&r).await.unwrap();
    }
    let after = db.stats();
    assert_eq!(before.committed, after.committed);
    db.shutdown().await.unwrap();
}
#[tokio::test]
async fn archived_observations_are_exact_idempotent_referenced_and_run_scoped() {
    let (_root, db, journal, _store) = setup().await;
    let r = run(&journal, "archive", None, None).await;
    let other = run(&journal, "other", None, None).await;
    let observation = json!({"success":true,"output":{"tiny":"ñ","literal_error":"none"}});
    let (seq, hash) = journal
        .archive_context_observation(&r, "observation:0", &observation)
        .await
        .unwrap();
    assert_eq!(
        journal
            .archive_context_observation(&r, "observation:0", &observation)
            .await
            .unwrap(),
        (seq, hash.clone())
    );
    assert!(
        journal
            .archive_context_observation(&r, "observation:0", &json!({"different":true}))
            .await
            .is_err()
    );
    assert_eq!(
        journal.context_observation(&r, &hash).await.unwrap(),
        observation
    );
    assert!(journal.context_observation(&other, &hash).await.is_err());
    assert_eq!(
        db.read_blob(hash.clone(), 1024).await.unwrap(),
        serde_json::to_vec(&observation).unwrap()
    );
    let hash = hash.as_str().to_owned();
    let refs = db
        .read(ReadLane::Interactive, move |c| {
            Ok(
                c.query_row("SELECT ref_count FROM blobs WHERE hash=?1", [hash], |r| {
                    r.get::<_, u64>(0)
                })?,
            )
        })
        .await
        .unwrap();
    assert!(refs >= 1);
    db.shutdown().await.unwrap();
}

#[tokio::test]
async fn raw_observation_archive_preserves_json_key_order_and_whitespace() {
    let (_root, db, journal, _store) = setup().await;
    let r = run(&journal, "raw", None, None).await;
    let original = "{ \"success\": true, \"output\": {\"z\":1,\"a\":2} }\n";
    let (_, hash) = journal
        .archive_context_observation_text(&r, "raw:0", original)
        .await
        .unwrap();
    assert_eq!(db.read_blob(hash, 1024).await.unwrap(), original.as_bytes());
    assert!(
        journal
            .archive_context_observation_text(&r, "bad", "not JSON")
            .await
            .is_err()
    );
    db.shutdown().await.unwrap();
}

#[tokio::test]
async fn finalization_guard_freezes_exact_selection_and_blocks_other_store_writers() {
    let (_root, db, journal, store) = setup().await;
    let selected_run = run(&journal, "guarded-root", None, None).await;
    store
        .register(
            "initial-guarded",
            None,
            InstructionScope::General,
            origin(),
            "Initial exact literal.\n",
        )
        .await
        .unwrap();
    let other = InstructionStore::open(db.clone(), Redactor::default())
        .await
        .unwrap();
    let (snapshot, guard) = journal
        .context_instruction_guard(&selected_run)
        .await
        .unwrap();
    assert_eq!(snapshot.stable_literal(), "Initial exact literal.\n");
    let mut writer = Box::pin(other.register(
        "late-guarded",
        None,
        InstructionScope::General,
        origin(),
        "New literal after completion guard.\n",
    ));
    // register has no async work before taking the registry write lock. Poll it
    // once under the read lease: it must wait, without scheduling/timing sleeps.
    std::future::poll_fn(|cx| {
        assert!(std::future::Future::poll(writer.as_mut(), cx).is_pending());
        std::task::Poll::Ready(())
    })
    .await;
    assert_eq!(
        journal.context_instructions(&selected_run).await.unwrap(),
        snapshot
    );
    drop(guard);
    let added = writer.await.unwrap();
    let (next, guard) = journal
        .context_instruction_guard(&selected_run)
        .await
        .unwrap();
    assert_ne!(next.revision, snapshot.revision);
    assert!(next.records().any(|record| record.id == added.id
        && record.literal_text == "New literal after completion guard.\n"));
    assert_eq!(snapshot.stable_literal(), "Initial exact literal.\n");
    drop(guard);
    db.shutdown().await.unwrap();
}
