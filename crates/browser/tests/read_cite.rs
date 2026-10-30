//! B7 light reading and citations over local synthetic pages.
//!
//! No network beyond loopback, no accounts, no Chrome. The SPA case
//! simulates content arriving 800 ms after load; the long-poll case
//! simulates a stable page with a busy network session.

use excalibur_browser::cite;
use excalibur_browser::extract;
use excalibur_browser::read::{self, Render, WaitGate};
use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

fn static_html() -> String {
    r#"<!doctype html><html><head><title>Harbor Guide</title></head><body>
    <nav>Buy now | Menu links that must not appear</nav>
    <article><h1>Harbor Guide</h1><h2>Tides</h2><p>First paragraph about tides and harbor marks.</p>
    <p>Second paragraph about night signals and fog horns.</p>
    <h2>Docks</h2><p>Third paragraph about dock fees and winter storage.</p></article>
    <footer>Footer ads that must not appear</footer>
    </body></html>"#
        .to_owned()
}

fn spa_shell_html() -> String {
    r#"<!doctype html><html><head><title>App</title></head><body>
    <div id="root"></div><noscript>Please enable JavaScript to view the app.</noscript>
    <script>one</script><script>two</script><script>three</script>
    </body></html>"#
        .to_owned()
}

fn spa_rendered_html() -> String {
    r#"<!doctype html><html><head><title>App</title></head><body>
    <div id="root"><h1>Schedule</h1><p>Data loaded after 800 ms: departure 08:20.</p></div>
    </body></html>"#
        .to_owned()
}

fn long_poll_html() -> String {
    r#"<!doctype html><html><head><title>Feed</title></head><body>
    <h1>Live feed</h1><p>Stable content while a long-poll request stays open.</p>
    </body></html>"#
        .to_owned()
}

#[test]
fn static_article_fragments_and_citations() {
    let doc = extract::extract_document(&static_html());
    assert_eq!(doc.title, "Harbor Guide");
    assert!(doc.markdown.contains("First paragraph about tides"));
    assert!(!doc.markdown.contains("Buy now"));
    assert!(!doc.markdown.contains("Footer ads"));
    assert!(doc.fragments.len() >= 4);
    for f in &doc.fragments {
        assert_eq!(&doc.markdown[f.start..f.end], f.text);
    }
    let tides: Vec<_> = doc
        .fragments
        .iter()
        .filter(|f| f.text.contains("First paragraph"))
        .collect();
    assert_eq!(tides.len(), 1);
    assert!(tides[0].heading_path.iter().any(|h| h.contains("Tides")));

    let hash = cite::content_hash(&doc.markdown);
    assert!(hash.starts_with("sha256:"));
    let (start, end, frag) = cite::locate(
        &doc.markdown,
        &doc.fragments,
        "First paragraph about tides and harbor marks.",
    )
    .expect("exact quote");
    assert_eq!(
        &doc.markdown[start..end],
        "First paragraph about tides and harbor marks."
    );
    assert_eq!(frag, tides[0].id);
    // Normalized match tolerates case and spacing.
    assert!(
        cite::locate(
            &doc.markdown,
            &doc.fragments,
            "  FIRST   paragraph\nabout TIDES "
        )
        .is_some()
    );
    assert!(cite::locate(&doc.markdown, &doc.fragments, "no such sentence xyz").is_none());

    let quote = "Second paragraph about night signals and fog horns.";
    let citation = cite::cite(
        "https://example.com/harbor",
        &doc.title,
        "2026-10-30T00:00:00Z",
        &hash,
        &doc.markdown,
        &doc.fragments,
        quote,
    )
    .expect("citation");
    assert!(citation.source_id.starts_with("w_"));
    assert!(citation.text_fragment_url.contains("#:~:text="));
    assert!(
        citation
            .text_fragment_url
            .starts_with("https://example.com/harbor")
    );
}

#[test]
fn wait_before_capture_with_spa_fallback() {
    let shell = extract::extract_document(&spa_shell_html());
    assert!(extract::needs_browser_render(&shell).is_some());
    assert!(matches!(
        read::decide_render(&shell),
        read::HttpDecision::Browser(_)
    ));

    // Capture without a valid wait is refused before touching the DOM.
    let stale_gate = WaitGate {
        ready: false,
        document_epoch: 9,
        degraded: vec![],
        strict_network: false,
        status: "timeout".into(),
    };
    let rendered = extract::extract_document(&spa_rendered_html());
    assert!(
        read::capture_with_wait(
            &stale_gate,
            9,
            "https://example.com/app",
            "https://example.com/app",
            &rendered,
            200,
            Render::Browser,
            "2026-10-30T00:00:00Z",
        )
        .unwrap_err()
            == "wait_required"
    );

    // After the 800 ms data arrives the ready wait for the same epoch allows capture.
    let gate = WaitGate::ready(9);
    let out = read::capture_with_wait(
        &gate,
        9,
        "https://example.com/app",
        "https://example.com/app",
        &rendered,
        200,
        Render::Browser,
        "2026-10-30T00:00:00Z",
    )
    .expect("ready wait captures");
    assert_eq!(out.render, "browser");
    assert!(out.markdown.contains("departure 08:20"));
    assert!(out.untrusted);
    // Stale epoch is refused even when ready.
    assert_eq!(
        read::capture_with_wait(
            &gate,
            10,
            "https://example.com/app",
            "https://example.com/app",
            &rendered,
            200,
            Render::Browser,
            "2026-10-30T00:00:00Z",
        )
        .unwrap_err(),
        "wait_stale_epoch"
    );
}

