# Context and cache

[Español](context-cache.es.md)

The shared `excalibur-context` library provides deterministic context compilation and a serializable `ContextSession`. The engine prepares that session from confirmed execution history, scoped backend task facts and literal instructions before a new model request. Confirmed requests are reused on recovery. Context preparation, masking and compaction receipts remain in the journal, alongside references to the full history. Telemetry projects a compact receipt and state hash/pointer; the complete session remains in the principal journal.

## Layers and updates

L0 holds role instructions, safety rules and name-sorted core tools; L1 holds frozen identity/catalog; L2 holds stable scope inputs; L3 holds the literal task; L4 carries append-only activity and versioned updates; L5 carries time, budgets, continuation and transient reports. Tools remain in the catalogue when disabled; the gateway changes `tool_choice`. Suggested skills are limited to three. The host supplies selected memories/preferences; this library does not start a memory retriever.

`ContextSession::update_layer_changed` appends a versioned update rather than rewriting the stable baseline. The engine does this for current requirements, criterion states and the micro plan. Literal instruction updates are separate versioned entries. A compaction rerenders the current L2/L3 state once. Every preparation records layer hashes, a stable-prefix hash, token estimates and drift information; changing L0/L1 within a session is reported as a defect by the drift detector. Tools and output/reasoning configuration are frozen in the run specification.

`InstructionStore` is a trusted Rust host API over the existing `instructions` table, with checksummed additive metadata, optimistic revisions, immutable history and replay receipts. It preserves the exact text, source, principal, scope (general/project/task/conversation) and active/retired state. The model cannot register instructions through a tool. User-message sources must belong to the same principal. Explicit run bindings and delegation select instructions without importing another principal's content. The engine notices registry revisions and rejects unadmitted tool calls from superseded responses. The independent verifier receives the same scoped literal records and revisions. Finalization acquires a short shared instruction lease: a changed revision requires a fresh response, and registry mutations are ordered after the terminal journal transition. No lease spans inference. Host adapters classify durable fragments; ordinary conversation content is not automatically made authoritative.

## Masking and compaction

Unmasked observations exceeding 24K estimated tokens trigger one masking batch toward 12K. The latest ten observations, pinned observations and recent errors remain literal; a protected set may exceed the target. Expired cache TTL allows an earlier batch. Agent calls remain unchanged. Each marker contains a content hash and a usable `get_tool_output` blob reference. Confirmed JSON tool observations, including small results, are archived without changing their original bytes; blob references participate in the existing GC/durability protocol. `history_search` is bounded and restricted to the current run, with query/cursor and optional sequence range.

The compaction threshold is `min(500000, compaction_cap, floor(0.85 * model_window) - output_reserve)`, with a 500K ceiling that a model profile may lower. Token decisions include a 10% margin. `SessionPolicy` supplies the model window, output reserve, TTL and recent-window limits. Its default 128K window is a configured planning envelope, not a claim about an unknown model's advertised capacity; the embedding host must supply the selected model's actual limits. `ExecOptions.context_policy` passes this configuration to the shared engine and the durable seed retains it.

Compaction derives the literal original request, active instructions, requirements, fresh criterion states, plan, artifact paths/operations/hashes, the last twenty commands with exits, exact open-error text, children and decisions from backend data. It retains the recent 20K activity window without splitting tool-call/result groups. Pins and indivisible interactions can exceed that target; receipts report the excess, and a view that cannot fit the configured window fails explicitly. A tool error is resolved only after the same operation and arguments succeed; unrelated changes do not imply resolution. Earlier commands and results remain in the journal. No mandatory information is silently truncated.

An optional extractor gateway, configured through `Engine::with_context_extractor`, returns a strict incremental summary of only the discarded segment. The engine requires the extractor role and a usage bound, journals admission and completion, honors control/timeout, accounts usage and uses a structured fallback on absence, provider failure, invalid output or interruption. Previous findings are merged rather than rewritten; missing open IDs are repaired mechanically. A pending extractor request reserves its upper bound and prevents distributing that share to children before reconciliation. An interrupted receipt is conservatively accounted and is not blindly redispatched. Summaries and quoted observations remain untrusted data and cannot replace structured state or literal instructions.

Conversation mode retains a 12K recent window and updates its summary only after 24K activity or an expired-TTL session boundary. Omitted segments remain searchable. This is a shared host-facing mode; it does not implement a master/channel service or a general subagent scheduler.

## Tokenizers, provider caches and verification

Known tokenizer mappings use local embedded `tiktoken-rs` vocabularies. Other models use a character estimate with an integer moving-average correction learned only from known provider input usage, separately per model/session. Missing or estimated usage does not calibrate the counter. The 10% margin is a policy reserve, not a universal accuracy guarantee for unseen data. Synthetic calibration tests measure error on declared fixtures.

[Provider cache contracts](provider-cache.md) describe verified official request fields, Anthropic marks/TTLs, opaque OpenAI namespaces, implicit caching and bounded first-delta sibling warming. Namespace keys hash principal/role/scope; no raw personal identity is sent as a cache key. Observed input, cache-read and cache-write counters remain distinct from theoretical prefix reuse. Missing counters remain unknown. Provider-native opaque replay state is preserved in history; it is not treated as an instruction.

```sh
cargo test -p excalibur-context
cargo test -p excalibur-models --test cache
cargo test -p excalibur-journal --test context_instructions
cargo test -p excalibur-engine --test context_compaction --test context_session_races
cargo test -p excalibur-telemetry --test context_summary
cargo fmt --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo build --workspace
```

Backend fixtures use synthetic records, local HTTP/SSE, real SQLite/files and isolated subprocess interruption. The cache suite compares expected reuse with returned fixture counters; it does not claim live provider hit rates or production savings. Linux runtime evidence does not imply Windows/macOS execution. No model credentials, paid inference, GPU or frontend tests are required.
