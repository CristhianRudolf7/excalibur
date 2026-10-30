# Browser search foundation

[Español](browser-search.es.md)

`excalibur-search` implements the standalone foundations for the search
milestone of the external `arquitectura-navegador.md` (sections 15.4–15.5
and 15.7–15.9). The crate is self-contained: it depends only on `rusqlite`,
`url`, `serde`, `serde_json`, `sha2` and `thiserror`, all already pinned in
the workspace lock, and it never touches the browser lifecycle, the network
broker or host resource accounting. No live crawl, no deployment and no
model access are involved; every check runs on local synthetic fixtures.

## Module map

| Module | Responsibility |
|---|---|
| `robots` | RFC 9309 parser and crawl decisions: longest-match wins, ties prefer `Allow`, `ExcaliburBot` token with `*` fallback, `/robots.txt` always allowed, `Crawl-delay` capped at 30 s, 4xx allows and 5xx/unreachable denies with a one-hour retry |
| `normalize` | WHATWG URL normalization: lower-cased host, dropped fragment and default ports, tracking parameters removed, registrable-domain approximation, robots path and search tokens |
| `frontier` | SQLite frontier (`hosts`, `urls`) with the architecture schema and states `queued`, `fetching`, `pending_commit`, `indexed`, `skipped`, `failed`, `gone`; one in flight per host, a global in-flight cap, politeness delay of `clamp(2 × duration, 1 s, 5 s)` plus deterministic jitter, `Retry-After` handling, parking after five consecutive errors, `contact_url` gate, and crash recovery that re-queues `fetching` and `pending_commit` rows |
| `dedup` | Exact duplicates by `sha256` over normalized Markdown; near duplicates by 64-bit SimHash over 3-word shingles with Hamming distance ≤ 3 inside one host |
| `ingest` | Bounded sitemap (`<loc>`, 50 000 URLs, 50 MiB), feed (RSS `<link>`, Atom `href`, JSON Feed `url`) and CirrusSearch line-pair parsing with the `incoming_links` threshold filter |
| `index` | Deterministic disk-backed document store with the architecture document model, BM25 (`k1 = 1.2`, `b = 0.75`, title ×3, headings ×1.5, URL tokens ×2, body ×1), 1 KiB stored snippets, delete-by-URL updates, atomic commits and a 95%-of-budget admission flag |
| `ranking` | Score blending (`bm25/max + 0.3·prior + 0.2·freshness` with freshness only on request or recency intent), harmonic-rank and popularity priors, at most two results per host, RRF fusion with `k = 60`, `k ≤ 50` |
| `search` | `web_search` service returning ranked results, coverage (`profile`, `documents`, `last_crawl_at`, `by_source`, `budget_reached`) and the `insufficient` flag, plus a per-query latency hook for later calibration |
| `config` | `minimo` (default, 1.5 GiB index, 0.5 GiB state) and `ampliado` profiles; crawl refuses to start without an operator contact URL |

The on-disk backend is an explicit `DiskIndex` behind plain method
boundaries. The production tantivy store plugs into the same document model
and query flow later without changing query call sites. Refresh follows the
architecture approximation: the next visit starts at 7 days, halves on
content change and doubles on stability within 1–60 days; feeds, Hacker News
and arXiv windows are integration policy on top of the same frontier states.

## Verification

Run the isolated backend suite for this scope:

```sh
cargo fmt --check
CARGO_BUILD_JOBS=2 cargo clippy -p excalibur-search --all-targets -- -D warnings
CARGO_BUILD_JOBS=2 cargo test -p excalibur-search
CARGO_BUILD_JOBS=2 cargo build -p excalibur-search
```

The suite covers RFC 9309 decisions (longest match, ties, wildcards with
end anchors, token scoping, status mapping, crawl-delay caps, sitemap
lines), politeness on a controlled clock, `Retry-After` backoff and host
parking, the contact-URL gate, `noindex`/`gone` handling, crash recovery
with real process-equivalent close/reopen, exact and near-duplicate merging,
a 100-document known-item check at success@10 ≥ 0.9, `site:`/language/`k`
filters, per-host diversity, the insufficient flag, freshness ordering, disk
persistence, budget admission and RRF fusion. The workspace lock records only
the new dependency edges for this crate; no package was added or re-pinned
and the root manifest is untouched.

## Not implemented here

Production tantivy wiring, broker and resource integration, live wide
crawls, feed scheduling daemons, full B8 acceptance and VPS coverage
measurement belong to the main lane. A search result remains a candidate:
only a later read creates a source event.
