# Browser environment

`excalibur-cdp` implements asynchronous CDP over native pipes. `excalibur-browser`
launches and supervises Chrome instances, verifies renderer sandbox state and
applies version policy, manages durable profiles, sessions and tabs, and provides
readiness checks, page observation, input actions, dialogs, downloads, uploads and
credential filling. `excalibur-web-adapter` is the trusted host library that applies
the harness authorization, file authority, file control and `SecretExecutor` to
those effects. `excalibur-webd` provides environment diagnostics and a local Unix
JSON-RPC service. `excalibur-search` remains an empty boundary. Agent browser tool
registration and host composition are not implemented; the intervention viewer is served by `excalibur-webd --serve` over real TCP (single-use code, `HttpOnly;SameSite=Strict` cookie, exact `Origin` checks, strict CSP headers and shell rendering), covered by `viewer_http` and `viewer_viewport` over local synthetic fixtures.

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
unique profile, starts an owned loopback listener that only accepts peers owned by the web UID (mapped through `/proc/net/tcp` and `/proc/net/tcp6`) and drops accepted diagnostic connections without forwarding,
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
an empty profile and `basic`. Application profile
management is not implemented. `--serve` starts the egress broker (`HostProxy`)
and forwards only same-UID loopback connections to its Unix socket, closing
foreign-UID peers without forwarding; the loopback proxy argument remains a
trusted host boundary, not a claim of complete egress isolation.

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
`denied(human_only_origin)` for a login origin. The readiness probe reports login and challenge markers; `takeover.start` and
`takeover.end` move a blocked session into `human_control` and back over RPC
(see the intervention section below).

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
is in `target/test-evidence/browser/{sessions,webd_boundary}/`. The viewer shell, authentication and security headers are served by `excalibur-webd --serve` for visual validation, covered by `viewer_http` (real TCP) and `viewer_viewport` (small/large Chrome presets) with evidence in `target/test-evidence/browser/viewer/`.


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
registration, model vision routing and the intervention viewer remain separate
components. Input actions are described in the next section.

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

## Actions, dialogs, files and credentials

Actions use two Unix protocol calls. `act.prepare` (a query) accepts `request` with
`action` (`click`, `type`, `select`, `hover`, `scroll`, `press`, `upload`, `drag`
or `dialog`), `snapshot_id`, `ref`, `value`, `keys`, `target_ref`, `files` and
`options` (`button`, `click_count`, `modifiers`, `key_events`, `html5`,
`direction`, `amount`, `accept`, `prompt_text`). It validates the reference and
the element frame's document origin and returns a `plan_id` valid for 60 seconds
together with the deterministic `action_class`, `effect`, `risk`, origin and
element name. `act.perform` takes `plan_id` and `op_key`, uses the durable
operation protocol and is accepted only for the same run, session and tab.
No arbitrary JavaScript, CDP command or host path is accepted.

Before input, webd takes the tab lease, rejects a non-`dialog` action while a
`confirm`/`prompt` is open (`dialog_open`), revalidates the reference
(`stale_ref` without effect), rechecks the read/write document origin of the
element frame (`denied(origin_not_writable)`) and recomputes the class; a changed
class returns `authorization_changed`. Actionability requires a connected,
visible, enabled (and, for `type`, editable) element whose box is unchanged over
two animation frames and that receives the hit test at its centre. Retries wait
0, 20, 100, 100 and 500 ms, alternating `center`, `end` and `start` scroll
alignment. Hit testing crosses user-agent and author shadow roots and frame
owners. Failure returns `not_actionable` with `covered_by`; input is never forced.
Only then does the operation move to `dispatched` in `web_ops`; a lost CDP
receipt after that point is reported as `uncertain` and is never repeated.

