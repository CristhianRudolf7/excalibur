//! Suite `mem-scope`: no memory, message, fragment, vector, relation or chunk leaks across
//! scopes or principals.
mod common;
use common::*;
use excalibur_memory::*;
use serde_json::json;

#[tokio::test]
async fn memories_never_cross_scopes_in_any_candidate_list() {
    let f = fixture().await;
    let both = grant(&["project:a", "project:b"]);
    let mut a = input(
        "project:a",
        MemoryKind::Fact,
        "The deploy key rotates every Friday",
    );
    a.entities = vec!["Deploy".into()];
    write(&f.store, &both, a, NOW - DAY).await;
    let mut b = input(
        "project:b",
        MemoryKind::Fact,
        "The deploy key rotates every Friday",
    );
    b.entities = vec!["Deploy".into()];
    write(&f.store, &both, b, NOW - DAY).await;
    message(
        &f.db,
        "m-a",
        "c-a",
        "project:a",
        "user",
        "deploy key rotation happened yesterday",
        NOW - DAY,
    )
    .await;

    let only_b = grant(&["project:b"]);
    for query in [
        "deploy key rotates",
        "Deploy",
        "what happened yesterday",
        "\"rotates every\"",
    ] {
        let result = search(&f.store, &only_b, query, NOW).await;
        assert!(
            result.items.iter().all(|i| i.scope == "project:b"),
            "{query}: {:?}",
            result.items
        );
        assert!(!result.items.iter().any(|i| i.id == "m-a"));
    }
    let semantic = f
        .store
        .embeddings()
        .search(&["project:b".into()], "deploy key rotates every friday", 50)
        .await
        .unwrap();
    assert!(semantic.hits.iter().all(|h| h.scope == "project:b") && !semantic.hits.is_empty());

    let mut q = RetrievalQuery::new("deploy");
    q.scopes = vec!["project:a".into()];
    assert!(matches!(
        f.store.retrieve(&only_b, q, NOW).await,
        Err(Error::Scope(_))
    ));
    assert!(matches!(
        f.store
            .memory_write(&only_b, input("project:a", MemoryKind::Fact, "x"), NOW)
            .await,
        Err(Error::Scope(_))
    ));
    assert!(
        MemoryGrant::new("owner", Vec::<String>::new())
            .select(&[])
            .is_err()
    );
}

