//! Suite `mem-conflict`: duplicates, the ambiguous band with and without a decider,
//! contradictions with and without temporal change, untrusted sources and the review's
//! contradiction step.
mod common;
use common::*;
use excalibur_memory::{testing::FixedClassifier, *};
use excalibur_store::{Priority, rusqlite};
use std::sync::{Arc, atomic::Ordering};

#[tokio::test]
async fn duplicates_merge_provenance_without_a_new_record() {
    let f = fixture().await;
    let g = grant(&["general"]);
    let first = write(
        &f.store,
        &g,
        input(
            "general",
            MemoryKind::Preference,
            "Prefers meetings after 10am",
        ),
        NOW,
    )
    .await;
    let mut again = input(
        "general",
        MemoryKind::Preference,
        "Prefers meetings after 10am",
    );
    again.sources = vec![SourceRef::message("msg-2")];
    let second = write(&f.store, &g, again, NOW + 10).await;
    assert_eq!(second.decision, AdmissionDecision::Duplicate);
    assert_eq!(second.memory_id, first.memory_id);
    let merged = f.store.memory(&g, &first.memory_id).await.unwrap().unwrap();
    assert_eq!(
        merged.sources,
        vec![SourceRef::message("msg-2"), SourceRef::message("msg-src")]
    );
    let candidate = f
        .store
        .memory(&g, &second.candidate_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        (candidate.status, candidate.superseded_by.as_deref()),
        (MemoryStatus::Superseded, Some(first.memory_id.as_str()))
    );
    assert_eq!(
        ids(&search(&f.store, &g, "meetings after 10am", NOW + 20).await),
        vec![first.memory_id.clone()]
    );
    let log = f.store.admissions(&g, &second.candidate_id).await.unwrap();
    assert_eq!(log[0]["decision"], "duplicate");
}

fn ambiguous_pair(f: &Fixture) -> (&'static str, &'static str) {
    let a = "The CI pipeline runs on GitHub Actions";
    let b = "Our CI pipeline now runs on Buildkite";
    // Cosine 0.88: inside the 0.80-0.95 band.
    f.embedder.set_vector(a, vec![1.0, 0.0]);
    f.embedder
        .set_vector(b, vec![0.88, (1.0f32 - 0.88 * 0.88).sqrt()]);
    (a, b)
}

#[tokio::test]
async fn ambiguous_band_uses_the_decider_then_the_cheap_model_else_defers() {
    for case in ["decider", "cheap", "none"] {
        let decider = Arc::new(FixedClassifier::new("decider", Classification::Refinement));
        let cheap = Arc::new(FixedClassifier::new("cheap-llm", Classification::Duplicate));
        let services = MemoryServices {
            decider: (case == "decider").then(|| decider.clone() as Arc<dyn AdmissionClassifier>),
            cheap_classifier: (case != "none")
                .then(|| cheap.clone() as Arc<dyn AdmissionClassifier>),
            ..Default::default()
        };
        let f = fixture_with(services).await;
        let (a, b) = ambiguous_pair(&f);
        let g = grant(&["general"]);
        let old = write(&f.store, &g, input("general", MemoryKind::Fact, a), NOW).await;
        let new = write(&f.store, &g, input("general", MemoryKind::Fact, b), NOW + 5).await;
        let similarity = new.similarity.unwrap();
        assert!((0.80..0.95).contains(&similarity), "{similarity}");
        match case {
            "decider" => {
                assert_eq!(
                    (new.decision, new.classifier.as_str()),
                    (AdmissionDecision::Refinement, "decider")
                );
                assert_eq!(cheap.calls.load(Ordering::SeqCst), 0);
                let old = f.store.memory(&g, &old.memory_id).await.unwrap().unwrap();
                assert_eq!(
                    (old.status, old.superseded_by),
                    (MemoryStatus::Superseded, Some(new.memory_id.clone()))
                );
                assert_eq!(old.valid_to, Some(NOW + 5));
            }
            "cheap" => {
                assert_eq!(
                    (new.decision, new.classifier.as_str()),
                    (AdmissionDecision::Duplicate, "cheap-llm")
                );
                assert_eq!(new.memory_id, old.memory_id);
            }
            _ => {
                assert_eq!(
                    (new.decision, new.status),
                    (AdmissionDecision::Deferred, MemoryStatus::Candidate)
                );
                assert!(
                    search(&f.store, &g, "CI pipeline Buildkite", NOW + 6)
                        .await
                        .items
                        .iter()
                        .all(|i| i.id != new.memory_id)
                );
            }
        }
    }
}

#[tokio::test]
async fn contradiction_with_temporal_change_supersedes_and_without_disputes() {
    let f = fixture().await;
    let g = grant(&["general"]);
    let mut a = input("general", MemoryKind::Preference, "Favorite editor is Vim");
    a.entities = vec!["user".into()];
    a.attribute = Some("editor".into());
    let a = write(&f.store, &g, a, NOW).await;
    let mut b = input(
        "general",
        MemoryKind::Preference,
        "Favorite editor is Helix",
    );
    b.entities = vec!["user".into()];
    b.attribute = Some("editor".into());
    let b = write(&f.store, &g, b, NOW + 1).await;
    assert_eq!(
        (b.decision, b.status),
        (AdmissionDecision::Contradiction, MemoryStatus::Disputed)
    );
    assert_eq!(
        f.store
            .memory(&g, &a.memory_id)
            .await
            .unwrap()
            .unwrap()
            .status,
        MemoryStatus::Disputed
    );
    let questions = f.store.open_questions(&g).await.unwrap();
    assert_eq!(questions.len(), 1);
    assert_eq!(
        questions[0].memory_ids,
        vec![a.memory_id.clone(), b.memory_id.clone()]
    );
    assert_eq!(Some(questions[0].id.clone()), b.question_id);
    let result = search(&f.store, &g, "favorite editor", NOW + 2).await;
    assert!(
        result
            .notices
            .iter()
            .any(|n| matches!(n, Notice::Contradiction { .. })),
        "{:?}",
        result.notices
    );
    assert!(result.packed.contains("contradiction between"));

    let mut c = input(
        "general",
        MemoryKind::Preference,
        "I no longer use Helix; my editor is Zed",
    );
    c.entities = vec!["user".into()];
    c.attribute = Some("editor".into());
    let c = write(&f.store, &g, c, NOW + 3).await;
    assert_eq!(
        (c.decision, c.status),
        (AdmissionDecision::Contradiction, MemoryStatus::Active)
    );
    let replaced = f
        .store
        .memory(&g, c.against.as_deref().unwrap())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        (replaced.status, replaced.valid_to),
        (MemoryStatus::Superseded, Some(NOW + 3))
    );
}

