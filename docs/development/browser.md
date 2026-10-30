# Browser environment

The workspace reserves `excalibur-cdp`, `excalibur-browser`, `excalibur-search` and
`excalibur-web-adapter` as empty library boundaries. `excalibur-webd` implements
Linux environment checks only. It does not serve requests or launch browser
sessions, and is not registered with the host tools.

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

`ready` means environment preparation succeeded. `sandbox_verified` remains
false: renderer sandbox verification and browser launching are not implemented.
Chrome is invoked only with `--version` in a separate process group. A five-second
total deadline covers the process and stdout, including output held open by
descendants. The group is killed on completion or error; output is capped at
4096 bytes. No remote
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
