//! Bounded sitemap, feed and CirrusSearch ingestion verification.

use excalibur_search::ingest::{
    cirrus_admitted, parse_cirrus_pair, parse_feed_links, parse_sitemap_locs,
};

#[test]
fn sitemap_extracts_bounded_same_host_targets() {
    let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<urlset xmlns="http://www.sitemaps.org/schemas/sitemap/0.9">
<url><loc>https://docs.example/guide</loc></url>
<url><loc>https://docs.example/api</loc></url>
<url><loc>https://other.example/stolen</loc></url>
<url><loc>ftp://docs.example/file</loc></url>
</urlset>"#;
    let locs = parse_sitemap_locs(xml, "docs.example");
    assert_eq!(
        locs,
        vec!["https://docs.example/guide", "https://docs.example/api"]
    );
}

#[test]
fn feed_links_support_rss_atom_and_json() {
    let rss = "<rss><channel><item><title>t</title><link>https://news.example/1</link></item></channel></rss>";
    assert_eq!(parse_feed_links(rss), vec!["https://news.example/1"]);
    let atom = "<feed><entry><link href=\"https://news.example/2\"/></entry></feed>";
    assert_eq!(parse_feed_links(atom), vec!["https://news.example/2"]);
    let json = "{\"items\": [{\"url\": \"https://news.example/3\"}]}";
    assert_eq!(parse_feed_links(json), vec!["https://news.example/3"]);
}

#[test]
fn cirrus_pairs_filter_by_incoming_links() {
    let index = "{\"index\": {\"_id\": \"1\"}}";
    let doc = "{\"title\": \"Rust\", \"opening_text\": \"Rust is a systems language\", \"incoming_links\": 42, \"popularity_score\": 0.9}";
    let parsed = parse_cirrus_pair(index, doc).unwrap();
    assert_eq!(parsed.title, "Rust");
    assert!(cirrus_admitted(&parsed, 10));
    assert!(!cirrus_admitted(&parsed, 100));
    assert!(parse_cirrus_pair(doc, doc).is_none());
}
