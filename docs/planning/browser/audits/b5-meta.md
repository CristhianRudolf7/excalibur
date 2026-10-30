# Auditoría B5 — Red, contenido, bloqueos e intervención (meta)

- **Commit auditado:** `e662bacb90762c3366e8741b34703013097bda96` (merge de `bd8676c` + `471f908`)
- **Fecha de auditoría:** 2026-10-30 (UTC)
- **Alcance:** Solo commit `e662bac`; B5 network/content/blocks/intervention (arch §§12.3, C2–C4, 12.6, 12.5, 9.1–9.3; cierra 9.5/12.9/18.5; NB-05/10/15/16)
- **Rol:** auditor independiente. Sin modificaciones a producto, arquitectura ni Git; árbol limpio en `e662bac` tras la auditoría.
- **Veredicto:** `approved` para el alcance B5 (8/8 requisitos verificados con Chrome real donde aplica; un hallazgo nuevo bajo no bloqueante B5-01; F-B4-01 cerrada con evidencia de este árbol)

## Base documental y referencias

- `AGENTS.md` (línea B5 actualizada por este commit: 15 denegaciones, viewer servido, matrices 9.5, viewport 18.5; pendientes declarados).
- `/home/cristhian/projects/Excalibur/browser/arquitectura-navegador.md` §§5.4–5.5, 9.1–9.3, 9.5, 12.3–12.6, 12.9, 18.5, 19.2 (fila B5), NB-05/10/15/16.
- Informes previos relevantes: `b4-reaudit2-meta.md` (F-B4-01 abierta pendiente de B5; F-B4-03/F-B4-04 fuera del conjunto B5).
- Frontend: solo visual (capturas del visor servido revisadas como artefactos: `small.png`, `large.png`); sin herramientas de agente registradas (el adaptador sigue siendo frontera de biblioteca).

## Alcance entregado por el commit (16 ficheros, +2635/−72)

- `crates/browser/src/{sessions,takeover,rpc,permissions}.rs` + `observation/{detectors,wait}.rs`: compuerta efímera C2 (`note_ephemeral_origin`), riesgo C4 (`navigation_risk`), normalización al vocabulario 9.1 (`normalize_reported_blocked`), reseteo de `response_blocked` por navegación, 15 denegaciones, `viewer.issue` por RPC, `intervention_outcome` (`input_required`/`blocked_requires_human`), expiración con epoch bump y `blocked_unresolved`.
- `crates/browser/tests/block_matrix.rs` (nuevo, 378 líneas): matrices 9.5 con Chrome real sobre páginas sintéticas locales + controles.
- `crates/webd/src/viewer.rs` (+856): endpoint servido `--serve` en `127.0.0.1:<aleatorio>` (código un solo uso, cookie `HttpOnly;SameSite=Strict`, `Origin` exacto para WS, CSP estricta, `textContent`/canvas, CSS ≥44 px / 1:1 hasta 1280 px).
- `crates/webd/tests/{viewer_http,viewer_viewport}.rs` (nuevos) + `webd_boundary.rs` (+64: recuperación durable con epoch bump y `stale_ref`, compuerta sobrevive al reinicio).
- `crates/webd/src/main.rs` (+15): sirve el visor junto al socket Unix; anuncia `"viewer"` en el JSON de listo.
- Docs pareadas `docs/development/browser.md` / `browser.es.md` (+92/+96) y línea B5 en `AGENTS.md`.

## Requisitos y evidencia

| # | Requisito | Estado | Evidencia |
|---|---|---|---|
| R1 | 12.3 red: oyente loopback con UID, `HostProxy`, flags `<-loopback`/resolver/WebRTC/QUIC | verificado | `net::check_loopback_uid` + `loopback::check_peer` (lib, 15 ok); `policy::flags_cannot_override_security_transport_or_profile` (1 ok); proxy/rebinding en `sandbox/tests/proxy.rs` (preexistente, fuera del diff) |
| R2 | C2–C4 + cierre F-B4-01: orígenes admitidos por documento, esquemas prohibidos también en efímera, `note_ephemeral_origin`, `navigation_risk` | verificado | `sessions::lifecycle_tests::ephemeral_gate_denies_schemes_tracks_origins_and_flags_c4` (browser lib, 36 ok); denegación `human_only_origin`/`denied` en `matrix.json` |
| R3 | 12.6 permisos y esquemas: 15 denegaciones `Browser.setPermission` antes de navegar | verificado | `permissions::denies_the_full_sensitive_set` + `denial_params_use_browser_set_permission_shape` (lib); Chrome 156 rechaza los nombres antiguos (commit msg + código) |
| R4 | 12.5 contenido: mundos aislados, readiness determinista, sin HTML no renderizado | verificado | `wait.rs` (`excalibur-ready`/`excalibur-listener`/`excalibur-actions`), lib 36 ok; `observe.rs` actualizado a `blocked_captcha` |
| R5 | 9.1 detectores + 9.5 matrices: 7 patrones bloquean, 3 controles no, sin falsos positivos | verificado | `block_matrix -- --nocapture` 1 passed con Chrome 156 real; `target/test-evidence/browser/block_matrix/matrix.json` (7 blocked + 3 unblocked + login + denial) |
| R6 | 9.2 intervención: `takeover.start/end`, 15 min (899 s resuelve / 901 s expira), epoch bump + `stale_ref`, `input_required` vs `blocked_requires_human` | verificado | `takeover_expiry_returns_to_blocked_with_epoch_bump`, `blocked_maps_to_input_required_or_immediate_failure`, `viewer_codes_are_single_use_tab_bound_and_expiring` (lib); reinicio durable en `webd_boundary` (1 passed) |
| R7 | 9.3 visor servido + 18.5 revisión: redeem-once, expira/reuso/desconocido, URL ignorada, sin cookie, origen ajeno, handshake 101, CSP, viewports | verificado | `viewer_http` 2 passed (TCP real); `viewer_viewport -- --nocapture` 1 passed con Chrome real; `target/test-evidence/browser/viewer/{small,large}.png + viewports.json` |
| R8 | Docs EN/ES pareadas + `AGENTS.md` con comandos B5 y límites honestos | verificado | `browser.md`/`browser.es.md` (+92/+96), línea B5 en `AGENTS.md`; pendientes declarados (elevación por estado live, vocabulario >9.1, F-B4-03/04). Ver B5-01 por 3 frases residuales |

