# Auditoria B5 retry — correction 1 at 4ad646e (meta)

- **Commit auditado:** `4ad646e01f6173552c7a17f9694c3b289f7ee974` (correction 1 sobre `e662bac`)
- **Fecha de auditoria:** 2026-10-30 (UTC, Europe/Berlin)
- **Alcance:** SOLO commit `4ad646e`; B5 network/content/blocks/intervention correction 1 (arch 12.3, C2-C4, 12.6, 12.5, 9.1-9.3; cierra 9.5/12.9/18.5; NB-05/10/15/16)
- **Rol:** auditor independiente. Sin modificaciones a producto, arquitectura ni Git; sin commits; arbol limpio en `4ad646e` tras la auditoria.
- **Veredicto:** `approved` para el alcance B5 (8/8 requisitos verificados con Chrome real donde aplica; B5-01 cerrada con evidencia en este arbol; F-B4-01 sigue cerrada con evidencia en `e662bac` aun ancestro).

## Base documental y referencias

- `AGENTS.md` linea B5 (15 denegaciones, viewer servido, matrices 9.5, viewport 18.5; pendientes honestos F-B4-03/F-B4-04 y elevacion de document-status).
- `/home/cristhian/projects/Excalibur/browser/arquitectura-navegador.md` secs 5.4-5.5, 9.1-9.3, 9.5, 12.3-12.6, 12.9, 18.5, 19.2 (fila B5), NB-05/10/15/16.
- Informes previos: `b5-meta.md` (8/8 en `e662bac` con B5-01 abierta), `b4-reaudit2-meta.md` (F-B4-01 abierta pendiente de B5, F-B4-03/F-B4-04 fuera de B5).
- Frontend: solo visual (capturas del visor servido revisadas como artefactos `small.png`/`large.png`); sin herramientas de agente registradas.

## Alcance entregado por el commit (2 ficheros, +4/-8)

- Docs pareadas `docs/development/browser.md` / `browser.es.md`: sustituye las 3 frases residuales de B5-01 por la descripcion del visor servido por `excalibur-webd --serve` sobre TCP real (codigo un solo uso, cookie `HttpOnly;SameSite=Strict`, `Origin` exacto, CSP estricta, shell) cubierto por `viewer_http` y `viewer_viewport` con fixtures sinteticas locales, con evidencias en `target/test-evidence/browser/viewer/`.
- Sin cambios de codigo, sin directorios nuevos (nombres en ingles intactos), mensaje en lenguaje natural. `git diff e662bac..HEAD --stat`: 2 ficheros.
- `e662bac` es ancestro de `HEAD` (verificado por `merge-base --is-ancestor`); el codigo B5 verificado en `b5-meta.md` sigue intacto en este arbol.

## Requisitos y evidencia (8/8)

| # | Requisito | Estado | Evidencia en este arbol |
|---|---|---|---|
| R1 | 12.3 red: oyente loopback con UID, `HostProxy`, flags `<-loopback`/resolver/WebRTC/QUIC | verificado | `cargo test -p excalibur-browser --lib` 36 passed incluye `net::check_loopback_uid`/`loopback::check_peer`; `cargo test -p excalibur-browser --test policy` 6 passed incluye `flags_cannot_override_security_transport_or_profile` |
| R2 | C2-C4 + cierre F-B4-01: origenes admitidos por documento, esquemas prohibidos tambien en efimera, `note_ephemeral_origin`, `navigation_risk` | verificado | `sessions::lifecycle_tests::ephemeral_gate_denies_schemes_tracks_origins_and_flags_c4` en lib 36 passed; fila `http://login.test/ => denied (human_only_origin)` en `target/test-evidence/browser/block_matrix/matrix.json` regenerado en este arbol |
| R3 | 12.6 permisos y esquemas: 15 denegaciones `Browser.setPermission` antes de navegar | verificado | `permissions::denies_the_full_sensitive_set` + `denial_params_use_browser_set_permission_shape` en lib 36 passed |
| R4 | 12.5 contenido: mundos aislados, readiness determinista, sin HTML no renderizado | verificado | `wait.rs` (`excalibur-ready`/`excalibur-listener`/`excalibur-actions`) en lib 36 passed; `observe.rs` actualizado a `blocked_captcha` |
| R5 | 9.1 detectores + 9.5 matrices: 7 patrones bloquean, 3 controles no, sin falsos positivos | verificado | `cargo test -p excalibur-browser --test block_matrix -- --nocapture` 1 passed, 1 ignored (child), Chrome 156.0.8078.79 real; `target/test-evidence/browser/block_matrix/matrix.json` (7 blocked + 3 unblocked + login + denial) |
| R6 | 9.2 intervencion: `takeover.start/end`, 15 min (899 s resuelve / 901 s expira), epoch bump + `stale_ref`, `input_required` vs `blocked_requires_human` | verificado | `takeover_expiry_returns_to_blocked_with_epoch_bump`, `blocked_maps_to_input_required_or_immediate_failure`, `viewer_codes_are_single_use_tab_bound_and_expiring` en lib 36 passed; reinicio durable en `cargo test -p excalibur-webd --test webd_boundary -- --nocapture` 1 passed |
| R7 | 9.3 visor servido + 18.5 revision: redeem-once, expira/reuso/desconocido, URL ignorada, sin cookie, origen ajeno, handshake 101, CSP, viewports | verificado | `cargo test -p excalibur-webd --test viewer_http` 2 passed (TCP real); `cargo test -p excalibur-webd --test viewer_viewport -- --nocapture` 1 passed, 1 ignored, Chrome real; `target/test-evidence/browser/viewer/{small,large}.png + viewports.json` revisados visualmente |
| R8 | Docs EN/ES pareadas + `AGENTS.md` con comandos B5 y limites honestos; B5-01 corregida | verificado | `grep` confirma 0 frases residuales (`no served viewer endpoint yet`/`sin endpoint servido todavia`/`no se sirve ningun endpoint`) y nuevas frases servidas en `browser.md:11,397` y `browser.es.md:12,420`; `AGENTS.md:41` intacta y correcta; `git status --short` limpio |

