# Auditoría B4 — acciones, diálogos, archivos y credenciales (re-auditoría 3, meta)

- **Hito y commit**: B4 · `9a374a274631d626946bd5c773d18ef2016994a9` («Integrate browser resource budgets into webd»), rama auditada en `/home/cristhian/projects/Excalibur/browser/excalibur-review-b4-reaudit3`.
- **Alcance auditado**: `arquitectura-navegador.md` §19.2 B4 con 8.1–8.4, 10 y 11; cierres 8.5, 10.5 y 11.3; requisitos NB-09, NB-11 y NB-12.
- **Fecha (UTC)**: 2026-10-30. **Auditor**: subagente AUDITOR independiente (sesión aislada, sin subagentes ni puente de auditoría).
- **Reglas cumplidas**: solo se auditó el commit publicado; sin cambios de producto, arquitectura, Git ni commits. `git status` limpio (`target/` propio, ignorado). Compilaciones y tests bajo `flock /home/cristhian/.local/state/excalibur-build/heavy.lock` con `CARGO_BUILD_JOBS=2` en el `target/` del propio worktree, sin tocar ni borrar artefactos ajenos. `EXCALIBUR_BROWSER_TESTS` no se usó (sin `skip`).

## Veredicto

**APROBAR B4** (solo B4). Todos los cierres 8.5/10.5/11.3 verificados con pruebas ejecutadas por esta auditoría. Sin hallazgos bloqueantes. Los cuatro hallazgos previos F-B4-01..F-B4-04 se conservan abiertos y no bloquean (evidencia abajo). Esta aprobación **no** declara la arquitectura completa (B0–B3, B5–B9, buscador y recursos siguen en sus hitos).

## Pruebas ejecutadas (órdenes exactas y resultado)

| Orden | Resultado |
|---|---|
| `cargo fmt --check` | Código 0 |
| `flock /home/cristhian/.local/state/excalibur-build/heavy.lock env CARGO_BUILD_JOBS=2 cargo clippy -p excalibur-browser -p excalibur-web-adapter -p excalibur-security -p excalibur-vault -p excalibur-cdp --all-targets -- -D warnings` | Código 0 |
| `flock /home/cristhian/.local/state/excalibur-build/heavy.lock env CARGO_BUILD_JOBS=2 cargo build -p excalibur-browser -p excalibur-web-adapter -p excalibur-security -p excalibur-vault` | Código 0 (3 m 13 s) |
| `flock /home/cristhian/.local/state/excalibur-build/heavy.lock env CARGO_BUILD_JOBS=2 cargo test -p excalibur-browser --lib observation` | Código 0; **10 aprobadas** (clases deterministas, límites de petición/teclas, orígenes exactos HTTPS sin userinfo, nombres saneados, tipos por contenido, más 5 de detectores B5 que no alteran rutas B4) |
| `flock /home/cristhian/.local/state/excalibur-build/heavy.lock env CARGO_BUILD_JOBS=2 cargo test -p excalibur-web-adapter --test actions -- --nocapture` | Código 0; **1 aprobada + 1 auxiliar `ignored`** (`action_child` ejecutada por la prueba padre); **122,91 s** con Chrome real 156.0.8078.79 como usuario `excalibur-web`, fixture HTTPS local con CA temporal (`openssl` + `certutil` presentes) |
| `git diff --check` | Código 0 (sin errores de blancos) |

## Evidencia

