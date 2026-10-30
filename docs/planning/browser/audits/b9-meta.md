# Auditoría B9 — Integración del arnés con catálogo y puertas eval (meta)

- **Commit auditado:** `7c71cc0aa2ae7750de694060c8e3f643656b2581` (merge de `5b4018c` sobre `9a3742a`)
- **Fecha de auditoría:** 2026-10-30 (UTC)
- **Alcance:** Solo commit `7c71cc0`; B9 harness integration (arq. 16 ToolRuntime/catálogo/manifiestos/reconciliación/cancelación/perfil eval; cierre 16.10; NB-13/24). Sin hallazgos previos.
- **Rol:** auditor independiente. Sin modificaciones a producto, arquitectura ni Git; árbol limpio en `7c71cc0` tras la auditoría.
- **Veredicto:** `approved` para el alcance B9 harness (8/8 requisitos verificados, sin hallazgos bloqueantes)

## Base documental y referencias

- `AGENTS.md` (línea B9 añadida por el commit) y `docs/development/browser.md` / `browser.es.md` (sección "Harness integration" / "Integración con el arnés", pareadas).
- `/home/cristhian/projects/Excalibur/browser/arquitectura-navegador.md` §§16.1–16.10, 17 (takeover), 18.2 (suite `web_integration`), 19.2 (fila B9), NB-13/NB-24.
- Informes previos relevantes: `b8-retry-meta.md` (carril de búsqueda; sin solape: este commit no toca `crates/search`).
- Frontend: no procede (sin visor ni herramientas registradas; el adaptador es frontera de biblioteca).

## Alcance entregado por el commit (11 ficheros, +2055/−3 vs `9a3742a`)

- `crates/web-adapter/src/harness.rs` (nuevo, 1200 líneas) + `pub mod harness` en `lib.rs`: `WebHarness` (`ToolRuntime`), catálogo diferido (12 `browser_*`) + núcleo (2 `web_*`), manifiestos 16.1, reconciliación 16.3/16.4, cancelación 16.5, perfil eval 16.8, envoltorios finos `takeover_start`/`viewer_issue`.
- `crates/eval/src/runner.rs` (+20/−3): composición `DeferredTools(ExecutionTools(SecuredTools(...)))` solo con `EXCALIBUR_WEB_HARNESS=1` + `EXCALIBUR_WEB_SOCKET` + flag de red; `deferred_adapters: ["browser"]` y hash de catálogo en metadatos.
- `crates/webd/tests/web_integration.rs` (nuevo, 710 líneas): 10 tests 16.10 con `FakeBackend` sintético (sin Chrome, sin red, sin demonio).
- `Cargo.toml`/`Cargo.lock`, `crates/web-adapter/Cargo.toml`, `crates/webd/Cargo.toml` (dependencia dev del adaptador), docs EN/ES pareadas.

## Requisitos y evidencia

| # | Requisito | Estado | Evidencia |
|---|---|---|---|
| R1 | 16.2 Catálogo: 12 `browser_*` diferidas + `web_search`/`web_read` núcleo; efecto, idempotencia, riesgo, `timeout_ms`, `fast`, clase de cancelación según tabla; nombres `snake_case` ≤ 64 B; sin CDP crudo/`Runtime.evaluate`/cookies | verificado | `harness.rs::browser_catalog/search_catalog` (valores idénticos a la tabla 16.2); `catalog_metadata_matches_harness_contract` (12+2, `fast` solo `browser_status`/`web_*`, `timeout_ms` 5 000/20 000); T3 10/10 |
| R2 | 16.1 Manifiestos de `browser` (12 operaciones, límites, concurrencia, estados, `blocked_kinds`) y `search` (operaciones, límites, retención) | verificado | `browser_manifest`/`search_manifest` + `manifests_declare_browser_and_search` en T3 |
| R3 | 16.3/16.4 Efecto por llamada y reconciliación: `browser_act` degrada en `prepare` (`observe`→`Pure`, `input`→`Idempotent`, commit→`NonIdempotent`); puras→`Retry`; `navigate` sondea `tab.list`; claveadas reenvían; commit consulta `op.get` (`completed`→`Done`, `received`→`Retry`, `failed`/`dispatched`/reinicio→`Uncertain`, sin repetir); conflicto de clave reutilizada | verificado | `prepare`/`reconcile` en `harness.rs:700-870`; `harness_crash_before_result_reconciles_done_without_resend` (contador 1, sin reenvío), `webd_restart_turns_dispatched_commit_uncertain` (`sends==0`), `idempotency_conflict_on_reused_key_with_different_request`; T3 10/10 |
| R4 | 16.5 Cancelación NB-13: `immediate`/`graceful`/`after_completion` por clase; con `cancelled` cierra efímeras rastreadas y conserva persistentes; `execution_state` libera el rastreo en pausa y estados terminales | verificado | `interrupt_execution`/`execution_state` + `cancellation_closes_ephemeral_and_keeps_persistent` (cierra `s-1` efímera, conserva persistente); T3 |
| R5 | 16.8 Perfil eval: deniega perfiles persistentes, bloquea fuentes benchmark en `web_read`/`browser_navigate`/`web_search`, convierte bloqueos humanos en fallo inmediato | verificado | `eval_gate` + filtrado en `execute` + `eval_profile_denies_persistent_and_benchmark_and_fails_blocks`; T3 |
| R6 | Composición del host: registro solo con `EXCALIBUR_WEB_HARNESS=1` + socket + red; ruta por defecto intacta; `browser_snapshot`/`browser_act` en top-5 de `search_tools` con hash estable; `deferred_adapters` en metadatos | verificado | `wrap_from_env`/`registration_enabled` + `runner.rs`; `catalog_retrieval_returns_snapshot_and_act_in_top_five` (14 herramientas, hash estable); T3; build/eval compilan |
| R7 | Filas 16.10 solo con fixtures sintéticas: recorrido efímero con fuentes, crash-antes-de-resultado, reinicio de webd, relanzamiento de escucha, cancelación, recuperación, denegaciones eval, conflicto de clave | verificado | `web_integration` 10/10 en 0.06 s (`harness_walkthrough_completes_with_evidence` con `sources`, `webd_restart_relaunches_listening_sessions`, resto de la tabla); T3_EXIT=0 |
| R8 | `AGENTS.md` + docs EN/ES pareadas con comandos B9 y pendientes declarados honestamente (sin afirmar lo no implementado) | verificado | diff pareado EN/ES equivalente; línea B9 en `AGENTS.md`; `cargo fmt --check` limpio |

