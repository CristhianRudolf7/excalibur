# Skills and bounded decisions

The shared `excalibur-skills` library validates Agent Skills front matter, reports invalid entries, resolves project/user/installed/builtin precedence, hashes resources and builds BM25 and optional host-provided vector indexes. Installed roots remain quarantined. Embedded skills cover the seven integrated groups; curated groups have definitions but are not shipped as audited content.

`SkillSession` freezes a catalog for a run. The caller supplies its tokenizer; L1 fits the smaller of 2% of the window and 4,000 tokens. Relevant groups receive descriptions when they fit; other groups receive headers. Extremely small budgets may omit headers. Suggestions are lexical over the frozen snapshot, at most three, with per-model negative-outcome filtering. `SkillIndex::search` additionally supports hybrid vector retrieval through a bounded embedding adapter.

Loading returns the frozen body as a tool result after a host callback resolves declared deferred tools. Loading never rewrites the catalog. Changed or unavailable entries cannot be newly loaded from a stale session. Active skills intersect `allowed-tools` with existing authority; callers must apply this intersection when executing tools. Deltas identify changes to loaded skills. `SkillWatcher` uses filesystem events and a 30-minute recovery scan; dropping it aborts its task.

The `excalibur-decider` library provides typed probability answers, bounded fallback, explicit refusal, conservative uncalibrated thresholds and a SQLite labeled ledger. Calibration reports Brier/ECE/MCE and coverage from supplied local labels. Final correctness verdicts have no decision port. The model-role router binds roles and supports isolated expert consultation while keeping an executor binding stable.

The daemon now binds durable worker skill sessions to the actual run and authorized project. The shared runtime supplies frozen L1 catalogs, at most three L3 suggestions, native search/load tools, associated deferred schemas, change deltas and execution restrictions. See [Worker skill sessions](skills-host.md). Master conversation sessions, all eight production decision ports, durable aggregate skill statistics, curated-skill audit/promotion and learning remain integration work. No provider quality or calibrated production accuracy is claimed. Tests use local deterministic doubles and never call paid APIs.

Verification:

```sh
cargo test -p excalibur-skills --test catalog
cargo test -p excalibur-decider --test contract
cargo clippy -p excalibur-skills -p excalibur-decider --all-targets -- -D warnings
```

Decision calibration is scoped by `CalibrationKey`: port, a hash of the complete typed question (instructions, ordered options/levels and descriptions), and the answer's backend/model origin. Use the key returned on `RoutedAnswer` when labeling/calibrating that decision family. Partial fallback preserves origins separately for each answer. The ledger retains old records and thresholds; old unscoped keys cannot match the new scoped route. Its decoded `calibration_key` exposes the model and question fingerprint without storing state input text.

The LLM adapter validates its response independently of provider schema enforcement: required fields/types, exactly one probability for every offered value, finite probabilities in [0,1], and a sum within 0.000001 of one. It preserves probabilities rather than normalizing arbitrary weights. Invalid JSON receives one correction attempt before bounded fallback. Request option/level descriptions and the complete serialized request have byte limits. `cargo test -p excalibur-decider --test llm_contract` exercises these paths through a deterministic mock model gateway, including retry, fallback and calibration isolation across model, backend and question changes.

OpenAI decision origins include a hash of the effective client's provider/endpoint identity and the primary/quota-fallback route. Calibration cannot transfer across different compatible endpoints merely because their model names match. Local HTTP fixtures verify both endpoint isolation and attribution to the transport that actually answered; these fixtures do not verify the live API contract.
