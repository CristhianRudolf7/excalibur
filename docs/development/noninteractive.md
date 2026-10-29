# Noninteractive execution and evaluation

[Español](noninteractive.es.md)

`excalibur exec` is a thin Rust command over `excalibur-eval`, the shared engine, security, tools, model gateway and telemetry. It creates one root execution with a coordinator role; it does not start a master conversation, HTTP server, scheduler or interactive approval channel. The command never prompts. Missing host-declared mandatory inputs block execution. Operations needing an approval that was not granted by the frozen host policy return a denied tool result so the model can choose an authorized alternative.

```sh
cargo build -p excalibur-cli
# EXCALIBUR_API_KEY is supplied by the host environment; it is never a CLI argument.
EXCALIBUR_MODEL=your-configured-model target/debug/excalibur exec 'Your task' \
  --project /path/to/workspace --json --logs-dir /path/to/fresh-trial
```

`EXCALIBUR_BASE_URL` or `--base-url` selects an OpenAI-compatible Chat Completions endpoint. The default is OpenAI's API. `--model` overrides `EXCALIBUR_MODEL`; `--reasoning-effort` fixes the gateway setting. The host owns credentials and registers the environment credential with the shared redactor before requests or artifacts are created. No credential file is discovered automatically. `--replay` needs no credential and does not initialize a live provider.

## Input, output and limits

Run `excalibur --help` for flags and `excalibur --version` for the compiled version/commit. `--project`, `--autonomy A0|A1|A2|A3`, `--output-schema`, `--json`, `--budget-usd`, `--max-steps`, `--max-tokens` and `--timeout` configure one execution. A cost budget requires a matching host-supplied `--price-table`; unknown prices are never treated as free inference. Costs use integer micro-USD. The gateway's conservative request bound can stop the next call before dispatch when the remaining token/cost allowance is insufficient. See [accounting and replay](replay-pricing.md) for estimate and invoice limits.

`--required-input NAME=VALUE` declares required task data; an empty value blocks before inference. `--criterion FILE` adds a backend `Criterion` JSON (tool success, exact tool-output pointer/value or host check). The default criterion checks a nonempty final response and, when provided, `--output-schema`. This validates delivery syntax, not arbitrary task semantics: declare observable acceptance criteria for stronger verification. Schema-validation failures do not become successful final outputs. The existing bounded gate reports unresolved criteria rather than accepting a completion claim.

| Root result | Exit code |
| --- | --- |
| Completed | 0 |
| Completed with pending criteria | 2 |
| Blocked / missing mandatory input | 3 |
| Paused for budget or wall time | 4 |
| Failed execution / provider failure | 5 |
| Cancelled by SIGINT/SIGTERM | 130 |
| Argument, startup or harness infrastructure error | 1 |

`--json` writes redacted event JSONL as actions are confirmed, followed by a `type=result` record. Without it, stdout receives the final text and stderr reports the reason/log directory. JSONL on disk and the subscription API have durable cursors; client disconnects do not remove persisted events. The engine's fallible durable projection is flushed after admission and before subsequent effects. A failed projection leaves source events recoverable and prevents advancing to an unrecorded effect. A model timeout at the wall-time boundary pauses the run. Cancellation preserves its durable state and stops owned terminal sessions.

Every execution uses fresh state under `LOGS/excalibur`, owner-only on Unix, and refuses an existing nonempty state directory or old primary artifacts. A reservation prevents concurrent reuse. Project-mode logs default to a unique system temporary path and survive process exit; `eval` defaults to `/logs/agent`. This command does not resume an old episode or silently erase it. The existing shared engine/journal recovery APIs remain available to host adapters.

## Evaluation host contract

`--profile eval` selects the binary-frozen `eval@1` policy. It fixes UTC and English by default, permits at most `min(2, task CPUs)` active child adapters, disables persistent memory/user identity, conversation/approval tools, agenda, personal external capabilities, local inference, learning and uploads, and records profile, effective configuration, actual tool and frozen skill hashes. `--task-cpus` records the task allocation, not a CPU resource override. `--task-timeout` uses at most 90% of the supplied task deadline; without it, the declared `--timeout` budget applies. No benchmark timeout or resource multiplier is changed.

The installed route requires an explicit trusted host declaration:

```sh
EXCALIBUR_EVAL_CONTAINER=1 target/debug/excalibur exec 'Task instruction' \
  --profile eval --container-runtime declared-runtime --project /app \
  --logs-dir /logs/agent --task-network none --json
```

The declaration pins observed Linux namespace identities. It is **not independent verification of Docker or Harbor**. The external adapter must supply a fresh isolated container, task resources/network, protected harness installation, read-only existing sensitive project metadata, and no verifier tests during the agent turn. The Linux execution adapter applies Landlock (ABI 3 or later) and seccomp without requiring nested user namespaces or bubblewrap. It never falls back to an ordinary unrestricted shell. `--read-root` and `--write-root` add canonical container roots supplied by the trusted adapter. The running harness executable is protected; install it in its own prefix outside writable task roots. Writable roots are explicit host ceilings disjoint from private state and evaluator paths; granting `/` or overlapping hidden paths fails. Existing protected metadata must have actual read-only mounts. This can require container provisioning beyond installing the binary. Project execution continues to require its validated bubblewrap boundary.

`/tests`, `/logs/verifier`, `/solution`, private harness data and sensitive configuration remain protected. The agent does not search for, run or alter benchmark verifiers. `--task-network none` prevents process network access; `open` uses the externally declared container network ceiling. This is separate from project-net's live proxy; no domain proxy is claimed for eval. The native adapter rejects unsupported domain restrictions. Local mock tests exercise the kernel setup on Linux; they are not an OCI/Harbor trial.

