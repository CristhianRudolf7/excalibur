# Scoped and current-run history

When memory is enabled and a worker has a durable memory binding, `history_search` retains the scoped memory adapter's schema and dispatcher. It searches messages and actions only inside the run's memory grant. `run_history_search` always addresses the bounded current-run journal and its archived observation references. Without a memory owner, `history_search` remains the existing journal alias.

A host-only `ToolRuntime::overrides_builtin` hook selects ownership before the first run context is compiled and when scheduling, preparing and executing a call. Decorators forward that hook rather than claiming ownership. Initial seed validation removes duplicate utility names; actual per-run context uses the bound owner's definition. Providing a definition never widens execution authority. `run_history_search` uses the same builtin audit gate and sanitization as its legacy alias.

Masked observations already identify `get_tool_output` with a durable blob hash; they do not direct journal recovery to scoped message history. That unambiguous retrieval hint remains intact.

`cargo test -p excalibur-daemon --test memory_history` exercises both actual dispatch paths with a mock model and local database. The engine utility tests compare the journal alias result with `history_search`. Final verification is pending at this source checkpoint; no full workspace or H11 completion is claimed.