`click` and `hover` dispatch mouse events at the element centre; `type`
focuses, selects all and inserts text, so repeating it is idempotent, and
`key_events: true` sends key events instead (at most 2,048 characters). `select`
chooses an option by value or label in an isolated world and emits `input` and
`change`. `press` accepts named keys, letters, digits and F1–F12 with
`Control`, `Alt`, `Meta` and `Shift`. `scroll` without a reference sends a wheel
event of `amount` screens in `direction`. `upload` sets staged files on an
`input[type=file]`. `drag` moves the mouse in five steps to `target_ref`; with
`html5: true` it intercepts the drag data and replays `dragEnter`, `dragOver` and
`drop`, refusing native file data. `dialog` accepts or dismisses the current
dialog, optionally with `prompt_text`.

The result contains `result` (`ok` or `dialog_open`), `navigated`, `url`,
`new_tabs`, `popup_blocked`, `dialog`, `alerts`, `download`, `file_chooser`,
`settled`, `wait`, `dom_changed`, `suggest_snapshot`, `action_class` and
`elapsed_ms`. Settling uses a 250 ms window and a 5 s readiness wait.
`dom_changed` and `suggest_snapshot` are conservative and always true. Results
are redacted and marked untrusted.

Classes are `observe` (hover, scroll), `input` (type, select), `navigate_link`
(a link outside a form without a click handler), `commit`, `upload`, `drag` and
`credential`. `commit` covers form buttons, Enter in a form or in a listening
site, accepting a dialog in a persistent profile, and accessible names containing
the commit lexicon (enviar, send, submit, publicar, post, comprar, buy, pagar, pay,
confirmar, confirm, eliminar, delete, borrar, transferir, transfer, firmar, sign,
reservar, book, suscribir, subscribe, aceptar, accept). In persistent profiles an
unclassified click is `commit`. Risk is low/medium in ephemeral sessions and
medium/high in persistent ones; uploads and credentials are always high. Per-site
class overrides are a site-scoped table consulted by `classify` (arch 8.3):
WhatsApp refines composer Enter to `commit` and non-form chat-surface clicks
without commit lexicon to `navigate_link`; form controls and lexicon hits never
downgrade. Every other site keeps the conservative unknown-to-`commit` default
in persistent profiles.

An `alert` opened during an action is accepted and its redacted message is
returned in `alerts`. `beforeunload` is accepted only for agent navigation or a
navigating action. `confirm` and `prompt` stay open: the action returns
`dialog_open` with type and message, because the page is suspended until the
dialog is handled. Pages opened by the acting tab are registered with the same
owner and reported in `new_tabs`. In persistent sessions a popup outside the
read/write origins is closed and reported in `popup_blocked`; tab limits also
close the popup.

Every instance and browser context starts with downloads denied. A download
started by an action is cancelled and reported as `download_blocked` with the
suggested name and origin. The `download` method (keyed by `op_key`) accepts
`trigger: {snapshot_id, ref}` or `{url}` and `timeout_ms` up to 120 s. It
serializes downloads per context, allows a single download from the tab's frames
into a fresh private `quarantine/<session>/<uuid>/` directory, uses the GUID as
the file name, rejects origins outside the session and cancels and deletes
anything above 200 MiB. On completion it computes SHA-256, size and a content
type from magic bytes, stores the bytes as a run-authorized blob, removes the
quarantine copy and restores the deny policy. The suggested name is sanitized
and never used as a path.

`act.stage_upload` receives base64 chunks of at most 2 MiB (`file_name`,
optional `staged_id`, `offset`, `final`) and writes
`staging/<session>/<uuid>/<sanitized name>` with mode `0600` in private
directories, up to 200 MiB per file and 128 staged files. Staged files belong to
the run and session, are deleted when the session closes and expire after one
hour. Chrome only receives these staged paths.

`act.check_credential_target` and `act.fill_secret` implement reference-only
credential filling. The session must declare the `secret://` reference. The
expected origin must be canonical HTTPS without userinfo. The committed security
origin and URL of the top frame and of the element's frame must equal it exactly;
an iframe is accepted only if the site lists its exact origin in
`credential_frames`. The element must be a visible, enabled, actionable
`input[type=password]` on a read/write origin. `act.fill_secret` repeats every
check under the tab lease immediately before and after focusing, confirms focus,
inserts the value and zeroizes the request buffer and the decoded string. The
filled element is redacted in later snapshots. Screenshots of that tab are
withheld (`vision_suppressed: true`) for 5 s and while the same document remains.
The method is not stored in `web_ops`; the host `SecretExecutor` provides the
durable use mark and fail-closed reconciliation.

