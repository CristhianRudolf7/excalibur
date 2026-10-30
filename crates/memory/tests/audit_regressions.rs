//! Regressions for the independent review of snapshot 41b90ecc.
mod common;
use common::*;
use excalibur_memory::{testing::ScriptedExtractor, *};
use excalibur_store::{Priority, rusqlite};
use serde_json::json;
use std::{
    collections::BTreeSet,
    sync::{Arc, atomic::AtomicBool},
};
fn ctx(provenance: Option<&str>, explicit: bool) -> EditContext {
    EditContext {
        actor: "actor".into(),
        provenance: provenance.map(str::to_string),
        explicit_user_request: explicit,
    }
}
fn applied(result: EditOutcome) -> (String, String) {
    match result {
        EditOutcome::Applied {
            fragment_id, hash, ..
        } => (fragment_id, hash),
        other => panic!("{other:?}"),
    }
}
#[tokio::test]
async fn notebook_history_requires_the_fragment_scope_even_after_deletion() {
    let f = fixture().await;
    let a = grant(&["project:a"]);
    let b = grant(&["project:b"]);
    let (id, hash) = applied(
        f.store
            .doc_edit(
                &b,
                &DocName::Notebook,
                DocEdit::Append {
                    heading_path: vec!["Private".into()],
                    scope: Some("project:b".into()),
                    text: "Project B private notes".into(),
                },
                ctx(None, false),
                NOW,
            )
            .await
            .unwrap(),
    );
    assert!(
        f.store
            .fragment_history(&a, &DocName::Notebook, &id)
            .await
            .unwrap()
            .is_empty()
    );
    f.store
        .doc_edit(
            &b,
            &DocName::Notebook,
            DocEdit::Delete {
                fragment_id: id.clone(),
                expected_hash: hash,
            },
            ctx(None, false),
            NOW + 1,
        )
        .await
        .unwrap();
    assert!(
        f.store
            .fragment_history(&a, &DocName::Notebook, &id)
            .await
            .unwrap()
            .is_empty()
    );
    let own = f
        .store
        .fragment_history(&b, &DocName::Notebook, &id)
        .await
        .unwrap();
    assert_eq!(own.len(), 2);
    assert!(own[0].1.contains("Project B private notes"));
}
#[tokio::test]
async fn profile_revert_obeys_the_same_provenance_request_and_trust_rules_as_edit() {
    let f = fixture().await;
    let g = grant(&["general"]);
    let profile = DocName::UserProfile;
    let (id, hash) = applied(
        f.store
            .doc_edit(
                &g,
                &profile,
                DocEdit::Append {
                    heading_path: vec!["Style".into()],
                    scope: None,
                    text: "Original style".into(),
                },
                ctx(Some("m1"), true),
                NOW,
            )
            .await
            .unwrap(),
    );
    f.store
        .doc_edit(
            &g,
            &profile,
            DocEdit::Replace {
                fragment_id: id.clone(),
                expected_hash: hash,
                text: "Current style".into(),
            },
            ctx(Some("m2"), true),
            NOW + 1,
        )
        .await
        .unwrap();
    assert!(
        f.store
            .revert_fragment(&g, &profile, &id, 1, ctx(None, false), NOW + 2)
            .await
            .is_err()
    );
    assert!(
        f.store
            .revert_fragment(&g, &profile, &id, 1, ctx(None, true), NOW + 2)
            .await
            .is_err()
    );
    assert!(matches!(
        f.store
            .revert_fragment(&g, &profile, &id, 1, ctx(Some("m3"), false), NOW + 2)
            .await
            .unwrap(),
        EditOutcome::Proposed { .. }
    ));
    assert!(matches!(
        f.store
            .revert_fragment(
                &g.clone().with_max_trust(Trust::Agent),
                &profile,
                &id,
                1,
                ctx(Some("m4"), true),
                NOW + 2
            )
            .await
            .unwrap(),
        EditOutcome::Proposed { .. }
    ));
    assert_eq!(
        f.store
            .fragment_history(&g, &profile, &id)
            .await
            .unwrap()
            .len(),
        2
    );
    applied(
        f.store
            .revert_fragment(&g, &profile, &id, 1, ctx(Some("m5"), true), NOW + 3)
            .await
            .unwrap(),
    );
    let versions = f.store.fragment_history(&g, &profile, &id).await.unwrap();
    assert_eq!(versions.len(), 3);
    assert_eq!(versions[2].1, "Original style");
}
#[tokio::test]
async fn project_worker_can_read_shared_general_memory_but_cannot_promote_project_data() {
    let f = fixture().await;
    f.store
        .memory_write(
            &grant(&["general"]),
            input("general", MemoryKind::Fact, "Shared public preference"),
            NOW,
        )
        .await
        .unwrap();
    let b = MemoryBinding {
        run: "a-worker".into(),
        principal: "owner".into(),
        scopes: BTreeSet::from(["general".into(), "project:a".into()]),
        primary_scope: "project:a".into(),
        conversation: Some("chat".into()),
        task: Some("task".into()),
        parent_run: None,
        role: BindingRole::Worker,
        timezone: chrono_tz::UTC,
    };
    let read = f
        .store
        .call_bound(
            &b,
            "memory_search",
            json!({"query":"Shared public preference","scopes":["general"]}),
            "read",
            false,
            NOW,
        )
        .await
        .unwrap();
    assert!(
        read["packed"]
            .as_str()
            .unwrap()
            .contains("Shared public preference")
    );
    let private = json!({"action":"add","scope":"general","kind":"fact","statement":"Project A private design"});
    assert!(matches!(
        f.store
            .call_bound(&b, "memory_write", private, "promote", false, NOW)
            .await,
        Err(Error::Scope(_))
    ));
    let write = f
        .store
        .call_bound(
            &b,
            "memory_write",
            json!({"action":"add","kind":"fact","statement":"Project A private design"}),
            "own",
            false,
            NOW,
        )
        .await
        .unwrap();
    let id = write["memory_id"].as_str().unwrap();
    assert_eq!(
        f.store
            .memory(&grant(&["project:a"]), id)
            .await
            .unwrap()
            .unwrap()
            .scope,
        "project:a"
    );
    assert!(
        f.store
            .memory(&grant(&["general", "project:b"]), id)
            .await
            .unwrap()
            .is_none()
    );
}
#[tokio::test]
async fn consolidation_never_supersedes_user_evidence_with_a_newer_agent_timestamp() {
    let f = fixture().await;
    f.db.write(Priority::Normal,|c| {
        c.execute("INSERT INTO principals(id,name) VALUES('owner','owner')",[])?;
        c.execute("INSERT INTO entities(id,canonical_name,principal_id,scope_id) VALUES('person','Person','owner','general')",[])?;
        for (id,text,trust,at) in [("user-memory","Phone 111","user",NOW-2*DAY),("agent-memory","Phone 222","agent",NOW-DAY)] {
            c.execute("INSERT INTO memories(id,kind,statement,entities,valid_from,recorded_at,state,sources,principal_id,scope_id,trust,attribute) VALUES(?1,'fact',?2,'[\"person\"]',?3,?3,'disputed','[{\"kind\":\"tool_call\",\"id\":\"fixture\"}]','owner','general',?4,'phone')",rusqlite::params![id,text,at,trust])?;
            c.execute("INSERT INTO memory_entity_links(memory_id,entity_id) VALUES(?1,'person')",[id])?;
        }
        Ok(())
    }).await.unwrap();
    let roles = ConsolidationRoles {
        extractor: Arc::new(ScriptedExtractor::new(|_| ExtractionOutput::default())),
        compactor: None,
    };
    f.store
        .run_review(
            "owner",
            &["general".into()],
            &roles,
            &ReviewBudget::default(),
            &AtomicBool::new(false),
            NOW,
        )
        .await
        .unwrap();
    let user = f
        .store
        .memory(&grant(&["general"]), "user-memory")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(user.trust, Trust::User);
    assert_eq!(user.status, MemoryStatus::Disputed);
    assert_eq!(user.superseded_by, None);
}
#[tokio::test]
async fn extraction_fails_closed_without_any_human_source_authority() {
    let f = fixture().await;
    // Bypass the fixture's human-authority helper: role=user alone proves nothing.
    f.db.write(Priority::Normal,|c| {
        c.execute("INSERT INTO principals(id,name) VALUES('owner','owner')",[])?;
        c.execute("INSERT INTO conversations(id,principal_id,active_scope) VALUES('chat','owner','general')",[])?;
        c.execute("INSERT INTO messages(id,conversation_id,role,text,principal_id,scope_id) VALUES('generated','chat','user','Prefers purple UI','owner','general')",[])?;
        Ok(())
    }).await.unwrap();
    let extractor = ScriptedExtractor::new(|r| ExtractionOutput {
        facts: r
            .turns
            .iter()
            .map(|t| ExtractedFact {
                message_id: t.message_id.clone(),
                kind: MemoryKind::Preference,
                statement: t.text.clone(),
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
    });
    let ids = f
        .store
        .close_segment(&grant(&["general"]), "general", &extractor, NOW)
        .await
        .unwrap();
    assert_eq!(ids.len(), 1);
    assert_eq!(
        f.store
            .memory(&grant(&["general"]), &ids[0])
            .await
            .unwrap()
            .unwrap()
            .trust,
        Trust::Agent
    );
}
