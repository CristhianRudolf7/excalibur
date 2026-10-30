//! Disk-backed index verification: known-item, duplicates, refresh, budget.

use excalibur_search::index::{DiskIndex, Doc, DocSource};
use excalibur_search::normalize::registrable_domain;
use excalibur_search::ranking::prior_for_harmonic_rank;
use excalibur_search::search::{SearchOptions, SearchService};

fn doc(i: usize, host: &str, title: &str, body: &str) -> Doc {
    let url = format!("https://{host}/page-{i}");
    Doc {
        url,
        host: host.to_owned(),
        domain: registrable_domain(host),
        source: DocSource::Web,
        lang: "en".to_owned(),
        license: String::new(),
        title: title.to_owned(),
        headings: vec![format!("Section {i}")],
        body: body.to_owned(),
        published_at: 0,
        fetched_at: 1_700_000_000,
        prior: prior_for_harmonic_rank((i + 1) as u64, 10_000),
    }
}

fn seeded_index() -> DiskIndex {
    let mut index = DiskIndex::open_memory(u64::MAX).unwrap();
    for i in 0..100 {
        let unique = format!("quokka{i} zephyr{i} wombat{i}");
        let stored = index
            .insert(&doc(
                i,
                "kb.example",
                &format!("Guide {unique}"),
                &format!("Body text {unique} with extra words to shape length"),
            ))
            .unwrap();
        assert!(stored, "doc {i} not stored");
    }
    index.commit().unwrap();
    index
}

#[test]
fn known_item_success_at_10() {
    let index = seeded_index();
    let service = SearchService::new(&index, "minimo");
    let mut hits = 0;
    for i in 0..100 {
        let response = service
            .search(
                &format!("quokka{i} zephyr{i} wombat{i}"),
                &SearchOptions {
                    k: 10,
                    ..Default::default()
                },
            )
            .unwrap();
        if response
            .results
            .iter()
            .any(|result| result.url.ends_with(&format!("/page-{i}")))
        {
            hits += 1;
        }
    }
    assert!(hits >= 90, "success@10 = {hits}/100");
}

#[test]
fn conjunctive_query_with_disjunctive_fallback() {
    let index = seeded_index();
    let service = SearchService::new(&index, "minimo");
    let response = service
        .search(
            "quokka7 zephyr7",
            &SearchOptions {
                k: 10,
                ..Default::default()
            },
        )
        .unwrap();
    assert!(!response.results.is_empty());
    assert!(
        response
            .results
            .iter()
            .any(|result| result.url.ends_with("/page-7"))
    );
    assert!(
        response.insufficient,
        "single-result answers must set the insufficient flag"
    );
    assert_eq!(response.coverage.documents, 100);
}

#[test]
fn exact_duplicates_are_not_indexed_twice() {
    let mut index = DiskIndex::open_memory(u64::MAX).unwrap();
    let first = doc(1, "dup.example", "Same title", "Identical body text here");
    assert!(index.insert(&first).unwrap());
    let mut second = first.clone();
    second.url = "https://dup.example/other".to_owned();
    assert!(!index.insert(&second).unwrap());
    assert_eq!(index.len().unwrap(), 1);
}

#[test]
fn near_duplicates_on_same_host_are_merged() {
    let mut index = DiskIndex::open_memory(u64::MAX).unwrap();
    let filler = "the quick brown fox jumps over the lazy dog near the river bank ".repeat(6);
    let first = doc(1, "near.example", "River fox guide", &filler);
    assert!(index.insert(&first).unwrap());
    let mut second = doc(
        2,
        "near.example",
        "River fox guide",
        &format!("{filler} extra"),
    );
    second.url = "https://near.example/page-2".to_owned();
    assert!(!index.insert(&second).unwrap());
    // Identical bodies merge globally, even on another host.
    let mut third = first.clone();
    third.host = "other.example".to_owned();
    third.url = "https://other.example/page-9".to_owned();
    assert!(!index.insert(&third).unwrap());
    assert_eq!(index.len().unwrap(), 1);
}

#[test]
fn remove_handles_gone_and_noindex() {
    let mut index = DiskIndex::open_memory(u64::MAX).unwrap();
    let stored = doc(1, "gone.example", "Vanishing page", "body words here");
    assert!(index.insert(&stored).unwrap());
    assert!(index.contains(&stored.url).unwrap());
    assert!(index.remove(&stored.url).unwrap());
    assert!(!index.contains(&stored.url).unwrap());
}

#[test]
fn site_lang_and_k_filters_apply() {
    let mut index = DiskIndex::open_memory(u64::MAX).unwrap();
    index
        .insert(&Doc {
            lang: "es".to_owned(),
            ..doc(
                1,
                "a.example",
                "Rust borrow checker",
                "guia de propiedad en espanol con ejemplos",
            )
        })
        .unwrap();
    index
        .insert(&Doc {
            lang: "en".to_owned(),
            ..doc(
                2,
                "b.example",
                "Rust borrow checker",
                "english ownership walkthrough with snippets",
            )
        })
        .unwrap();
    let service = SearchService::new(&index, "minimo");
    let spanish = service
        .search(
            "Rust borrow",
            &SearchOptions {
                lang: Some("es".to_owned()),
                k: 10,
                ..Default::default()
            },
        )
        .unwrap();
    assert!(
        spanish
            .results
            .iter()
            .all(|result| result.url.contains("a.example"))
    );
    let site = service
        .search("site:b.example Rust borrow", &SearchOptions::default())
        .unwrap();
    assert!(!site.results.is_empty());
    assert!(
        site.results
            .iter()
            .all(|result| result.url.contains("b.example"))
    );
    let capped = service
        .search(
            "Rust",
            &SearchOptions {
                k: 50,
                ..Default::default()
            },
        )
        .unwrap();
    assert!(capped.results.len() <= 50);
}

