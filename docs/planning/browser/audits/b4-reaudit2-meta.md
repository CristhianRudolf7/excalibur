# Auditoría B4 — acciones, diálogos, archivos y credenciales (re-auditoría 2, meta)

- **Hito y commit**: B4 · `d8db5985b3181c1385cec8d8aa75e4e95738ab84` («Add browser input actions, dialogs, staged uploads, quarantined downloads and reference-only credential filling»), rama `browser`.
- **Alcance auditado**: `arquitectura-navegador.md` §19.2 B4 con 8.1–8.4, 10 y 11; cierres 8.5, 10.5 y 11.3; requisitos NB-09, NB-11 y NB-12.
- **Fecha (UTC)**: 2026-10-30. **Auditor**: subagente AUDITOR independiente (sesión aislada, sin subagentes ni puente de auditoría).
- **Reglas cumplidas**: solo se auditó el commit publicado; sin cambios de producto, arquitectura, Git ni commits. `git status` limpio (solo `target/` propio, ignorado). Compilaciones bajo `flock /home/cristhian/.local/state/excalibur-build/heavy.lock` con `CARGO_BUILD_JOBS=2` en el `target/` del propio worktree, sin tocar artefactos ajenos.

## Veredicto

**APROBAR B4** (solo B4). Todos los cierres 8.5/10.5/11.3 verificados con pruebas ejecutadas por esta auditoría. Sin hallazgos bloqueantes. Esta aprobación **no** declara la arquitectura completa (B0–B3, B5–B9, buscador y recursos siguen en sus hitos).

## Pruebas ejecutadas (órdenes exactas y resultado)

| Orden | Resultado |
|---|---|
| `cargo fmt --check` | Código 0 |
| `flock … CARGO_BUILD_JOBS=2 cargo clippy -p excalibur-browser -p excalibur-web-adapter -p excalibur-security -p excalibur-vault -p excalibur-cdp --all-targets -- -D warnings` | Código 0 |
| `flock … CARGO_BUILD_JOBS=2 cargo build -p excalibur-browser -p excalibur-web-adapter -p excalibur-security -p excalibur-vault` | Código 0 (3 m 04 s en frío) |
| `cargo test -p excalibur-browser --lib observation` | Código 0; **5 aprobadas** (clases deterministas, límites de petición/teclas, orígenes exactos HTTPS sin userinfo, nombres saneados, tipos por contenido) |
| `cargo test -p excalibur-web-adapter --test actions -- --nocapture` | Código 0; **1 aprobada + 1 auxiliar `ignored` ejecutada por la prueba padre; 109,74 s** con Chrome real como usuario `excalibur-web`, fixture HTTPS local con CA temporal (`openssl` + `certutil` presentes) |
| `git diff --check 3a61e35..d8db5985` | Código 0 (sin errores de blancos) |

## Evidencia

- `target/test-evidence/browser/actions/contract.json` — cubre cada acción sobre su control, botón tapado (`not_actionable` sin clic), botón en movimiento, escritura idempotente, arrastres de ratón y HTML5, alerta/`confirm` (`dialog_open` + rechazo de acción no-`dialog`), ventanas emergentes efímeras adoptadas y persistentes bloqueadas (`popup_blocked`), denegación fuera de `rw`, aprobación de `commit` antes del efecto con resultado repetido una sola vez (`commit_replayed_once`), `click` desconocido en persistente → `commit`, subida con contenido exacto, descargas no armada/armada/sobredimensionada (`download_too_large`), nombre `../../etc/passwd` aislado (`_.._etc_passwd`), relleno con redacción y `vision_suppressed`, rechazos entre orígenes/subdominio/HTTP/userinfo sin resolución (`credential_resolutions` sin incremento), carrera de navegación entre comprobación y relleno sin escritura, caída simulada tras la marca de uso sin reintento (`crash_retry` = reconciliación exigida) y ausencia del secreto en archivos/eventos/auditoría/salida.
- `target/test-evidence/browser/actions/runner.log` — `action_child … ok`; el aviso de pánico del worker `tokio-rt-worker` es el ruido esperado de la caída simulada (línea 229 del test), convertido en error de reconciliación con estado de salida correcto.
- Búsqueda propia: `fixture-site-password…` ausente en `target/test-evidence/` (además de las aserciones del propio test sobre `webd.db`, blobs, eventos, auditoría y salida).

## Mapeo de cierres (todas las filas verificadas)

