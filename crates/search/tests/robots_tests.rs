//! RFC 9309 verification with local synthetic fixtures (section 15.11).

use excalibur_search::robots::{RobotsFetch, RobotsRules, decide, parse_robots_directives};

const AGENT: &str = "ExcaliburBot/0.1";

#[test]
fn longest_rule_wins() {
    let rules = RobotsRules::parse("User-agent: *\nDisallow: /private\nAllow: /private/open\n");
    assert!(rules.is_allowed(AGENT, "/private/open/doc"));
    assert!(!rules.is_allowed(AGENT, "/private/closed"));
}

#[test]
fn tie_break_prefers_allow() {
    let rules = RobotsRules::parse("User-agent: *\nDisallow: /tie\nAllow: /tie\n");
    assert!(rules.is_allowed(AGENT, "/tie/page"));
}

#[test]
fn default_is_allowed_without_matching_group() {
    let rules = RobotsRules::parse("User-agent: OtherBot\nDisallow: /\n");
    assert!(rules.is_allowed(AGENT, "/anything"));
}

#[test]
fn bot_token_preferred_over_star() {
    let rules = RobotsRules::parse(
        "User-agent: *\nDisallow: /\n\nUser-agent: ExcaliburBot\nAllow: /docs\nDisallow: /secret\n",
    );
    assert!(rules.is_allowed(AGENT, "/docs/page"));
    assert!(!rules.is_allowed(AGENT, "/secret/page"));
    assert!(rules.is_allowed(AGENT, "/other"));
}

#[test]
fn wildcard_and_end_anchor() {
    let rules = RobotsRules::parse("User-agent: *\nDisallow: /*.pdf$\nAllow: /files/open/\n");
    assert!(!rules.is_allowed(AGENT, "/files/report.pdf"));
    assert!(rules.is_allowed(AGENT, "/files/report.pdf/view"));
    // Longer Allow wins over the wildcard Disallow.
    assert!(rules.is_allowed(AGENT, "/files/open/report.pdf"));
    assert!(rules.is_allowed(AGENT, "/files/page"));
}

#[test]
fn robots_txt_itself_always_allowed() {
    let rules = RobotsRules::parse("User-agent: *\nDisallow: /\n");
    assert!(rules.is_allowed(AGENT, "/robots.txt"));
}

#[test]
fn status_mapping_not_found_allows_and_unavailable_denies() {
    let allow = decide(RobotsFetch::NotFound, None, AGENT, "/secret");
    assert!(allow.allowed);
    assert_eq!(allow.retry_after_secs, None);
    let deny = decide(RobotsFetch::Unavailable, None, AGENT, "/public");
    assert!(!deny.allowed);
    assert_eq!(deny.retry_after_secs, Some(3600));
}

#[test]
fn crawl_delay_capped_and_scoped_to_group() {
    let rules = RobotsRules::parse("User-agent: *\nCrawl-delay: 120\n");
    assert_eq!(rules.crawl_delay_secs(AGENT), Some(30));
    let scoped = RobotsRules::parse("User-agent: OtherBot\nCrawl-delay: 5\n");
    assert_eq!(scoped.crawl_delay_secs(AGENT), None);
}

#[test]
fn sitemap_lines_collected() {
    let rules = RobotsRules::parse(
        "User-agent: *\nAllow: /\nSitemap: https://example.com/sitemap.xml\nSitemap: https://example.com/news.xml\n",
    );
    let decision = decide(RobotsFetch::Ok, Some(&rules), AGENT, "/page");
    assert!(decision.allowed);
    assert_eq!(decision.sitemaps.len(), 2);
}

#[test]
fn meta_and_header_directives() {
    assert_eq!(parse_robots_directives("noindex, follow"), (true, false));
    assert_eq!(parse_robots_directives("INDEX, NOFOLLOW"), (false, true));
    assert_eq!(parse_robots_directives("index, follow"), (false, false));
}

#[test]
fn tie_break_prefers_allow_both_orders() {
    // P01: Allow-then-Disallow must still allow.
    let allow_first = RobotsRules::parse("User-agent: *\nAllow: /tie\nDisallow: /tie\n");
    assert!(allow_first.is_allowed(AGENT, "/tie/page"));
    // Existing order still allows.
    let disallow_first = RobotsRules::parse("User-agent: *\nDisallow: /tie\nAllow: /tie\n");
    assert!(disallow_first.is_allowed(AGENT, "/tie/page"));
}

#[test]
fn repeated_star_groups_are_merged() {
    // P02: two `*` blocks combine per RFC 9309 2.2.1.
    let rules = RobotsRules::parse("User-agent: *\nDisallow: /a\n\nUser-agent: *\nDisallow: /b\n");
    assert!(!rules.is_allowed(AGENT, "/a/x"));
    assert!(!rules.is_allowed(AGENT, "/b/x"));
    assert!(rules.is_allowed(AGENT, "/c"));
}

#[test]
fn exact_token_beats_prefix_group() {
    // P03: exact `excaliburbot` wins over earlier prefix `excalibur`.
    let rules = RobotsRules::parse(
        "User-agent: excalibur\nDisallow: /\n\nUser-agent: excaliburbot\nAllow: /\n",
    );
    assert!(rules.is_allowed(AGENT, "/page"));
}

#[test]
fn top_level_sitemap_is_kept() {
    // P04: Sitemap before any group is global.
    let rules = RobotsRules::parse("Sitemap: https://x.example/s.xml\nUser-agent: *\nDisallow:\n");
    assert_eq!(rules.sitemaps(), &["https://x.example/s.xml".to_owned()]);
}

#[test]
fn exact_group_ignores_prefix_group() {
    // Q01 / BSF-03: an exact `Allow: /` group must not be polluted by an
    // earlier prefix group that disallows `/private`.
    let rules = RobotsRules::parse(
        "User-agent: excalibur\nDisallow: /private\n\nUser-agent: excaliburbot\nAllow: /\n",
    );
    assert!(rules.is_allowed(AGENT, "/private/x"));
    assert!(rules.is_allowed(AGENT, "/other"));
}

#[test]
fn prefix_group_applies_without_exact_match() {
    // Prefix groups still combine when no exact group matches.
    let rules = RobotsRules::parse("User-agent: excalibur\nDisallow: /private\n");
    assert!(!rules.is_allowed(AGENT, "/private/x"));
    assert!(rules.is_allowed(AGENT, "/other"));
}
