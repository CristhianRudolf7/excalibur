# Browser environment

`excalibur-cdp` implements asynchronous CDP over native pipes. `excalibur-browser`
launches and supervises Chrome instances, verifies renderer sandbox state and
applies version policy, manages durable profiles, sessions and tabs, and provides
readiness checks, page observation and private evidence blobs.
`excalibur-webd` provides environment diagnostics and a local Unix JSON-RPC service. `excalibur-search` and `excalibur-web-adapter` remain empty boundaries.
The host adapter, agent browser tools and viewer frontend are not implemented.

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
permission mode, privacy, configured shared socket access and write access. No profiles or sockets are created by
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


## Sessions and local protocol

Run `target/debug/excalibur-webd --serve /absolute/path/web.toml` in the foreground
under a supervisor, or let a trusted host use `setsid`. The process drops privileges
before creating its runtime, database, socket or Chrome profiles. SIGTERM/SIGINT
stop accepting clients, drain dispatched operations and close Chrome. This command
does not install or deploy a service. Its owned proxy denies all network access;
the network broker and host adapter are separate capabilities. Tests inject a
local synthetic proxy through the Rust manager options.

The service binds `<run_dir>/webd.sock` with mode `0660`, and authenticates every
connection using `SO_PEERCRED`. Only `web.host_uid` (default `0`) and root are
accepted; rejected connections receive no response. `web.socket_gid` defaults to
the final primary GID. A different host UID needs membership in that shared group
and traversal of the runtime directory: an administrator may provision it as
`0710`, owned by the web UID and configured socket GID. The preflight reports this
exception as `shared_socket_directory`, not as a private directory. Data and
profiles remain `0700`; the socket group must be a group available to the final
identity. Neither socket access nor a run ID authorizes an untrusted program to
act as the host.

Example trusted profile configuration (origins are exact HTTP(S) origins):

```toml
[web.profiles.personal]
class = "persistent"
isolation = "shared_instance" # alternatively per_site

[[web.profiles.personal.sites]]
name = "chat"
rw = ["https://chat.example.org"]
login = ["https://login.example.org"]
listen = true
listen_probe = { unread_selector = "" } # empty: title changes only
wait = { selector = "main", strict_network = false }
```

The implicit `ephemeral` profile creates a new disposable BrowserContext per run,
with at most three live contexts, and shares one ephemeral Chrome instance. Closing
or suspending disposes the context; its cookies and storage are not reused.
The instance stops after ten minutes without live contexts and its profile is
removed. `web.compact = true` puts these isolated contexts in a configured shared
persistent instance instead; it deliberately shares the browser process.
Persistent profiles use the default context and reuse one instance per profile,
or one per configured site with `per_site`. Profile names cannot contain paths or
the reserved `--` separator. Profiles enable Chrome session restoration; the
installed-Chrome contract records whether session cookies actually survive a
restart. Persistent cookies, localStorage, IndexedDB and host-reported site login
state are preserved. The manager probes the user’s Secret Service without retrieving secrets (a two-second
deadline); a responding service selects `gnome-libsecret`. Otherwise it selects
`basic`, explicitly warned in status: filesystem permissions are its protection,
not encryption against someone who can read the profile. There is no arbitrary CDP or cookie RPC.

Sessions have an owner run, profile, site, mode, credential references and lifecycle
state. Tabs retain their owner/subtask label, URL/title, document epoch and stable
logical ID. Every tab is created with `newWindow: true`. Limits are four live tabs
per run and twelve globally; failed admission returns `tab_limit` or
`capacity_exceeded`. A listening site shares a single tab across its sessions;
operations acquire the same exclusive tab lease, wait up to thirty seconds and
then return `tab_busy`. Other tabs can hold independent leases. Logical active
selection is per run, and switching calls `Page.bringToFront`. Suspension saves
URLs/titles, closes live targets and resumes on the next tab operation. Listening
sessions return `listener_cannot_suspend`.

The manager represents opening, ready, busy, blocked, human_control, suspended,
recovering, lost, closing and closed states and validates lifecycle transitions.
Headless is the default. Native `headed` requires DISPLAY or WAYLAND_DISPLAY;
an instance already in another mode returns `mode_unavailable`. Without a
display, `headed` also returns that error until intervention support is available. Agent document admission accepts only
configured read/write origins; login origins are human-only. The input lease
checks the actual current frame URL under exclusion and returns
`denied(human_only_origin)` for a login origin. Observation, action dispatch,
login detection and human takeover are not yet exposed by RPC.

