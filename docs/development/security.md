# Mandatory security

[Español](security.es.md)

Security belongs to the shared backend and host adapters. External content is data with provenance, never authorization. `ContextInput::untrusted` carries an explicit source and content; compilation serializes both in a data envelope. Tool messages receive an untrusted provenance annotation without changing their protocol role or tool call identifier. These markers support policy enforcement; they do not establish that a model will resist prompt injection.

Local file mutations reject sensitive configuration paths during preparation and again at publication, including `.git`, `.agents`, `.codex`, `.excalibur`, credential directories and CI configuration. A general write permission does not remove this restriction. Canonical root checks and existing fingerprint/lease checks still apply.

Backend evidence: `cargo test -p excalibur-context --test trust` and `cargo test -p excalibur-tools --test protected_paths`. Tests use synthetic content, temporary files and no model services or real credentials.

## Host contracts

`excalibur-security::Security` uses the principal database to register a frozen `RunSecurity` base, append grants and record decisions. The host supplies a ceiling, canonical workspace, parent, project and explicit autonomy settings. The most restrictive setting wins; full autonomy requires explicit acknowledgement. Denials dominate questions, which dominate allows. A child and subsequent grants remain bounded by the parent's effective token and the host ceiling. Protected vault/evaluation paths are host inputs, never model configuration.

Approvals identify the exact action and preserve request identity across channel retries. The host supplies authenticated responses and explicit time; task/project scopes and expiration are durable. Host adapters drive reminders and expiry checks; these APIs do not constitute a scheduler or messaging service. `SecuredTools` is the authorization boundary around a `ToolRuntime`; embedders must use the same redactor for the security runtime, local tools and vault.

`excalibur-vault` stores authenticated ciphertext with a random master key protected by an explicit operating-system key-store adapter. `SecretValue` has no serialization contract and redacts its debug representation. `secret://` references and metadata are the agent-facing contract. Secure client input and native key-store availability belong to the host. Missing native protection fails closed; tests inject a synthetic key store. The shared live `Redactor` filters registered exact values and supported credential patterns. This is not detection of arbitrary encodings or unknown secret formats.

## Vault

`excalibur-vault::Vault` is a synchronous host adapter; async hosts invoke it on a blocking lane. `open(path, Arc<dyn MasterKeyStore>, key_id)` accepts an explicit OS protection adapter; `open_native(path, namespace)` selects the native implementation. There is no plaintext fallback. A missing master for existing ciphertext, failed native protection or an authentication failure returns a typed error without including input values or underlying credential-store errors.

The vault persists XChaCha20-Poly1305 ciphertext using a random 32-byte master and a fresh random 24-byte nonce per update. Each record authenticates its metadata as associated data, binding the value to its reference, kind, name, usage and optional website URL. Writes use a same-directory temporary file, file synchronization and atomic replacement; Unix also synchronizes the parent directory. An interprocess file lock serializes reload/change/publication across handles. New Unix directories use mode `0700`; vault and lock files use `0600`. Hosts protect the vault directory and native protected-master files from tool access and configure appropriate permissions on pre-existing directories. There is no claim of filesystem rollback resistance, protection from a compromised host process or forensic erasure of old filesystem snapshots.

| API | Host contract |
| --- | --- |
| `SecretRef::parse("secret://name")` | Validated reference, safe for model-facing metadata. |
| `SecretMetadata` / `SecretKind` | Name, reference, use, type and optional public website URL; API key, website password, host/IP, token, SSH key and model-provider credential are represented. |
| `SecretValue::new(bytes)` | Secure client input only; owned memory is zeroized, debug is redacted, and there is no serialization/deserialization implementation. A value contains 1–65,536 bytes. |
| `Vault::list()` | Authenticated metadata only; it never returns secret values. |
| `Vault::put(metadata, value)` | Host-side creation/update. Tool/chat JSON must never supply the value. |
| `Vault::with_secret(reference, closure)` | Trusted executor resolves a reference only when executing. The host records an audit event containing the reference and must not publish the closure's borrowed bytes to context, journal, telemetry or blobs. |
| `Vault::delete(reference, DeleteApproval)` | The host authenticates user approval before constructing the reference-bound capability. Wrong-reference approval fails. |
| `Vault::redactor()` | Clone of the live shared redaction registry. Use this same registry for security, tools, streaming and persistence boundaries. |

