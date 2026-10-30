# Memory and documents

[Español](memory.es.md)

`excalibur-memory` is a shared library for scoped long-term memory, the Retriever, background consolidation, embedding spaces, identity documents and the document library with deterministic citations. It installs a checksummed additive schema over the principal SQLite database and keeps every query inside explicit scopes. Model-backed roles (extractor, decider, cheap classifier, reranker, query rewriter, note compactor, vision extraction, transcription, context sentences) are traits supplied by the host; the library starts no model, network service or daemon.

The library is not yet wired into the conversational master, the engine tool runtime or the context compiler. Those connection points are listed at the end of this page.

## Grants and scopes

Every entry point takes a `MemoryGrant`: principal, between 1 and 64 explicit scopes, optional run, conversation and task, user time zone and a trust ceiling. Requested scopes must be a subset of the grant; an empty grant is rejected. Lexical, semantic, relational and temporal candidates are filtered by principal and scope in SQL and checked again before results are returned. Vectors live in scope-partitioned `vec0` tables. Relations and graph traversal only follow edges of the requested scopes. Consolidation prompts contain turns of a single scope, and each prompt's scope set is recorded.

## Records and admission

A memory has kind (`fact`, `preference`, `event`, `commitment`, `belief`, `relation`, `procedure`), statement, entities, optional attribute, valid time (`valid_from`, `valid_to`), record time (`recorded_at`, `invalidated_at`), optional expiry, status (`candidate`, `active`, `expired`, `superseded`, `disputed`, `retired`), confidence, importance, sources and trust (`user`, `agent`, `untrusted_external`). Statements pass through the shared secret redactor.

- `memory_write` stores an explicit memory and admits it immediately.
- `memory_propose` stores a candidate with its source; it is retrievable only in the originating conversation or task until consolidation admits it.
- Admission compares a candidate with records of the same scope that share an entity, match lexically or are vector neighbours. Similarity ≥ 0.95 with a shared entity merges provenance into the existing record. Between 0.80 and 0.95 the decider classifies, then the cheap classifier; with neither available the record stays a candidate. A refinement supersedes the previous record and closes its validity. Same entity and attribute with a different statement is a contradiction: with an explicit temporal change (flag or wording such as "now" or "ya no") the new record supersedes the old one; otherwise both become disputed and a low-priority question is queued.
- Content from external pages is marked `untrusted_external`; a preference from such a source is stored as a fact and is never proposed for the user profile.
- Updates never destroy records; every status change is kept. `purge_memory` is the only destructive operation and removes content, keys, vectors and admission detail at the user's request.

## Retrieval

`retrieve` performs deterministic query analysis: dates and relative expressions in English and Spanish resolved in the user's time zone, known entity names and aliases as relational seeds, quoted phrases and identifiers as mandatory matches, and historical cues. "What was true on X" uses valid time; "what did we believe on X" uses record time. Four candidate lists of up to 50 items are produced: lexical (FTS5 BM25 with diacritic folding, plus trigram indexes for identifiers, over memories, messages, enriched keys and document chunks), semantic (KNN over every live embedding space), relational (one or two hops) and temporal. Reciprocal Rank Fusion uses k = 60. Expiry is enforced at read time; superseded, expired and retired records appear only in historical queries. Decay half-lives are 7 days for events, 90 for preferences and commitments and 365 for the rest; importance, confidence and trust adjust the score. An optional reranker reorders close leaders and its order is used directly. Notices report insufficient evidence, contradictions, disputes, scope ambiguity, unavailable semantic search and a query rewrite. A single cheap rewrite happens only when there are no results. Packing groups results by scope with id, statement, validity and source within a token budget. Extracted facts are indexed as additional keys pointing to their original turn.

`history_search` pages past messages by text, dates, conversation, scope and role.

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

## Verification

```sh
cargo test -p excalibur-memory
cargo test -p excalibur-memory --test mem_scope --test mem_time --test mem_conflict --test mem_cite
cargo fmt --check
cargo clippy --workspace --all-targets -- -D warnings
```

`mem_scope`, `mem_time`, `mem_conflict` and `mem_cite` cover isolation, bitemporal answers and expiry, admission and conflict handling, and citations. Further suites cover retrieval, embeddings, identity documents and consolidation. Tests use temporary SQLite databases and deterministic local doubles from `excalibur_memory::testing`; they make no model or network calls. LongMemEval-S and the MemoryAgentBench conflict subset have not been run.

## Pending connections and limits

- Master and engine: registering the memory tools in the tool runtime, binding grants to conversations, runs and tasks, closing segments from the master and calling `validate_citations` before a response is sent.
- Context compiler: the agent-profile core in layer 1, relevant preferences and project context in layer 2 and automatic retrieval at task start.
- Scheduler: running `plan_review` and `run_review` in the background P3 lane with live user activity, provider batch APIs and model roles from the router.
- Security: the full file inspection of the security layer behind `IngestGuard`; the built-in guard only checks size, type signature and UTF-8.
- Run tree: `TaskRunLinkTree` reads task parent links; the daemon's agent-tree view still has to be adapted to `RunTree`.
- Not implemented: local ONNX/candle model runtimes, an external HNSW index above 500 000 vectors per scope, Office and video extraction, and tool/status filters in `history_search`.