Document-origin blocks retain a `blocked_reason` (`needs_login` or
`document_origin`). Reconciliation reads the actual top-level frame, including
network error documents, for every owner of a shared listener. Returning all live
tabs to permitted document origins clears those blocks, emits `session_state`
and admits agent leases again. Security/version blocks and human control are
not cleared by a URL change. Lifecycle changes use the checked transition table;
returning to a permitted origin does not by itself assert a logged-in account.

Frame reconciliation uses navigation events and cached, verified frame URLs.
Browser target discovery runs outside the model lock, concurrently across
instances. Renderer frame/count probes also run outside that lock, with a total
500 ms budget, at most one pending probe per tab and twelve globally. A timeout
leaves the last known state intact; it does not declare a crash. Completed probes
are discarded if the target, CDP session or document epoch changed. This keeps
other runs and listening events responsive when a page executes endless script.
Input admission and suspension capture fresh frames under their own tab leases,
outside the model lock, and check the identity again before applying results.

Listening instances use the three background throttling flags and focus emulation.
No suspend, freeze, discard or memory pressure command is sent to a listener.
Target metadata is reconciled periodically; title changes produce
`listener_activity` with no message content, at most once per five seconds per
site. Changes inside that window are merged into a pending notification and
flushed when the window closes, even if the page then stays unchanged. The event
retains the newest unread count; `kinds` identifies title and/or unread activity,
while `kind` remains `unread_count` when a count changed, otherwise
`title_changed`. Titles and message bodies are not copied into the event.
A configured unread selector runs a MutationObserver in an isolated world
and emits only an unread count through a scoped binding. A separate lifecycle
channel and periodic health/count probes reconcile ordinary event backpressure. A crashed listener
renderer is replaced with a new target, focus/probe reinstalled and document epoch
advanced; its logical tab ID remains unchanged. Recovery validates the saved URL
before creating a page and falls back to a configured read/write origin for login
or error documents. Failed replacement setup closes its candidate target; one
tab's recovery failure does not interrupt other tabs or instances. Persistent
browser crashes enter recovering, with delays of 1, 5 and 30 seconds and at most
five relaunches in ten minutes; exhaustion produces `instance_failed` and lost
sessions. Restarting the service explicitly clears that retry circuit. Ephemeral
sessions become lost. Recovery waits up to thirty seconds for tab operations.

`webd.db` is private SQLite in WAL with FULL synchronization and one serialized
writer on the blocking pool. It stores profiles, site state, sessions, tabs,
operations and cursor events. A filesystem lock prevents concurrent managers for
one data directory; another lock protects socket replacement. Restart marks
surviving ephemeral sessions lost, restores persistent URLs immediately, and
advances document epochs (`stale_ref(browser_restarted)`). It removes only a
profile SingletonLock whose recorded PID is demonstrably absent. No old CDP
session or in-memory snapshot remains valid.

JSON-RPC 2.0 uses newline-delimited UTF-8 objects, at most 4 MiB per frame. Invalid
or oversized frames return typed errors and leave the connection usable. Requests
include `run_id`, `deadline_ms` (1–300000), and an `op_key` for effects. Implemented
methods are `session.open/close/suspend/list`, `tab.new/list/switch/close`,
`status.get`, `events.list`, `op.get`, and the trusted-host metadata operation
`site.set_login_state`. The latter records an assertion by the host, not a login
verification. The service binds run ownership at every session/tab operation.

```json
{"jsonrpc":"2.0","id":1,"method":"session.open","params":{"op_key":"run:step:call","run_id":"run","deadline_ms":30000,"profile":"ephemeral"}}
```

Effects are durably recorded as received, then dispatched before manager execution,
then completed/failed with their result. Requests with the same key/hash attach to
a live operation or replay its saved result; different payloads return
`idempotency_conflict`. The deadline is excluded from the hash so polling can use
a new wait budget. After two seconds (or the shorter request wait), a live effect
returns `working{op_key}` and continues independently of client disconnection.
`op.get` reports live work, saved outcomes or `uncertain` for dispatched work from
a previous service lifetime. Uncertain effects are never re-executed. Terminal
operation records and events have a 24-hour retention window, pruned on startup.
Events are streamed as `event` notifications with durable monotonic cursors;
`events.list{after}` replays up to 256 records per page. Slow socket clients are
closed after a ten-second write deadline and can reconnect/replay. A cursor is
not an acknowledgement of processing by the host.

