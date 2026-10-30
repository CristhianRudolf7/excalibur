# Worker skill sessions

The shared `SkillRuntime` wraps the existing authorized tool runtime. The daemon binds each worker run to its effective agent scope after workspace, security, planning and memory bindings have succeeded. Each project indexes only its own `.agents/skills` directories plus configured user/installed roots and builtin skills; General has no project roots. Installed packages retain their quarantine state.

A run stores its original validated index snapshot, full skill bodies, immutable grouped catalog and up to three lexical suggestions. The selected provider's token counter and host window bound the catalog to min(window/50, 4000) tokens. Scope rebinding is refused. The serialized session has a checked SHA-256 digest and survives a new daemon process, including its original catalog and loaded restrictions. The host redactor checks frozen material before it is stored. Aggregate suggestion/load statistics remain in memory.

The engine obtains host session context before first context compilation: the catalog appends to L1 without replacing identity/memory, suggestions enter L3, and changes to loaded skills append identified L4 updates. Loading returns a body and the run's resolved core/deferred schemas in a tool result; it does not mutate the eager catalog or provider's native deferred catalog. `search_skills(query, group?)` searches the run's frozen index. `load_skill(name)` rejects an entry whose current hash no longer matches. Resource files remain subject to normal filesystem tools and their existing capability checks.

Activation and the keyed tool result commit in one database transaction. Repeated keys return the original result; changed calls cannot reuse a key. Failed schema resolution or incompatible active restrictions do not activate the skill. Active `allowed-tools` lists intersect one another and are enforced at preparation, execution of already prepared work, reconciliation and engine-owned builtin auditing. Deferred `call_tool` checks its target rather than treating its transport as a grant. Every underlying filesystem, memory, planning and security permission still applies. Stable tool definitions are retained for cache reuse; visibility is not authority.

Filesystem scans, tokenizer work and session decoding are offloaded to blocking workers. The watcher coalesces create/modify/remove events and performs the 30-minute recovery scan; file-access notifications do not retrigger scans. Host shutdown stops its watchers. The initial model window can be supplied by trusted `SkillsOptions`, otherwise it follows the provider context policy or the existing context default; unknown-model counting remains an estimate.

Verification commands (local mocks, no paid providers):

```sh
cargo test -p excalibur-skills --test catalog --test runtime
cargo test -p excalibur-daemon --test skills_host --test memory_host
cargo test -p excalibur-engine --test context_compaction --test context_session_races
cargo clippy -p excalibur-skills -p excalibur-daemon -p excalibur-engine --all-targets -- -D warnings
```

Final suite results and fixed-commit independent review remain pending for this checkpoint. This block does not close H11: master conversation/scope skill sessions, hybrid frozen-session suggestions, all eight decider ports, expert routing, durable aggregate statistics, curated content evaluation and H13 promotion/learning remain separate required work. Parent suggestions are optional hints, never substitutes for pinned mandatory instructions.
