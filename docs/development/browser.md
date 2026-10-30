# Browser environment

`excalibur-cdp` implements asynchronous CDP over native pipes. `excalibur-browser`
launches and supervises Chrome instances, verifies renderer sandbox state and
applies version policy. `excalibur-webd` exposes one-shot environment and instance
diagnostics. `excalibur-search` and `excalibur-web-adapter` remain empty boundaries.
There are no application sessions, RPC server, browser tools or viewer frontend.

Install a stable Rust toolchain with rustfmt and clippy, a C/C++ compiler,
`pkg-config`, OpenSSL development headers and a verified bubblewrap build (at
least 0.12). Ubuntu compiler packages are `build-essential pkg-config libssl-dev`;
use the verified bubblewrap 0.13.1 archive, checksum and installation path in
[Security](security.md). Building it requires `meson ninja-build libcap-dev`;
existing sandbox tests also use `curl` and local C/Rust compilation. Install Google Chrome stable from its
official Linux package. Keep Chrome's sandbox enabled; never use `--no-sandbox`
and never disable the system's user namespace restrictions globally.

An administrator prepares a dedicated identity and private writable directories:

```sh
sudo useradd --system --user-group --home-dir /var/lib/excalibur-web --shell /usr/sbin/nologin excalibur-web
sudo install -d -o excalibur-web -g excalibur-web -m 0700 /var/lib/excalibur-web /run/excalibur-web
```

Skip user creation if the identity already exists. Recreate the runtime directory
after reboot. No service installation or deployment is required for these checks.
For development, `os_user = "nobody"` is supported with separately prepared
writable directories. An ordinary non-root caller keeps its own identity.

Supply a dedicated, trusted configuration file (not the host configuration):

```toml
[web]
os_user = "excalibur-web"
instance_class = "ephemeral"
chrome_args = []
chrome_binary = "/usr/bin/google-chrome-stable" # optional: otherwise probe known Linux paths
data_dir = "/var/lib/excalibur-web"
run_dir = "/run/excalibur-web"
disk_reserve_bytes = 5368709120
search_budget_bytes = 2147483648
```

All paths must be absolute. Unknown settings are rejected. The free disk reserve
cannot be reduced below 5 GiB; the default search budget includes a 1.5 GiB index
and 0.5 GiB state. Data and runtime directories must already be writable by the
final identity, owned by its UID and mode 0700. Symlinks at these directory
paths are rejected. JSON `directories` entries report the path, owner UID, numeric
permission mode, privacy and write access. No profiles or sockets are created by
the check.

```sh
cargo build -p excalibur-webd
sudo target/debug/excalibur-webd --preflight /absolute/path/web.toml
```

The root launcher clears supplementary groups, changes GID and UID, verifies it
cannot recover root and rejects retained effective, permitted, inheritable or
ambient capabilities before probing Chrome or disk. Root targets and unknown
users fail. JSON contains the final identity, Chrome executable and reported
version, available disk bytes, reserve and search admission, errors and cgroup v2
observations. Exit status is zero only when environment checks pass; argument,
configuration and identity failures also return JSON with `ready = false`.

`ready` in preflight means environment preparation succeeded. Preflight keeps
`sandbox_verified` false because it does not launch renderers. Its version probe
invokes Chrome only with `--version` in a separate process group. A five-second
total deadline covers the process and stdout, including output held open by
descendants. The group is killed on completion or error; output is capped at
4096 bytes. Transient executable-busy errors share the same deadline. The probe also clears
the inherited environment. No remote
model or account is used. A shared or inaccessible cgroup is reported explicitly;
it requires process-tree accounting when resource management is implemented.
Cgroup availability is optional and does not imply memory limits are enforced.
The disk admission helper rejects growth that would consume the reserve; the
preflight requires the search budget plus reserve to fit on the data filesystem.
Continuous monitoring and enforcement on future operations are not implemented.

Verify with local, synthetic backend fixtures:

```sh
(umask 0077; cargo test -p excalibur-webd --test preflight)
(umask 0022; cargo test -p excalibur-webd --test preflight)
cargo fmt --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo build --workspace
```

The backend suite covers configuration errors, exact disk boundaries, executable
and version failures, descendant cleanup, total probe deadline, directory ownership
and permissions, umask-independent fixtures, rejected root identities, non-root
startup, irreversible
privilege removal and live `/proc` identity/capability evidence. Root-only cases
run when the test launcher is root. There is no frontend to review.


Run a real, temporary instance check after preflight:

```sh
sudo target/debug/excalibur-webd --check-instance /absolute/path/web.toml
```

This diagnostic drops privileges before constructing Tokio, prepares a private
unique profile, starts an owned loopback proxy that rejects every connection,
launches headless Chrome, obtains `Browser.getVersion`, verifies the renderer tree
and prints JSON evidence before closing and removing its temporary profile. It
exposes no RPC or viewer endpoint and does not browse external sites. `sandbox_verified`
is true only for a successful renderer inspection. The ordinary `--preflight`
command retains its environment-only meaning.

The shared `Instance` API requires an existing profile owned by the current UID
with mode 0700, an absolute Chrome binary and an explicit IPv4 loopback proxy.
The launcher rejects root, retained capabilities and profiles containing
`DevToolsActivePort`. Child descriptors 3 and 4 are blocking native pipe ends;
parent ends use Tokio. Chrome has its own process group and
`PR_SET_PDEATHSIG(SIGKILL)`, including the parent-death race check. Its environment
is cleared, with only the private HOME, controlled PATH/LANG and explicitly needed
display or session-bus values passed. Close and drop clean the process group and
observed crash handlers, checking PID start times before individual signals.
Process discovery includes double-forked crash handlers identified by their
executable and private profile, without taking unrelated Chrome trees.

