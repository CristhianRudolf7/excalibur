//! RFC 9309 `robots.txt` parsing and crawl decisions.
//!
//! Pure parser with no network access. The caller enforces the 500 KiB cap,
//! the 24 h cache for reachable hosts (1 h retry for 5xx/unreachable), at
//! most 5 redirects and the fetch-status mapping (4xx allows everything,
//! 5xx/unreachable denies everything with a retry in one hour).
//! `/robots.txt` itself is always allowed.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// Maximum `robots.txt` body the crawler may parse (500 KiB).
pub const MAX_ROBOTS_BYTES: usize = 512_000;
/// Upper bound honoured for `Crawl-delay` (30 s, non-standard but allowed).
pub const MAX_CRAWL_DELAY_SECS: u64 = 30;
/// Cache lifetime for a fetched `robots.txt` (24 h).
pub const ROBOTS_TTL_SECS: u64 = 24 * 3600;
/// Deny-all retry after an unreachable host or 5xx status (1 h).
pub const ROBOTS_FAILURE_RETRY_SECS: u64 = 3600;
/// Token advertised by this crawler.
pub const BOT_TOKEN: &str = "excaliburbot";

/// How a `robots.txt` fetch completed, decided by the caller.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RobotsFetch {
    /// Body fetched with 2xx; carries no allow/deny by itself.
    Ok,
    /// 4xx status: the whole host is allowed.
    NotFound,
    /// 5xx status or unreachable host: the whole host is denied for now.
    Unavailable,
}

/// A single allow/disallow rule.
#[derive(Debug, Clone)]
struct Rule {
    pattern: String,
    allow: bool,
}

/// Parsed group of rules for one or more user-agent tokens.
#[derive(Debug, Clone, Default)]
struct Group {
    agents: Vec<String>,
    rules: Vec<Rule>,
    crawl_delay_secs: Option<u64>,
    sitemaps: Vec<String>,
}

/// Parsed `robots.txt` ready for decisions.
#[derive(Debug, Clone, Default)]
pub struct RobotsRules {
    groups: Vec<Group>,
    sitemaps: Vec<String>,
}

impl RobotsRules {
    /// Parse a `robots.txt` body. Bodies larger than [`MAX_ROBOTS_BYTES`]
    /// must be truncated by the caller before invoking this function.
    #[must_use]
    pub fn parse(body: &str) -> Self {
        let mut groups: Vec<Group> = Vec::new();
        let mut current: Option<Group> = None;
        let mut seen_rule = false;
        let mut top_sitemaps: Vec<String> = Vec::new();

        let flush = |groups: &mut Vec<Group>, current: &mut Option<Group>| {
            if let Some(group) = current.take()
                && !group.agents.is_empty()
            {
                groups.push(group);
            }
        };

        for raw_line in body.lines() {
            let line = raw_line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let Some((field, value)) = line.split_once(':') else {
                continue;
            };
            let field = field.trim().to_ascii_lowercase();
            let mut value = value.trim().to_owned();
            if let Some(hash) = value.find('#') {
                value = value[..hash].trim().to_owned();
            }
            match field.as_str() {
                "user-agent" => {
                    if seen_rule {
                        flush(&mut groups, &mut current);
                        seen_rule = false;
                    }
                    let group = current.get_or_insert_with(Group::default);
                    group.agents.push(value.to_ascii_lowercase());
                }
                "allow" | "disallow" => {
                    seen_rule = true;
                    if value.is_empty() {
                        continue;
                    }
                    let group = current.get_or_insert_with(Group::default);
                    group.rules.push(Rule {
                        pattern: value,
                        allow: field == "allow",
                    });
                }
                "crawl-delay" => {
                    seen_rule = true;
                    if let Some(group) = current.as_mut() {
                        let seconds = parse_crawl_delay(&value);
                        group.crawl_delay_secs = Some(seconds.min(MAX_CRAWL_DELAY_SECS));
                    }
                }
                "sitemap" if !value.is_empty() => {
                    if let Some(group) = current.as_mut() {
                        if seen_rule {
                            group.sitemaps.push(value.clone());
                        } else {
                            // Sitemap lines never belong to a group per
                            // RFC 9309: keep them global even when a
                            // User-agent block is open but has no rules.
                            top_sitemaps.push(value.clone());
                        }
                    } else {
                        top_sitemaps.push(value);
                    }
                }
                _ => {}
            }
        }
        flush(&mut groups, &mut current);
        let mut sitemaps = top_sitemaps;
        for group in &groups {
            sitemaps.extend(group.sitemaps.iter().cloned());
        }
        Self { groups, sitemaps }
    }

    /// Rules for an empty body: everything is allowed.
    #[must_use]
    pub fn allow_all() -> Self {
        Self::default()
    }