#[test]
fn long_poll_degraded_vs_strict() {
    let doc = extract::extract_document(&long_poll_html());
    let degraded = WaitGate::degraded(4, &["network_busy"]);
    let out = read::capture_with_wait(
        &degraded,
        4,
        "https://example.com/feed",
        "https://example.com/feed",
        &doc,
        200,
        Render::Http,
        "2026-10-30T00:00:00Z",
    )
    .expect("non-strict degraded wait captures");
    assert_eq!(out.render, "http");

    let mut strict = WaitGate::degraded(4, &["network_busy"]);
    strict.strict_network = true;
    assert_eq!(
        read::capture_with_wait(
            &strict,
            4,
            "https://example.com/feed",
            "https://example.com/feed",
            &doc,
            200,
            Render::Http,
            "2026-10-30T00:00:00Z",
        )
        .unwrap_err(),
        "timeout"
    );

    let mut unobserved = WaitGate::degraded(4, &["network_unobserved"]);
    unobserved.strict_network = true;
    assert_eq!(unobserved.check(4).unwrap_err(), "timeout");
    assert!(
        WaitGate::degraded(4, &["network_unobserved"])
            .check(4)
            .is_ok()
    );
}

#[test]
fn fragment_slicing_and_inline_limit() {
    let doc = extract::extract_document(&static_html());
    let gate = WaitGate::ready(1);
    let out = read::capture_with_wait(
        &gate,
        1,
        "https://example.com/harbor",
        "https://example.com/harbor",
        &doc,
        200,
        Render::Http,
        "2026-10-30T00:00:00Z",
    )
    .unwrap();
    let metas = out.fragments.clone();
    let single = read::slice_fragments(&doc.markdown, &metas, "p2").expect("single fragment");
    assert!(!single.is_empty());
    let range = format!("p1-p{}", metas.len());
    let all = read::slice_fragments(&doc.markdown, &metas, &range).expect("range");
    assert!(all.len() >= single.len());
    assert!(read::slice_fragments(&doc.markdown, &metas, "p999").is_err());

    // Documents over 50 KiB travel inline truncated; fragments still address the blob.
    let big_para = "word ".repeat(20_000);
    let big_html =
        format!("<html><head><title>Big</title></head><body><p>{big_para}</p></body></html>");
    let big = extract::extract_document(&big_html);
    let big_out = read::build_read(
        "https://example.com/big",
        "https://example.com/big",
        &big.title,
        &big.markdown,
        &big.fragments,
        200,
        Render::Http,
        "2026-10-30T00:00:00Z",
    );
    assert!(big_out.truncated);
    assert!(big_out.markdown.len() <= read::INLINE_LIMIT);
    assert!(big_out.full_len > read::INLINE_LIMIT);
}

fn serve_once(body: String, counter: Arc<AtomicUsize>) -> (String, std::thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!(
        "http://127.0.0.1:{}/article",
        listener.local_addr().unwrap().port()
    );
    let handle = std::thread::spawn(move || {
        for stream in listener.incoming().take(8) {
            let Ok(mut stream) = stream else { continue };
            let mut buf = [0u8; 4096];
            let _ = stream.read(&mut buf);
            counter.fetch_add(1, Ordering::SeqCst);
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(response.as_bytes());
        }
    });
    (url, handle)
}

#[tokio::test]
async fn http_first_with_cache_hits() {
    let counter = Arc::new(AtomicUsize::new(0));
    let (url, _server) = serve_once(static_html(), counter.clone());
    let cache = read::ReadCache::with_default_ttl();

    let normalized = read::normalize_url(&url).unwrap();
    assert!(cache.get(&normalized, 1_000).is_none());

    let (status, final_url, body) = read::fetch_http(&url).await.expect("local fetch");
    assert_eq!(status, 200);
    assert!(body.contains("Harbor Guide"));
    assert_eq!(counter.load(Ordering::SeqCst), 1);

    let doc = extract::extract_document(&body);
    assert!(matches!(
        read::decide_render(&doc),
        read::HttpDecision::Http
    ));
    let gate = WaitGate::ready(11);
    let first = read::capture_with_wait(
        &gate,
        11,
        &normalized,
        &final_url,
        &doc,
        status,
        Render::Http,
        "2026-10-30T00:00:00Z",
    )
    .unwrap();
    cache.put(
        &normalized,
        first.clone(),
        doc.markdown.clone(),
        doc.fragments.clone(),
        1_000,
    );

    // Cache hit within TTL: no second HTTP request is needed.
    let hit = cache.get(&normalized, 1_000 + 60).expect("cache hit");
    assert_eq!(hit.content_hash, first.content_hash);
    assert_eq!(hit.fetched_at, first.fetched_at);
    assert_eq!(counter.load(Ordering::SeqCst), 1);

    // TTL expiry forces a fresh fetch path.
    let expiring = read::ReadCache::new(10);
    expiring.put(
        &normalized,
        first.clone(),
        doc.markdown.clone(),
        doc.fragments,
        500,
    );
    assert!(expiring.get(&normalized, 500 + 11).is_none());
}