`Redactor` returns sanitized copies through `redact_text`, `redact_bytes` and recursive `redact_json`; `contains_secret` also supports rejection of unsafe preimages. Exact byte matches are merged with known GitHub, API-key, AWS access-ID, authorization-header, credential-assignment and private-key patterns. Valid standalone `secret://` references remain readable. Old values stay registered in process memory after an update/delete so pending output is still sanitized; deleted values are absent from persistent vault storage. Restarting a vault registers its current values again.

`Redactor::stream()` has `push(bytes)` and `finish()` and shares live registry updates. It emits safe prefixes immediately, including prompts without newlines, and retains only potential credential prefixes and exact-value tails across chunks. Exact matching processes 64 KiB chunks and retains at most a secret-length tail; identified credential values and PEM bodies are suppressed with bounded state instead of buffering a whole line. Large nonsecret output is preserved byte for byte. Arbitrary encodings, unknown formats and secrets unknown to the registry are outside the detector's guarantees.

| Native protection | Implementation and evidence |
| --- | --- |
| Linux | Explicit Secret Service adapter through `keyring-core` and `zbus-secret-service-keyring-store`, with encrypted bus transfer and no mock selection. Compiled on Linux; real service not opened in tests. |
| macOS | Native Keychain adapter. Cross-compilation check passed for `aarch64-apple-darwin`; no native runtime test. |
| Windows | Current-user DPAPI protects the master in a separate ciphertext file; interactive UI is forbidden. Cross-compilation check passed for `x86_64-pc-windows-gnu`; no native runtime test. |
| iOS | Protected-data Keychain adapter; application entitlements and runtime availability belong to the host. Implementation was inspected; target/runtime not tested here. |
| Android | Android Keystore-backed named store. The application must initialize `ndk-context` before construction. Implementation was inspected; target/runtime not tested here. |

The backend command `cargo test -p excalibur-vault` passed eight synthetic tests: encrypted CRUD/reopen and file permissions; ciphertext/nonce/metadata tampering; nonce freshness and missing/wrong master; 64 concurrent writes through eight independent handles; secure-input/reference validation; exact/pattern/JSON/binary redaction; all stream chunk boundaries including multiline secrets/PEM; and preservation of 2 MiB safe unterminated output, immediate prompts, live registry updates and bounded suppression of a 1 MiB credential value. `cargo clippy -p excalibur-vault --all-targets -- -D warnings` passed. No test reads real credentials or opens the native key store.