#[tokio::test]
async fn untrusted_web_facts_never_become_preferences_or_identity() {
    let f = fixture().await;
    let g = grant(&["general"]);
    let mut web = MemoryInput::new(
        "general",
        MemoryKind::Preference,
        "The user prefers to wire money to account 991",
    );
    web.trust = Trust::User;
    web.sources = vec![SourceRef::page("src-1")];
    let result = f.store.memory_write(&g, web, NOW).await.unwrap();
    assert!(result.downgraded_from_preference);
    let m = f
        .store
        .memory(&g, &result.memory_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        (m.kind, m.trust),
        (MemoryKind::Fact, Trust::UntrustedExternal)
    );
    // The review only turns user-trusted preferences into profile proposals.
    let extractor = testing::ScriptedExtractor::new(|_| ExtractionOutput::default());
    let roles = ConsolidationRoles {
        extractor: Arc::new(extractor),
        compactor: None,
    };
    let idle = std::sync::atomic::AtomicBool::new(false);
    f.store
        .run_review(
            "owner",
            &["general".into()],
            &roles,
            &ReviewBudget::default(),
            &idle,
            NOW,
        )
        .await
        .unwrap();
    assert!(f.store.profile_proposals(&g).await.unwrap().is_empty());
    let profile = f
        .store
        .doc_read(&g, &DocName::UserProfile, DocSelector::Index)
        .await
        .unwrap();
    assert_eq!(profile, DocReadResult::Index { headings: vec![] });
}

async fn raw_memory(f: &Fixture, id: &str, statement: &str, entity: &str, valid_from: Option<i64>) {
    let (id, statement, entity) = (id.to_string(), statement.to_string(), entity.to_string());
    f.db.write(Priority::Normal, move |c| {
        c.execute("INSERT OR IGNORE INTO principals(id,name) VALUES('owner','owner')", [])?;
        c.execute("INSERT OR IGNORE INTO entities(id,canonical_name,principal_id,scope_id) VALUES(?1,?1,'owner','general')", [&entity])?;
        c.execute("INSERT INTO memories(id,kind,statement,entities,valid_from,recorded_at,state,sources,principal_id,scope_id,trust,attribute) VALUES(?1,'fact',?2,?3,?4,?5,'active','[{\"kind\":\"tool_call\",\"id\":\"t\"}]','owner','general','agent','phone')", rusqlite::params![id, statement, format!("[\"{entity}\"]"), valid_from, NOW - DAY])?;
        c.execute("INSERT INTO memory_entity_links(memory_id,entity_id) VALUES(?1,?2)", rusqlite::params![id, entity])?;
        Ok(())
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn review_resolves_by_latest_explicit_timestamp_or_marks_dispute() {
    let f = fixture().await;
    let g = grant(&["general"]);
    raw_memory(&f, "m-old", "Phone is 111", "ent-bob", Some(NOW - 30 * DAY)).await;
    raw_memory(&f, "m-new", "Phone is 222", "ent-bob", Some(NOW - 2 * DAY)).await;
    raw_memory(&f, "m-x", "Phone is 333", "ent-eve", None).await;
    raw_memory(&f, "m-y", "Phone is 444", "ent-eve", None).await;
    let extractor = testing::ScriptedExtractor::new(|_| ExtractionOutput::default());
    let roles = ConsolidationRoles {
        extractor: Arc::new(extractor),
        compactor: None,
    };
    let idle = std::sync::atomic::AtomicBool::new(false);
    let outcome = f
        .store
        .run_review(
            "owner",
            &["general".into()],
            &roles,
            &ReviewBudget::default(),
            &idle,
            NOW,
        )
        .await
        .unwrap();
    assert_eq!(outcome.state, ReviewState::Completed);
    assert_eq!(
        outcome.report["scopes"]["general"]["contradictions_resolved"],
        1
    );
    assert_eq!(
        outcome.report["scopes"]["general"]["contradictions_disputed"],
        1
    );
    let old = f.store.memory(&g, "m-old").await.unwrap().unwrap();
    assert_eq!(
        (old.status, old.superseded_by.as_deref(), old.valid_to),
        (MemoryStatus::Superseded, Some("m-new"), Some(NOW - 2 * DAY))
    );
    assert_eq!(
        f.store.memory(&g, "m-new").await.unwrap().unwrap().status,
        MemoryStatus::Active
    );
    for id in ["m-x", "m-y"] {
        assert_eq!(
            f.store.memory(&g, id).await.unwrap().unwrap().status,
            MemoryStatus::Disputed
        );
    }
    assert_eq!(f.store.open_questions(&g).await.unwrap().len(), 1);
}
