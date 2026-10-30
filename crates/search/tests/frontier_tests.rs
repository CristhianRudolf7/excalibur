//! Frontier and polite-scheduler verification on a controlled clock.

use excalibur_search::frontier::{FetchOutcome, Frontier, Priority, Source};
use excalibur_search::{SearchError, normalize_url};

fn ok_outcome() -> FetchOutcome {
    FetchOutcome {
        status: Some(200),
        retry_after_secs: None,
        duration_ms: 400,
        content_changed: true,
        noindex: false,
        nofollow: false,
    }
}

#[test]
fn missing_contact_url_refuses_to_start() {
    let result = Frontier::open_memory("");
    assert!(matches!(result, Err(SearchError::MissingContactUrl)));
}

#[test]
fn url_normalization_strips_tracking_and_defaults() {
    let first = normalize_url("HTTP://Example.COM:80/docs?utm_source=x&keep=1#frag").unwrap();
    let second = normalize_url("http://example.com/docs?keep=1").unwrap();
    assert_eq!(first.canonical, second.canonical);
    assert_eq!(first.host, "example.com");
    assert_eq!(first.domain, "example.com");
    let sub = normalize_url("https://a.b.example.co.uk:443/x").unwrap();
    assert_eq!(sub.domain, "co.uk");
}

#[test]
fn one_inflight_per_host_and_global_order() {
    let mut frontier = Frontier::open_memory("https://operator.example/contact").unwrap();
    frontier.add_host("a.example", 0.5, Some(30)).unwrap();
    frontier.add_host("b.example", 0.9, Some(30)).unwrap();
    frontier
        .enqueue(
            "https://a.example/1",
            0,
            Priority::Normal,
            Source::Web,
            0,
            2,
        )
        .unwrap();
    frontier
        .enqueue(
            "https://a.example/2",
            0,
            Priority::Normal,
            Source::Web,
            0,
            2,
        )
        .unwrap();
    frontier
        .enqueue("https://b.example/1", 0, Priority::High, Source::Web, 0, 2)
        .unwrap();

    let tasks = frontier.select_due(0).unwrap();
    assert_eq!(tasks.len(), 2);
    assert_eq!(tasks[0].host, "b.example");
    assert_eq!(tasks[1].host, "a.example");

    // Second selection sees both hosts busy: nothing new.
    let again = frontier.select_due(0).unwrap();
    assert!(again.is_empty());
}

#[test]
fn politeness_interval_respected_after_success() {
    let mut frontier = Frontier::open_memory("https://operator.example/contact").unwrap();
    frontier.add_host("a.example", 0.5, Some(30)).unwrap();
    frontier
        .enqueue(
            "https://a.example/1",
            0,
            Priority::Normal,
            Source::Web,
            0,
            2,
        )
        .unwrap();
    frontier
        .enqueue(
            "https://a.example/2",
            0,
            Priority::Normal,
            Source::Web,
            0,
            2,
        )
        .unwrap();
    let tasks = frontier.select_due(0).unwrap();
    assert_eq!(tasks.len(), 1);
    frontier
        .record_outcome(
            "https://a.example/1",
            &ok_outcome(),
            0,
            Some("hash-1"),
            Some(1),
        )
        .unwrap();
    let next = frontier.host_next_fetch("a.example").unwrap().unwrap();
    assert!(next >= 1, "next_fetch_at={next}");
    // Not due before the computed delay.
    assert!(frontier.select_due(0).unwrap().is_empty());
    // Due once the delay elapses.
    let tasks = frontier.select_due(next + 1).unwrap();
    assert_eq!(tasks.len(), 1);
    assert_eq!(tasks[0].url, "https://a.example/2");
}

#[test]
fn retry_after_and_parking() {
    let frontier = Frontier::open_memory("https://operator.example/contact").unwrap();
    frontier.add_host("slow.example", 0.5, Some(30)).unwrap();
    frontier
        .enqueue(
            "https://slow.example/page",
            0,
            Priority::Normal,
            Source::Web,
            0,
            2,
        )
        .unwrap();
    let outcome = FetchOutcome {
        status: Some(429),
        retry_after_secs: Some(90),
        duration_ms: 100,
        content_changed: false,
        noindex: false,
        nofollow: false,
    };
    let state = frontier
        .record_outcome("https://slow.example/page", &outcome, 1000, None, None)
        .unwrap();
    assert_eq!(state, excalibur_search::UrlState::Queued);
    let next = frontier.host_next_fetch("slow.example").unwrap().unwrap();
    assert_eq!(next, 1090);

    for _ in 0..4 {
        frontier
            .record_outcome("https://slow.example/page", &outcome, 1000, None, None)
            .unwrap();
    }
    // After five consecutive errors the host parks for 24 h.
    let mut frontier = frontier;
    let tasks = frontier.select_due(2000).unwrap();
    assert!(tasks.is_empty());
    let parked = frontier.select_due(1000 + 25 * 3600).unwrap();
    assert_eq!(parked.len(), 1);
}

