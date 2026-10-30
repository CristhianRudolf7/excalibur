# Project tools and editing concurrency

The shared Rust libraries provide project-domain tools through a deferred catalogue. The embedding host owns project paths, approved language-server commands, task identities, permissions and lifecycle. The noninteractive backend wires the project graph, change monitor and catalogue into its existing execution path; it adds no frontend server.

## Catalogue and permissions

`LocalTools` is the physical runtime. `project::deferred_catalog()` describes `file_history`, `revert_edit`, `workspace_snapshot`, `worktree`, `integration_queue`, `code_graph` and `lsp`. `DeferredTools` exposes their schemas through `search_tools` and executes `call_tool` through the same secured delegate as a direct call. Their schemas are absent from the eager model prefix. See [tool retrieval](tool-retrieval.md) for BM25/vector ranking, role filtering, native provider support and schema validation.

A host can compose `DeferredTools(ExecutionTools(EpisodeTools(SecuredTools(LocalTools))))`. The deferred wrapper resolves the actual name before it enters authorization. Entry history/restoration requires authority over the canonical parent directory, so replacing a symlink never follows its current target for authorization. Restoration targets are explicit; rename results outside the authorized path set are rejected. A model cannot choose the Git identity, task authority or integration test executor. `LocalTools::authorize_parent` installs those trusted capabilities. Project branches are never pushed or merged into the user's branch by these tools.

## Durable edits, history and command snapshots

Every content restoration reuses preparation, durable preimages, fingerprint checks, write leases, atomic publication and `file_versions`. `file_history` lists a file's recorded versions. `revert_edit` plans a three-way reversal against the current file: unrelated later changes survive; conflicts return a report without overwriting. Reopening and reconciliation reuse the same prepared edit and version identity.

Steps of a run that uses exec take a before shadow snapshot when the step starts and an after snapshot when the step ends; exec calls in the middle reuse the step's before snapshot instead of walking the tree again. Leaving active execution through pause, cancellation, approval wait or block also closes the current step with an after snapshot, so steps that end outside the normal boundary stay restorable. Runs that never touch exec record no snapshots. Blobs are hashed in a single process per snapshot and the index is written NUL-terminated, so hostile file names travel literally and can neither corrupt nor forge index entries; oversized or secret content is skipped with a warning instead of failing the snapshot, and step boundaries degrade to a warning on the next exec result instead of failing the step. Each tree gets an immutable Git ref before its SQLite receipt is published. Before rate-limited object collection, legacy receipts from all runs at that root are protected too. Active history has no arbitrary snapshot-count cutoff. Temporal expiry and orphan-ref cleanup remain pending H9 retention work; this conservative policy can retain extra objects. Automatic Git housekeeping is disabled on these repositories. This does not stage files or create commits in the user's repository. `workspace_snapshot` lists the current run's snapshots, shows a diff or restores explicitly selected files. Snapshot identifiers from other runs are rejected. A running command can span several steps; each step records the observed filesystem state rather than claiming the command has finished.

Snapshot inputs pass descriptor-bound root and private-hardlink checks. Credential names, sensitive metadata, protected host paths, `.git`, `target`, `node_modules` and ignore rules are excluded. The graph consumes authorized regular UTF-8 files; snapshot entry handling also retains executable and link metadata where the host filesystem supports them. Restoration never follows an external link to read or overwrite its target. Unsupported or conflicting entry changes are reported explicitly.

## Language services and source graph

`excalibur-code-intelligence` indexes Rust, Python, JavaScript, TypeScript/TSX, JSON and Markdown through real tree-sitter grammars. File hashes prevent redundant parses; cached trees support incremental edits. Persisted nodes/edges cover symbols, imports, calls/references, tests, document sections and citations. Query results distinguish syntactic name resolution from LSP enrichment. Explicit requirement links retain their host evidence reference. `impacted_tests` follows the indexed dependency graph; incomplete or dynamic relationships are not inferred as proven facts.

`ProjectIntelligence` connects this graph to the file tools and monitor. An edit returns newly introduced syntax errors and current-version LSP errors, without repeating pre-existing diagnostics. The parser also checks missing punctuation. Binary, protected or oversized source files are reported as unindexed and stale graph entries are removed. The source indexing limit is 8 MiB per file; ordinary file tooling and snapshot byte limits remain separately enforced.

`LspManager` registers servers without starting them, shares one session per project/language, and reaps sessions after ten idle minutes. Its operations include definition, references, document/workspace symbols, diagnostics and rename. The concrete Linux spawner uses a read-only sandbox, a private home, closed networking and explicit host environment. Server `workspace/applyEdit` requests are rejected. Renames are proposals until the ordinary file pipeline validates document hashes, versions, Unicode UTF-16 ranges, overlap and authorized targets. Server resource-operation edits that were not negotiated are rejected.

General/user and project TOML may declare `language_servers.<language>` with `command`, `args`, `enabled` and `initialization_options`. Commands resolve against the owning configuration file, independently of cwd. Scope files cannot override them; project files cannot redirect storage or reduce durability. Loading configuration never launches a server: the host must approve the declaration and create a confined `ServerConfig`. Trusted `ExecOptions.language_servers` supplies approved servers to noninteractive execution; an empty map starts none.

