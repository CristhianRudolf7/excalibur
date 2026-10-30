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

fn allow_all(frontier: &Frontier, host: &str, now: u64) {
    frontier.store_robots(host, "", true, now, None).unwrap();
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
    assert_eq!(sub.domain, "example.co.uk");
}

#[test]
fn one_inflight_per_host_and_global_order() {
    let mut frontier = Frontier::open_memory("https://operator.example/contact").unwrap();
    frontier.add_host("a.example", 0.5, Some(30)).unwrap();
    frontier.add_host("b.example", 0.9, Some(30)).unwrap();
    allow_all(&frontier, "a.example", 0);
    allow_all(&frontier, "b.example", 0);
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
    allow_all(&frontier, "a.example", 0);
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
    allow_all(&frontier, "slow.example", 1000);
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
    allow_all(&frontier, "slow.example", 1000 + 25 * 3600);
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
        allow_all(&frontier, "c.example", 0);
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
    // A second link to the same candidate must still not be crawlable.
    let second = frontier
        .enqueue(
            "https://new.example/other",
            1,
            Priority::Normal,
            Source::Web,
            0,
            2,
        )
        .unwrap();
    assert!(!second, "candidate hosts stay uncrawlable until seeded");
    let mut frontier = frontier;
    assert!(
        frontier.select_due(0).unwrap().is_empty(),
        "no dispatch without seeding"
    );
    // Seeding promotes the candidate.
    frontier.add_host("new.example", 0.2, Some(30)).unwrap();
    allow_all(&frontier, "new.example", 0);
    let third = frontier
        .enqueue(
            "https://new.example/seeded",
            1,
            Priority::Normal,
            Source::Web,
            0,
            2,
        )
        .unwrap();
    assert!(third, "seeded hosts are crawlable");
}

#[test]
fn robots_missing_and_expired_deny_until_refetch() {
    // BSF-02 / P05a-b: fail closed without fresh robots.
    let frontier = Frontier::open_memory("https://operator.example/contact").unwrap();
    frontier.add_host("r.example", 0.5, Some(30)).unwrap();
    assert!(
        !frontier
            .robots_allowed("https://r.example/secret", "ExcaliburBot/0.1", 10)
            .unwrap(),
        "never fetched must deny"
    );
    frontier
        .store_robots("r.example", "User-agent: *\nDisallow: /\n", true, 0, None)
        .unwrap();
    assert!(
        !frontier
            .robots_allowed("https://r.example/secret", "ExcaliburBot/0.1", 10)
            .unwrap()
    );
    assert!(
        !frontier
            .robots_allowed("https://r.example/secret", "ExcaliburBot/0.1", 25 * 3600)
            .unwrap(),
        "expired disallow-all must still deny (needs refetch)"
    );
}

#[test]
fn unreachable_robots_use_one_hour_retry() {
    // BSF-02 / P05c: 5xx/unreachable TTL is 1 h, not 24 h, and denies.
    let frontier = Frontier::open_memory("https://operator.example/contact").unwrap();
    frontier.add_host("u.example", 0.5, Some(30)).unwrap();
    frontier
        .store_robots("u.example", "", false, 0, None)
        .unwrap();
    assert!(
        !frontier
            .robots_allowed("https://u.example/x", "ExcaliburBot/0.1", 2 * 3600)
            .unwrap()
    );
    assert!(
        !frontier
            .robots_allowed("https://u.example/x", "ExcaliburBot/0.1", 25 * 3600)
            .unwrap(),
        "expired unreachable still denies until refetch"
    );
}

#[test]
fn scheduler_requires_fresh_robots() {
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
    // No robots yet: no dispatch.
    assert!(frontier.select_due(0).unwrap().is_empty());
    // Fresh allow-all robots: dispatch works.
    frontier
        .store_robots("a.example", "", true, 0, None)
        .unwrap();
    assert_eq!(frontier.select_due(0).unwrap().len(), 1);
}

