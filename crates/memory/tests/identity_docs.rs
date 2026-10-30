//! Identity documents and notebook: concurrent `doc_edit`, escalation, versions, profile
//! proposals with provenance and personas.
mod common;
use common::*;
use excalibur_memory::*;

fn ctx(actor: &str, provenance: Option<&str>, explicit: bool) -> EditContext {
    EditContext {
        actor: actor.into(),
        provenance: provenance.map(str::to_string),
        explicit_user_request: explicit,
    }
}
fn applied(outcome: EditOutcome) -> (String, String) {
    match outcome {
        EditOutcome::Applied {
            fragment_id, hash, ..
        } => (fragment_id, hash),
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn concurrent_edits_apply_once_return_current_and_escalate_after_two_mismatches() {
    let f = fixture().await;
    let g = grant(&["project:a"]);
    let nb = DocName::Notebook;
    let (id, hash) = applied(
        f.store
            .doc_edit(
                &g,
                &nb,
                DocEdit::Append {
                    heading_path: vec!["Plan".into()],
                    scope: Some("project:a".into()),
                    text: "Ship v1 on Monday".into(),
                },
                ctx("run-a", None, false),
                NOW,
            )
            .await
            .unwrap(),
    );
    let (a, b) = tokio::join!(
        f.store.doc_edit(
            &g,
            &nb,
            DocEdit::Replace {
                fragment_id: id.clone(),
                expected_hash: hash.clone(),
                text: "Ship v1 on Tuesday".into()
            },
            ctx("run-a", None, false),
            NOW + 1
        ),
        f.store.doc_edit(
            &g,
            &nb,
            DocEdit::Replace {
                fragment_id: id.clone(),
                expected_hash: hash.clone(),
                text: "Ship v1 on Wednesday".into()
            },
            ctx("run-b", None, false),
            NOW + 1
        ),
    );
    let outcomes = [a.unwrap(), b.unwrap()];
    let winners = outcomes
        .iter()
        .filter(|o| matches!(o, EditOutcome::Applied { .. }))
        .count();
    assert_eq!(winners, 1);
    let (loser, winner_text) = match &outcomes {
        [
            EditOutcome::Applied { .. },
            m @ EditOutcome::Mismatch { .. },
        ] => (("run-b", m.clone()), "Ship v1 on Tuesday"),
        [
            m @ EditOutcome::Mismatch { .. },
            EditOutcome::Applied { .. },
        ] => (("run-a", m.clone()), "Ship v1 on Wednesday"),
        other => panic!("{other:?}"),
    };
    let EditOutcome::Mismatch {
        current: Some(current),
        consecutive: 1,
        escalate: false,
    } = loser.1
    else {
        panic!()
    };
    assert_eq!(current.text, winner_text);
    let again = f
        .store
        .doc_edit(
            &g,
            &nb,
            DocEdit::Replace {
                fragment_id: id.clone(),
                expected_hash: hash.clone(),
                text: "stale".into(),
            },
            ctx(loser.0, None, false),
            NOW + 2,
        )
        .await
        .unwrap();
    assert!(matches!(
        again,
        EditOutcome::Mismatch {
            consecutive: 2,
            escalate: true,
            ..
        }
    ));
    // Rebasing on the current hash succeeds and resets the counter.
    applied(
        f.store
            .doc_edit(
                &g,
                &nb,
                DocEdit::Replace {
                    fragment_id: id.clone(),
                    expected_hash: current.hash,
                    text: "Ship v1 on Thursday".into(),
                },
                ctx(loser.0, None, false),
                NOW + 3,
            )
            .await
            .unwrap(),
    );
    let history = f.store.fragment_history(&g, &nb, &id).await.unwrap();
    assert_eq!(
        history.iter().map(|h| h.1.as_str()).collect::<Vec<_>>(),
        vec!["Ship v1 on Monday", winner_text, "Ship v1 on Thursday"]
    );
    applied(
        f.store
            .revert_fragment(&g, &nb, &id, 1, ctx("user", None, true), NOW + 4)
            .await
            .unwrap(),
    );
    let DocReadResult::Fragments { fragments } = f
        .store
        .doc_read(&g, &nb, DocSelector::Section(vec!["Plan".into()]))
        .await
        .unwrap()
    else {
        panic!()
    };
    assert_eq!(
        (fragments[0].text.as_str(), fragments[0].version),
        ("Ship v1 on Monday", 4)
    );
}

#[tokio::test]
async fn reads_are_selective_and_inserts_keep_order() {
    let f = fixture().await;
    let g = grant(&["general"]);
    let nb = DocName::Notebook;
    let append = |path: &str, text: &str| DocEdit::Append {
        heading_path: vec![path.into()],
        scope: Some("general".into()),
        text: text.into(),
    };
    let (first, hash) = applied(
        f.store
            .doc_edit(
                &g,
                &nb,
                append("Ideas", "Use SQLite for state"),
                ctx("r", None, false),
                NOW,
            )
            .await
            .unwrap(),
    );
    applied(
        f.store
            .doc_edit(
                &g,
                &nb,
                append("Ideas", "Benchmark on Fridays"),
                ctx("r", None, false),
                NOW,
            )
            .await
            .unwrap(),
    );
    applied(
        f.store
            .doc_edit(
                &g,
                &nb,
                append("Todo", "Renew certificate"),
                ctx("r", None, false),
                NOW,
            )
            .await
            .unwrap(),
    );
    applied(
        f.store
            .doc_edit(
                &g,
                &nb,
                DocEdit::InsertAfter {
                    fragment_id: first,
                    expected_hash: hash,
                    text: "WAL mode always".into(),
                },
                ctx("r", None, false),
                NOW,
            )
            .await
            .unwrap(),
    );
    let DocReadResult::Index { headings } =
        f.store.doc_read(&g, &nb, DocSelector::Index).await.unwrap()
    else {
        panic!()
    };
    assert_eq!(headings.len(), 4);
    let DocReadResult::Fragments { fragments } = f
        .store
        .doc_read(&g, &nb, DocSelector::Section(vec!["Ideas".into()]))
        .await
        .unwrap()
    else {
        panic!()
    };
    assert_eq!(
        fragments
            .iter()
            .map(|f| f.text.as_str())
            .collect::<Vec<_>>(),
        vec![
            "Use SQLite for state",
            "WAL mode always",
            "Benchmark on Fridays"
        ]
    );
    let DocReadResult::Fragments { fragments } = f
        .store
        .doc_read(&g, &nb, DocSelector::Search("certificate".into()))
        .await
        .unwrap()
    else {
        panic!()
    };
    assert_eq!(fragments.len(), 1);
}

#[tokio::test]
async fn profile_changes_need_provenance_and_become_proposals_until_confirmed() {
    let f = fixture().await;
    let g = grant(&["general"]);
    let up = DocName::UserProfile;
    let pref = || DocEdit::Append {
        heading_path: vec!["Preferences".into()],
        scope: None,
        text: "Prefers concise answers".into(),
    };
    assert!(
        f.store
            .doc_edit(&g, &up, pref(), ctx("r", None, false), NOW)
            .await
            .is_err()
    );
    let EditOutcome::Proposed {
        proposal_id,
        evidence_count: 1,
    } = f
        .store
        .doc_edit(&g, &up, pref(), ctx("r", Some("m1"), false), NOW)
        .await
        .unwrap()
    else {
        panic!()
    };
    assert_eq!(
        f.store.doc_read(&g, &up, DocSelector::Index).await.unwrap(),
        DocReadResult::Index { headings: vec![] }
    );
    assert!(
        f.store
            .confirm_profile_proposals(&g, None, NOW)
            .await
            .unwrap()
            .is_empty(),
        "one source is not repeated evidence"
    );
    let EditOutcome::Proposed {
        evidence_count: 2, ..
    } = f
        .store
        .doc_edit(&g, &up, pref(), ctx("r", Some("m2"), false), NOW)
        .await
        .unwrap()
    else {
        panic!()
    };
    assert_eq!(
        f.store
            .confirm_profile_proposals(&g, None, NOW)
            .await
            .unwrap(),
        vec![proposal_id]
    );
    let DocReadResult::Index { headings } =
        f.store.doc_read(&g, &up, DocSelector::Index).await.unwrap()
    else {
        panic!()
    };
    assert_eq!(headings.len(), 1);
    // An explicit user request applies immediately.
    applied(
        f.store
            .doc_edit(
                &g,
                &up,
                DocEdit::Append {
                    heading_path: vec!["Style".into()],
                    scope: None,
                    text: "Call me Sam".into(),
                },
                ctx("r", Some("m3"), true),
                NOW,
            )
            .await
            .unwrap(),
    );
}

#[tokio::test]
async fn personas_vary_the_agent_profile_without_touching_user_memory() {
    let f = fixture().await;
    let g = grant(&["general"]);
    applied(
        f.store
            .doc_edit(
                &g,
                &DocName::AgentProfile,
                DocEdit::Append {
                    heading_path: vec!["Identity".into()],
                    scope: None,
                    text: "I am Excalibur.".into(),
                },
                ctx("host", Some("setup"), true),
                NOW,
            )
            .await
            .unwrap(),
    );
    applied(
        f.store
            .doc_edit(
                &g,
                &DocName::UserProfile,
                DocEdit::Append {
                    heading_path: vec!["Preferences".into()],
                    scope: None,
                    text: "Likes tables".into(),
                },
                ctx("host", Some("m"), true),
                NOW,
            )
            .await
            .unwrap(),
    );
    let persona = f
        .store
        .create_persona(&g, "Merlin", Some("wizard"), NOW)
        .await
        .unwrap();
    let pd = DocName::Persona(persona.clone());
    let DocReadResult::Fragments { fragments } = f
        .store
        .doc_read(&g, &pd, DocSelector::Section(vec![]))
        .await
        .unwrap()
    else {
        panic!()
    };
    applied(
        f.store
            .doc_edit(
                &g,
                &pd,
                DocEdit::Replace {
                    fragment_id: fragments[0].id.clone(),
                    expected_hash: fragments[0].hash.clone(),
                    text: "I am Merlin.".into(),
                },
                ctx("host", Some("m"), false),
                NOW,
            )
            .await
            .unwrap(),
    );
    let base = f.store.agent_core(&g, None, 1000).await.unwrap();
    let merlin = f.store.agent_core(&g, Some(&persona), 1000).await.unwrap();
    assert_eq!(
        (base.text.as_str(), merlin.text.as_str()),
        ("I am Excalibur.", "I am Merlin.")
    );
    assert_eq!(
        base,
        f.store.agent_core(&g, None, 1000).await.unwrap(),
        "stable snapshot for the cached prefix"
    );
    let DocReadResult::Index { headings } = f
        .store
        .doc_read(&g, &DocName::UserProfile, DocSelector::Index)
        .await
        .unwrap()
    else {
        panic!()
    };
    assert_eq!(headings.len(), 1);
    assert!(
        f.store
            .doc_read(
                &MemoryGrant::new("other", ["general"]),
                &pd,
                DocSelector::Index
            )
            .await
            .is_err()
    );
}