`excalibur-web-adapter` provides `WebAdapter` over an in-process `LocalBackend` or
the Unix `SocketBackend`. `act` rejects arguments containing a registered secret,
prepares the plan, and authorizes it as `Action::Browser` (`browser.<class>`
external action and class-based risk) or as the `browser.observe` data scope
through the durable host-effect ledger, so an approval or a repeated key never
dispatches twice. `credential` reads only vault metadata, requires a
`WebsitePassword` with a URL, checks the target in webd without the secret and
then calls `SecretExecutor::execute`, which records the use and audit before
resolving the value. `stage_path` resolves the path through a capability
directory handle inside the run workspace, rejects protected paths, credential
names and symlinks leaving the root, and requires read authorization; `stage_blob`
requires the `blob:<sha256>` data scope. Content containing a registered secret
is not staged. `download` authorizes a high-risk browser effect, reads the
quarantined blob in 1 MiB windows and passes it to host file control:
executables, oversized files and secret-bearing content stay quarantined, an
optional `FileScanner` runs, and admitted files become untrusted host blobs with
an audit record. These are library entry points; no agent tool is registered.

Run `cargo test -p excalibur-browser --lib observation` and
`cargo test -p excalibur-web-adapter --test actions -- --nocapture`. The second
uses real sandboxed Chrome as the dedicated user and a local synthetic HTTPS
fixture with a temporary CA, which requires `openssl` and `certutil`
(`libnss3-tools`). It checks each action on its control, an overlay
returning `not_actionable` without a click, a moving button, idempotent typing,
mouse and HTML5 drag, alert/confirm handling, ephemeral and blocked popups,
foreign-frame denial, commit approval before effect with a single replayed
result, unknown persistent clicks as `commit`, exact upload content, rejected
upload sources that never reach webd, unarmed, armed and oversized downloads,
credential fill and redaction, cross-origin frame, subdomain and HTTP refusal
without resolution, a navigation between check and fill, a simulated host crash
after the use mark that is not retried, and the absence of the secret value from
webd and host files, events, audit and test output.
Evidence is written to `target/test-evidence/browser/actions/`.

## Network, content, blocks and intervention

`--serve` binds the Chrome proxy listener on `127.0.0.1:<random>` and checks
every accepted connection against `/proc/net/tcp` and `/proc/net/tcp6`: only a
peer owned by the web UID reaches the egress broker (`HostProxy` with pinned
DNS, no private/loopback answers, web ports only) over its Unix socket
(`excalibur_webd::loopback::{accept_owned, forward_to_unix}`); any other peer
is closed without forwarding, so the listener cannot be reused as an open
proxy. `net::check_loopback_uid` parses both proc tables (little-endian IPv4,
per-word reversed IPv6) and fails closed on unknown peers. The diagnostic
`--check-instance` path applies the same UID check and drops accepted
connections without forwarding.

Document admission compares committed frame origins, never raw strings
(`net::document_origin`: exact `scheme://host[:port]`, no userinfo).
Top-level navigation to `file:`, `chrome:`, `chrome-extension:`, `devtools:`,
`javascript:` or `data:` is denied (`net::is_forbidden_scheme`). Persistent
profiles admit only configured document origins: reads accept `rw` or `ro`,
input requires `rw` (`denied(origin_not_writable)`), and `login` origins stay
human-only (`denied(human_only_origin)`). `credential_frames` must be exact
HTTPS origins and `ro` origins must not overlap `rw` or `login`. Ephemeral
sessions record each newly navigated origin
(`Manager::note_ephemeral_origin`); the deterministic C4 check
(`net::exfiltration_check` via `Manager::navigation_risk`) flags
cross-origin POSTs and queries or fragments longer than 128 characters (or
above 4.5 bits of Shannon entropy per character) sent to a new origin, so a
contaminated run can require approval before committing.

