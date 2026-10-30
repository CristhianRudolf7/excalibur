//! Retriever: diacritics, identifiers, relational hops, fusion, no LLM expansion unless empty,
//! sufficiency and scope ambiguity notices, reranking and budgeted packing.
mod common;
use common::*;
use excalibur_memory::{testing::*, *};
use std::sync::{Arc, atomic::Ordering};

#[tokio::test]
async fn lexical_folds_accents_and_requires_quoted_phrases_and_identifiers() {
    let f = fixture().await;
    let g = grant(&["general"]);
    let cafe = write(
        &f.store,
        &g,
        input(
            "general",
            MemoryKind::Preference,
            "Le gusta el café de Perú",
        ),
        NOW,
    )
    .await;
    let ident = write(
        &f.store,
        &g,
        input(
            "general",
            MemoryKind::Fact,
            "The bug lives in parse_config.rs near load_settings",
        ),
        NOW,
    )
    .await;
    write(
        &f.store,
        &g,
        input(
            "general",
            MemoryKind::Fact,
            "The config parser was rewritten",
        ),
        NOW,
    )
    .await;
    assert_eq!(
        ids(&search(&f.store, &g, "cafe peru", NOW).await).first(),
        Some(&cafe.memory_id)
    );
    assert_eq!(
        ids(&search(&f.store, &g, "CAFÉ", NOW).await).first(),
        Some(&cafe.memory_id)
    );
    let result = search(&f.store, &g, "where is parse_config.rs", NOW).await;
    assert_eq!(
        result.analysis.identifiers,
        vec!["parse_config.rs".to_string()]
    );
    assert_eq!(ids(&result), vec![ident.memory_id.clone()]);
    let quoted = search(&f.store, &g, "config \"was rewritten\"", NOW).await;
    assert_eq!(quoted.items.len(), 1);
    assert!(quoted.items[0].text.contains("was rewritten"));
}

#[tokio::test]
async fn relational_seeds_reach_memories_two_hops_away() {
    let f = fixture().await;
    let g = grant(&["general"]);
    f.store
        .relate(&g, "general", "Ana", "works_at", "Acme", None, "m", NOW)
        .await
        .unwrap();
    f.store
        .relate(
            &g,
            "general",
            "Acme",
            "part_of",
            "Grupo Andino",
            None,
            "m",
            NOW,
        )
        .await
        .unwrap();
    let mut fact = input("general", MemoryKind::Fact, "Ana leads the payments team");
    fact.entities = vec!["Ana".into()];
    let fact = write(&f.store, &g, fact, NOW).await;
    f.embedder.set_available(false);
    let result = search(&f.store, &g, "Grupo Andino", NOW).await;
    assert_eq!(ids(&result), vec![fact.memory_id]);
    assert!(result.items[0].lists.contains(&"relational".to_string()));
    assert_eq!(result.analysis.entity_seeds.len(), 1);
    assert!(!result.notices.contains(&Notice::InsufficientEvidence));
}

#[tokio::test]
async fn fusion_rewrite_insufficiency_and_ambiguity() {
    let rewriter = Arc::new(FixedRewriter {
        rewrite: "invoice".into(),
        calls: Default::default(),
    });
    let f = fixture_with(MemoryServices {
        rewriter: Some(rewriter.clone()),
        ..Default::default()
    })
    .await;
    let g = grant(&["project:a", "project:b"]);
    write(
        &f.store,
        &g,
        input(
            "project:a",
            MemoryKind::Fact,
            "Invoice numbers start at 1000",
        ),
        NOW,
    )
    .await;
    write(
        &f.store,
        &g,
        input(
            "project:b",
            MemoryKind::Fact,
            "Invoice numbers start at 5000",
        ),
        NOW,
    )
    .await;
    let found = search(&f.store, &g, "invoice numbers", NOW).await;
    assert_eq!(
        rewriter.calls.load(Ordering::SeqCst),
        0,
        "no expansion when results exist"
    );
    assert!(
        found
            .notices
            .iter()
            .any(|n| matches!(n, Notice::AmbiguousScopes { .. }))
    );
    assert!(
        found
            .items
            .iter()
            .all(|i| i.lists.iter().any(|l| l.starts_with("lexical"))
                && i.lists.contains(&"semantic".to_string()))
    );
    assert!(
        found.packed.contains("[scope project:a]") && found.packed.contains("[scope project:b]")
    );

    f.embedder.set_available(false);
    let rewritten = search(&f.store, &g, "zzqx", NOW).await;
    assert_eq!(rewriter.calls.load(Ordering::SeqCst), 1);
    assert!(matches!(
        rewritten.notices.first(),
        Some(Notice::QueryRewritten { .. })
    ));
    assert_eq!(rewritten.items.len(), 2);

    let none = fixture().await;
    write(
        &none.store,
        &grant(&["general"]),
        input("general", MemoryKind::Fact, "Totally unrelated statement"),
        NOW,
    )
    .await;
    let weak = search(
        &none.store,
        &grant(&["general"]),
        "quantum chromodynamics lattice",
        NOW,
    )
    .await;
    assert!(
        weak.notices.contains(&Notice::InsufficientEvidence),
        "{weak:?}"
    );
    assert!(weak.packed.starts_with("! insufficient evidence"));
}