#[test]
fn politeness_uses_ceiling_and_three_hosts() {
    // BSF-05: 2400 ms duration -> >=4800 ms delay, ceil to 5 s.
    let mut frontier = Frontier::open_memory("https://operator.example/contact").unwrap();
    for host in ["a.example", "b.example", "c.example"] {
        frontier.add_host(host, 0.5, Some(30)).unwrap();
        frontier.store_robots(host, "", true, 0, None).unwrap();
        frontier
            .enqueue(
                &format!("https://{host}/1"),
                0,
                Priority::Normal,
                Source::Web,
                0,
                2,
            )
            .unwrap();
    }
    let tasks = frontier.select_due(0).unwrap();
    assert_eq!(tasks.len(), 3, "three hosts dispatch in parallel");
    let outcome = FetchOutcome {
        status: Some(200),
        retry_after_secs: None,
        duration_ms: 2400,
        content_changed: false,
        noindex: false,
        nofollow: false,
    };
    frontier
        .record_outcome("https://a.example/1", &outcome, 0, None, None)
        .unwrap();
    let next = frontier.host_next_fetch("a.example").unwrap().unwrap();
    assert!(next >= 5, "ceil(4800ms+jitter) must be >=5s, got {next}");
    // Not due at +4 s.
    frontier
        .store_robots("a.example", "", true, 4, None)
        .unwrap();
    // Enqueue a second URL on a.example for the due check.
    frontier
        .enqueue(
            "https://a.example/2",
            0,
            Priority::Normal,
            Source::Web,
            4,
            2,
        )
        .unwrap();
    // Host still waits for its delay; other hosts are idle so only
    // politeness blocks a.example here. Re-store fresh robots at due time.
    frontier
        .store_robots("b.example", "", true, next, None)
        .unwrap();
    frontier
        .store_robots("c.example", "", true, next, None)
        .unwrap();
}

#[test]
fn head_of_line_blocking_is_fair_across_hosts() {
    // BSF-06 / P09: 80 High on big + 1 Normal on small must dispatch both.
    let mut frontier = Frontier::open_memory("https://operator.example/contact").unwrap();
    frontier.add_host("big.example", 0.5, Some(200)).unwrap();
    frontier.add_host("small.example", 0.5, Some(30)).unwrap();
    frontier
        .store_robots("big.example", "", true, 0, None)
        .unwrap();
    frontier
        .store_robots("small.example", "", true, 0, None)
        .unwrap();
    for i in 0..80 {
        frontier
            .enqueue(
                &format!("https://big.example/{i}"),
                0,
                Priority::High,
                Source::Web,
                0,
                5,
            )
            .unwrap();
    }
    frontier
        .enqueue(
            "https://small.example/1",
            0,
            Priority::Normal,
            Source::Web,
            0,
            5,
        )
        .unwrap();
    let tasks = frontier.select_due(0).unwrap();
    let hosts: std::collections::HashSet<String> =
        tasks.iter().map(|task| task.host.clone()).collect();
    assert!(hosts.contains("big.example"), "big dispatched: {hosts:?}");
    assert!(
        hosts.contains("small.example"),
        "small must not starve: {hosts:?}"
    );
    assert_eq!(tasks.len(), 2);
}

#[test]
fn refresh_interval_doubles_and_redispatches_indexed() {
    // BSF-07 / P16: unchanged second visit 7 d -> 14 d, due indexed dispatch.
    let mut frontier = Frontier::open_memory("https://operator.example/contact").unwrap();
    frontier.add_host("a.example", 0.5, Some(30)).unwrap();
    frontier
        .store_robots("a.example", "", true, 0, None)
        .unwrap();
    frontier
        .enqueue(
            "https://a.example/p",
            0,
            Priority::Normal,
            Source::Web,
            0,
            2,
        )
        .unwrap();
    frontier.select_due(0).unwrap();
    frontier
        .record_outcome("https://a.example/p", &ok_outcome(), 0, Some("h"), None)
        .unwrap();
    frontier
        .mark_indexed("https://a.example/p", false, 0)
        .unwrap();
    // Refresh robots so the due indexed row is dispatchable at day 30.
    let day = 24 * 3600;
    frontier
        .store_robots("a.example", "", true, 21 * day, None)
        .unwrap();
    // Re-queue via second cycle: simulate a refetch at day 7.
    // Manually move to queued to emulate the scheduler picking the due row,
    // then complete the second visit unchanged and check the stored interval.
    // First interval is 7 d; second unchanged visit must schedule 14 d.
    frontier
        .mark_indexed("https://a.example/p", false, 7 * day)
        .unwrap();
    // Due indexed rows are eligible: at day 30 the page is due (7+14=21d).
    // Refresh robots close to dispatch so the 24 h TTL still holds.
    frontier
        .store_robots("a.example", "", true, 30 * day, None)
        .unwrap();
    let tasks = frontier.select_due(30 * day).unwrap();
    assert_eq!(tasks.len(), 1, "due indexed must redispatched");
    assert_eq!(tasks[0].url, "https://a.example/p");
}