Policy permission and implementation availability are separate. The actual catalog includes file/terminal tools, episode-only `task_db`, clock/status, tool search/call, current-run history search, bounded observation pins, continuation/directive utilities and immutable programming-skill search/load. The small task scratchpad supports scoped records, versions, atomic batches and idempotent receipts; it does not implement full project planning. Pins retain at most eight observations within a 4 KiB serialized budget; eviction is deterministic by source sequence.

The complete planner/verifier, subagent orchestrator, full skills system, code/document/media/web adapters and alternate ACP/external connectors belong to their own implementation scopes. Their admissible policy states and child ceiling are recorded, while unregistered tools are absent from the real catalog and rejected if requested. Availability metadata identifies these dependencies explicitly. This follows the architecture's rule deferring verification of capabilities not yet constructed; it does not claim that a policy row implements a capability. A registered web adapter must use the task network signal and source restrictions; the current decorator rejects known benchmark-source requests before delegation. No web adapter or discovery of arbitrary third-party solution mirrors is claimed here.

`--memory-store`, an undeclared skills store or `--credentials-store` aborts evaluation startup. The shipped programming catalog is immutable. Optional task skills require an explicit `--profile-override` declaring `/runtime/skills`; the loader bounds and hashes that snapshot without installing or learning skills. `--profile-override FILE` is strict JSON: supported language, same-model role declarations and task-skill mount overrides become recorded configuration/ATIF deviations. A different role gateway requires a registered host adapter and is rejected by this single-model command. Security denials and ceilings cannot be overridden by workspace content.

## Artifacts and connection

`/logs/agent/trajectory.json` and `agent-context.json` are primary outputs. `/logs/agent/excalibur` contains the databases, redacted durable-journal export, events, metadata, metrics, resource samples, optional OTLP JSON and per-run exports/manifests. Confirmed exchanges also produce `replay.json`; incomplete/no-response recordings instead receive an explicit `replay-export.json` status. Files are published incrementally, so an interrupted episode has valid partial artifacts. See [telemetry](telemetry.md) for mixed-generation recovery and the pinned official ATIF/AgentContext validator.

`--trial-id`, `--task-id`, `--task-digest`, `--benchmark` and `--harbor-version` carry host-supplied trial identity. Metadata also records model/provider, reasoning setting, configuration and catalogs, compiled source commit/dirty status and environment. Missing benchmark identity, tariffs or resource counters remain unknown. A dirty binary or incomplete external trial metadata is not evidence of a comparable benchmark result. Verifier reward/output are attached only through `Telemetry::record_verifier` by the trusted external adapter after the agent turn; the agent never reads verifier files. Nothing uploads results.

`--cgroup DIR --cgroup-scope task_children|task_container|shared` supplies an existing delegated cgroup v2. The native launcher joins a configured group before user code; failure prevents that command. The sampler distinguishes daemon RSS/CPU from group memory peak, CPU, PIDs and OOM counters. No cgroup is created and no limit changed. Without a usable exclusive group, child/container counters remain explicitly unavailable or shared. Kernel-native OOM experiments and Windows/macOS execution are not claimed; platform-specific resource module compilation is documented separately.

The user supplies the concrete Harbor installed-agent adapter: install/configure the binary and container contract, pass the literal instruction and declared metadata/budgets, invoke `exec`, collect the exit code/ATIF/AgentContext, then attach verifier results. No ACP server or Harbor adapter is installed by this work.

`--seed` and `--temperature` are explicit optional host sampling requests; model support and determinism are not assumed. Their requested values and reasoning effort enter metadata and the compiled context, so a replay with changed sampling stops at divergence. `--metadata FILE` supplies structured host trial metadata, including optional requested sampling. See the provider limits in [accounting and replay](replay-pricing.md). A model can report a missing mandatory datum through `status` with `action=block` and a reason; the engine confirms that result and blocks without another inference call.

Failed installed executions also publish `excalibur/failure.json`, with a safe internal class and an `exception_type` label from [Harbor 0.25.0’s pinned error policy](https://github.com/harbor-framework/harbor/blob/509b119db0d97b141dcd9c80f4f7ab861c47c2d6/src/harbor/models/trial/error_policy.py). Quota, rate limit, overload, context-window and authentication failures are distinct when the provider supplies a recognized code/status. Unknown cases remain conservative. The external adapter consumes this descriptor; the Rust command does not raise Python exceptions or retry quota errors.

## Backend verification

```sh
cargo test -p excalibur-cli -p excalibur-eval -p excalibur-telemetry -p excalibur-models
cargo test -p excalibur-engine --test event_projection
cargo fmt --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo build --workspace
```

Tests use synthetic tasks, local HTTP/SSE fixtures, real temporary file/process effects, scoped database checks, actual Linux kernel boundaries, controlled signals and process interruption. They cover denied actions returning to the model, output schemas and criteria, exit codes, fresh trials, profile/store rejection, source guards, incremental recovery, replay and resource accounting. Providers remain mocked. No paid model, model-service credential, GPU, service deployment or full benchmark is used.

Verification on the frozen source tree passed all four workspace checks with 338 passing tests and zero failures; eleven ignored subprocess helpers are invoked by active parent tests. A bounded installed-command check used three local mock HTTP responses, a real file write and confined command, and a checked final schema. The pinned official validator accepted its actual artifacts together with the reference fixtures: five ATIF, four AgentContext and three OTLP documents. This is local synthetic validation, not a benchmark or OCI deployment.