- `target/test-evidence/browser/actions/contract.json` — cubre cada acción sobre su control, botón tapado (`overlay_rejected` sin clic), botón en movimiento (`moving:true` y acierto), arrastres de ratón y HTML5, alerta con `alerts[0].message="fixture alert"` y `confirm` con `dialog.type="confirm"` + `dialog_open`, ventanas emergentes efímeras adoptadas (`ephemeral_popup`) y persistentes bloqueadas (`popup.popup_blocked[0].reason="origin_not_writable"`), denegación fuera de `rw`, aprobación de `commit` antes del efecto (`approval_before_commit:true`) con resultado repetido una sola vez (`commit_replayed_once:true`), clic desconocido→`commit` (`unknown_commit`), subida con contenido exacto (`upload_exact`), descargas no armada/armada/sobredimensionada (`large_download="download_too_large"`), descarga admitida con `blob` SHA-256 `52daf414…`, `mime="application/pdf"`, nombre `../../etc/passwd` aislado a `_.._etc_passwd`, relleno con `credential_filled.filled:true`, `credential_resolutions:3` (solo el relleno legítimo resolvió), `vision_suppressed:true`, `race_rejected` sin escritura, caída simulada tras la marca de uso con `crash_retry="secret operation is already dispatched; reconciliation is required"` y `secrets_absent` más búsqueda propia del valor `fixture-site-password-not-for-context-746912` ausente en `contract.json` y `runner.log`.
- `target/test-evidence/browser/actions/runner.log` — `test action_child ... ok`; el aviso de pánico del worker `tokio-rt-worker` en `crates/web-adapter/tests/actions.rs:229` (`simulated host crash between secret use mark and fill`) es el ruido esperado de la caída simulada, convertido en error de reconciliación con estado de salida correcto.
- Núcleo B4 intacto en este commit: `git diff d8db5985..9a374a2 -- crates/browser/src/observation/actions.rs crates/browser/src/observation/files.rs crates/security crates/vault crates/web-adapter` vacío; solo cambiaron `observation/mod.rs` (añade `pub mod detectors`), `rpc.rs` (rutas `takeover.start/end`, `viewer.issue`) y `sessions.rs` (extensiones B5: `ro`, `credential_frames`, C2/C4, takeover), sin alterar rutas B4.

## Mapeo de cierres (todas las filas verificadas)

- **8.5** (9 filas): acciones por control, `not_actionable{covered_by}` (`overlay_rejected`), estabilidad (`moving`), idempotencia de `type`, `commit`/riesgo alto + aprobación previa en persistente (`approval_before_commit`), desconocido→`commit` (`unknown_commit`), `dialog_open` + bloqueo posterior, `popup_blocked`, `denied(origin_not_writable)`. Verificado en `contract.json` + código (`classify`, `actionable`, `adopt_action_popups`).
- **10.5** (6 filas): `filled:true` + valor en página; 4 rechazos sin resolución (iframe otro origen, HTTP, userinfo, subdominio, `credential_resolutions:3`) + carrera sin escritura (`race_rejected`); ausencia del valor en `webd.db`/registros/eventos/auditoría/blobs (`secrets_absent`, búsqueda propia negativa); `[redacted]` + `vision_suppressed:true`; `secret_not_bound`; caída tras marca → fallo cerrado + reconciliación sin doble relleno (`crash_retry`). Verificado en test + `credential_target`/`fill_secret` (`actions.rs`), `SecretExecutor` (`security/executor.rs`), `rpc.rs` (`act.fill_secret` con `SecretValue` + `line.fill(0)`).
- **11.3** (6 filas): `download_blocked` sin disco; PDF armado con blob/hash/tipo y cuarentena vacía después; >200 MiB cancelado (`download_too_large`); nombre con traversal saneado a GUID (`_.._etc_passwd`); subida de ruta protegida/enlace fugitivo/inexistente rechazada en el adaptador sin llegar a webd; subida exacta. Verificado en test + `files.rs` (`private` 0700, `O_NOFOLLOW`, GUID validado como UUID, rechequeo de origen, `DownloadGuard`) y `web-adapter/lib.rs` (`stage_path` con autoridad de archivos + `stage_blob` + `admit_download`).
- **Fidelidad documental**: `docs/development/browser.md` y `browser.es.md` mantienen la sección de acciones/diálogos/archivos/credenciales en ambos idiomas, comandos de verificación B4 y límites honestos (sin ajustes de clase por sitio; adaptador como frontera de biblioteca sin registro de herramientas). `AGENTS.md` solo añade las líneas de verificación B4/B5/B6. Mensaje del commit en lenguaje natural. Sin directorios nuevos (nombres en inglés intactos). `crates/browser/tests/cdp_contract.rs` amplía el catálogo con `TargetGetTargetInfo`; `sessions.rs` añade `credential_frames` + política `deny` de descargas por contexto.