    /// Product token of `user_agent` (lower-cased, before `/`).
    fn product_token(user_agent: &str) -> String {
        user_agent
            .to_ascii_lowercase()
            .split('/')
            .next()
            .unwrap_or("")
            .trim()
            .to_owned()
    }

    /// All groups whose agent matches `token`, with match length.
    /// Exact token equality outranks prefix matches; `*` only applies when
    /// no other group matches (RFC 9309 section 2.2.1 fallback).
    fn matching_groups(&self, user_agent: &str) -> Vec<&Group> {
        let token = Self::product_token(user_agent);
        let mut matched: Vec<(&Group, usize)> = Vec::new();
        let mut stars: Vec<&Group> = Vec::new();
        for group in &self.groups {
            let mut best: Option<usize> = None;
            for name in &group.agents {
                if name == "*" {
                    if !stars.iter().any(|existing| std::ptr::eq(*existing, group)) {
                        stars.push(group);
                    }
                } else if *name == token {
                    best = Some(best.map_or(name.len(), |b| b.max(name.len())));
                } else if !name.is_empty() && token.starts_with(name.as_str()) {
                    best = Some(best.map_or(name.len(), |b| b.max(name.len())));
                }
            }
            if let Some(len) = best {
                matched.push((group, len));
            }
        }
        if matched.is_empty() {
            return stars;
        }
        // RFC 9309 section 2.2.1: combine every matching group.
        matched.into_iter().map(|(group, _)| group).collect()
    }

    /// Decide whether `path` (path + optional query) may be fetched.
    /// `/robots.txt` is always allowed.
    #[must_use]
    pub fn is_allowed(&self, user_agent: &str, path: &str) -> bool {
        if path == "/robots.txt" || path.starts_with("/robots.txt?") {
            return true;
        }
        let matched = self.matching_groups(user_agent);
        if matched.is_empty() {
            return true;
        }
        let mut best_len: Option<usize> = None;
        let mut best_allow = true;
        for group in matched {
            for rule in &group.rules {
                if rule_matches(&rule.pattern, path) {
                    let len = match_len(&rule.pattern);
                    match best_len {
                        None => {
                            best_len = Some(len);
                            best_allow = rule.allow;
                        }
                        Some(prev) if len > prev => {
                            best_len = Some(len);
                            best_allow = rule.allow;
                        }
                        Some(prev) if len == prev && rule.allow => {
                            // RFC 9309 and section 15.4: ties prefer Allow.
                            best_allow = true;
                        }
                        _ => {}
                    }
                }
            }
        }
        best_allow
    }

    /// Effective `Crawl-delay` for `user_agent`, if any.
    #[must_use]
    pub fn crawl_delay_secs(&self, user_agent: &str) -> Option<u64> {
        let matched = self.matching_groups(user_agent);
        // Prefer the delay from the most specific matching group.
        let mut best: Option<(usize, u64)> = None;
        // Recompute specificity for delay selection.
        let token = Self::product_token(user_agent);
        for group in matched {
            let Some(delay) = group.crawl_delay_secs else {
                continue;
            };
            let mut specificity = 0_usize;
            for name in &group.agents {
                if name == "*" {
                    continue;
                } else if *name == token {
                    specificity = specificity.max(name.len() + 1_000);
                } else if token.starts_with(name.as_str()) {
                    specificity = specificity.max(name.len());
                }
            }
            // Star-only groups get specificity 0.
            if best.is_none_or(|(prev, _)| specificity >= prev) {
                best = Some((specificity, delay));
            }
        }
        best.map(|(_, delay)| delay)
    }

    /// `Sitemap:` targets seen anywhere in the file.
    #[must_use]
    pub fn sitemaps(&self) -> &[String] {
        &self.sitemaps
    }
}

/// Parsed fetch outcome combining status mapping and optional rules.
#[derive(Debug, Clone)]
pub struct RobotsDecision {
    /// Whether crawling may proceed.
    pub allowed: bool,
    /// Seconds the caller must wait before retrying when denied by failure.
    pub retry_after_secs: Option<u64>,
    /// Sitemap targets to seed the frontier.
    pub sitemaps: Vec<String>,
    /// Crawl delay advertised for our token.
    pub crawl_delay_secs: Option<u64>,
}

/// Combine the fetch status with parsed rules into a decision.
#[must_use]
pub fn decide(
    fetch: RobotsFetch,
    rules: Option<&RobotsRules>,
    user_agent: &str,
    path: &str,
) -> RobotsDecision {
    match fetch {
        RobotsFetch::NotFound => RobotsDecision {
            allowed: true,
            retry_after_secs: None,
            sitemaps: Vec::new(),
            crawl_delay_secs: None,
        },
        RobotsFetch::Unavailable => RobotsDecision {
            allowed: false,
            retry_after_secs: Some(ROBOTS_FAILURE_RETRY_SECS),
            sitemaps: Vec::new(),
            crawl_delay_secs: None,
        },
        RobotsFetch::Ok => {
            let rules = rules.cloned().unwrap_or_default();
            RobotsDecision {
                allowed: rules.is_allowed(user_agent, path),
                retry_after_secs: None,
                sitemaps: rules.sitemaps().to_vec(),
                crawl_delay_secs: rules.crawl_delay_secs(user_agent),
            }
        }
    }
}

