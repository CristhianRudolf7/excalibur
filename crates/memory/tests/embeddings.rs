//! Embedding degradation, indexing queue, dual-read migration and lazy local models.
mod common;
use common::*;
use excalibur_memory::{testing::*, *};
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicI64, AtomicUsize, Ordering},
};

#[test]
fn matryoshka_truncation_and_int8_quantization() {
    let v: Vec<f32> = (0..1024).map(|i| if i < 2 { 3.0 } else { 0.001 }).collect();
    let q = prepare_vector(&v, 512);
    assert_eq!(q.len(), 512);
    assert!(q[0] > 80 && q.iter().all(|x| (-127..=127).contains(x)));
    assert!((cosine_i8(&q, &q) - 1.0).abs() < 1e-6);
    assert_eq!(
        EmbeddingProfile::ServerOrRoomyDesktop.descriptor().license,
        "Apache-2.0"
    );
    assert!(
        EmbeddingProfile::DesktopLimitedMemory
            .descriptor()
            .license_requires_acceptance
    );
}

#[tokio::test]
async fn provider_outage_degrades_to_lexical_and_queues_indexing() {
    let f = fixture().await;
    let g = grant(&["general"]);
    f.embedder.set_available(false);
    let written = write(
        &f.store,
        &g,
        input("general", MemoryKind::Fact, "Backups run nightly at 02:00"),
        NOW,
    )
    .await;
    assert!(written.semantic_pending);
    assert_eq!(f.store.embeddings().queued().await.unwrap(), 1);
    let result = search(&f.store, &g, "backups nightly", NOW).await;
    assert_eq!(ids(&result), vec![written.memory_id.clone()]);
    assert!(result.notices.contains(&Notice::SemanticUnavailable));
    assert!(result.packed.contains("semantic search was unavailable"));
    f.embedder.set_available(true);
    let report = f.store.embeddings().process_queue(10, NOW).await.unwrap();
    assert_eq!((report.indexed, report.remaining), (1, 0));
    let semantic = f
        .store
        .embeddings()
        .search(&["general".into()], "Backups run nightly at 02:00", 5)
        .await
        .unwrap();
    assert_eq!(
        semantic.hits[0].object_id,
        format!("memory:{}", written.memory_id)
    );
    assert!(
        !search(&f.store, &g, "backups nightly", NOW)
            .await
            .notices
            .contains(&Notice::SemanticUnavailable)
    );
}

#[tokio::test]
async fn model_change_reindexes_with_dual_reads_and_no_loss() {
    let f = fixture().await;
    let g = grant(&["general"]);
    let mut written = Vec::new();
    for s in [
        "Alpha service owns billing",
        "Beta service owns search",
        "Gamma service owns email",
    ] {
        written.push(
            write(&f.store, &g, input("general", MemoryKind::Fact, s), NOW)
                .await
                .memory_id,
        );
    }
    let next = Arc::new(HashEmbedder::new("hash-b", 128));
    let space = f
        .store
        .embeddings()
        .begin_migration(next.clone(), 512, NOW)
        .await
        .unwrap();
    assert_eq!((space.state.as_str(), space.dimensions), ("migrating", 128));
    assert!(
        f.store
            .embeddings()
            .finish_migration(&space.id, NOW)
            .await
            .is_err(),
        "cannot finish with queued objects"
    );
    // Before reindexing, the old space still answers.
    let during = f
        .store
        .embeddings()
        .search(&["general".into()], "Gamma service owns email", 3)
        .await
        .unwrap();
    assert_eq!(during.spaces.len(), 2);
    assert_eq!(during.hits[0].object_id, format!("memory:{}", written[2]));
    // New writes go to both spaces.
    let late = write(
        &f.store,
        &g,
        input("general", MemoryKind::Fact, "Delta service owns chat"),
        NOW,
    )
    .await
    .memory_id;
    f.store.embeddings().process_queue(2, NOW).await.unwrap();
    let partial = f
        .store
        .embeddings()
        .search(&["general".into()], "Beta service owns search", 4)
        .await
        .unwrap();
    assert!(
        partial
            .hits
            .iter()
            .any(|h| h.object_id == format!("memory:{}", written[1]))
    );
    f.store.embeddings().process_queue(10, NOW).await.unwrap();
    f.store
        .embeddings()
        .finish_migration(&space.id, NOW)
        .await
        .unwrap();
    let spaces = f.store.embeddings().spaces().await.unwrap();
    assert_eq!(
        spaces
            .iter()
            .filter(|s| s.state == "active")
            .map(|s| s.model.as_str())
            .collect::<Vec<_>>(),
        vec!["hash-b"]
    );
    for id in written.iter().chain([&late]) {
        let statement = f.store.memory(&g, id).await.unwrap().unwrap().statement;
        let hits = f
            .store
            .embeddings()
            .search(&["general".into()], &statement, 1)
            .await
            .unwrap();
        assert_eq!(hits.hits[0].object_id, format!("memory:{id}"));
        assert_eq!(hits.spaces, vec![space.id.clone()]);
    }
    assert_eq!(
        f.store.embeddings().vector_count("general").await.unwrap(),
        4
    );
    assert!(
        f.store
            .embeddings()
            .attach(Arc::new(HashEmbedder::new("hash-c", 8)), 512, NOW)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn lazy_local_model_loads_on_demand_and_unloads_when_idle() {
    let clock = Arc::new(AtomicI64::new(1_000));
    let c2 = clock.clone();
    let loader = Arc::new(MockLoader {
        inner: Arc::new(HashEmbedder::new("local-q", 32)),
        fail: AtomicBool::new(false),
        loads: AtomicUsize::new(0),
    });
    let lazy = LazyLocalEmbedder::new(loader.clone(), Arc::new(move || c2.load(Ordering::SeqCst)));
    assert!(!lazy.loaded());
    lazy.embed(&["a".into()]).await.unwrap();
    assert!(lazy.loaded());
    clock.fetch_add(599, Ordering::SeqCst);
    assert!(!lazy.unload_if_idle());
    clock.fetch_add(1, Ordering::SeqCst);
    assert!(lazy.unload_if_idle() && !lazy.loaded());
    loader.fail.store(true, Ordering::SeqCst);
    assert!(
        lazy.embed(&["a".into()]).await.is_err(),
        "insufficient RAM surfaces as an embedding error"
    );

    // In the store the failure becomes queued indexing, not a lost write.
    let dir = tempfile::tempdir().unwrap();
    let db = excalibur_store::Database::open(
        dir.path().join("m.db"),
        excalibur_store::DatabaseKind::Main,
        Default::default(),
    )
    .unwrap();
    let store = MemoryStore::open(db).await.unwrap();
    store
        .embeddings()
        .attach(Arc::new(lazy), 512, NOW)
        .await
        .unwrap();
    let r = store
        .memory_write(
            &grant(&["general"]),
            input("general", MemoryKind::Fact, "Local model test"),
            NOW,
        )
        .await
        .unwrap();
    assert!(r.semantic_pending);
    assert_eq!(store.embeddings().queued().await.unwrap(), 1);
    loader.fail.store(false, Ordering::SeqCst);
    assert_eq!(
        store
            .embeddings()
            .process_queue(5, NOW)
            .await
            .unwrap()
            .indexed,
        1
    );
    assert_eq!(loader.loads.load(Ordering::SeqCst), 2);
    assert_eq!(store.config().external_index_threshold, 500_000);
}
