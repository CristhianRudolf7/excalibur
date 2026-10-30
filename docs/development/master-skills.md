# Master conversation skill sessions

The daemon composes `MasterSkillFastActions` outside memory and the authorized fast-tool adapter. Its schema delegate sees only `FastActions` definitions. It has no worker runtime, eager file/terminal tools or engine-only utility catalog. `search_skills` and `load_skill` are the only added master tool names. A skill requiring an unavailable associated schema fails before activation. Optional deferred fast tools remain subject to the existing master deferred adapter and its authorization.

`MasterContextRequest` carries the configured principal, conversation, original source scope and accepted source ID from the shared master's context compiler. The daemon verifies those fields against the durable message and inbox, then reads the literal source itself. Tool arguments cannot select a binding. Conversation/catalog and source-turn keys are domain-separated SHA-256 hashes of typed JSON values, preserving field boundaries.

The shared `SkillRuntime` freezes one original catalog snapshot per principal/conversation/scope. Each accepted source turn starts from that snapshot with its own loaded restrictions and at most three frozen lexical suggestions derived from the literal source. Suggestions are hints, never permissions or replacements for mandatory literal instructions. Returning to A after B or General reuses A's catalog; another source never inherits A's previous turn restrictions. The binding, original snapshot, loaded policy, suggestions and keyed skill receipts survive database reopening. A changed skill cannot be loaded from the old snapshot; an already loaded body remains its original tool receipt, with a versioned change delta.

The compiler appends the catalog to L1, leaving identity and memory blocks in place, and the suggestions to L3 without replacing the original source. Schema migration v5 stores L4 context updates with their immutable source binding, stable ID, body and sequence reference. Repeated updates reuse that reference; reusing an ID with different content fails. Original-source scope controls context even when the conversation's active scope has changed. Existing literal-source and redaction checks still apply.

Ordinary fast effects hold the shared runtime's read admission guard through `invoke_source_scoped`; skill activation holds its exclusive write guard. Policy is rechecked inside the shared runtime. The adapter forwards original conversation, scope, source and operation key to memory/fast actions. Human `confirm_cancel` forwards directly to the inner host-only port, retaining its durable human-decision checks and no model schema. Explicit user command handling keeps its existing host path.

Verification commands (local deterministic fixtures only):

```sh
cargo test -p excalibur-skills --test catalog --test runtime
cargo test -p excalibur-master --test context --test fast_tools --test cancel_confirmation
cargo test -p excalibur-daemon --test master_skills --test memory_host --test memory_history --test cancel_confirmation --test source_authority
cargo clippy -p excalibur-skills -p excalibur-master -p excalibur-daemon --all-targets -- -D warnings
cargo build -p excalibur-skills -p excalibur-master -p excalibur-daemon
```

This checkpoint has implementation and backend fixtures but has not yet been compiled, tested or independently approved. The new daemon fixture drives the actual master model/tool/context path and database reopening; direct adapter assertions additionally cover replay, scope/principal denial, unavailable worker/engine schemas and host-only confirmation forwarding. These are intended checks until execution results are recorded. Hybrid suggestions, expert routing, all decider ports, durable aggregate statistics, curated content evaluation, promotion/learning and H11 overall remain incomplete. There is no interface or provider quality claim.