```sh
cargo test -p excalibur-browser --test sessions -- --nocapture
cargo test -p excalibur-webd --test webd_boundary -- --nocapture
```

These real-Chrome suites exercise persistent cookies/IndexedDB, session cookie
restoration, isolated ephemeral contexts, compact/per-site instances, ownership,
tab/context limits, exclusive queues, durable replay/uncertainty, socket framing
and peer rejection, service restart and SIGKILL with two Chrome instances. The
listening contract runs a synthetic long-polling page for ten minutes while other
windows are active, verifies title events and live timers, and kills its renderer
to verify recovery. `EXCALIBUR_LISTEN_TEST_SECONDS` can shorten a development run;
only the default 600-second run satisfies the prolonged listening check. Evidence
is in `target/test-evidence/browser/{sessions,webd_boundary}/`. There is no viewer
frontend to validate visually.


`cargo test -p excalibur-browser --test sessions listener_events_and_document_recovery -- --nocapture`
runs the real-Chrome burst/recovery fixture without the prolonged listening run.
It verifies final unread delivery after changes inside five seconds, shared-owner
login/network recovery and target accounting after a listener crashes on login.
Its evidence is `target/test-evidence/browser/listener_reconciliation/reconciliation.json`.
The long-polling fixture disables response caching, consumes each response body
and records its actual page changes alongside notification counts.
The service boundary fixture waits for the error-document block expected from its
deny-all proxy, and session fixtures keep the live session ID explicitly.


`cargo test -p excalibur-browser --test sessions other_sessions_stay_responsive_with_a_spinning_renderer -- --nocapture`
uses an actual infinite-loop renderer alongside a healthy context in the same
browser and a persistent listener. It bounds list/status, another tab's lease and
switch, and Unix RPC reads to one second, checks listener event gaps and bounded
input probing, and verifies shutdown. Measurements are local synthetic checks,
recorded in `target/test-evidence/browser/renderer_isolation/contract.json`.

## Waiting and observing a page

The browser library and Unix protocol provide `observe.wait_ready`,
`observe.snapshot`, `observe.subtree`, `observe.validate_ref`, `blob.read`,
`devtools.console`, `devtools.network`, `devtools.performance`, `emulate.viewport`
and `nav.go|back|forward|reload`. These are shared backend operations. Agent tool
registration, input actions, model vision routing and the intervention viewer
remain separate components.

Every tab operation requires `run_id`, `session_id`, `tab_id` and `deadline_ms`.
Navigation and emulation also require `op_key` and use the durable operation
protocol. Queries return `state: completed` with their value in `result`.
For example, the method `observe.wait_ready` accepts:

```json
{"run_id":"local-run","session_id":"SESSION","tab_id":"TAB","deadline_ms":15000,"criteria":{"selector":"main.loaded","network_idle_ms":500,"dom_stable_ms":500,"timeout_ms":10000,"max_inflight":0,"strict_network":false}}
```

Readiness combines navigation commit and document readiness, network activity
from the main frame and OOPIF sessions, visible selector/text conditions, and a
MutationObserver and animation heartbeat in isolated worlds. Defaults are 500 ms
of network quiet and DOM stability and a 10 s deadline. WebSocket, EventSource,
ping, media and prefetch traffic do not count. Requests older than 3 s are long
lived; an otherwise stable page can return `ready` with `network_busy` after an
additional quiet window of 500 ms + 2 s. Strict network mode times out instead.
`timeout` includes `criteria_met` and diagnostic URLs. Visible challenge frames,
JavaScript dialogs, file choosers, empty verification fields and login indicators
produce `blocked`; observing does not solve
them. Sites may supply the same criteria in their optional `wait` configuration.
Non-listening targets enable network events, lifecycle events and the isolated-world
stability script while still blank, before initial or restored navigation.
For a late attachment or a listener resuming collection, `network_unobserved`
means earlier requests could not be measured: `criteria_met.network` stays false.
Non-strict waits may return ready with that degradation after DOM/condition
readiness and the additional quiet window; strict waits time out. A newly
observed document replaces this uncertainty. Disabling listener collection also
invalidates its cached ready result.
A ready result is remembered with the frame loaders. Navigation invalidates that
match; a snapshot without a current result waits automatically.