## Hallazgos (ninguno bloqueante; IDs conservados)

- **F-B4-01 · media · abierta · no bloquea** · Evidencia en `9a374a2`: `crates/browser/src/sessions.rs:1197-1222` (`Manager::document_allowed`) mantiene `if s.profile == "ephemeral" { return Ok(()); }` (líneas 1205–1206); `note_ephemeral_origin` (1123–1132) solo registra el origen navegado y `navigation_risk`/C4 solo informa riesgo, sin denegar. La autorización de dominio del arnés y la compuerta de documentos por `Fetch` (C2–C4) pertenecen a **B5** según §19.2. La compuerta B4 (clase/riesgo/aprobación por efecto en el adaptador + rechequeo de origen en `prepare`/`perform`) sí está implementada. **Recomendación**: verificar C2–C4 en la auditoría B5; no re-auditar aquí.
- **F-B4-02 · baja · abierta · no bloquea** · Evidencia en `9a374a2`: `docs/development/browser.md:584-585` declara «Per-site class overrides are not implemented»; `crates/browser/src/observation/actions.rs::classify` no consulta configuración por sitio. El defecto es conservador (desconocido→`commit` en persistente). **Recomendación**: implementar en seguimiento o matizar la frase permisiva de §8.3.
- **F-B4-03 · info · abierta · no bloquea** · Evidencia en `9a374a2`: `crates/web-adapter/src/lib.rs:161` (`fill["value"] = json!(text)`) copia el secreto a un `Value` sin `zeroize` explícito; el transporte (`rpc.rs:613` con `line.fill(0)`), `SecretValue` y `actions.rs:878` (`Zeroizing`) sí sanean las suyas. Solo memoria; la ausencia en disco/eventos/salida está probada (`secrets_absent`, búsqueda negativa). **Recomendación**: sanear la copia del adaptador tras la petición.
- **F-B4-04 · info · abierta · no bloquea** · Evidencia en `9a374a2`: `crates/browser/src/observation/files.rs:324-336` (`magic()`) no detecta ZIP, mientras `crates/security/src/file_control.rs:15-16` sí (`PK\x03\x04` → `application/zip`), que es el control efectivo en `admit_download`; solo afecta al `mime` del blob intermedio de webd. **Recomendación**: unificar ambas tablas.

## Pendientes / no verificado (fuera del alcance B4, no bloquean este hito)

- Arquitectura completa B0–B3, B5–B9 y NB restantes (incluidos buscador NB-20–22 y recursos NB-17–18/B6): pendientes de sus hitos; esta aprobación cubre solo B4.
- Suites completas `sessions` (fixture de escucha de 600 s) y `cdp_contract`: no re-ejecutadas aquí por la carga y la latencia sensible en paralelo; las rutas de sesión tocadas por B4 (apertura, pestañas, ventanas, arriendos, política de descargas) quedan ejercitadas por el test `actions` con Chrome real.
- Recorridos manuales 18.4 en sitios reales: fuera del alcance B4.
- Frontend: sin nada que revisar visualmente para B4 (sin visor ni herramientas registradas en este alcance; el visor servido por webd pertenece a B5 y su revisión visual a 18.5).

## Entrega

- Commit auditado (publicado, sin modificar): `9a374a274631d626946bd5c773d18ef2016994a9`.
- Informe: este archivo (`/home/cristhian/.local/state/excalibur-browser-owner/reports/b4-reaudit3-meta.md`).