#[test]
fn diversity_caps_hosts_at_two() {
    let mut index = DiskIndex::open_memory(u64::MAX).unwrap();
    for i in 0..6 {
        index
            .insert(&doc(
                i,
                "mono.example",
                &format!("Shared topic {i}"),
                &format!("shared topic words repeated with unique tail number {i} quokka{i}"),
            ))
            .unwrap();
    }
    let service = SearchService::new(&index, "minimo");
    let response = service
        .search(
            "shared topic",
            &SearchOptions {
                k: 6,
                ..Default::default()
            },
        )
        .unwrap();
    assert!(
        response.results.len() <= 2,
        "got {}",
        response.results.len()
    );
}

#[test]
fn insufficient_flag_and_coverage() {
    let index = seeded_index();
    let service = SearchService::new(&index, "minimo");
    let response = service
        .search("xylophone quantum zebra", &SearchOptions::default())
        .unwrap();
    assert!(response.insufficient);
    assert_eq!(response.coverage.profile, "minimo");
    assert_eq!(response.coverage.documents, 100);
    assert!(response.coverage.by_source.contains_key("web"));
}

#[test]
fn freshness_orders_recent_first_when_requested() {
    let mut index = DiskIndex::open_memory(u64::MAX).unwrap();
    let mut old = doc(
        1,
        "old-news.example",
        "Elections results",
        "elections results vote count announced after polls closed",
    );
    old.published_at = 1_000_000;
    old.fetched_at = 1_000_000;
    let mut recent = doc(
        2,
        "fresh-news.example",
        "Elections results update",
        "breaking elections update with fresh tallies and reactions today",
    );
    recent.url = "https://fresh-news.example/page-2".to_owned();
    recent.published_at = 1_700_000_000;
    recent.fetched_at = 1_700_000_000;
    assert!(index.insert(&old).unwrap());
    assert!(index.insert(&recent).unwrap());
    let service = SearchService::new(&index, "minimo");
    let response = service
        .search(
            "elections latest",
            &SearchOptions {
                k: 10,
                now_secs: 1_700_000_100,
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(
        response.results.first().map(|result| result.url.as_str()),
        Some("https://fresh-news.example/page-2")
    );
}

#[test]
fn disk_backed_index_persists_and_reports_size() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("index.db");
    {
        let mut index = DiskIndex::open(&path, 1_610_612_736).unwrap();
        index
            .insert(&doc(
                1,
                "disk.example",
                "Persistent title",
                "persistent body words",
            ))
            .unwrap();
        let state = index.commit().unwrap();
        assert!(!state.budget_reached);
    }
    assert!(std::fs::metadata(&path).unwrap().len() > 0);
    let index = DiskIndex::open(&path, 1_610_612_736).unwrap();
    assert_eq!(index.len().unwrap(), 1);
    assert!(index.contains("https://disk.example/page-1").unwrap());
}

#[test]
fn tiny_budget_reports_budget_reached() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("index.db");
    let mut index = DiskIndex::open(&path, 1024).unwrap();
    let mut rejected = 0;
    for i in 0..10 {
        let res = index.insert(&doc(
            i,
            "big.example",
            &format!("Title number {i}"),
            &"body words ".repeat(50),
        ));
        if let Err(err) = res {
            assert!(matches!(err, excalibur_search::SearchError::BudgetReached));
            rejected += 1;
        }
    }
    let state = index.commit().unwrap();
    assert!(state.budget_reached, "bytes={}", state.bytes);
    assert!(rejected > 0, "low-priority ingestion must stop at 95%");
    // High-priority sources stay admissible after the budget flag.
    let high = Doc {
        source: DocSource::Docs,
        ..doc(
            99,
            "big.example",
            "High priority title",
            "high priority body words",
        )
    };
    assert!(index.insert(&high).unwrap(), "docs source must still admit");
}

#[test]
fn deterministic_tie_ordering_repeats() {
    // BSF-10 / P12: identical queries must repeat identically.
    let mut index = DiskIndex::open_memory(u64::MAX).unwrap();
    for i in 0..40 {
        index
            .insert(&Doc {
                url: format!("https://h{i:02}.example/p"),
                host: format!("h{i:02}.example"),
                domain: format!("h{i:02}.example"),
                source: DocSource::Web,
                lang: "en".to_owned(),
                license: String::new(),
                title: "Alpha".to_owned(),
                body: format!("alpha z{i:02}"),
                headings: vec![],
                published_at: 0,
                fetched_at: 1,
                prior: 0.2,
            })
            .unwrap();
    }
    let service = excalibur_search::search::SearchService::new(&index, "minimo");
    let first = service
        .search(
            "alpha",
            &excalibur_search::search::SearchOptions {
                k: 5,
                ..Default::default()
            },
        )
        .unwrap()
        .results
        .iter()
        .map(|result| result.url.clone())
        .collect::<Vec<_>>();
    for _ in 0..30 {
        let next = service
            .search(
                "alpha",
                &excalibur_search::search::SearchOptions {
                    k: 5,
                    ..Default::default()
                },
            )
            .unwrap()
            .results
            .iter()
            .map(|result| result.url.clone())
            .collect::<Vec<_>>();
        assert_eq!(first, next, "tie ordering must be stable");
    }
}