/// Parse `<meta name=robots>` / `X-Robots-Tag` style directives.
/// Returns `(noindex, nofollow)`.
#[must_use]
pub fn parse_robots_directives(value: &str) -> (bool, bool) {
    let mut noindex = false;
    let mut nofollow = false;
    for token in value
        .split(',')
        .map(|part| part.trim().to_ascii_lowercase())
    {
        match token.as_str() {
            "noindex" => noindex = true,
            "nofollow" => nofollow = true,
            _ => {}
        }
    }
    (noindex, nofollow)
}

fn parse_crawl_delay(value: &str) -> u64 {
    value
        .trim()
        .parse::<f64>()
        .map(|secs| secs.max(0.0) as u64)
        .unwrap_or(0)
}

/// RFC 9309 path matching with `*` (any run) and `$` (end anchor).
fn rule_matches(pattern: &str, path: &str) -> bool {
    if pattern.is_empty() {
        return false;
    }
    let (pattern, anchored) = match pattern.strip_suffix('$') {
        Some(rest) => (rest, true),
        None => (pattern, false),
    };
    if !pattern.contains('*') {
        if anchored {
            return path == pattern;
        }
        return path.starts_with(pattern);
    }
    wildcard_match(pattern, path, anchored)
}

fn wildcard_match(pattern: &str, path: &str, anchored: bool) -> bool {
    let parts: Vec<&str> = pattern.split('*').collect();
    if parts.is_empty() {
        return !anchored || path.is_empty();
    }
    let mut rest = path;
    if !parts[0].is_empty() {
        if !rest.starts_with(parts[0]) {
            return false;
        }
        rest = &rest[parts[0].len()..];
    }
    for (index, part) in parts.iter().enumerate().skip(1) {
        if part.is_empty() {
            continue;
        }
        let last = index == parts.len() - 1;
        match rest.find(*part) {
            None => return false,
            Some(pos) => {
                if last && anchored {
                    return pos + part.len() == rest.len();
                }
                rest = &rest[pos + part.len()..];
            }
        }
    }
    true
}

fn match_len(pattern: &str) -> usize {
    pattern.strip_suffix('$').unwrap_or(pattern).len()
}

/// Group index helper used by the frontier for bookkeeping.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RobotsCacheEntry {
    /// Lower-cased host this entry applies to.
    pub host: String,
    /// When the entry was fetched (unix seconds).
    pub fetched_at_secs: u64,
    /// Whether the host was reachable.
    pub reachable: bool,
    /// Raw body when fetched (already capped at [`MAX_ROBOTS_BYTES`]).
    pub body: String,
    /// Advertised crawl delay in seconds, if any.
    pub crawl_delay_secs: Option<u64>,
}

impl RobotsCacheEntry {
    /// Whether the cached entry is still fresh at `now_secs`.
    /// Reachable entries live 24 h; unreachable/5xx entries retry after 1 h.
    #[must_use]
    pub fn fresh_at(&self, now_secs: u64) -> bool {
        let ttl = if self.reachable {
            ROBOTS_TTL_SECS
        } else {
            ROBOTS_FAILURE_RETRY_SECS
        };
        now_secs.saturating_sub(self.fetched_at_secs) < ttl
    }

    /// Rebuild parsed rules from the cached body.
    #[must_use]
    pub fn rules(&self) -> RobotsRules {
        RobotsRules::parse(&self.body)
    }
}

/// Serialize robots rules bookkeeping for the frontier store.
pub(crate) fn rules_map_key(host: &str) -> String {
    host.to_ascii_lowercase()
}

/// Cache key namespace to keep clippy's pedantic lints quiet about maps.
#[derive(Debug, Clone, Default)]
pub struct RobotsCache {
    entries: BTreeMap<String, RobotsCacheEntry>,
}

impl RobotsCache {
    /// Empty cache.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert or replace the entry for `host`.
    pub fn insert(&mut self, entry: RobotsCacheEntry) {
        self.entries.insert(rules_map_key(&entry.host), entry);
    }

    /// Fresh entry for `host` at `now_secs`, if any.
    #[must_use]
    pub fn fresh(&self, host: &str, now_secs: u64) -> Option<&RobotsCacheEntry> {
        self.entries
            .get(&rules_map_key(host))
            .filter(|entry| entry.fresh_at(now_secs))
    }
}
