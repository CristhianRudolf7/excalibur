# Landlock fallback policy

[Español](landlock-fallback.es.md) · [Security contracts](../development/security.md)

Status: accepted. This decision concerns the Linux process adapter in architecture section 20.6; it does not change the portability of the shared Rust core.

Project execution requires its configured, validated bubblewrap runtime on Linux. If that runtime is absent, unusable or cannot establish the requested boundary, execution is denied before the user command starts. Landlock is not selected automatically. There is no implicit host-network or unsandboxed fallback.

## Evidence and rationale

Landlock is an unprivileged, additive kernel access-control mechanism. Rules grant handled rights beneath filesystem objects; grants in one layer combine, while stacked layers must all allow an operation. Rules do not provide a negative pathname exception underneath a broadly writable grant. Existing open descriptors also require separate treatment. These are relevant properties of the [kernel's Landlock contract](https://docs.kernel.org/userspace-api/landlock.html#layers-of-file-path-access-rights).

| Capability | Upstream ABI | Consequence for this adapter |
| --- | --- | --- |
| Cross-directory rename/link control | 2 | A fallback must preserve directory moves without widening access. |
| File truncation control | 3 | Write protection also needs truncation protection. |
| TCP bind/connect ports | 4 | Port controls cannot enforce a domain allowlist on their own. |
| Abstract UNIX sockets/signals scoping | 6 | Earlier ABIs need another boundary for these interactions. |
| Pathname UNIX socket resolution | 9 | Earlier ABIs do not supply this filesystem right. |
| UDP bind/connect/send ports | 10 | Earlier ABIs require separate UDP restrictions. |

The ABI requirements come from the [kernel UAPI definitions](https://github.com/torvalds/linux/blob/master/include/uapi/linux/landlock.h). The [Linux 6.8 documentation](https://raw.githubusercontent.com/torvalds/linux/v6.8/Documentation/userspace-api/landlock.rst) also describes runtime ABI discovery and its TCP-only network policy. These kernel contracts do not establish an automatic project fallback.

Excalibur's writable project contract permits ordinary directory creation/rename while protecting host-provisioned root configuration, existing nested metadata and hidden vault/data paths. New reserved names inside newly created directories do not acquire authority over the harness: its trust root is pinned. Bubblewrap does not implement a negative filename glob for every future directory. A broad Landlock project grant would include the existing sensitive descendants too; translating the current mount exceptions requires a deliberate layout or broker. A narrower Landlock design is possible, with descriptor hygiene, seccomp and a separate enforcing network boundary. This is an application-design conclusion, not a claim that Landlock cannot build useful sandboxes.

A read-only fallback over a host-provided, isolated input set remains a candidate. It is not enabled merely because the ABI query succeeds: its supported profile, excluded data, inherited descriptors, process/IPC boundary and network behavior must first be specified and tested. The upstream Rust binding provides [safe syscall abstractions](https://github.com/landlock-lsm/rust-landlock/blob/main/README.md); adding that dependency alone would not satisfy these conditions.

## Verification and reconsideration

On the checked Linux x86-64 environment, kernel `6.8.0-147-generic`, the read-only capability query `landlock_create_ruleset(NULL, 0, LANDLOCK_CREATE_RULESET_VERSION)` returned ABI `4`. The capability query does not install a policy or establish confinement. Bubblewrap reported `0.13.1`. These observations establish available mechanisms, not proof of a Landlock fallback.

`cargo test -p excalibur-tools --test sandbox_adversarial missing_runtime_never_selects_an_unimplemented_fallback` verifies missing-runtime rejection with temporary synthetic configuration and a marker command. It requires no working bubblewrap runtime and does not weaken the test when Landlock is available. The existing sandbox suite separately exercises real configured kernel confinement.

Reconsider this decision when a named fallback profile can prove the same applicable guarantees, detects its required ABI at runtime and fails closed on missing features. Add real negative filesystem, descriptor, process/IPC, TCP/UDP and bypass tests before enabling it. Until then, the explicit denial behavior satisfies the unavailable-sandbox policy; it does not claim an automatic project fallback or runtime coverage of untested platforms.

The explicitly selected [evaluation container profile](../development/noninteractive.md) uses its own host-attested Landlock/seccomp contract. It is not an automatic fallback for project execution. Its kernel tests and required container provisioning are documented separately.