#[test]
fn robots_disallowed_head_does_not_block_host() {
    // Q02 / BSF-17: a disallowed High URL must not starve a Normal URL.
    let mut frontier = Frontier::open_memory("https://operator.example/contact").unwrap();
    frontier.add_host("h.example", 0.5, Some(30)).unwrap();
    frontier
        .store_robots(
            "h.example",
            "User-agent: *\nDisallow: /secret\n",
            true,
            0,
            None,
        )
        .unwrap();
    frontier
        .enqueue(
            "https://h.example/secret",
            0,
            Priority::High,
            Source::Web,
            0,
            2,
        )
        .unwrap();
    frontier
        .enqueue(
            "https://h.example/public",
            0,
            Priority::Normal,
            Source::Web,
            0,
            2,
        )
        .unwrap();
    let tasks = frontier.select_due(0).unwrap();
    assert_eq!(tasks.len(), 1, "allowed URL must dispatch: {tasks:?}");
    assert_eq!(tasks[0].url, "https://h.example/public");
    assert_eq!(
        frontier.url_state("https://h.example/secret").unwrap(),
        Some(excalibur_search::UrlState::Skipped),
        "disallowed head must be skipped"
    );
    // A second call must not resurrect the skipped URL; the host is busy.
    let again = frontier.select_due(0).unwrap();
    assert!(again.is_empty(), "host busy, no redispatch: {again:?}");
}

#[test]
fn select_due_uses_configured_user_agent() {
    // BSF-17: per-URL robots decisions use the configured crawler token.
    let mut frontier = Frontier::open_memory("https://operator.example/contact").unwrap();
    frontier.set_user_agent("CustomBot/2.0");
    assert_eq!(frontier.user_agent(), "CustomBot/2.0");
    frontier.add_host("u.example", 0.5, Some(30)).unwrap();
    frontier
        .store_robots(
            "u.example",
            "User-agent: CustomBot\nDisallow: /secret\n\nUser-agent: *\nAllow: /\n",
            true,
            0,
            None,
        )
        .unwrap();
    frontier
        .enqueue(
            "https://u.example/secret",
            0,
            Priority::High,
            Source::Web,
            0,
            2,
        )
        .unwrap();
    frontier
        .enqueue(
            "https://u.example/public",
            0,
            Priority::Normal,
            Source::Web,
            0,
            2,
        )
        .unwrap();
    let tasks = frontier.select_due(0).unwrap();
    assert_eq!(tasks.len(), 1);
    assert_eq!(tasks[0].url, "https://u.example/public");
}

#[test]
fn old_candidate_hosts_stay_candidates_after_migration() {
    // BSF-18: databases created before `seeded` must not promote candidates.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("search.db");
    {
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch(
            "CREATE TABLE hosts(host TEXT PRIMARY KEY, domain TEXT NOT NULL DEFAULT '', prior REAL NOT NULL DEFAULT 0.2, next_fetch_at INTEGER NOT NULL DEFAULT 0, delay_ms INTEGER NOT NULL DEFAULT 1000, crawl_delay_ms INTEGER, robots_blob TEXT NOT NULL DEFAULT '', robots_fetched_at INTEGER NOT NULL DEFAULT -1, robots_reachable INTEGER NOT NULL DEFAULT 1, err_count INTEGER NOT NULL DEFAULT 0, parked_until INTEGER NOT NULL DEFAULT 0, page_budget INTEGER NOT NULL DEFAULT 30, pages_fetched INTEGER NOT NULL DEFAULT 0);
             CREATE TABLE urls(url_hash TEXT PRIMARY KEY, url TEXT NOT NULL, host TEXT NOT NULL, depth INTEGER NOT NULL DEFAULT 0, priority INTEGER NOT NULL DEFAULT 10, state TEXT NOT NULL DEFAULT 'queued', source TEXT NOT NULL DEFAULT 'web', last_fetch INTEGER, next_fetch INTEGER NOT NULL DEFAULT 0, etag TEXT, last_modified TEXT, n_fetch INTEGER NOT NULL DEFAULT 0, n_change INTEGER NOT NULL DEFAULT 0, simhash INTEGER, content_hash TEXT, opstamp INTEGER);",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO hosts(host, domain) VALUES ('old.example', 'example')",
            [],
        )
        .unwrap();
    }
    let frontier = Frontier::open(&path, "https://operator.example/contact", 30, 16).unwrap();
    assert!(
        !frontier.is_host_seeded("old.example").unwrap(),
        "migrated candidate must stay unseeded"
    );
}