#[test]
fn gone_and_noindex_states() {
    let frontier = Frontier::open_memory("https://operator.example/contact").unwrap();
    frontier.add_host("gone.example", 0.5, Some(30)).unwrap();
    frontier
        .enqueue(
            "https://gone.example/old",
            0,
            Priority::Normal,
            Source::Web,
            0,
            2,
        )
        .unwrap();
    frontier
        .enqueue(
            "https://gone.example/noindex",
            0,
            Priority::Normal,
            Source::Web,
            0,
            2,
        )
        .unwrap();
    let gone = FetchOutcome {
        status: Some(404),
        ..ok_outcome()
    };
    assert_eq!(
        frontier
            .record_outcome("https://gone.example/old", &gone, 0, None, None)
            .unwrap(),
        excalibur_search::UrlState::Gone
    );
    let noindex = FetchOutcome {
        noindex: true,
        ..ok_outcome()
    };
    assert_eq!(
        frontier
            .record_outcome("https://gone.example/noindex", &noindex, 0, None, None)
            .unwrap(),
        excalibur_search::UrlState::Skipped
    );
}

#[test]
fn robots_decision_blocks_disallowed_paths() {
    let frontier = Frontier::open_memory("https://operator.example/contact").unwrap();
    frontier.add_host("r.example", 0.5, Some(30)).unwrap();
    frontier
        .store_robots(
            "r.example",
            "User-agent: *\nDisallow: /secret\n",
            true,
            0,
            None,
        )
        .unwrap();
    assert!(
        !frontier
            .robots_allowed("https://r.example/secret/page", "ExcaliburBot/0.1", 10)
            .unwrap()
    );
    assert!(
        frontier
            .robots_allowed("https://r.example/public", "ExcaliburBot/0.1", 10)
            .unwrap()
    );
    // Unreachable host denies with retry semantics.
    frontier
        .store_robots("r.example", "", false, 20, None)
        .unwrap();
    assert!(
        !frontier
            .robots_allowed("https://r.example/public", "ExcaliburBot/0.1", 30)
            .unwrap()
    );
}

#[test]
fn crash_recovery_requeues_interrupted_urls() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("search.db");
    {
        let mut frontier =
            Frontier::open(&path, "https://operator.example/contact", 30, 16).unwrap();
        frontier.add_host("c.example", 0.5, Some(30)).unwrap();
        frontier
            .enqueue(
                "https://c.example/1",
                0,
                Priority::Normal,
                Source::Web,
                0,
                2,
            )
            .unwrap();
        frontier
            .enqueue(
                "https://c.example/2",
                0,
                Priority::Normal,
                Source::Web,
                0,
                2,
            )
            .unwrap();
        let tasks = frontier.select_due(0).unwrap();
        assert_eq!(tasks.len(), 1);
        frontier
            .record_outcome(
                "https://c.example/1",
                &ok_outcome(),
                0,
                Some("hash-1"),
                Some(7),
            )
            .unwrap();
        assert_eq!(frontier.stats().unwrap().pending_commit, 1);
    }
    {
        let frontier = Frontier::open(&path, "https://operator.example/contact", 30, 16).unwrap();
        let recovered = frontier.recover_interrupted().unwrap();
        assert_eq!(recovered, 1);
        let stats = frontier.stats().unwrap();
        assert_eq!(stats.queued, 2);
        assert_eq!(stats.pending_commit, 0);
    }
}

#[test]
fn unknown_cross_host_links_only_register_candidates() {
    let frontier = Frontier::open_memory("https://operator.example/contact").unwrap();
    frontier.add_host("seed.example", 0.5, Some(30)).unwrap();
    let crawled = frontier
        .enqueue(
            "https://new.example/page",
            1,
            Priority::Normal,
            Source::Web,
            0,
            2,
        )
        .unwrap();
    assert!(!crawled, "unknown hosts must not be crawled before seeding");
    assert_eq!(frontier.stats().unwrap().hosts, 2);
}