## Comprobaciones ejecutadas (target aislado `/tmp/b9-meta-target`, `CARGO_BUILD_JOBS=2`, `flock heavy-build.lock`; sin tocar artefactos ajenos)

- `cargo fmt --check` → salida 0, limpio (`/tmp/b9-meta-fmt.log`, `FMT_EXIT=0`).
- `cargo clippy --workspace --all-targets -- -D warnings` → salida 0, sin avisos (`/tmp/b9-meta-clippy.log`, `CLIPPY_EXIT=0`).
- `cargo test -p excalibur-web-adapter` → ok (`T1_EXIT=0`; lib 0 tests; `actions` 1 passed, 1 ignored en 107.7 s con Chrome real como usuario dedicado).
- `cargo test -p excalibur-cdp -p excalibur-browser -p excalibur-search -p excalibur-web-adapter` → ok (`T2_EXIT=0`; ~150 passed, 0 failed: browser lib 36, block_matrix 1+1ign, cdp_contract 2+2ign con Chrome real, observe 1+1ign, policy 6, read_cite 13, sessions 5+3ign con fixture de escucha 600 s, transport 10, search 75 en total, actions 1+1ign).
- `cargo test -p excalibur-webd --test web_integration -- --nocapture` → 10 passed, 0 failed (`T3_EXIT=0`).
- `cargo build --workspace` → salida 0 (`/tmp/b9-meta-build.log`, `BUILD_EXIT=0`).
- Fixtures del alcance sintéticas y locales; sin cuentas, sin APIs de pago, sin red externa. Chrome real solo en suites preexistentes del carril (actions/sessions/observe/cdp_contract), no en la suite 16.10.

## Hallazgos

Ninguno (ni bloqueantes ni informativos dentro del alcance). Los pendientes conocidos van solo al resumen, según lo ordenado.

## Resumen

B9 entrega el `ToolRuntime` del arnés con el catálogo 16.2 completo, los manifiestos 16.1, la reconciliación 16.3/16.4 con `op.get`, la cancelación 16.5 por clases con limpieza de efímeras, el perfil eval 16.8 con denegaciones y el registro en la composición del host tras configuración explícita, cubierto por la suite sintética `web_integration` (10/10) y la documentación bilingüe, con `fmt`/`clippy`/`tests`/`build` en verde. **Se aprueba el alcance B9 harness**: 8/8 requisitos verificados con evidencia real y sin hallazgos. Trabajo futuro fuera de este alcance (solo resumen, no verificado aquí): plomería CLI `excalibur browser takeover` (el CLI hoy solo implementa `exec`), RPC `op.cancel` en webd para operaciones ya despachadas (la cancelación actual es paro de sondeo + limpieza efímera), filas con caídas del demonio real y recorridos manuales 18.4 con sitios reales pendientes del usuario — más el resto de la arquitectura completa (B0–B8 con buscador y recursos integrados), que esta auditoría no declara ni evalúa.
