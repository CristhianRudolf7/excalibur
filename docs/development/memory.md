# Memory and documents

[Español](memory.es.md)

`excalibur-memory` is a shared library for scoped long-term memory, the Retriever, background consolidation, embedding spaces, identity documents and the document library with deterministic citations. It installs a checksummed additive schema over the principal SQLite database and keeps every query inside explicit scopes. Model-backed roles (extractor, decider, cheap classifier, reranker, query rewriter, note compactor, vision extraction, transcription, context sentences) are traits supplied by the host; the library starts no model, network service or daemon.

The daemon connects this library to its conversational master, worker tool runtime and context preparation through `MemoryHost`. The foreground daemon enables it with lexical, temporal and relational retrieval; embedding providers remain an explicit host option. Embedded hosts opt in through `HostOptions.memory`. The configured memory principal must match the master principal.

## Grants and scopes

Every entry point takes a `MemoryGrant`: principal, between 1 and 64 explicit scopes, optional run, conversation and task, user time zone and a trust ceiling. Requested scopes must be a subset of the grant; an empty grant is rejected. Lexical, semantic, relational and temporal candidates are filtered by principal and scope in SQL and checked again before results are returned. Vectors live in scope-partitioned `vec0` tables. Relations and graph traversal only follow edges of the requested scopes. Consolidation prompts contain turns of a single scope, and each prompt's scope set is recorded.

## Records and admission

A memory has kind (`fact`, `preference`, `event`, `commitment`, `belief`, `relation`, `procedure`), statement, entities, optional attribute, valid time (`valid_from`, `valid_to`), record time (`recorded_at`, `invalidated_at`), optional expiry, status (`candidate`, `active`, `expired`, `superseded`, `disputed`, `retired`), confidence, importance, sources and trust (`user`, `agent`, `untrusted_external`). Statements pass through the shared secret redactor.

- `memory_write` stores an explicit memory and admits it immediately.
- `memory_propose` stores a candidate with its source; it is retrievable only in the originating conversation or task until consolidation admits it.
- Admission compares a candidate with records of the same scope that share an entity, match lexically or are vector neighbours. Similarity ≥ 0.95 with a shared entity merges provenance into the existing record. Between 0.80 and 0.95 the decider classifies, then the cheap classifier; with neither available the record stays a candidate. A refinement supersedes the previous record and closes its validity. A lower-trust refinement or contradiction remains a candidate rather than invalidating a higher-trust assertion. Same entity and attribute with a different statement is a contradiction: with an explicit temporal change (flag or wording such as "now" or "ya no") the new record supersedes the old one; otherwise both become disputed and a low-priority question is queued.
- Content from external pages is marked `untrusted_external`; a preference from such a source is stored as a fact and is never proposed for the user profile.
- Updates never destroy records; every status change is kept. `purge_memory` is the only destructive operation and removes content, keys, vectors and admission detail at the user's request.

## Retrieval

`retrieve` performs deterministic query analysis: dates and relative expressions in English and Spanish resolved in the user's time zone, known entity names and aliases as relational seeds, quoted phrases and identifiers as mandatory matches, and historical cues. "What was true on X" uses valid time; "what did we believe on X" uses record time. Four candidate lists of up to 50 items are produced: lexical (FTS5 BM25 with diacritic folding, plus trigram indexes for identifiers, over memories, messages, enriched keys and document chunks), semantic (KNN over every live embedding space), relational (one or two hops) and temporal. Reciprocal Rank Fusion uses k = 60. Expiry is enforced at read time; superseded, expired and retired records appear only in historical queries. Decay half-lives are 7 days for events, 90 for preferences and commitments and 365 for the rest; importance, confidence and trust adjust the score. An optional reranker reorders close leaders and its order is used directly. Notices report insufficient evidence, contradictions, disputes, scope ambiguity, unavailable semantic search and a query rewrite. A single cheap rewrite happens only when there are no results. Packing groups results by scope with id, statement, validity and source within a token budget. Extracted facts are indexed as additional keys pointing to their original turn.

