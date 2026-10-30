//! Frontier -> ingest -> index -> ranking -> web_search wiring (15.4-15.9).
//!
//! Local synthetic fixtures only: sitemap/feed ingestion is bounded, the
//! scheduler runs on a controlled clock, page bodies come from an in-memory
//! table (no network), refresh follows the stored 15.6 intervals, and budget
//! plus `insufficient` signalling matches `web_search` (15.9).

use std::collections::HashMap;

use excalibur_search::index::{DiskIndex, DocSource};
use excalibur_search::pipeline::{Pipeline, PipelinePage, bytes_per_doc, known_item_recall};
use excalibur_search::search::SearchOptions;
use excalibur_search::wire::{AllowAllGate, DenyAllGate, FetchGate, HostProxyWire};
use excalibur_search::{SearchConfig, SearchProfile};

fn test_config() -> SearchConfig {
    SearchConfig {
        profile: SearchProfile::Minimo,
        contact_url: "https://operator.example/contact".to_owned(),
        user_agent: "ExcaliburBot/0.1".to_owned(),
        data_dir: "search-data".to_owned(),
    }
}

fn frontier_with_robots(now: u64) -> excalibur_search::Frontier {
    let frontier =
        excalibur_search::Frontier::open_memory("https://operator.example/contact").unwrap();
    frontier
        .store_robots("docs.example", "", true, now, None)
        .unwrap();
    frontier
        .store_robots("feed.example", "", true, now, None)
        .unwrap();
    frontier
}

fn page(title: &str, body: &str, source: DocSource) -> PipelinePage {
    PipelinePage {
        title: title.to_owned(),
        headings: vec!["Guide".to_owned()],
        body: body.to_owned(),
        source,
        lang: "en".to_owned(),
        license: String::new(),
        published_at: 0,
        fetched_at: 1_700_000_000,
        prior: 0.4,
        noindex: false,
        status: Some(200),
        content_changed: true,
    }
}

#[test]
fn sitemap_to_search_end_to_end() {
    let now = 1_700_000_000_u64;
    let frontier = frontier_with_robots(now);
    let index = DiskIndex::open_memory(u64::MAX).unwrap();
    let config = test_config();
    let mut pipeline = Pipeline::new(frontier, index, &config, "minimo");
    let xml = r#"<?xml version="1.0"?>
<urlset><url><loc>https://docs.example/guide-rust</loc></url>
<url><loc>https://docs.example/guide-python</loc></url></urlset>"#;
    let admitted = pipeline
        .seed_with_sitemap(
            "docs.example",
            "https://docs.example/sitemap.xml",
            xml,
            0.8,
            now,
        )
        .unwrap();
    assert_eq!(admitted, 2);
    let mut pages = HashMap::new();
    pages.insert(
        "https://docs.example/guide-rust".to_owned(),
        page(
            "Rust borrow checker guide",
            "rust ownership borrow checker guide",
            DocSource::Docs,
        ),
    );
    pages.insert(
        "https://docs.example/guide-python".to_owned(),
        page(
            "Python packaging guide",
            "python packaging virtualenv pip guide",
            DocSource::Docs,
        ),
    );
    let first = pipeline.run_once(&pages, &AllowAllGate, now + 10).unwrap();
    assert_eq!(first.indexed, 1, "first tick dispatches one per host");
    // Politeness delay is at most 5 s; the second URL is due on the next tick.
    let second = pipeline.run_once(&pages, &AllowAllGate, now + 20).unwrap();
    assert_eq!(second.indexed, 1, "second tick dispatches the next URL");
    let response = pipeline
        .search("rust borrow", &SearchOptions::default())
        .unwrap();
    assert!(
        response
            .results
            .iter()
            .any(|result| result.url.ends_with("/guide-rust")),
        "results={:?}",
        response
            .results
            .iter()
            .map(|result| &result.url)
            .collect::<Vec<_>>()
    );
    assert_eq!(response.coverage.documents, 2);
    assert_eq!(response.coverage.profile, "minimo");
    assert!(!response.coverage.budget_reached);
}