Every new browser context denies fifteen sensitive permissions through
`Browser.setPermission` before any navigation (`permissions::denial_params`:
geolocation, notifications, camera, microphone, MIDI, clipboard-read,
clipboard-write, accelerometer, gyroscope, magnetometer, idle detection,
local network access, display capture, window management and NFC); a denial
failure fails the session open, so no context runs without them (bare
`clipboard`, `sensors` and `shared-storage` are rejected by current Chrome
and are not used). Site configuration may grant concrete permissions later.

Block detection is deterministic. The readiness probe runs in an isolated
world and reports `{kind, vendor, signals, confidence, url}` for
`blocked_captcha`, `blocked_by_site`, `needs_login` and `needs_2fa`, covering
challenge frames, invisible reCAPTCHA markers, perimeter block pages and
identity origins, with corroborating title/text signals. `wait_ready` accepts
the probe payload only through `observation::detectors::classify_probe_blocked`
(unknown kinds, low confidence and empty signals never block) and elevates
medium confidence with corroborating title/text signals; low-confidence
signals stay `degraded`. Header and dialog signals that bypass the probe are
normalized to the same vocabulary
(`observation::detectors::normalize_reported_blocked`, so the legacy
`captcha` kind surfaces as `blocked_captcha`). The header-block signal is
scoped to its document: each navigation drops the previous page's stored
header block before the new one commits, so `wait` neither reports a stale
block nor misses the fresh one. Corroboration against the live
document HTTP status (`elevate_with_status`) is unit-tested but not wired to
network state yet.

A blocked session can enter human intervention over RPC: `takeover.start`
moves `Blocked` to `human_control` with a start timestamp, while
`takeover.end{resolved}` returns a resolved intervention to `Ready`, advancing
every live tab document epoch and invalidating prior references as
`stale_ref(takeover)`; an unresolved or over-deadline (15 minutes)
intervention returns to `Blocked` with `blocked_unresolved`. Deterministic
lifecycle tests pin the deadline boundary (899 s resolves, 901 s expires),
the epoch bump and the `blocked_unresolved` state.

The viewer is served by `--serve` on `127.0.0.1:<random>` (`webd::viewer`):
`viewer.issue` mints an 8-character single-use code over the peer-credential
Unix socket while the session is in `human_control`; `POST /auth` redeems it
once into an `HttpOnly; SameSite=Strict` cookie bound to the tab and
deadline, and `GET /`, `/viewer.js` and `/style.css` require that cookie. The
WebSocket upgrade additionally requires an exact `Origin` match before
answering `101`; every other case (unknown, reused or expired code, missing
cookie, foreign origin) fails closed with `403`. The code is never read from
the URL: query strings are ignored by construction. Responses carry
`Content-Security-Policy: default-src 'none'; img-src data: blob:; script-src
'self'; style-src 'self'; connect-src 'self'; frame-ancestors 'none';
base-uri 'none'; form-action 'self'; require-trusted-types-for 'script'` plus
`nosniff`, no referrer and no-store headers; the client script only reaches
the DOM through `textContent`, canvas images and `document.title`, and the
stylesheet stacks ≥44 px controls on small viewports while keeping the canvas
1:1 up to 1280 px on large ones. Screencast streaming past the `101` ready
frame stays pending on harness integration.

Blocked reports map onto the harness task outcome without model judgement
(`browser::takeover::intervention_outcome`): interactive callers park the
execution in `input_required` with the block and instructions, while
non-interactive callers (`excalibur exec`, eval profile) fail immediately
with `blocked_requires_human`. Ephemeral navigation keeps the C2 origin set:
new origins are recorded on every committed navigation (`note_ephemeral_origin`
in `nav.go` and `tab.new`), forbidden schemes stay denied, and the C4 risk
(`navigation_risk`: long or entropic cross-origin payloads, cross-origin
POSTs) is reported before committing; domain authorization itself belongs to
the harness, which is outside this crate boundary.