`history_search` pages past messages and journaled tool results by text, dates, conversation, scope, role, tool and success/error status. Action visibility comes from durable host run bindings, rather than model-supplied run ids.

## Embeddings

`EmbeddingProvider` is the common interface for local and remote embedders. Vectors are truncated to 512 dimensions, normalized and stored as int8; the original text is always retained. Spaces are versioned in `embedding_spaces`. When a provider is unavailable, writes enter an indexing queue and retrieval continues with lexical, temporal and relational lists while reporting the degradation. `begin_migration` creates a new space and queues every indexed object; reads use both spaces until `finish_migration` verifies that nothing is missing, then the old space is retired. `LazyLocalEmbedder` loads a local model on first use and releases it after 10 idle minutes; a load failure becomes queued indexing. `EmbeddingProfile` describes the default model choices; local model runtimes are not linked in this crate.

## Identity documents and notebook

The agent profile, user profile, notebook and personas are stored as fragments with heading path, order, text, version and hash. `doc_read` returns the heading index, one section or matching fragments. `doc_edit` replaces, inserts or deletes by fragment id and expected hash; on a mismatch nothing changes and the current fragment is returned, and the second consecutive mismatch for the same actor and fragment requests escalation. Every version is kept and can be restored. Profile changes require the motivating message; unless explicitly requested by the user they become proposals and are applied when repeated evidence reaches two sources. Notebook entries belong to a scope. Personas copy the agent profile and do not touch user memory. `agent_core` returns a bounded, hashed snapshot for a stable prompt prefix.

## Consolidation

`close_segment` extracts and admits new turns of one scope (30 minutes of inactivity are detected with `segment_idle`). `plan_review` uses a light mode during the first 5 active days (03:30 local after 2 idle hours) and afterwards chooses the quietest contiguous window from a 14-day hourly histogram of user messages and host-reported activity, recomputed weekly. `run_review` works per scope with a token budget, checkpoints after each batch, yields when the activity signal reports the user and resumes from its checkpoint. Its steps are extraction and admission, expiry, provenance checks, contradiction resolution (most recent explicit timestamp wins, otherwise dispute and question), deterministic entity links, notebook compaction with an append-only timeline (rewrites removing more than 25 % of entries are rejected), profile proposals from repeated user preferences, skill-learning candidates and a stored report. Learned relation types are proposed after 5 unmatched facts within 28 days and become active after consistent use or user confirmation; types can be merged.

## Library and citations

`library_ingest` passes files through an `IngestGuard`, stores the original as a versioned blob and extracts text: native for text and Markdown, the `VisionExtractor` contract for PDF and images, the `Transcriber` contract for audio. Office formats and video report unsupported until connected. Chunks keep document, version, page, paragraph, character offsets, section path, line range or time range. Documents marked as library get a one-time context sentence for ambiguous chunks. `library_search` returns citable chunks and records them for the calling run. `validate_citations` resolves `[[doc:ID#cN]]` against chunks retrieved by the responding run or its descendants and renders "Title, p. 12, para. 3", or section, paragraph and lines with an explicit "no pagination" marker, or a time range. Invalid citations are removed and the sentence is marked unsourced. Web sources are built only from pages recorded with `record_source_read` in the same task tree and cited by the answer.

## Tools

`memory_tool_definitions` and `call_memory_tool` expose `memory_write`, `memory_propose`, `memory_search`, `history_search`, `doc_read`, `doc_edit`, `library_ingest` and `library_search`. Arguments may narrow scopes but never widen them, trust is capped by the grant and whether a profile edit was explicitly requested is supplied by the host.

## Host integration and recovery

`MemoryTools` exposes the core memory tools only to host-bound worker runs. The durable binding fixes principal, read scopes, primary write scope, conversation, task, parent run, worker/master role and time zone; rebinding may not change any of them. A restarted runtime reloads that binding. Worker writes have at most agent trust and are restricted to the primary scope even when general memory is readable. Master writes get user trust only for the current durable human source; a generated report cannot borrow an older human message to authorize an explicit profile edit. Agent profile suggestions require user confirmation even after repeated suggestions; only user-trust evidence is eligible for automatic consolidation. The master uses scoped tools, and `library_ingest` is a deferred worker domain tool.