#[tokio::test]
async fn principals_are_isolated_even_with_equal_scope_names() {
    let f = fixture().await;
    write(
        &f.store,
        &grant(&["general"]),
        input("general", MemoryKind::Preference, "Prefers green tea"),
        NOW,
    )
    .await;
    let other = MemoryGrant::new("intruder", ["general"]);
    assert!(
        search(&f.store, &other, "green tea", NOW)
            .await
            .items
            .is_empty()
    );
    message_for(
        &f.db,
        "intruder",
        "m-x",
        "c-x",
        "general",
        "user",
        "green tea talk",
        NOW,
    )
    .await;
    let mine = search(&f.store, &grant(&["general"]), "green tea", NOW).await;
    assert!(!mine.items.iter().any(|i| i.id == "m-x"));
    let page = f
        .store
        .history_search(
            &grant(&["general"]),
            HistoryQuery {
                text: Some("green".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert!(page.entries.is_empty());
}

#[tokio::test]
async fn proposals_stay_inside_their_conversation_until_consolidated() {
    let f = fixture().await;
    let origin = grant(&["general"]).with_conversation("conv-1");
    let mut i = input(
        "general",
        MemoryKind::Fact,
        "The staging database lives on port 6543",
    );
    i.sources = vec![SourceRef::message("m-1")];
    let candidate = f.store.memory_propose(&origin, i, NOW).await.unwrap();
    assert_eq!(
        ids(&search(&f.store, &origin, "staging database port", NOW).await),
        vec![candidate.clone()]
    );
    let elsewhere = grant(&["general"]).with_conversation("conv-2");
    assert!(
        search(&f.store, &elsewhere, "staging database port", NOW)
            .await
            .items
            .is_empty()
    );
    f.store.admit("owner", &candidate, NOW).await.unwrap();
    assert_eq!(
        ids(&search(&f.store, &elsewhere, "staging database port", NOW).await),
        vec![candidate]
    );
    assert!(
        f.store
            .memory_propose(
                &grant(&["general"]),
                input("general", MemoryKind::Fact, "x"),
                NOW
            )
            .await
            .is_err()
    );
}

#[tokio::test]
async fn relations_notebook_and_library_respect_scopes() {
    let f = fixture().await;
    let both = grant(&["project:a", "project:b"]);
    f.store
        .relate(
            &both,
            "project:a",
            "Ana",
            "works_at",
            "Acme",
            None,
            "m",
            NOW,
        )
        .await
        .unwrap();
    let mut fact = input("project:b", MemoryKind::Fact, "Ana reviews the budget");
    fact.entities = vec!["Ana".into()];
    write(&f.store, &both, fact, NOW).await;
    // Seeds from Acme exist only in project:a; project:b's Ana is another entity.
    let result = search(&f.store, &grant(&["project:b"]), "Acme", NOW).await;
    assert!(result.items.is_empty(), "{:?}", result.items);

    let ctx = EditContext {
        actor: "run-1".into(),
        provenance: None,
        explicit_user_request: false,
    };
    f.store
        .doc_edit(
            &both,
            &DocName::Notebook,
            DocEdit::Append {
                heading_path: vec!["Notes".into()],
                scope: Some("project:a".into()),
                text: "secret plan for A".into(),
            },
            ctx.clone(),
            NOW,
        )
        .await
        .unwrap();
    let read = f
        .store
        .doc_read(
            &grant(&["project:b"]),
            &DocName::Notebook,
            DocSelector::Search("secret plan".into()),
        )
        .await
        .unwrap();
    assert_eq!(read, DocReadResult::Fragments { fragments: vec![] });
    let denied = f
        .store
        .doc_edit(
            &grant(&["project:b"]),
            &DocName::Notebook,
            DocEdit::Append {
                heading_path: vec![],
                scope: Some("project:a".into()),
                text: "x".into(),
            },
            ctx,
            NOW,
        )
        .await;
    assert!(matches!(denied, Err(Error::Scope(_))));

    let req = IngestRequest {
        scope: "project:a".into(),
        title: "Plan A".into(),
        media_type: "text/markdown".into(),
        origin: None,
        bytes: b"# Plan\n\nConfidential roadmap for alpha.".to_vec(),
        document_id: None,
        library: false,
    };
    f.store.library_ingest(&both, req, NOW).await.unwrap();
    let (chunks, _) = f
        .store
        .library_search(
            &grant(&["project:b"]).with_run("r"),
            "confidential roadmap",
            5,
            NOW,
        )
        .await
        .unwrap();
    assert!(chunks.is_empty());
}

#[tokio::test]
async fn tools_cannot_widen_the_grant_or_raise_trust() {
    let f = fixture().await;
    let g = MemoryGrant::new("owner", ["general"]).with_run("run-1");
    let denied = call_memory_tool(
        &f.store,
        &g,
        "memory_search",
        json!({"query":"x","scopes":["project:z"]}),
        false,
        NOW,
    )
    .await;
    assert!(matches!(denied, Err(Error::Scope(_))));
    let written = call_memory_tool(&f.store, &g, "memory_write", json!({"scope":"general","kind":"preference","statement":"Likes dark mode","sources":[{"kind":"message","id":"m1"}]}), false, NOW).await.unwrap();
    let memory = f
        .store
        .memory(&g, written["memory_id"].as_str().unwrap())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(memory.trust, Trust::Agent);
    assert_eq!(memory_tool_definitions().len(), MEMORY_TOOLS.len());
}