#[test]
fn feed_ingestion_enqueues_bounded_links() {
    let now = 1_700_000_000_u64;
    let frontier = frontier_with_robots(now);
    let index = DiskIndex::open_memory(u64::MAX).unwrap();
    let config = test_config();
    let mut pipeline = Pipeline::new(frontier, index, &config, "minimo");
    let feed = r#"<?xml version="1.0"?><rss><channel>
<item><title>A</title><link>https://feed.example/a</link></item>
<item><title>B</title><link>https://feed.example/b</link></item>
</channel></rss>"#;
    let admitted = pipeline
        .seed_with_feed("feed.example", feed, 0.5, now)
        .unwrap();
    assert_eq!(admitted, 2);
    let mut pages = HashMap::new();
    pages.insert(
        "https://feed.example/a".to_owned(),
        page(
            "Fresh news alpha",
            "fresh news alpha today",
            DocSource::News,
        ),
    );
    pages.insert(
        "https://feed.example/b".to_owned(),
        page("Fresh news beta", "fresh news beta today", DocSource::News),
    );
    let first = pipeline.run_once(&pages, &AllowAllGate, now + 10).unwrap();
    assert_eq!(first.indexed, 1);
    let second = pipeline.run_once(&pages, &AllowAllGate, now + 20).unwrap();
    assert_eq!(second.indexed, 1);
    let response = pipeline
        .search(
            "fresh news",
            &SearchOptions {
                freshness: true,
                now_secs: (now + 10) as i64,
                k: 10,
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(response.results.len(), 2);
}

#[test]
fn refresh_redispatches_indexed_after_interval() {
    let now = 1_700_000_000_u64;
    let frontier =
        excalibur_search::Frontier::open_memory("https://operator.example/contact").unwrap();
    frontier
        .store_robots("docs.example", "", true, now, None)
        .unwrap();
    let index = DiskIndex::open_memory(u64::MAX).unwrap();
    let config = test_config();
    let mut pipeline = Pipeline::new(frontier, index, &config, "minimo");
    let xml = r#"<urlset><url><loc>https://docs.example/refresh-me</loc></url></urlset>"#;
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
        "https://docs.example/refresh-me".to_owned(),
        page("Refresh target", "stable body version one", DocSource::Docs),
    );
    let first = pipeline.run_once(&pages, &AllowAllGate, now + 10).unwrap();
    assert_eq!(first.indexed, 1);
    // Nothing due before the 7-day interval expires.
    let idle = pipeline.run_once(&pages, &AllowAllGate, now + 100).unwrap();
    assert_eq!(idle.dispatched, 0, "no refresh before interval");
    assert_eq!(idle.indexed, 0);
    // After 7 days plus margin the indexed row is redispatched (15.6).
    let week_later = now + 7 * 86_400 + 20;
    // Refresh robots freshness (24 h TTL) so dispatch is allowed.
    pipeline
        .frontier()
        .store_robots("docs.example", "", true, week_later, None)
        .unwrap();
    let mut changed = page("Refresh target", "stable body version one", DocSource::Docs);
    changed.content_changed = false;
    changed.fetched_at = week_later as i64;
    let mut pages2 = HashMap::new();
    pages2.insert("https://docs.example/refresh-me".to_owned(), changed);
    let second = pipeline
        .run_once(&pages2, &AllowAllGate, week_later)
        .unwrap();
    assert_eq!(second.dispatched, 1, "indexed due must redispatch");
    assert_eq!(second.indexed, 1);
}

#[test]
fn budget_and_insufficient_signalling() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("search.db");
    let frontier =
        excalibur_search::Frontier::open(&path, "https://operator.example/contact", 30, 16)
            .unwrap();
    let index_path = dir.path().join("index.db");
    let index = DiskIndex::open(&index_path, 1024).unwrap();
    let config = test_config();
    let mut pipeline = Pipeline::new(frontier, index, &config, "minimo");
    let now = 1_700_000_000_u64;
    pipeline
        .frontier()
        .store_robots("big.example", "", true, now, None)
        .unwrap();
    pipeline
        .frontier()
        .add_host("big.example", 0.9, Some(100))
        .unwrap();
    let mut pages = HashMap::new();
    for i in 0..10 {
        let url = format!("https://big.example/page-{i}");
        pipeline
            .frontier()
            .enqueue(
                &url,
                0,
                excalibur_search::Priority::Normal,
                excalibur_search::Source::Web,
                now,
                2,
            )
            .unwrap();
        pages.insert(
            url,
            page(
                &format!("Title number {i} filler"),
                &"body words ".repeat(50),
                DocSource::Web,
            ),
        );
    }
    // One dispatch per host per tick plus politeness: step the clock to
    // drain all 10 same-host URLs (or stop early at the 95% budget gate).
    for step in 0..10 {
        let tick = now + 10 + step * 10;
        let stats = pipeline.run_once(&pages, &AllowAllGate, tick).unwrap();
        if stats.dispatched == 0 {
            break;
        }
    }
    let coverage = pipeline.coverage_snapshot().unwrap();
    // Low-priority ingestion must stop at 95%: either the flag is set or
    // fewer than all pages were admitted; high-priority docs stay admissible.
    let state = pipeline.index().budget_state();
    assert!(
        state.budget_reached || coverage.documents < 10,
        "budget must gate low-priority ingestion"
    );
    // A nonsense query reports insufficient with coverage.
    let empty = pipeline
        .search("xylophone quantum zebra", &SearchOptions::default())
        .unwrap();
    assert!(empty.insufficient);
    assert_eq!(empty.coverage.profile, "minimo");
}

