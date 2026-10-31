# Owner identity and sessions

The shared `excalibur-identity` library implements the owner identity and session contract of architecture sections 20.12 (owner access and entry gate) and 20.13 (multiple users and teams), with the 20.14 representation primitives (per-principal authority objects) and the 20.15 reference-vulnerability mitigations that belong in the credential layer. It lives in the shared core (`crates/identity/src/`), not in the CLI or HTTP API. Tests use synthetic principals, locally generated device keys and an injected manual clock; they never sleep, use the network, call a paid API or store a real secret.

## Account registry

- The first principal (the owner) is created exactly once over the host-attested local channel (`LocalChannel::host_attested`); any later bootstrap attempt is a conflict that leaves the registry untouched.
- The owner mints further principals with isolated data namespaces (`AuthenticatedSession::data_namespace`, one stable namespace per principal). Only the owner creates or disables principals, and disabling revokes every session of that principal.
- Passwords are verified with argon2id (OWASP defaults: 19 MiB, 2 iterations, parallelism 1; configurable). Unknown users hash against a per-process dummy salt with the same cost, so wrong-password and unknown-user failures are indistinguishable (`Unauthenticated` either way). A successful login against changed costs transparently rehashes.
- Password changes require the current password and revoke every other session of the principal.

## Devices and sessions

- Each device enrolls a 32-byte Ed25519 public key (local channel after a password check, or remote pairing). Every login additionally proves possession of the device key: the client signs `device_proof_message` over a fresh single-use challenge, and the challenge is consumed whatever the outcome, so a captured proof never replays.
- Successful logins open device-bound sessions with an absolute lifetime and a sliding idle limit that never extends past it. Bearer tokens (`exs1.<id>.<secret>`) carry a 256-bit secret of which only the SHA-256 digest is stored; lookup is by id and the comparison is constant-time. Unknown, malformed, expired and revoked tokens deny identically.
- Every per-principal operation takes an `AuthenticatedSession` that can only be produced by verifying a live token, and the session row, its device and its principal are revalidated inside the same storage transaction that reads or changes data, so a revocation that commits first always wins.
- Login attempts are rate-limited per username with pessimistic counting before hashing, so concurrent guesses cannot exceed the limit; success clears the counter.

## CSRF and pairing

- Each session carries single-purpose CSRF tokens (`exc1.<id>.<secret>`, bounded per session). Verification is bound to the issuing session and its expiry; a token from another session, or a session bearer token presented as CSRF, denies.
- Remote enrollment uses short-lived single-use pairing codes (Crockford base-32, case/space/hyphen tolerant) redeemed with a host-supplied source key. Failures are rate-limited per source and globally, and the limits also block correct codes. Consumed and expired codes deny without enrolling anything.

## Teams, quotas and projects (§20.13)

- A team has exactly one host. The host (and, for member roles, admins) issues single-use invites; admins can never invite admins, and an invite whose issuer lost the entitlement no longer admits anyone. Non-members cannot distinguish a foreign team from a missing one (`NotFound` either way).
- The host shares providers (always with a host vault reference, never a secret value) or tools, and configures per-member or team-default quotas per resource and time window. Members admit usage against a quota with an idempotent `request_key`: a replay with identical parameters returns the original admission without charging again, while different parameters conflict. Settling below the reservation refunds the difference and is idempotent per value.
- Members never receive host credentials or their references: member views expose only kind and name, and the host proxy ticket (`HostProxyTicket`) resolves the credential inside the host vault through `with_host_credential`, whose closure must not return or publish the borrowed bytes. The ticket is neither `Clone` nor `Serialize` and its `Debug` is redacted.
- Shared projects need an explicit per-member grant (`read` or `write`, with `write` implying `read`); the host owns its projects without a grant row. Grants and revocations are host-only.

## Representation (§20.14)

Cross-agent representation policy itself (shareable categories, authorized purposes, audit of each exchange) belongs to the permission engine in 14.11 and is not re-implemented here. This crate supplies the authority primitives that policy consumes: unforgeable per-principal sessions, team roles, project grants and host-only proxy tickets.

## Reference vulnerabilities (§20.15)

| Reference incident class | Mitigation in this layer |
|---|---|
| Local gate without authentication, permissive CORS (OpenCode CVE-2026-22812) | Tokens mandatory, bearer values never travel in URLs, per-session CSRF tokens; gateway `Origin` checks and TLS remain host responsibilities |
| Forced cross-site upgrade (OpenCode GHSA-632h-h47v-g4x4) | State-changing calls require a live session plus its bound CSRF token |
| Gateway token leaked through a link URL (OpenClaw CVE-2026-25253) | Bearer values contain no URL metacharacters and never parse back from a query string; links carry no authority in this crate |
| Terminal reached from the web UI (OpenCode CVE-2026-22813) | Sessions are capabilities without ambient privilege; the UI would need its own approval policy on top |
| Sandbox widened by arguments, indirect execution, symlinks (Codex CVEs, Overpatch) | Out of scope here; covered by the sandbox and path-authority layers |

## Limits

This is a library, not a running gate: HTTP wiring, `Origin` checks, TLS termination, secure-input UI and remote-transport pairing delivery are the host's responsibility and are not implemented here. Daemon, master and gateway authentication call paths are owned by another lane and are deliberately untouched. Audit-chain verification of identity rows (20.11) and vault redaction search (20.10) are covered by their own components.

## Verification

`cargo test -p excalibur-identity` covers allow/deny authentication, scope isolation between principals, device-proof single use, idle/absolute expiry, revocation, CSRF binding, single-use expiring rate-limited pairing, keyed admission idempotency with quota enforcement, host-only credential resolution, explicit project grants, token URL hygiene and reopen persistence of sessions, teams and codes. All fixtures are synthetic and local with a manual clock.