The cryptographic and adapter contracts were checked against [RustCrypto's AEAD documentation](https://docs.rs/chacha20poly1305/0.10.1/chacha20poly1305/), [keyring-core](https://docs.rs/keyring-core/1.0.1/keyring_core/), [Secret Service's adapter](https://docs.rs/zbus-secret-service-keyring-store/1.0.1/zbus_secret_service_keyring_store/), [Apple's adapter](https://docs.rs/apple-native-keyring-store/1.0.0/apple_native_keyring_store/), [Android's adapter and initialization requirement](https://docs.rs/android-native-keyring-store/1.0.0/android_native_keyring_store/) and [Microsoft's DPAPI contract](https://learn.microsoft.com/en-us/windows/win32/api/dpapi/nf-dpapi-cryptprotectdata). This crate supplies the host boundary; it does not implement a client secure-input interface or browser forms.

## Linux process boundary

`TerminalConfig::sandbox` is mandatory for execution. The trusted host fixes the workspace and grants canonical read/write roots; model arguments cannot select a different working directory. Per-run boundaries narrow the host configuration and are reapplied from effective capabilities at execution. File tools, process execution and approval decisions are distinct checks; an `allow` decision does not disable the kernel boundary.

The Linux adapter uses verified bubblewrap, separate namespaces, a private temporary filesystem and a sealed seccomp filter. It never falls back to an ordinary host shell. Filesystem masks protect existing sensitive metadata and configured hidden paths; host application data and the encrypted vault must not become command input. The host supplies the same protected paths to `Security::with_protected_paths`, `ToolPolicy::protected_paths` and the process configuration. File searches prune these paths before opening or matching files.

The writable `ProjectRestricted` and tool-package profiles use a host-prepared directory hierarchy. `prepare_project_boundary` is an explicit host operation, not an agent tool or an automatic action of `exec`. Directory creation and renaming inside these writable profiles are denied. This supports existing-file/regular-file work within the prepared hierarchy, not unrestricted build/install commands. Read-only and file-analysis profiles retain private temporary storage. Unsupported profiles and unavailable confinement return explicit errors. Domain-proxy networking, a general writable project profile and an alternative Landlock-only runtime are not supplied by this adapter. The evaluation/container boundary belongs to the separate evaluation scope.

Kernel isolation must be tested on the actual host. Namespaces, seccomp and read-only bind mounts have different roles; the presence of a Landlock ABI alone is not evidence that a complete fallback policy exists. See the [bubblewrap security contract](https://github.com/containers/bubblewrap/security), its [namespace and seccomp documentation](https://github.com/containers/bubblewrap/blob/main/README.md) and [Linux Landlock limitations](https://www.kernel.org/doc/html/v6.8/userspace-api/landlock.html).

### Secret executor

`excalibur-security::SecretExecutor::new(security, Arc<Vault>)` binds authorization to the vault and rejects different redaction registries. `execute(run, operation_id, reference, now, operation)` accepts a stable host effect key, explicit time and a trusted blocking closure returning `ToolResult`. It validates the exact `UseSecret` reference against current capabilities and rules, obtains or reuses a full-plan admission, claims any applicable single-use approval and durably claims dispatch. It records secret taint and reference-only audit **before** resolving the vault value or entering the closure. The closure borrows the bytes only for the actual authorized environment/header/form operation. This API is for trusted backend adapters; it never supplies a secret value to agent tool JSON.

The executor redacts returned inline output before caller delivery and durable completion, marks that output untrusted and rejects unchecked `full_output_blob` references. While current authorization and its admission remain valid, a completed retry returns its sanitized receipt without executing the closure again. Expired admissions or revoked authority fail closed; they do not repeat the effect. Concurrent or interrupted claims without a completion receipt fail closed and require external reconciliation; they are never automatically replayed. A failed authorization, taint write or audit write cannot enter the secret closure.

Secret permission supplements authorization for the outer effect. The host must separately authorize its actual network destination, external action or other mutation before executing it. This helper independently claims the secret-use admission; it does not delegate a previously consumed `SecuredTools` single-use ticket. It does not implement a network client, secure client input UI or browser forms, and the trusted closure must not independently log or publish raw borrowed bytes.

`cargo test -p excalibur-security --test secret_executor` covers exact-reference denial, registry binding, redacted returned/persisted output, audit and taint before a deterministically held closure, concurrent duplicate dispatch, single-use approval/channel retries, cached completion and interrupted claims without automatic repetition. All fixtures use a synthetic key store and local backend data, without model calls or real credentials.

## Authorization and verification

`Security::request_grant` binds the proposed capability payload and expiry to its approval; an approval for another action cannot grant it. `answer_approval` is an atomic, authenticated host operation. Once approvals are claimed at admission/dispatch and cannot authorize another call. `renew_approval` requires a new authenticated instruction. `approval_maintenance(now)` reports the 24-hour reminder and seven-day expiry; the host runs this maintenance and `verify_audit()` periodically. No scheduler, channel or notification daemon is started by these libraries.

`SecuredTools` rechecks current rules, inherited grants and taint at dispatch. It binds an admission to the complete prepared plan, records whether dispatch began, and caches a redacted completion. A late approval wait therefore retains the intention without guessing that a non-idempotent effect ran. The engine pauses without another inference call or a fabricated result. Reconciliation distinguishes proven non-dispatch, confirmed completion and genuine uncertainty. Capability `budget` bounds total admitted tool calls and is intersected with the engine's budget. Settings, sandbox-disable and raw-vault tool names remain absolute denials.

Bash risk classification uses an AST plus normalized command words, including pipelines, substitutions and redirects. Computed/unknown commands and unsupported shell syntax require approval. Terminal input is authorized as exact input, rather than assuming keystrokes are harmless. External input and file/search results mark provenance; a generic initial user request does not authorize a new risky action arising from those data. Secret use is audited by reference and taints the run before executor dispatch. Outgoing network/external adapters still authorize their own action as well as secret use.

Tool JSON is redacted before persistence, file snapshots containing detected secrets are rejected, and terminal output is filtered before spooling and again when publishing a prefix. Old blob retrieval is inspected over the complete verified decode stream before any selected bytes are returned or encoded. This remains one O(total size) pass with bounded memory; policy denial is distinct from storage corruption. Audit rows are append-only and hash chained; complete verification has no query-page limit. Without an external anchor, this is not protection from a privileged attacker who rewrites the database and recomputes the entire chain.

| Contract | Backend evidence |
| --- | --- |
| Capability limits, inheritance, exact grants, expiry and lower ceilings after restart | `security/tests/policy_adversarial.rs`, `secured_engine.rs` |
| Conflicting rules, sensitive paths, dangerous/unknown commands and terminal input | Policy/engine tests; `tools/tests/protected_paths.rs` |
| Approval pause, resumption, once-only effects and secret-free durable events | `security/tests/secured_engine.rs`, `secret_executor.rs` |
| Secret encryption, CRUD, concurrency and stream boundaries | `vault/tests/vault.rs` |
| Audit mutation, all-row chain verification and redaction | `security/tests/policy_adversarial.rs` |
| Real Linux escape prevention, sockets, seccomp, descendants and narrowed roots | `tools/tests/sandbox.rs`, previous `terminal.rs` process tests |
| Provenance and protected files/searches | `context/tests/trust.rs`, `tools/tests/protected_paths.rs` |

Paths are relative to `crates/`. Full checks from the root are `cargo fmt --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace` and `cargo build --workspace`. Providers remain mocked. Tests do not use real credentials, inference, GPU or paid services. Client authentication, secure-input UI, ingress/CSRF, multi-user routing, schedulers, ingestion scanning and evaluation integration belong to their separate application scopes; these libraries expose explicit host boundaries rather than claiming those interfaces exist.

On the checked Linux x86-64 VPS, the configured runtime is bubblewrap 0.13.1 at `/opt/excalibur-toolchain/bubblewrap-0.13.1/bin/bwrap` (`/usr/local/bin/bwrap` points there). The adapter requires a trusted, non-setuid executable outside the workspace and an upstream version containing the verified security fix (at least 0.12). The [official 0.13.1 source archive](https://github.com/containers/bubblewrap/releases/download/v0.13.1/bubblewrap-0.13.1.tar.xz) was verified against SHA-256 `0e85c8702248b8dd64144f594147553650cdd0e4c951690345faace79c217c4d` before building. Meson, Ninja and libcap development headers are build prerequisites; no source archive or runtime binary is part of the product Git repository. Execution roots currently remain inside the fixed workspace, in addition to the adapter's read-only system runtime mounts; user-managed toolchains outside that boundary are not mounted automatically.

Workspace evidence: 248 passing backend tests, zero failures and six ignored subprocess helpers invoked by their parent tests. This includes 47 existing provider tests, all previous storage/engine regressions and 41 added security checks. Linux namespaces/seccomp and filesystem/process effects are real. The current terminal integration has no verified Windows/macOS runtime or complete cross-platform workspace build. The vault's isolated cross-compilation evidence above is separate. Unsupported Linux profiles remain explicit pending capabilities; passing checks does not claim those profiles exist.