Flags select the pipe transport, headless mode by default, site isolation, disabled
extensions/background services, bounded disk caches, proxy routing including
loopback, disabled DNS prefetch resolution, QUIC and non-proxied WebRTC UDP.
Listening background flags are restricted to persistent listening instances.
Extra flags have a strict allowlist: sandbox disabling, TCP debugging, certificate
bypasses, automation and overrides of managed flags are rejected. The host may
select the Secret Service password store only after verifying that service;
otherwise `basic` carries an explicit local-reader warning. The diagnostic uses
an empty profile and `basic`. Network broker integration and application profile
management are not implemented; the loopback proxy argument is a trusted host
boundary, not a claim of complete egress isolation.

Renderer checks require `Seccomp: 2`, `NoNewPrivs: 1`, and user/network namespaces
different from the browser process. Initial startup requires at least one verified
renderer within five seconds. New targets and OOPIFs are attached with flattened
sessions and `waitForDebuggerOnStart`; they are resumed only after verification.
New process identities have up to one second to finish sandbox initialization
while their targets remain paused. A failure after a renderer has passed is
immediate. An independent timer samples processes every 50 ms, using
`spawn_blocking` for `/proc` inspection and child status checks. Console, network
and other page events do not trigger process scans. Failure closes the instance
with `sandbox_unavailable`; renderer crashes produce target events.

Supervision consumes a separate bounded channel containing only attachment,
detachment, target-crash and transport lifecycle notifications. Ordinary event
observers may receive `Lagged` when their bounded queue overflows; this neither
closes CDP nor kills Chrome. If the lifecycle queue itself lags, the supervisor
reconciles active attachments from the reader's independent current-state cache,
verifies paused targets before resuming them and emits `events_resynchronized`
with the skipped count. This is state reconciliation, not a lossless historical
event journal. Process polling continues independently, and transport termination
is retained in a separate watch channel even if notifications are lost.

`instance_crashed` means unexpected browser exit/disappearance or CDP transport
failure: EOF, invalid protocol JSON/envelopes or attachment payloads, response
session mismatch, an oversized browser/unidentified frame, or interrupted/failed
transport writes. An event observer falling behind is not a crash. Inability to
inspect the process from the blocking worker blocks supervision instead of
claiming a browser crash. Explicit `Instance::close` or drop produces
`instance_stopped`; closing the raw CDP client outside that lifecycle is a
transport failure. The browser process itself remains in the host namespaces; no outer network namespace is added here.

CDP exposes a handwritten method catalog, common typed request/response structs,
and generic JSON parameters. It handles NUL-delimited fragmented or concatenated
frames, concurrent monotonically numbered requests, protocol errors and events
by flattened session. Deadlines cover lock/write/response waits: ten seconds by
default, fifteen for screenshots and twenty for DOM snapshots. Cancelled and timed
out requests release their pending entries; IDs are never reused. Cancelling a
partial frame write closes the transport. Incoming messages are limited to 64 MiB;
an oversized target frame is discarded through its delimiter, its CDP session is
invalidated and detached, and other sessions continue. An unidentifiable or
browser-level oversized frame, malformed JSON or EOF closes the connection.

Version checks use the shared bounded installed-binary probe before Chrome opens
a persistent profile, then `Browser.getVersion` after startup. Optional policy
settings are configured in the dedicated TOML:

```toml
[web.version]
# stable_major can be supplied from trusted release metadata.
# installed_release_unix_secs is the known publication time, not the file mtime.
# Public lookup is opt-in; omit this key to stay offline:
# stable_metadata_url = "https://googlechromelabs.github.io/chrome-for-testing/last-known-good-versions.json"
```

Relative to the supplied stable major, zero or one behind is normal, two emits a
warning, and three or more blocks persistent instances while ephemeral instances
remain available. A known release age greater than 42 days also blocks persistent
instances. Unknown release age is reported as null; it is not invented from an
installation timestamp. With no stable metadata, freshness is explicitly unknown.
The optional public lookup is bounded to five seconds and 64 KiB, does not follow redirects
and rejects credential-bearing URLs, and supports local HTTP fixtures. The policy is
refreshed every 24 hours and through `Instance::refresh_version`; warnings and
blocking transitions are observable events. A failed lookup emits a warning and
retains the last supplied metadata, without claiming it was refreshed. No updater
or automatic binary download is implemented.

Verify the transport, policy and installed-browser contract:

```sh
cargo test -p excalibur-cdp -p excalibur-browser
cargo test -p excalibur-browser --test cdp_contract -- --nocapture
```

The contract runs as the dedicated unprivileged user with a fresh profile and a
local synthetic HTTP proxy. It exercises every catalog command against the
installed Chrome, including experimental methods; explicit missing-state errors
are recorded for operations such as cancelling a nonexistent download. It checks
real OOPIFs, all four renderer sandbox conditions, process-tree TCP listeners and
absence of `DevToolsActivePort`, a real 70 MiB DOM snapshot isolated to one session,
version warning/block/refresh behavior, a copied binary lacking a usable sandbox,
renderer/browser crashes and SIGKILL of a parent owning two instances. A burst of
10,000 real console events intentionally overruns an ordinary observer while the
instance remains Running/Degraded, answers `Browser.getVersion`, retains verified
renderers and attaches another target. Transport fixtures also exercise filtered
lifecycle delivery and recovery of 400 active attachments after lifecycle lag. Redacted
method/result summaries and process evidence are written under
`target/test-evidence/browser/cdp_contract/`. Subprocess helpers are marked ignored
for direct invocation but are executed by their parent tests. Missing Chrome or
confinement fails the suite; only explicit `EXCALIBUR_BROWSER_TESTS=skip` opts out
of real Chrome tests and must be recorded in the delivery report. No model API,
real account or paid benchmark is used.