## Comprobaciones ejecutadas (blanco pesado con `flock …/heavy-build.lock`, `CARGO_BUILD_JOBS=2`, target aislado `/tmp/excalibur-b5meta-target`, sin tocar artefactos ajenos)

- `cargo fmt --check` → salida 0, limpio.
- `cargo clippy --workspace --all-targets -- -D warnings` → salida 0, sin avisos (`/tmp/b5meta-clippy.log`).
- `cargo test -p excalibur-browser --lib` → 36 passed, 0 failed (`/tmp/b5meta-t1.log`).
- `cargo test -p excalibur-webd --lib` → 15 passed, 0 failed (`/tmp/b5meta-t2.log`).
- `cargo test -p excalibur-webd --test viewer_http` → 2 passed, 0 failed (`/tmp/b5meta-t3.log`).
- `cargo test -p excalibur-browser --test block_matrix -- --nocapture` → 1 passed, 1 ignored (child por diseño), Chrome 156.0.8078.79 real como `excalibur-web` (`/tmp/b5meta-t4.log` + `target/test-evidence/browser/block_matrix/`).
- `cargo test -p excalibur-webd --test viewer_viewport -- --nocapture` → 1 passed, 1 ignored, Chrome real (`/tmp/b5meta-t5.log` + `target/test-evidence/browser/viewer/`).
- `cargo test -p excalibur-webd --test webd_boundary -- --nocapture` → 1 passed (`/tmp/b5meta-t6.log`).
- `cargo test -p excalibur-browser --test policy flags_cannot_override_security_transport_or_profile` → 1 passed (`/tmp/b5meta-t7.log`).
- `cargo build -p excalibur-webd` → salida 0 (`/tmp/b5meta-b1.log`).
- No re-ejecutadas (fuera del alcance B5, como en b4-reaudit2): suites completas `sessions` (fixture de escucha 600 s), `cdp_contract`, tests del sandbox/harness.

## Hallazgos

- **B5-01 · baja · no bloquea · abierta**: 3 frases residuales contradicen el visor servido nuevo: intro EN (`browser.md:11` «no served viewer endpoint yet»), intro ES (`browser.es.md:12` «sin endpoint servido todavía») y sección de sesiones EN (`browser.md:~398`) / ES (`browser.es.md:421-422` «no se sirve ningún endpoint del visor para revisión visual»), mientras §B5 documenta `--serve` + tests `viewer_http`/`viewer_viewport` en verde. **Recomendación:** actualizar esas 3 frases en ambos idiomas; no bloquea el cierre B5.
- **F-B4-01 · media · no bloquea · resuelta**: la compuerta efímera ahora deniega `file:`/`javascript:`/`data:`/`chrome:` en sesiones efímeras (`document_allowed`), registra orígenes C2 y reporta riesgo C4. Evidencia: `sessions::lifecycle_tests::ephemeral_gate_denies_schemes_tracks_origins_and_flags_c4` en verde + fila `http://login.test/ => denied (human_only_origin)` en `matrix.json`. **Cierre:** verificado en este árbol; no re-auditar.

## Resumen y entrega

- Commit auditado (publicado, sin modificar): `e662bacb90762c3366e8741b34703013097bda96`. Árbol limpio tras la auditoría.
- Informe: este archivo (`/home/cristhian/.local/state/excalibur-browser-owner/reports/b5-meta.md`).
- Trabajo futuro fuera de alcance (no verificado aquí, solo resumen): elevación de `document_status` con estado live de red, detectores más allá del vocabulario fijo 9.1 (cookie-wall/geo-block sin marcadores 9.1 no bloquean), streaming de screencast tras el `101`, F-B4-03 (zeroize de la copia `fill["value"]`) y F-B4-04 (ZIP en `magic()` de `browser/files.rs`) — ambos fuera del conjunto B5 y declarados en docs/AGENTS; arquitectura completa B0–B9 (incluidos buscador NB-20–22/B8 y recursos integrados NB-17–18/B6), cableado broker/harness y registro de herramientas (B9), recorridos manuales 18.4 en sitios reales.
