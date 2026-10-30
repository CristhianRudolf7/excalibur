# H11 checkpoint and remaining integration

Base: `4888773` (preserved decision/routing foundation), descending from `6e7ed6f`.
This checkpoint completes a reviewable shared-library block, not H11 or H13.

| Architecture requirement | Implemented surface | Remaining work |
| --- | --- | --- |
| 12.1 format | Manifest validation and diagnostics | Cross-check against upstream format changes when upgrading |
| 12.2 index | Root precedence, conflicts, resource hashes, lexical/vector search, filesystem events and periodic scans | Persist statistics; connect lifetime and filesystem scopes to daemon projects |
| 12.3 catalog | Frozen grouped catalog with host tokenizer and strict token cap | Compile into actual L1; define explicit overflow reporting when headers cannot fit |
| 12.4 load/suggest | At most three lexical session suggestions, body loading, tool-resolution callback, authority intersection and deltas | Wire native `search_skills`/`load_skill`, deferred schemas, L3 suggestions, deltas and active authority enforcement into engine; use hybrid suggestions in frozen sessions |
| 12.5 catalog | 24 builtin skills in seven integrated groups; 22 group definitions | Curated catalog content, independent audit and evaluation before activation |
| 19 role routing | Role configuration, deterministic expert triggers and bounded isolated brief | Host routing/fallback integration, accounting and durable per-run routing state |
| 19 decider | Typed answers, refusal/fallback, SQLite labels and calibration | Connect all eight ports, abstention to ordinary agent turn, telemetry and deployment configuration |
| H13 | Installed roots are quarantined | Package audit, approval, changed-hash quarantine after promotion, A/B replicas, canary/rollback and preference learning |

The OpenAI Decisions adapter is preserved from the base commit. This checkpoint's tests exercise deterministic local decision doubles rather than confirming a live provider contract or making a paid request. The role-routing fixtures do not prove daemon integration. Skill statistics are in-memory and negative outcome filtering is an observational rule, not a randomized estimate of causal benefit.

For verification commands and runtime contracts see [Skills and bounded decisions](skills-decider.md). Full workspace format, clippy, tests, build and independent audit of the integrated commit remain delivery requirements. Do not issue a completion signal from this checkpoint.

Checkpoint verification: `cargo fmt --all --check` passed. `cargo test -p excalibur-skills --test catalog -p excalibur-decider --test contract -p excalibur-models --test routing` passed all 15 local tests (9 catalog, 4 decisions, 2 routing). The fixtures cover escaped JSON brief bounds and inconsistent decision distributions as well as the index/session contracts. Verification uses the global heavy-build lock, two build jobs and a separate target with debug symbols disabled.