Before a keyed write, the runtime commits a pending receipt. A completed receipt returns its original result on replay. If a process stops between the receipt and effect completion, reconciliation returns `Uncertain` and blocks automatic replay, because an effect may already exist. This prevents duplicate effects but does not promise transparent recovery of that interruption: the host must inspect and resolve the uncertain operation. Concurrent calls with the same key cannot both perform the effect; changed arguments or tool names are rejected.

Task context preparation replaces inherited memory blocks with blocks for the child's own grant: the bounded agent core in L1, user preferences in L2 and automatic task recall in L3. Empty grants remove inherited memory from all three layers. The stored task context remains stable after preparation. The master's final response uses the admitted source turn's scope even after a conversation switches projects. Citation validation follows durable run bindings, includes descendants and the master's conversation workers, and filters by principal and granted scope. A validation failure replaces citation tokens with an unsourced marker.

The work runtime periodically runs expiry, embedding queues/migrations, idle segment extraction and daily review in the maintenance model lane. Activity is refreshed while a pass runs so review can yield between batches. Stopping the host aborts its maintenance task. Admission can use calibrated roles from `MemoryOptions.services`, with a cheap fallback on the user model lane so active user turns do not stall behind paused maintenance. `memory.*` RPC methods query and curate the same store, inspect provenance, confirm proposals and relation types, and require explicit confirmation for destructive memory purging.

## Verification

```sh
cargo test -p excalibur-memory
cargo test -p excalibur-daemon --test memory_host
cargo test -p excalibur-gateway --test shared_backend
cargo test -p excalibur-memory --test mem_scope --test mem_time --test mem_conflict --test mem_cite
cargo fmt --check
cargo clippy --workspace --all-targets -- -D warnings
```

`mem_scope`, `mem_time`, `mem_conflict` and `mem_cite` cover isolation, bitemporal answers and expiry, admission and conflict handling, and citations. Further suites cover retrieval, embeddings, identity documents and consolidation. Tests use temporary SQLite databases and deterministic local doubles from `excalibur_memory::testing`; they make no model or network calls. LongMemEval-S and the MemoryAgentBench conflict subset have not been run.

Snapshot `41b90ecc53bb8ea2d6666e51b645e30aae136ca8` (2026-10-30) passed the complete memory crate suite: 45 tests, no failures, and one ignored child fixture that the process-interruption test launches explicitly. The initial runtime run failed the pinned-preference validity regression; a stable-tree rerun with the validity filter passed. Host/gateway integration tests and directed Clippy remain pending at this checkpoint; these results do not establish full H10 completion or external benchmark performance.

The independent review of that snapshot found five defects, addressed in its first correction: extraction now takes human authority and literal text from `master_user_sources` (role alone never grants user trust); bound calls distinguish shared reads from primary-scope writes; notebook version history filters fragment scopes in SQL, including deleted fragments; profile reverts obey provenance, explicit-request and trust rules; and consolidation refuses a timestamp winner below another assertion's trust. `audit_regressions` covers the library boundaries and `memory_host` exercises actual urgent notices through the master. Verification of this correction is pending until its checks finish; the earlier 45-test result applies to the earlier snapshot.

## Pending connections and limits

- The maintenance scope list is currently captured at host startup; automatically enrolling projects registered later remains pending. The noninteractive `exec` host does not yet enable long-term memory.
- Provider batch APIs, local ONNX/candle model runtimes and an external HNSW index above 500 000 vectors per scope remain unimplemented. The foreground daemon defaults to degraded retrieval until an embedder is supplied; `RemoteEmbedder` adapts the shared provider client and `text-embedding-3-small`.
- Full security-layer file inspection behind `IngestGuard` remains pending; the built-in guard checks size, type signature and UTF-8. The model-facing ingest tool currently accepts text and Markdown, not arbitrary paths.
- Vision/transcription/context-sentence roles are contracts that hosts must supply. Office and video extraction are unsupported.
- The fixed memory-model adapters and curation/maintenance entry points are backend building blocks; no memory management UI or measured external benchmark score is claimed.
