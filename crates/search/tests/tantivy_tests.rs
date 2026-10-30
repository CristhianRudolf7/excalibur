//! Production tantivy index wiring (section 15.5).
//!
//! Local synthetic fixtures only. Requires `--features tantivy`; the default
//! suite keeps using the deterministic `DiskIndex`.

#![cfg(feature = "tantivy")]

use std::collections::HashMap;

use excalibur_search::index::{Doc, DocSource};
use excalibur_search::normalize::registrable_domain;
use excalibur_search::pipeline::{Pipeline, PipelinePage, bytes_per_doc, known_item_recall};
use excalibur_search::search::SearchOptions;
use excalibur_search::tantivy_index::TantivyIndex;
use excalibur_search::wire::AllowAllGate;
use excalibur_search::{IndexBackend, SearchConfig, SearchProfile};

fn test_config() -> SearchConfig {
    SearchConfig {
        profile: SearchProfile::Minimo,
        contact_url: "https://operator.example/contact".to_owned(),
        user_agent: "ExcaliburBot/0.1".to_owned(),
        data_dir: "search-data".to_owned(),
    }
}

fn doc(i: usize, host: &str, title: &str, body: &str, source: DocSource) -> Doc {
    Doc {
        url: format!("https://{host}/page-{i}"),
        host: host.to_owned(),
        domain: registrable_domain(host),
        source,
        lang: "en".to_owned(),
        license: String::new(),
        title: title.to_owned(),
        headings: vec![format!("Section {i}")],
        body: body.to_owned(),
        published_at: 0,
        fetched_at: 1_700_000_000,
        prior: 0.4,
    }
}

#[test]
fn tantivy_matches_disk_document_model() {
    let mut index = TantivyIndex::open_in_ram(u64::MAX).unwrap();
    let stored = doc(
        1,
        "model.example",
        "Rust borrow guide",
        "rust ownership body",
        DocSource::Docs,
    );
    assert!(index.insert(&stored).unwrap());
    index.commit().unwrap();
    assert!(index.contains("https://model.example/page-1").unwrap());
    assert_eq!(index.len().unwrap(), 1);
    // Same document model: 1 KiB snippet, delete-by-URL, stable tie-breaks.
    let terms = excalibur_search::tokenize("rust borrow");
    let candidates = index.search_bm25(&terms, true, 10, None, &[]).unwrap();
    assert_eq!(candidates.len(), 1);
    assert!(candidates[0].doc.snippet.chars().count() <= 1024);
    assert!(index.remove("https://model.example/page-1").unwrap());
    index.commit().unwrap();
    assert_eq!(index.len().unwrap(), 0);
}

#[test]
fn tantivy_dedup_and_budget_match_disk() {
    let mut index = TantivyIndex::open_in_ram(u64::MAX).unwrap();
    let first = doc(
        1,
        "dup.example",
        "Same title",
        "Identical body text here",
        DocSource::Web,
    );
    assert!(index.insert(&first).unwrap());
    index.commit().unwrap();
    let mut second = first.clone();
    second.url = "https://dup.example/other".to_owned();
    // Exact duplicate on another URL is rejected like DiskIndex.
    assert!(!index.insert(&second).unwrap());
    index.commit().unwrap();
    assert_eq!(index.len().unwrap(), 1);
}

