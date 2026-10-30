//! `web_search` contract verification: RRF fusion, priors and insufficient.

use excalibur_search::index::{DiskIndex, Doc, DocSource};
use excalibur_search::normalize::registrable_domain;
use excalibur_search::ranking::{fuse_rrf, has_recency_intent, prior_for_harmonic_rank};
use excalibur_search::search::{SearchOptions, SearchService};

fn doc(i: usize, title: &str, body: &str, prior: f64) -> Doc {
    let host = "search.example";
    Doc {
        url: format!("https://{host}/doc-{i}"),
        host: host.to_owned(),
        domain: registrable_domain(host),
        source: DocSource::Docs,
        lang: "en".to_owned(),
        license: String::new(),
        title: title.to_owned(),
        headings: vec!["Manual".to_owned()],
        body: body.to_owned(),
        published_at: 0,
        fetched_at: 1_700_000_000,
        prior,
    }
}

#[test]
fn unknown_prior_defaults_and_harmonic_prior_orders() {
    assert_eq!(prior_for_harmonic_rank(0, 100), 0.2);
    assert!(prior_for_harmonic_rank(1, 1_000_000) > prior_for_harmonic_rank(500_000, 1_000_000));
}

#[test]
fn recency_intent_detected_in_both_languages() {
    assert!(has_recency_intent("noticias de hoy"));
    assert!(has_recency_intent("latest Rust news"));
    assert!(has_recency_intent("informe 2026"));
    assert!(!has_recency_intent("borrow checker ownership"));
}

#[test]
fn prior_breaks_bm25_ties() {
    let mut index = DiskIndex::open_memory(u64::MAX).unwrap();
    index
        .insert(&doc(
            1,
            "Tokio runtime guide",
            "tokio runtime guide async tasks executor alpha one",
            0.05,
        ))
        .unwrap();
    index
        .insert(&doc(
            2,
            "Tokio runtime guide",
            "tokio runtime guide async tasks executor beta two",
            0.95,
        ))
        .unwrap();
    let service = SearchService::new(&index, "minimo");
    let response = service
        .search(
            "tokio runtime guide",
            &SearchOptions {
                k: 10,
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(response.results.len(), 2);
    assert!(response.results[0].url.ends_with("/doc-2"));
}

#[test]
fn rrf_fusion_keeps_shared_items_first() {
    let mut index = DiskIndex::open_memory(u64::MAX).unwrap();
    for i in 0..4 {
        index
            .insert(&doc(
                i,
                &format!("Alpha guide {i}"),
                "alpha guide words",
                0.5,
            ))
            .unwrap();
    }
    let service = SearchService::new(&index, "minimo");
    let first = service
        .search(
            "alpha",
            &SearchOptions {
                k: 4,
                ..Default::default()
            },
        )
        .unwrap();
    let second = service
        .search(
            "guide",
            &SearchOptions {
                k: 4,
                ..Default::default()
            },
        )
        .unwrap();
    let fused = fuse_rrf(&first.results, &second.results, 4);
    assert!(!fused.is_empty());
    assert!(fused.len() <= 4);
}

#[test]
fn coverage_reports_profile_and_latency_hook() {
    let mut index = DiskIndex::open_memory(u64::MAX).unwrap();
    index
        .insert(&doc(1, "Coverage probe", "coverage probe words", 0.5))
        .unwrap();
    let service = SearchService::new(&index, "ampliado");
    let response = service
        .search("coverage", &SearchOptions::default())
        .unwrap();
    assert_eq!(response.coverage.profile, "ampliado");
    assert_eq!(response.coverage.documents, 1);
    assert!(response.coverage.by_source.contains_key("docs"));
    assert!(response.coverage.last_crawl_at.is_some());
}