Run `cargo test -p excalibur-browser --lib` and
`cargo test -p excalibur-webd --lib`. These cover proc-table parsing and UID
matching, origin/entropy/exfiltration checks, the fifteen default denials and
their CDP shape, detector classification and control pages, single-use and
expiring takeover and viewer codes, cookie/page/socket viewer checks, CSP
headers and markup escaping, the takeover deadline boundary, the ephemeral
document gate and the `input_required`/`blocked_requires_human` mapping.
`cargo test -p excalibur-webd --test viewer_http` exercises the served
endpoint over real TCP (redeem-once, expired/reused/unknown codes, code in
URL ignored, missing cookie, foreign origin, upgrade handshake, strict
headers). `cargo test -p excalibur-browser --test block_matrix -- --nocapture`
runs the real-Chrome 9.5 matrices over local synthetic pages (captcha frame
and markers, perimeter block pages, `cf-mitigated` header, password on a
logged-in site, one-time-code fields and OTP groups, corroboration elevation,
the human-only login-origin denial, plus invisible-badge, injected-path and
plain control pages) with the matrix in
`target/test-evidence/browser/block_matrix/`.
`cargo test -p excalibur-webd --test viewer_viewport -- --nocapture` renders
the served shell in real Chrome at the small (360×740) and large (1440×900)
presets over synthetic pages only, asserting width, no horizontal overflow,
touch targets and the committed origin header, with screenshots in
`target/test-evidence/browser/viewer/`. Still pending: document-status
elevation wired to live network state, detector kinds beyond the fixed 9.1
vocabulary (cookie-wall/geo-block variants without 9.1 markers do not
block). The F-B4-03 adapter fill-copy zeroization and the F-B4-04 ZIP magic
unification are resolved in the B4 correction round (the adapter zeroes its
fill copy after the request; `magic()` labels ZIP `PK\x03\x04` to mirror
`file_control`).

## Light reading and citations

`excalibur_browser::{extract, read, cite}` with `excalibur_webd::read::Service`
provide the `web_read` backend path and the `cite` backing store. No agent
tool is registered; the browser fallback render is injected as already-waited
HTML (ephemeral context plus `observe.wait_ready` plus `DOM.getOuterHTML`),
so the service stays deterministic without real Chrome.

URL validation accepts only `http`/`https` without userinfo and at most
2,048 characters; fragments are stripped for cache identity. `fetch_http` (secure default) uses explicit 10 s connect and 30 s total
timeouts, follows at most five redirects manually with per-hop
loopback/private-range/metadata-IP/sensitive-suffix re-validation plus an
HTTPS downgrade refusal, consumes the body in chunks with a running counter
aborting past 10 MiB (`body_too_large`), stores and replays no cookies (no
jar; `FetchPolicy::isolate_cookies`), and enforces an allowlist of
text/HTML/XML/feed/JSON content types. `FetchPolicy::test_loopback` admits
loopback literals for local synthetic fixtures while keeping every other
guard. `extract::extract_document` produces
title, Markdown and `pN` fragments with UTF-8 byte offsets and heading paths
after removing `script`, `style`, `nav`, `footer` and `aside` blocks, with
inline links kept as text. The HTTP-first heuristic escalates to the browser
on SPA shells (little visible text with several scripts), text/markup ratios
below 0.02, empty mounts (`#root`, `#app`, `__next`, `ng-app`) or
`<noscript>` JavaScript requests; embedded `__NEXT_DATA__`, `__NUXT__` or
JSON-LD avoids escalation. A missing injected render returns
`needs_browser:<reason>`.

Capture requires a current `ready` wait for the tab epoch (`WaitGate::check`,
rule 7.4-1): non-ready or stale epochs return `wait_required` or
`wait_stale_epoch` before any DOM is touched. Non-strict waits allow
`network_busy` and `network_unobserved` degradations; strict network mode
turns them into `timeout`, matching the long-poll contract. Results carry
`title`, inline Markdown up to 50 KiB floored to the preceding UTF-8 char boundary with `truncated`, full length,
`content_hash` (`sha256:`), `source_id` (`w_<16 hex>`), fetch time, HTTP
status, `render` (`http`/`browser`), extractor label and fragment metadata
with `untrusted: true`. `slice_fragments` serves `pN` or `pA-pB` ranges from
the stored full Markdown by hash. The in-memory cache keeps entries for 24 h
with a `fresh` bypass preserving the original fetch date.