- **8.5** (9 filas): acciones por control, `not_actionable{covered_by}`, estabilidad, idempotencia de `type`, `commit`/riesgo alto + aprobación previa en persistente, desconocido→`commit`, `dialog_open` + bloqueo posterior, `popup_blocked`, `denied(origin_not_writable)`. Verificado en `contract.json` + código (`classify`, `actionable`, `adopt_action_popups`).
- **10.5** (6 filas): `filled:true` + valor en página; 4 rechazos sin resolución (iframe otro origen, HTTP, userinfo, subdominio) + carrera sin escritura; ausencia del valor en `webd.db`/registros/eventos/auditoría/blobs; `[redacted]` + `vision_suppressed:true` + `screenshot_ref:null`; `secret_not_bound`; caída tras marca → fallo cerrado + reconciliación sin doble relleno. Verificado en test + `credential_target`/`fill_secret` (`actions.rs`), `SecretExecutor` (`security/executor.rs`), `rpc.rs` (`act.fill_secret` con `SecretValue` + `line.fill(0)`).
- **11.3** (6 filas): `download_blocked` sin disco; PDF armado con blob/hash/tipo y cuarentena vacía; >200 MiB cancelado + parcial borrado; nombre con traversal saneado a GUID; subida de ruta protegida/enlace fugitivo/inexistente rechazada en el adaptador sin llegar a webd; subida exacta + limpieza al cerrar sesión. Verificado en test + `files.rs` (`private` 0700, `O_NOFOLLOW`, GUID validado como UUID, rechequeo de origen en `downloadWillBegin`, `DownloadGuard` que restaura `deny` y borra el directorio) y `web-adapter/lib.rs` (`stage_path` con autoridad de archivos + `stage_blob` + `admit_download`).
- **Fidelidad documental**: `docs/development/browser.md` (+149) y `browser.es.md` (+162) con la sección de acciones/diálogos/archivos/credenciales en ambos idiomas, comandos de verificación B4 y límites honestos (sin ajustes de clase por sitio; adaptador como frontera de biblioteca sin registro de herramientas ni visor). `AGENTS.md` solo añade los comandos B4 (línea verificada). Mensaje de commit en lenguaje natural. Sin directorios nuevos (nombres en inglés intactos). `crates/browser/tests/cdp_contract.rs` amplía el catálogo con `TargetGetTargetInfo`; `sessions.rs` añade `credential_frames` + política `deny` de descargas por contexto.

## Hallazgos (ninguno bloqueante)

- **F-B4-01 · media · abierta · no bloquea**: `document_allowed` (`sessions.rs`) permite cualquier origen en sesiones efímeras; la autorización de dominio del arnés y la compuerta de documentos por `Fetch` (C2–C4) pertenecen a **B5** según §19.2. La compuerta B4 (clase/riesgo/aprobación por efecto en el adaptador + rechequeo de origen en `prepare`/`perform`) sí está implementada. **Recomendación**: verificar C2–C4 en la auditoría B5; no re-auditar aquí.
- **F-B4-02 · baja · abierta · no bloquea**: §8.3 permite afinar clases por sitio, pero no está implementado; la documentación lo declara («Per-site class overrides are not implemented») y el defecto es conservador (desconocido→`commit` en persistente). **Recomendación**: implementar en seguimiento o matizar la frase permisiva de §8.3.
- **F-B4-03 · info · abierta · no bloquea**: la copia `fill["value"]` del adaptador se descarta sin `zeroize` explícito (transporte, `rpc.rs` y `SecretValue` sí sanean las suyas). Solo memoria; la ausencia en disco/eventos/salida está probada. **Recomendación**: sanear la copia del adaptador tras la petición.
- **F-B4-04 · info · abierta · no bloquea**: `magic()` de `browser/files.rs` no detecta ZIP (sí lo hace `security/file_control.rs`, que es el control efectivo en `admit_download`); solo afecta al `mime` del blob intermedio de webd. **Recomendación**: unificar ambas tablas.

## Pendientes / no verificado (fuera del alcance B4, no bloquean este hito)

- Arquitectura completa B0–B3, B5–B9 y NB restantes (incluidos buscador NB-20–22 y recursos NB-17–18/B6): pendientes de sus hitos; esta aprobación cubre solo B4.
- Suites completas `sessions` (fixture de escucha de 600 s) y `cdp_contract`: no re-ejecutadas aquí por la carga y la latencia sensible en paralelo; las rutas de sesión tocadas por B4 (apertura, pestañas, ventanas, arriendos, política de descargas) quedan ejercitadas por el test `actions` con Chrome real.
- Recorridos manuales 18.4 en sitios reales: fuera del alcance B4.
- Frontend: sin nada que revisar visualmente (sin visor ni herramientas registradas, declarado en la documentación).

## Entrega

- Commit auditado (publicado, sin modificar): `d8db5985b3181c1385cec8d8aa75e4e95738ab84`.
- Informe: este archivo (`/home/cristhian/.local/state/excalibur-browser-owner/reports/b4-reaudit2-meta.md`).