## Checks (formato/clippy/tests/build)

- `cargo fmt --check` -> salida 0, limpio.
- `cargo clippy --workspace --all-targets -- -D warnings` -> salida 0, sin avisos (primera pasada completa con `CARGO_BUILD_JOBS=2` bajo `flock heavy-build.lock`; segunda pasada `Finished dev profile in 1.09s`).
- tests (todos bajo `flock heavy-build.lock` + `CARGO_BUILD_JOBS=2`, target del repo sin borrar artefactos ajenos):
  - `cargo test -p excalibur-browser --lib` -> 36 passed, 0 failed.
  - `cargo test -p excalibur-webd --lib` -> 15 passed, 0 failed.
  - `cargo test -p excalibur-webd --test viewer_http` -> 2 passed, 0 failed (TCP real).
  - `cargo test -p excalibur-browser --test block_matrix -- --nocapture` -> 1 passed, 1 ignored (child), Chrome real + `matrix.json`.
  - `cargo test -p excalibur-webd --test viewer_viewport -- --nocapture` -> 1 passed, 1 ignored (child), Chrome real + `viewer/small.png,large.png,viewports.json`.
  - `cargo test -p excalibur-webd --test webd_boundary -- --nocapture` -> 1 passed (restart + parent death, `contract.json` con Chrome 156).
  - `cargo test -p excalibur-browser --test policy` -> 6 passed (incluye `flags_cannot_override_security_transport_or_profile`).
- `cargo build -p excalibur-webd` -> salida 0 (`Finished dev profile in 0.28s` en pasada cached; binario `target/debug/excalibur-webd` 221M del 2026-10-30 17:47).
- Frontend solo visual: `small.png` (360px, botones apilados) y `large.png` (1440px, botones en fila, canvas 1:1) revisadas con visor de imagenes; texto como texto, sin HTML crudo.
- No re-ejecutadas (fuera del alcance B5, como en `b5-meta.md`): suites completas `sessions` (fixture 600 s), `cdp_contract`, tests del sandbox/harness.

## Hallazgos (ninguno bloqueante; IDs conservados)

- **B5-01 - baja - resuelta - no bloquea**: 3 frases residuales en `b5-meta.md` contradecian el visor servido. **Cierre con evidencia en este arbol:** `4ad646e` las sustituye en ambos idiomas; `grep` verifica 0 ocurrencias residuales y presencia de `served by excalibur-webd --serve over real TCP` / `lo sirve excalibur-webd --serve por TCP real` + `viewer_http (real TCP)` / `viewer_viewport (small/large)`; tests `viewer_http`/`viewer_viewport` en verde y artefactos `viewer/` regenerados. No re-auditar.
- **F-B4-01 - media - resuelta - no bloquea**: compuerta efimera que denegaba `file:`/`javascript:`/`data:`/`chrome:` con `note_ephemeral_origin` y `navigation_risk`. **Cierre preservado:** evidencia sigue en el historial (`e662bac` es ancestro de `HEAD`); re-verificado aqui via `ephemeral_gate_denies_schemes_tracks_origins_and_flags_c4` en verde + fila `human_only_origin`/`denied` en `matrix.json`. No re-auditar.

## Resumen y entrega

- Commit auditado (publicado, sin modificar): `4ad646e01f6173552c7a17f9694c3b289f7ee974`. Arbol limpio tras la auditoria.
- Informe: este archivo (`/home/cristhian/.local/state/excalibur-browser-owner/reports/b5-retry-meta.md`).
- Trabajo futuro fuera de alcance (no verificado aqui, solo resumen): elevacion de `document_status` con estado live de red, detectores mas alla del vocabulario fijo 9.1 (cookie-wall/geo-block sin marcadores 9.1 no bloquean), streaming de screencast tras el `101`, F-B4-03 (zeroize de la copia `fill["value"]`) y F-B4-04 (ZIP en `magic()` de `browser/files.rs`) — ambos fuera del conjunto B5 y declarados en docs/AGENTS; arquitectura completa B0-B9 (incluidos buscador NB-20-22/B8 y recursos integrados NB-17-18/B6), cableado broker/harness y registro de herramientas (B9), recorridos manuales 18.4 en sitios reales.
