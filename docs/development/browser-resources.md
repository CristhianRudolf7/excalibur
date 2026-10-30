# Browser resource foundation

[Español](browser-resources.es.md)

`excalibur-browser-resources` implements the standalone resource foundation
of the external `arquitectura-navegador.md` (section 13 with the 18.3
measurement procedure). The crate is self-contained with no dependencies: it
never touches the browser lifecycle, the network broker or host accounting.
Every check runs on local synthetic fixtures plus one live self-read of
`/proc/self/status`; no model access, no live crawl, no real user sessions
and no deployment are involved.

## Module map

| Module | Responsibility |
|---|---|
| `sampling` | Tiered sampling: level A (`memory.current - inactive_file`, 2 s), level B (`RssAnon + VmSwap` per `/proc/<pid>/status`, 10 s), level C (`Pss` per `smaps_rollup`, 60 s), level D (cgroup events, PSI `some avg10` > 10 %); process enumeration from a `/proc`-shaped root sorted by PID with `--type=` classification (`browser`, `zygote`, `gpu-process`, `utility`, `renderer`, `crashpad_handler`, `webd`); PSS and working-set totals grouped by type; desired `oom_score_adj` (ephemeral renderers 1000, persistent renderers 300, GPU/utility 200, browser 0) with the same-user raise rule |
| `budgets` | Explicit RAM and disk constants: soft global 1.5 GiB, hard 2.0 GiB, cgroup `memory.max` 2.5 GiB, listening-tab warning 700 MiB, live reload at 1 GiB for 10 min with 2 min quiet, ephemeral warning 500 MiB and close 800 MiB, at most 3 contexts and 3 tabs ephemeral, 4 tabs per execution and 12 global, default estimate 200 MiB (400 MiB heavy) plus 200 MiB margin, PSI admission below 20 %, webd 60 MiB RSS, Chrome cache 32 MiB ephemeral and 64 MiB persistent, profile warn 1 GiB and clear-allowed 1.5 GiB, 5 GiB free reserve, 7 GiB low-disk threshold, webd data 2.5 GiB (snapshots 500 MiB LRU 24 h, read cache 200 MiB, quarantine 300 MiB reject), search `minimo` 1.5 GiB index plus 0.5 GiB state and `ampliado` 6 GiB plus 1 GiB |
| `admission` | Per-origin moving-average estimator (200 MiB default, 400 MiB WhatsApp/Teams class); decisions deny tab limits, PSI at or above 20 % and disk growth breaching the reserve immediately, admit when estimate plus margin fits under soft, otherwise wait up to 120 s before `capacity_exceeded` with the memory breakdown |
| `eviction` | Classes LIVE, ACTIVE (60 s window), IDLE (5 min TTL) and HTTP; ordered steps 1-7 (webd caches, IDLE LRU including non-listening persistent tabs, ephemeral GC plus moderate pressure only on the ephemeral instance, ephemeral browser cache, close of one over-limit ephemeral ACTIVE tab with `tab_evicted`, ephemeral restart, LIVE quiet reload only); listening tabs never frozen, discarded, pressured or suspended; `suspend_session` returns `listener_cannot_suspend` on listening sessions without user approval; forbidden operations listed (`prepareForLeakDetection`, freeze, strict `memory.high` without swap) |
| `disk` | Admission gates (`disk_admission_allowed`, `search_start_allowed` requiring budget plus reserve), profile actions, low-disk cleanup that empties ephemeral caches and persistent `cache_storage`/`shader_cache` of non-listening origins only, webd data-budget state and a generic oldest-first LRU budget |
| `telemetry` | `BrowserResourceSample` with `browser_working_set_bytes`, `browser_pss_bytes`, `browser_instances`, `browser_tabs` and their peaks, reported separately from harness and container RAM |
| `measure` | Reproducible procedure summary: blank instance, local static site, ten heavy ephemeral tabs, 30 s settle, level A/B/C reads with PSS by type, exactly 3 repetitions with median/min/max plus Chrome, kernel and flag metadata |

## Verification

Run the isolated backend suite for this scope (standalone workspace, own
target directory):

```sh
cargo fmt --check --manifest-path crates/browser-resources/Cargo.toml
CARGO_BUILD_JOBS=2 CARGO_TARGET_DIR=/tmp/browser-resources-target cargo clippy --manifest-path crates/browser-resources/Cargo.toml --all-targets -- -D warnings
CARGO_BUILD_JOBS=2 CARGO_TARGET_DIR=/tmp/browser-resources-target cargo test --manifest-path crates/browser-resources/Cargo.toml
CARGO_BUILD_JOBS=2 CARGO_TARGET_DIR=/tmp/browser-resources-target cargo build --manifest-path crates/browser-resources/Cargo.toml
```

The suite covers `--type=` classification of every Chrome class, level-B
and level-C parsing, level-A arithmetic with `inactive_file`, event and PSI
thresholds, enumeration of a six-process synthetic tree with totals matching
an independent read, the OOM raise rule, RAM and disk defaults, heavy-origin
estimates with the moving average, admit/wait/deny paths with the 120 s
timeout and breakdown, tab-limit/PSI/reserve denials, eviction ordering with
IDLE LRU, ephemeral-only pressure and per-limit close, LIVE quiet-reload
gating, suspend refusal, low-disk protection of listening origins, search
start gates, LRU eviction order, telemetry peaks and three-repetition
medians. The crate has no dependencies, so no lock or root manifest changes
are needed.

## Not implemented here

Live `webd` wiring, cgroup scope management, real Chrome supervision, broker
pressure signals, CDP GC/pressure/cache calls, profile directory walks with
`Storage.getUsageAndQuota`, VPS measurement runs and full B6 acceptance
belong to the main lane. A resource decision remains an input to admission
and eviction: only the wired service enforces it.
