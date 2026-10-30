# Provider errors, backoff and circuit breaker

The shared `excalibur-models` library implements the provider-error contract of architecture section 19.5 (finding AUD570-11): bounded retries with exponential backoff and jitter, `Retry-After` handling, a circuit breaker and failover to a role's fallback route. It lives in the shared core (`crates/models/src/resilience.rs`), not in the CLI or HTTP API. Tests use local mock HTTP servers and an injected manual clock; they never sleep, use the network or call a paid API.

## Shared state

`ProviderResilience` holds the retry policy, every breaker and a bounded evidence log behind thread-safe locks. `ProviderResilience::process()` returns the single process instance (default policy, real clock); `install_process` may configure it once before first use. Every gateway that receives the same `Arc` (for example the master's streaming gateway and worker gateways) shares the same breakers. `ProviderResilience::new` builds an isolated instance with an injected `ResilienceClock` (monotonic time, wall time for HTTP-dates and a sleeper).

`OpenAiGateway::with_resilience` opts a gateway in. This is additive: without it the gateway keeps its previous single-attempt behaviour, and opting in changes neither the wire request, the gateway identity nor the configuration hash.

## Retries

- Retryable: transport failures, HTTP 408, 429 (rate limit), 5xx except 501/505, and the classified `overloaded` and `timeout` categories.
- Not retryable: other 4xx including 401/403 (authentication/permission), quota exhaustion (429 with a spend/credit code), context-window errors, output validation, truncation and stream-consumer failures. These are returned unchanged.
- A retry resends the identical request body. The compatible gateway has no idempotency-key header, so none is sent; the mock verifies the bodies are byte-identical.
- Default policy: 2 retries, exponential backoff from 500 ms capped at 8 s with equal jitter (uniform in [d/2, d]).
- `Retry-After` is honoured as delta-seconds or HTTP-date (IMF-fixdate, RFC 850 and asctime, relative to the injected wall clock; past dates mean now). The provider-specific `retry-after-ms` header takes precedence when valid. The server delay is used exactly, without jitter. A requested delay above `max_retry_after_ms` (default 60 s) is not waited for: the call fails, so failover can act, rather than retrying sooner than the server allowed.
- Streams retry only while the stream is being opened. Once a delta has reached the sink, a failure is reported and never silently restarted.
- Each retried attempt is recorded in `ModelCallMetrics.attempts` with its classified failure and no reported usage, and counts in `provider_calls`.

## Circuit breaker

Defaults follow section 19.5: the breaker opens after 5 consecutive availability failures, or when failures are strictly above 50 % of the outcomes in a sliding 1-minute window. It stays open for 60 s, then admits exactly one half-open probe. A successful probe closes it; a failed probe reopens it for another 60 s. A cancelled (dropped) probe releases its slot without deciding. While open, no request is sent and the call fails with the new `FailureClass::CircuitOpen`.

The specification does not set a minimum sample for the rate rule; this implementation requires 10 outcomes in the window (`min_samples`), so one or two early failures cannot open it.

Availability failures are retryable failures plus non-retryable 5xx and quota exhaustion. Client errors, authentication/permission failures and output validation do not count against the breaker.

The specification keys the breaker per provider, and that is the default (`BreakerScope::Provider`). `BreakerScope::ProviderModel` is available when a host prefers one breaker per provider and model. The task brief asked for per provider/model; the specification wins as the default and the difference is explicit.

`snapshot` returns the state, counters, remaining open time and how many times it opened; `events` returns retries, `Retry-After` caps, opens, half-open transitions, closes, rejections and failovers.

## Failover

`FailoverGateway` holds a role's ordered routes (primary plus up to 8 fallbacks, the same bound as `RoleConfig`). A request moves to the next route when the current one is rejected by its open breaker or fails with an availability failure after its own bounded retries. Client errors, authentication and validation never fail over. A stream that already delivered a delta is never restarted on another route. Each route renders its own wire format, so a fallback of another family receives its own rendering; the conversation cache is lost, as the specification accepts. The returned metrics identify the route that answered and include the earlier routes' failed attempts and provider calls.

## Error hygiene

Only closed failure categories and HTTP status codes reach callers, metrics and evidence. `excalibur-providers` now captures the `Retry-After` and `retry-after-ms` header values (at most 64 visible ASCII bytes) through the additive `native_chat_detailed` and `stream_native_chat_detailed` methods; the existing `Error` type and methods are unchanged. A test sends secret-looking text in an error body and in a malformed `Retry-After` header and checks that neither appears in errors, metrics, events or snapshots.

## Limits

- Not wired into hosts yet: the daemon, master, engine and CLI still build `OpenAiGateway` without `with_resilience`, and no host builds a `FailoverGateway` from `RoleConfig`. Building fallback gateways needs host credentials and configuration that this change does not add.
- Content errors (refusal, truncated output) keep their existing handling: one validation retry with the error, otherwise reported. A larger output limit on truncation is not implemented.
- Breaker state lives in memory for one process and is not persisted across restarts.
- Usage of failed attempts is recorded as unreported; providers that bill failed requests are not reconciled.
- Verified on Linux with local mocks only.

## Verification

```sh
cargo test -p excalibur-models --test resilience
cargo test -p excalibur-models
cargo test -p excalibur-providers
cargo clippy -p excalibur-models -p excalibur-providers --all-targets -- -D warnings
```