`cite::locate` finds exact quotes first and falls back to whitespace- and
case-normalized matching, mapping the normalized hit back to the exact raw
byte span covering the matched words (char-boundary aligned); normalizing the
covered slice reproduces the normalized quote.
`cite` builds `Citation` records (`source_id`, URL, title, fragment, offsets,
`#:~:text=` link, fetch time, hash) and `format_citation` renders them
without inventing page numbers. `webd::read::Service` stores pages by
content hash for fragment reads and quote validation.

Run `cargo test -p excalibur-browser --test read_cite -- --nocapture`,
`cargo test -p excalibur-browser --lib` and
`cargo test -p excalibur-webd --lib read`. These cover static extraction and
fragment offsets, SPA wait-before-capture with injected render, long-poll
degraded versus strict timeout, exact/normalized/missing quotes, fragment slicing with the char-boundary 50 KiB cap, the 50 MiB chunked-cap
rejection, HTTP-first decisions, the local-loopback cache and TTL expiry,
plus the browser-side 14.4 rows (loopback/private/metadata refusal,
redirect-to-private refusal, downgrade detection, robots allow/deny,
meta-noindex index hook, cookie non-replay). Full broker egress (DNS-bound
host binding, `robots.txt` retrieval/caching, actual index-queue insertion,
served-viewer/RPC registration), the `dom_smoothie`/`htmd` production
extractors, PDF/feed/charset handling, live ephemeral-context rendering and
RPC method registration stay pending (B9 broker lane).

## Resources, budgets, admission and measurement

`webd::resources::ResourceGate` wires the `excalibur-browser-resources`
foundation into the service (architecture 13.1-13.6, 18.3; NB-17, NB-18).
Admission uses the per-origin moving-average estimate (200 MiB default,
400 MiB heavy class) plus a 200 MiB margin under the 1.5 GiB soft budget,
with immediate denials for tab limits (4 per run, 12 global), PSI
`some avg10` at or above 20 % and disk growth breaching the 5 GiB reserve;
over-budget requests wait up to 120 s before `capacity_exceeded` with the
memory breakdown. Eviction follows steps 1-7 (webd caches, IDLE LRU,
ephemeral GC plus moderate pressure only on the ephemeral instance,
ephemeral browser cache, close of one over-limit ephemeral ACTIVE tab with
`tab_evicted`, ephemeral restart, LIVE quiet reload only); listening tabs
are never frozen, discarded, pressured or suspended, and
`browser_suspend_session` on a listening session returns
`listener_cannot_suspend` without user approval. Low-disk policy (below
7 GiB free or webd budget exceeded) empties ephemeral caches and persistent
`cache_storage`/`shader_cache` of non-listening origins only. `serve`
refuses to grow below the configured reserve. Telemetry adds
`browser_working_set_bytes`, `browser_pss_bytes`, `browser_instances` and
`browser_tabs` with peaks, separate from harness and container RAM.

```sh
cargo test -p excalibur-webd --test resources_gates -- --nocapture
cargo test -p excalibur-webd --test resources_measure -- --nocapture
```

