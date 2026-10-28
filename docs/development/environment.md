# Development environment

[Español](environment.es.md)

Excalibur currently contains a Rust provider library. The agent, CLI, Axum server and frontend are planned; there is no application or development server to start yet. Do not treat the dependency checks below as implemented product features.

## Environment checked on this VPS

- Ubuntu 24.04, x86-64; Rust stable 1.99.1 and Cargo 1.99.1 via rustup, with rustfmt and Clippy; GCC 13.3 and pkg-config 1.8.1.
- Node.js 24.22.1, npm 11.19.1 and pnpm 10.34.9 are installed in `/opt/excalibur-toolchain/node24`. The Node archive was checked against the published SHA-256 checksum. The system `/usr/bin/node` remains version 18; it is too old for current Vite.
- To use the isolated installation in a shell on this VPS:

  ```sh
  export PATH="/opt/excalibur-toolchain/node24/bin:$PATH"
  node --version
  pnpm --version
  ```

The `/opt` path is local to this machine. Elsewhere, install a compatible Node.js LTS and pnpm using their official instructions. Current Vite requires Node 20.19+ or 22.12+; choose a supported LTS ([Vite](https://vite.dev/guide/), [Node releases](https://nodejs.org/en/about/previous-releases), [pnpm](https://pnpm.io/installation)).

## Dependency verification

A temporary Rust project compiled Axum 0.8.9 and Tokio 1.54.1 with this toolchain. Axum is not yet a dependency of Excalibur; Tokio is used by the existing provider library tests. A separate temporary pnpm project installed and imported React 19.3.1, React DOM 19.3.1, Vite 8.3.5, its React plugin 6.1.2, Tailwind CSS and its Vite plugin 4.3.3, and TypeScript 7.0.2. `vite --version` and `tsc --version` ran successfully. These are versions observed in the temporary check, not dependencies pinned or installed into this repository; choose and lock compatible versions when the frontend is actually created.

## Existing Rust checks

From the repository root:

```sh
cargo fmt --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

Backend tests cover logic, data and permissions, using mocks for provider calls. Once a frontend exists, visual-only work uses its development server and visual review at small and large screen sizes; do not run a production build for each visual-only change or add frontend functional test suites. Apply compilation, types and lint checks as appropriate to other changes. No `pnpm install` or `pnpm dev` command applies to this repository yet.