#[test]
fn fetch_gate_deny_leaves_budget_untouched() {
    let now = 1_700_000_000_u64;
    let frontier = frontier_with_robots(now);
    let index = DiskIndex::open_memory(u64::MAX).unwrap();
    let config = test_config();
    let mut pipeline = Pipeline::new(frontier, index, &config, "minimo");
    let xml = r#"<urlset><url><loc>https://docs.example/gated</loc></url></urlset>"#;
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
        "https://docs.example/gated".to_owned(),
        page("Gated title", "gated body words", DocSource::Docs),
    );
    let stats = pipeline.run_once(&pages, &DenyAllGate, now + 10).unwrap();
    assert_eq!(stats.indexed, 0);
    assert!(stats.gate_denied > 0);
    assert_eq!(pipeline.index().len().unwrap(), 0);
}

#[test]
fn host_proxy_wire_surface_matches_policy() {
    let wire = HostProxyWire::new(&["docs.example"]);
    assert!(wire.allow_fetch("docs.example", "https://docs.example/a"));
    assert!(wire.allow_fetch("sub.docs.example", "https://sub.docs.example/a"));
    assert!(!wire.allow_fetch("evil.example", "https://evil.example/a"));
    let deny = HostProxyWire::default();
    assert!(!deny.allow_fetch("docs.example", "https://docs.example/a"));
    // Budget surface mirrors the configured profile (B9 wire point).
    let profile = SearchProfile::Minimo;
    assert_eq!(profile.index_budget_bytes(), 1_610_612_736);
    assert_eq!(profile.state_budget_bytes(), 536_870_912);
    assert!(excalibur_search::is_low_priority_source(DocSource::Web));
    assert!(!excalibur_search::is_low_priority_source(DocSource::Wiki));
}

#[test]
fn known_item_recall_over_synthetic_corpus() {
    let mut index = DiskIndex::open_memory(u64::MAX).unwrap();
    let mut queries = Vec::new();
    for i in 0..100 {
        let unique = format!("quokka{i} zephyr{i} wombat{i}");
        let doc = excalibur_search::Doc {
            url: format!("https://kb.example/page-{i}"),
            host: "kb.example".to_owned(),
            domain: excalibur_search::registrable_domain("kb.example"),
            source: DocSource::Docs,
            lang: "en".to_owned(),
            license: String::new(),
            title: format!("Guide {unique}"),
            headings: vec![format!("Section {i}")],
            body: format!("Body text {unique} with extra words"),
            published_at: 0,
            fetched_at: 1_700_000_000,
            prior: 0.4,
        };
        index.insert(&doc).unwrap();
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
    let (success_at_10, mrr) = known_item_recall(&pipeline, &query_refs).unwrap();
    assert!(success_at_10 >= 0.9, "success@10={success_at_10}");
    assert!(mrr > 0.5, "mrr={mrr}");
    // Real bytes/doc on the synthetic corpus (file-backed DiskIndex).
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("index.db");
    let mut file_index = DiskIndex::open(&path, u64::MAX).unwrap();
    for i in 0..100 {
        let unique = format!("quokka{i} zephyr{i} wombat{i}");
        file_index
            .insert(&excalibur_search::Doc {
                url: format!("https://kb.example/page-{i}"),
                host: "kb.example".to_owned(),
                domain: excalibur_search::registrable_domain("kb.example"),
                source: DocSource::Docs,
                lang: "en".to_owned(),
                license: String::new(),
                title: format!("Guide {unique}"),
                headings: vec![format!("Section {i}")],
                body: format!("Body text {unique} with extra words"),
                published_at: 0,
                fetched_at: 1_700_000_000,
                prior: 0.4,
            })
            .unwrap();
    }
    file_index.commit().unwrap();
    let bytes = std::fs::metadata(&path).map(|meta| meta.len()).unwrap_or(0);
    let per_doc = bytes_per_doc(bytes, 100).unwrap();
    assert!(per_doc > 50.0 && per_doc < 20_000.0, "bytes/doc={per_doc}");
}
