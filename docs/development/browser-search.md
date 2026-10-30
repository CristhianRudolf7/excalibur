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
| `robots` | RFC 9309 parser and crawl decisions: longest-match wins, ties prefer `Allow`, exact-token groups combined when any match with prefix groups only when no exact group matches and `*` as fallback, `/robots.txt` always allowed, top-level `Sitemap:` lines kept, `Crawl-delay` capped at 30 s, 4xx allows and 5xx/unreachable denies with a one-hour retry |
| `normalize` | WHATWG URL normalization: lower-cased host, dropped fragment and default ports, tracking parameters removed, registrable-domain approximation with common two-level suffixes, robots path and search tokens |
| `frontier` | SQLite frontier (`hosts`, `urls`) with the architecture schema and states `queued`, `fetching`, `pending_commit`, `indexed`, `skipped`, `failed`, `gone`; only seeded hosts are crawlable while cross-host links register candidates and old candidate rows stay unseeded after migration, fresh `robots.txt` required before any dispatch (24 h reachable, 1 h retry for 5xx/unreachable, fail-closed otherwise), one in flight per host, a global in-flight cap plus a per-second page cap, politeness delay of `clamp(2 × duration, 1 s, 5 s)` plus deterministic jitter rounded up to whole seconds, explicit per-URL refresh intervals (7 d start, halve on change, double on stability, 1–60 d) with due `indexed` rows redispatched, robots-disallowed candidates marked `skipped` with the next candidate tried in the same call under the configured user agent, `Retry-After` handling, parking after five consecutive errors, `contact_url` gate, and crash recovery that re-queues `fetching` and `pending_commit` rows on close/reopen |
| `dedup` | Exact duplicates by `sha256` over normalized Markdown (case preserved); near duplicates by 64-bit SimHash over 3-word shingles with Hamming distance ≤ 3 inside one host |
| `ingest` | Bounded sitemap (`<loc>`, 50 000 URLs, 50 MiB) with host matching and single-pass XML-entity decoding plus a sitemap-directory variant, feed (RSS `<link>`, Atom `href`, JSON Feed `url`) and CirrusSearch line-pair parsing with the `incoming_links` threshold filter and null-tolerant `opening_text` |
| `index` | Deterministic disk-backed document store (`DiskIndex`) with the architecture document model, BM25 (`k1 = 1.2`, `b = 0.75`, title ×3, headings ×1.5, URL tokens ×2, body ×1), 1 KiB stored snippets, delete-by-URL updates, atomic per-insert durability with `commit` refreshing the 95%-of-budget state, source-priority admission (general web, news and HN stop at 95%, wiki/docs/arXiv/read stay admissible) and stable URL tie-breaks in every ordering |
| `ranking` | Score blending (`bm25/max + 0.3·prior + 0.2·freshness` with freshness only on request or recency intent), harmonic-rank and popularity priors, at most two results per host, `source_id` derived from `content_hash`, stable URL tie-breaks, RRF fusion with `k = 60`, `k ≤ 50` |
| `search` | `web_search` service returning ranked results, coverage (`profile`, `documents`, `last_crawl_at`, `by_source`, `budget_reached`) and the `insufficient` flag on raw BM25 and result count, `site:`/domain/language filters applied before candidate truncation, default `k = 10`, plus a per-query latency hook for later calibration |
| `config` | `minimo` (default, 1.5 GiB index, 0.5 GiB state) and `ampliado` profiles; crawl refuses to start without an operator contact URL |

The on-disk backend is an explicit `DiskIndex` with plain method
boundaries. The production tantivy store plugs into the same document model
and query flow later without changing query call sites. Refresh follows the
architecture approximation with explicitly stored intervals; feeds, Hacker
News and arXiv windows are integration policy on top of the same frontier states.
The `opstamp` column is reserved for the production writer handoff; in this
backend every insert commits in its own transaction.

## Verification

Run the isolated backend suite for this scope:

```sh
cargo fmt --check
CARGO_BUILD_JOBS=2 cargo clippy -p excalibur-search --all-targets -- -D warnings
CARGO_BUILD_JOBS=2 cargo test -p excalibur-search
CARGO_BUILD_JOBS=2 cargo build -p excalibur-search
```

The suite covers RFC 9309 decisions (longest match, ties in both orders,
merged groups, exact-only groups with prefix fallback, wildcards with end
anchors, token scoping, status mapping, crawl-delay caps, global and group
sitemap lines), missing/expired robots denial, candidate promotion with
migration keeping old candidates unseeded, disallowed-head skipping with
the next candidate dispatched under the configured user agent, politeness
ceilings on a controlled clock with three hosts, `Retry-After` backoff and
host parking, per-host fairness under head-of-line pressure, the
contact-URL gate, `noindex`/`gone` handling, crash recovery with
close/reopen, refresh doubling to 14 days with due indexed redispatch,
exact and near-duplicate merging, a 100-document known-item check at
success@10 ≥ 0.9, `site:`/language/`k` filters before truncation with deep
result sets, deterministic repeat ordering, per-host diversity, the
insufficient flag, freshness ordering, disk persistence, source-priority
budget admission, sitemap host and single-pass entity handling, null-tolerant Cirrus
parsing and RRF fusion. The workspace lock records only the new dependency
edges for this crate; no package was added or re-pinned and the root
manifest is untouched.

## Not implemented here

Production tantivy wiring, broker and resource integration, live wide
crawls, feed scheduling daemons, full B8 acceptance and VPS coverage
measurement belong to the main lane. Pending main-lane work includes the
streaming Cirrus reader with `Doc` conversion carrying `popularity_score`
priors over a 10 000-line fixture, calibration of the raw-BM25
`insufficient` threshold against the manual query set, a real subprocess
interruption test over frontier plus index, and tuning of the per-second
page cap on the VPS. A search result remains a candidate: only a later
read creates a source event.