A matching diagnostic version does not prove that the server has finished all background analysis. A server may first publish an empty list and later publish errors for the same version. Fresh checks and syntax diagnostics remain distinct from a successful build/test result.

## Leases, partitions and integration

`excalibur-workspace` combines durable canonical-path leases with an in-memory map and operating-system locks for cooperating processes. Leases expire after ten minutes without heartbeat; active runs renew them during long tools. The renewal loop retries a failed owner on the next tick instead of stopping, and it keeps refreshing while any managing host is alive. Human waits, pause, blockage and terminal states release them. A conflict waits up to thirty seconds and reports the holder and activity. An admitted noninterruptible effect retains its OS guard until it finishes even if the run releases its durable lease. Read fingerprints still detect user changes, which do not participate in leasing.

Hosts forward `ToolRuntime::execution_state`, `begin_step` and `end_step` through decorators. Standalone tools without motor lifecycle notifications release their leases after each call. Engine-managed runs retain them until the appropriate state transition. Durable state is checked when renewing or admitting a lease; stale notifications do not revive a terminal run.

The parent can assign file sets with direct graph dependencies; overlaps are persisted and reported to the parent. Large dependency queries report their bound rather than silently producing a complete-looking partition. Without a graph, the low-level workspace API can record an explicit host-provided set.

Worktrees and integration branches live in the host data directory outside the project root. Projects without Git or with submodules use shared-tree mode. Only a parent with explicit task authority can create/checkpoint/remove child worktrees and process the integration queue. Checkpoints stage authorized captured entries through a private index, with repeated content/metadata verification and a ref CAS; unselected files retain their prior staged content. Dirty child worktrees cannot enter the queue.

Integration is serialized and durable: rebase onto the task integration branch, preserve both diffs and intent on conflict, run the host's impacted checks, and fast-forward only the integration branch when checks pass. Failed checks or conflicts return the branch for resolution. Retrying a returned or conflicted entry finishes an in-progress rebase, requires a clean worktree, and atomically adopts the current child checkpoint as the new queue source; the entry is rechecked from there, while a retry without new state is rejected. No successful check is invented for an absent test executor. Project Git hooks, includes and custom execution drivers cannot run through these operations; unsupported custom-driver configuration fails closed. Shared mode does not create commits on the active user branch.

## External changes and lifecycle

`ProjectMonitor` owns a native filesystem watcher and a durable change queue. File notifications hash only affected eligible files; directory/ignore changes and overflow trigger a rescan, and `.git/info/exclude` always reaches the watcher even though the rest of `.git` is never scanned. The host configures the checklist projection paths whose generated sections are canonicalized before semantic comparison. Content the agent produced through `exec` is attributed to Excalibur: changes observed while an exec step is open are not marked external, the closing snapshot records the produced content, and pending rows queued by a scan that raced the step close are corrected. Pending observer work is retried without periodically rereading the whole tree. Changes invalidate scoped task evidence, refresh/remove graph entries and notify a lease holder through a durable directive. Old read snapshots are retained so fingerprint conflicts can show the actual diff.

The host retains one monitor handle while a project is active, inspects `last_error` and calls `stop` when closing it. Queued work remains durable if interrupted. The noninteractive backend performs a final scan, stops the monitor, closes language sessions and shuts down its graph/database resources.

Terminal exit publication runs the spool `sync_all` barrier without holding the output-state mutex. Status, resource accounting and async wait deadlines can still inspect the running session while synchronization is pending; `exited`, exit code and timeout status become visible together after the barrier returns. Synchronization retains the existing best-effort error policy. The deterministic `slow_spool_sync_keeps_status_readable_and_exit_unpublished` backend test blocks that barrier and checks both mutex availability and publication ordering. This establishes the mutex defect and its correction, not the cause of every historical HCC-1 timeout.

## Verification

```sh
cargo test -p excalibur-workspace --test concurrency --test shadow_entries --test typed_checkpoint
cargo test -p excalibur-code-intelligence
cargo test -p excalibur-tools --lib slow_spool_sync_keeps_status_readable_and_exit_unpublished
cargo test -p excalibur-tools --test terminal
cargo test -p excalibur-tools --test tool_retrieval --test terminal_vs_tools
cargo test -p excalibur-tools --test project_tools --test project_monitor --test project_monitor_races --test child_integration
cargo test -p excalibur-models --test deferred --test replay
cargo test -p excalibur-tasks --test verification
cargo fmt --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo build --workspace
```

Backend tests use synthetic repositories, local protocol fixtures, real confined processes, SQLite, OS locks and process interruption. Source graph and language-server tests include real parsers and a separately identified rust-analyzer runtime check on Linux. These checks establish no execution result for Windows or macOS. Tool-retrieval accuracy and terminal comparisons describe their synthetic fixtures, not paid model benchmarks or general performance guarantees. External providers, MCP clients and package installation/creation remain separate capabilities; a host may only register an actually implemented runtime.