`resources_gates` verifies only the decision-layer subset of the 13.6 rows
over synthetic fixtures only (budget constants, moving-average admission,
eviction order 1-7 with LIVE protection, disk gates,
`listener_cannot_suspend`, telemetry peaks); the full 13.6 closure stays OPEN
for the main lane. `resources_measure` runs the 18.3 procedure shape (blank
instance, static local page, ten heavy ephemeral tabs, level A/B/C with PSS
by type, 3 repetitions with median/min/max) with the report in
`target/test-evidence/browser/resources/measure.json`. Logged-in
WhatsApp/Teams figures (18.4, 1 h/24 h) stay pending-user and are never
claimed. Pending for full 13.6: real cgroup with `memory.high`, heavy-tab
fill with LIVE continuity, real wait to `capacity_exceeded`, suspend via the
session lane, LIVE over 1 GiB with `logged_in`, small-filesystem gate and
sampling-cost microbench (never claimed). `serve` retains the `ResourceGate`,
enforces the configured disk reserve on the data and run directories at
startup with startup/shutdown telemetry, and documents per-operation memory
admission as pending main-lane work (serve does not mediate tab creation).
The wait path reports `capacity_exceeded` only after the 120 s eviction wait
expires; `listener_cannot_suspend` is enforced in `sessions::Manager`, with
the eviction and webd helpers only computing the decision. See [Browser
resource foundation](browser-resources.md).

## Harness integration

`excalibur-web-adapter::harness` provides the harness `ToolRuntime`, the
tool catalog and the capability manifests (arch 16). The twelve `browser_*`
tools register as deferred (outside the eager prefix, through
`search_tools`/`call_tool`); `web_search` and `web_read` are core (arch
14.3). Static metadata follows 16.2 (effect, idempotency, risk,
`timeout_ms`, fast, cancel class); `browser_act` downgrades its effect per
call in `prepare` (`observe` to `Pure`, `input` to `Idempotent`, commit
classes stay `NonIdempotent`).

Reconciliation follows 16.3: pure tools retry, `browser_navigate` probes the
tab URL (`Done` when the tab already shows the target document, `Retry`
otherwise), keyed tools resend with the same key, and commit classes consult
`op.get(op_key)` (`completed` to `Done` with the stored result, `received`
to `Retry`, `failed`/`dispatched`/restarted to `Uncertain`, never repeated
blindly). Cancellation follows 16.5: `immediate` stops client-side polling,
`graceful` and `after_completion` additionally close the run's tracked
ephemeral sessions when cancelled while persistent sessions stay alive;
`execution_state` releases the run tracking on pause and terminal states.
The eval profile (16.8) denies persistent profiles, blocks benchmark
sources in `web_read`/`browser_navigate`/search results and turns human
blocks into immediate failures. Registration in the host composition
(`DeferredTools(ExecutionTools(SecuredTools(...)))` chain in
`crates/eval/src/runner.rs`) happens only when the harness is explicitly
enabled via `EXCALIBUR_WEB_HARNESS=1` with `EXCALIBUR_WEB_SOCKET` pointing at
the webd socket and the host network flag allowing web tools; the default path
is unchanged. A dedicated `op.cancel` RPC
for in-flight dispatched operations is still pending in webd, so
cancellation of already-dispatched work relies on client-side polling stop
plus ephemeral cleanup. Takeover stays a thin wrapper: `takeover_start`
calls the existing `takeover.start` RPC and `viewer_issue` calls the
existing `viewer.issue` RPC with the same owner checks; no new
authorization is introduced. Wiring the `excalibur browser takeover
<session>` command into the CLI binary is pending (the CLI currently only
implements `exec`).

```sh
cargo test -p excalibur-web-adapter
cargo test -p excalibur-cdp -p excalibur-browser -p excalibur-search -p excalibur-web-adapter
cargo test -p excalibur-webd --test web_integration -- --nocapture
cargo fmt --check
cargo clippy --workspace --all-targets -- -D warnings
```

`web_integration` covers the 16.10 rows over local synthetic fixtures only:
an ephemeral harness walkthrough (open, navigate a local URL, wait,
snapshot, input action, `web_read`) with source events, crash-before-result
reconciliation to `Done` without resend, webd restart turning a dispatched
commit `Uncertain` without resend, listener relaunch bookkeeping,
cancellation closing ephemeral sessions while persistent ones stay alive,
`search_tools` retrieval returning `browser_snapshot`/`browser_act` in the
top five with a stable catalog hash, eval denials, and the reused-key
conflict. Real-daemon crash rows and the 18.4 manual runs with real sites
stay pending-user and are never claimed.