`observe.snapshot` accepts `options` with `dom: interactive|full`,
`extent: viewport|fullpage`, `screenshot: bool` and optional `wait` criteria.
Capture combines DOMSnapshot with accessibility trees in their own CDP sessions,
including same-process frames and OOPIFs, under a 3 s collection budget.
Unavailable components appear in `missing` with `partial: true`; available DOM
is retained when accessibility capture is incomplete. An image is withheld if
missing DOM prevents complete sensitive-field masking. Frame owner rectangles
translate child coordinates into the top-level viewport. The normalized tree
contains hierarchy, filtered attributes, roles, names, geometry, visibility,
enabled state and the reasons each control is interactive. Inline text is at
most 16 KiB and covers the viewport plus a screen above and below; `omitted`
counts controls retained only in the complete tree blob. Regions and headings
preserve context. Page text, console and network output are untrusted data.

A screenshot is stored as a JPEG quality 60 blob, after bringing the tab to the
front. Full-page height is capped at 16,384 CSS pixels. Password, one-time-code
and credit-card values are redacted in text, elements and the tree; their image
rectangles are masked. The shared vault redactor can be populated by the host
through `manager.observation.redactor` and is applied before serialization.
The snapshot includes `vision_recommended` for an explicit image request or a
mostly visual viewport with fewer than 20 useful accessibility nodes. It does
not send an image to a model.

References (`eN`, or `fKeN` in frames) use target, CDP session, frame, loader and
backend-node identity. Living nodes retain their references across snapshots;
numbers are not recycled. Only a snapshot supersedes the previous snapshot.
`observe.validate_ref` accepts `snapshot_id` and `ref` and checks the current
snapshot, loader, frame, live node, connection and role/name fingerprint.
It returns `valid` or `stale_ref` with `superseded`, `navigated`, `frame_gone`,
`node_gone`, `node_detached`, `changed`, `takeover` or `browser_restarted`.
A superseded key may include `remapped_ref`; validation never dispatches input
or silently chooses another node. The host can invalidate a tab on intervention
with `manager.observation.invalidate(tab_id, "takeover").await`.
`observe.subtree` reads the saved compressed tree, selected by `snapshot_id` and
optional `ref`, without recapturing the page. `delivery: inline` returns the nodes
when their serialized size is at most 1 MiB. Larger selections return
`delivery: blob`, `node_count`, `root_ref` and `source_blob`, with an empty `nodes`
array. Read that authorized complete-tree blob through `blob.read` and select the
indicated root locally; responses stay below the 4 MiB protocol frame limit.

Blobs use the existing store's SHA-256 and zstd implementation in the private
`observations/` directory. A webd ledger authorizes each hash for the requesting
run for 24 hours. `blob.read` requires `sha256` and accepts uncompressed `offset`
and `length` (1–1,048,576); it returns a verified base64 window and `total_bytes`.
A different run cannot read a hash merely by knowing it.

Console and network rings hold at most 500 records each. Filters include
`since_seq`, `limit`, `level` and `url_contains`; results include the next sequence
and dropped-record count. Console backpressure is isolated from readiness and
supervision events. Readiness event loss is reported explicitly and cannot
produce a normal ready result. Network summaries contain URL, method, status,
type, size and timing, never bodies or authorization/cookie headers. Set
`tainted: true` to remove query strings and fragments as well; this is sticky
for the tab. Network buffers are limited to 2 MiB total and 512 KiB per resource;
listening tabs enable collection only during explicit operations, including
cancellation cleanup. The host also has `settle_observation`: a 250 ms navigation
window followed by a 5 s wait, reporting `settled` without dispatching input. Performance returns Chrome's performance metrics.

Viewport presets are `small` (360×740, DPR 3, mobile), `tablet` (768×1024),
`desktop` (1280×900) and `large` (1440×900). `preset: custom` accepts `width`,
`height`, `device_scale_factor` and `mobile`. Images use logical CSS dimensions.
An optional `user_agent` override is accepted only in an ephemeral session;
persistent accounts retain their browser identity.

Run `cargo test -p excalibur-browser --test observe -- --nocapture` for real Chrome
against synthetic local pages. It checks waits, blocked conditions, 300 controls,
open/closed shadow trees, frames and OOPIF coordinates, 5,000-control capture,
redaction, references, saved subtrees, private blobs, background capture,
network header filtering, console bounds, metrics and mobile viewport size. It also
checks initial slow requests through `tab.new`, automatic first-snapshot waits,
late listener attachment and blob delivery for oversized subtrees.
Evidence is written to `target/test-evidence/browser/observation/`.