#[test]
fn tantivy_end_to_end_search_with_insufficient() {
    let frontier =
        excalibur_search::Frontier::open_memory("https://operator.example/contact").unwrap();
    let now = 1_700_000_000_u64;
    frontier
        .store_robots("docs.example", "", true, now, None)
        .unwrap();
    let index = TantivyIndex::open_in_ram(u64::MAX).unwrap();
    let config = test_config();
    let mut pipeline = Pipeline::new(frontier, index, &config, "minimo");
    let xml = r#"<urlset><url><loc>https://docs.example/tantivy-rust</loc></url></urlset>"#;
    pipeline
        .seed_with_sitemap(
            "docs.example",
            "https://docs.example/sitemap.xml",
            xml,
            0.8,
            now,
        )
        .unwrap();
    let mut pages = HashMap::new();
    pages.insert(
        "https://docs.example/tantivy-rust".to_owned(),
        PipelinePage {
            title: "Rust tantivy guide".to_owned(),
            headings: vec!["Manual".to_owned()],
            body: "rust tantivy indexing guide with bm25".to_owned(),
            source: DocSource::Docs,
            lang: "en".to_owned(),
            license: String::new(),
            published_at: 0,
            fetched_at: now as i64,
            prior: 0.5,
            noindex: false,
            status: Some(200),
            content_changed: true,
        },
    );
    let stats = pipeline.run_once(&pages, &AllowAllGate, now + 10).unwrap();
    assert_eq!(stats.indexed, 1);
    let hit = pipeline
        .search("rust tantivy", &SearchOptions::default())
        .unwrap();
    assert!(
        hit.results
            .iter()
            .any(|result| result.url.ends_with("/tantivy-rust"))
    );
    let miss = pipeline
        .search("xylophone quantum zebra", &SearchOptions::default())
        .unwrap();
    assert!(miss.insufficient);
    assert_eq!(miss.coverage.documents, 1);
}

#[test]
fn tantivy_known_item_and_bytes_per_doc() {
    let mut index = TantivyIndex::open_in_ram(u64::MAX).unwrap();
    let mut queries = Vec::new();
    for i in 0..100 {
        let unique = format!("quokka{i} zephyr{i} wombat{i}");
        index
            .insert(&doc(
                i,
                "kb.example",
                &format!("Guide {unique}"),
                &format!("Body text {unique} with extra words"),
                DocSource::Docs,
            ))
            .unwrap();
        queries.push((
            format!("quokka{i} zephyr{i} wombat{i}"),
            format!("https://kb.example/page-{i}"),
        ));
    }
    index.commit().unwrap();
    let frontier =
        excalibur_search::Frontier::open_memory("https://operator.example/contact").unwrap();
    let config = test_config();
    let pipeline = Pipeline::new(frontier, index, &config, "minimo");
    let query_refs: Vec<(&str, &str)> = queries
        .iter()
        .map(|(query, url)| (query.as_str(), url.as_str()))
        .collect();
    let (success_at_10, _) = known_item_recall(&pipeline, &query_refs).unwrap();
    assert!(success_at_10 >= 0.9, "success@10={success_at_10}");

    // Real bytes/doc on a file-backed tantivy index (synthetic corpus only).
    let dir = tempfile::tempdir().unwrap();
    let index_dir = dir.path().join("tantivy-index");
    std::fs::create_dir_all(&index_dir).unwrap();
    let mut file_index = TantivyIndex::open(&index_dir, u64::MAX).unwrap();
    for i in 0..100 {
        let unique = format!("quokka{i} zephyr{i} wombat{i}");
        file_index
            .insert(&doc(
                i,
                "kb.example",
                &format!("Guide {unique}"),
                &format!("Body text {unique} with extra words"),
                DocSource::Docs,
            ))
            .unwrap();
    }
    file_index.commit().unwrap();
    let mut bytes = 0_u64;
    let mut stack = vec![index_dir.clone()];
    while let Some(current) = stack.pop() {
        for entry in std::fs::read_dir(&current).unwrap() {
            let entry = entry.unwrap();
            let entry_path = entry.path();
            if entry_path.is_dir() {
                stack.push(entry_path);
            } else {
                bytes += entry.metadata().unwrap().len();
            }
        }
    }
    let per_doc = bytes_per_doc(bytes, 100).unwrap();
    assert!(
        per_doc > 100.0 && per_doc < 100_000.0,
        "bytes/doc={per_doc} bytes={bytes}"
    );
}