#[tokio::test]
async fn close_leaders_use_reranker_order_and_packing_respects_budget() {
    let f = fixture_with(MemoryServices {
        reranker: Some(Arc::new(ReverseReranker)),
        ..Default::default()
    })
    .await;
    let g = grant(&["general"]);
    // Without vectors these near-identical statements are admitted as separate records.
    f.embedder.set_available(false);
    for i in 0..3 {
        write(
            &f.store,
            &g,
            input(
                "general",
                MemoryKind::Fact,
                &format!("Server rack {i} sits in room B"),
            ),
            NOW,
        )
        .await;
    }
    let plain = search(&f.store, &g, "server rack room", NOW).await;
    let mut q = RetrievalQuery::new("server rack room");
    q.allow_rerank = true;
    let reranked = f.store.retrieve(&g, q, NOW).await.unwrap();
    assert!(reranked.reranked);
    assert_eq!(
        ids(&reranked),
        ids(&plain).into_iter().rev().collect::<Vec<_>>()
    );
    let mut tight = RetrievalQuery::new("server rack room");
    tight.token_budget = 30;
    let packed = f.store.retrieve(&g, tight, NOW).await.unwrap().packed;
    assert!(
        packed.lines().filter(|l| l.starts_with("- ")).count() < 3,
        "{packed}"
    );
}

#[tokio::test]
async fn history_search_pages_with_filters() {
    let f = fixture().await;
    let g = grant(&["general", "project:x"]);
    for i in 0..5 {
        message(
            &f.db,
            &format!("h{i}"),
            "c1",
            "general",
            if i % 2 == 0 { "user" } else { "assistant" },
            &format!("deploy log {i}"),
            NOW + i,
        )
        .await;
    }
    message(&f.db, "hx", "c2", "project:x", "user", "deploy log x", NOW).await;
    let page = f
        .store
        .history_search(
            &g,
            HistoryQuery {
                text: Some("deploy".into()),
                scopes: vec!["general".into()],
                limit: Some(2),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(
        page.entries
            .iter()
            .map(|e| e.id.as_str())
            .collect::<Vec<_>>(),
        vec!["h4", "h3"]
    );
    let next = f
        .store
        .history_search(
            &g,
            HistoryQuery {
                text: Some("deploy".into()),
                scopes: vec!["general".into()],
                limit: Some(2),
                cursor: page.next_cursor,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(
        next.entries
            .iter()
            .map(|e| e.id.as_str())
            .collect::<Vec<_>>(),
        vec!["h2", "h1"]
    );
    let users = f
        .store
        .history_search(
            &g,
            HistoryQuery {
                role: Some("user".into()),
                from: Some(NOW + 1),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(
        users
            .entries
            .iter()
            .map(|e| e.id.as_str())
            .collect::<Vec<_>>(),
        vec!["h4", "h2"]
    );
    let conv = f
        .store
        .history_search(
            &g,
            HistoryQuery {
                conversation: Some("c2".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(conv.entries.len(), 1);
}

#[tokio::test]
async fn purge_on_user_request_removes_content_indexes_and_vectors() {
    let f = fixture().await;
    let g = grant(&["general"]);
    let written = write(
        &f.store,
        &g,
        input("general", MemoryKind::Fact, "My passport number is X123"),
        NOW,
    )
    .await;
    f.store
        .purge_memory(&g, &written.memory_id, NOW)
        .await
        .unwrap();
    assert!(
        f.store
            .memory(&g, &written.memory_id)
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        search(&f.store, &g, "passport X123", NOW)
            .await
            .items
            .is_empty()
    );
    assert!(
        f.store
            .embeddings()
            .search(&["general".into()], "My passport number is X123", 5)
            .await
            .unwrap()
            .hits
            .is_empty()
    );
}
