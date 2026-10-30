# Recomendaciones: acciones, diálogos, archivos y credenciales (B4)

Alcance: entrada «Acciones, diálogos, archivos y credenciales» de la sección 19.2 de `arquitectura-navegador.md` (canalización y acciones 8.1–8.2, clases 8.3, diálogos y ventanas 8.4, descargas y subidas 11, relleno de credenciales con `SecretExecutor` 10). Cierres 8.5, 10.5 y 11.3. Requisitos NB-09, NB-11 y NB-12.

## Respuesta de implementación

Operación `flow-operation:28ac01c7622847cd8b05001d65bf9a81`. Corrección 0 (0=implementación). Fecha UTC: 2026-10-30T13:15:34+00:00.

Se continúa el trabajo B4 existente dejado por Claude en la rama `browser` sin descartarlo ni rehacerlo: se conservan el escenario, el contador de corrección y todos los cambios en curso, y se finalizan pruebas, documentación, informe, commit y push. No se avanza al siguiente hito. La auditoría ordinaria de Claude queda pendiente.

### Cambios

- `crates/browser/src/observation/actions.rs` (+`actions/target.js`): pasarela de acciones de la parte confiable; `ActionRequest` con clases deterministas (`observe`, `input`, `navigate_link`, `commit`, `upload`, `credential`, `drag`), límites de claves y peticiones, comprobación de accionabilidad con reintentos y desplazamiento alternado, y `dialog_open`/`popup_blocked`/`not_actionable`/`denied(origin_not_writable)`/`stale_ref` sin efectos parciales.
- `crates/browser/src/observation/files.rs`: descargas denegadas por defecto en cada instancia y contexto; armado por contexto con un solo permiso, cuarentena privada con GUID, tope de 200 MiB con borrado del parcial, SHA-256 y tipo por bytes mágicos; preparación de subidas en `staging/<session>/<uuid>/` con modo 0600 y caducidad de una hora. Chrome solo recibe rutas preparadas.
- `crates/browser/src/observation/mod.rs`, `tree.rs`, `sessions.rs`, `rpc.rs`, `lib.rs`: `act.prepare`/`act.perform`/`act.stage_upload`/`download`/`act.check_credential_target`/`act.fill_secret`, estado de acciones por pestaña, origen de seguridad comprometido por marco, redacción de elementos rellenados desde secreto y supresión de visión durante 5 s, diálogos aceptados solo dentro de acciones de entrada y `beforeunload` aceptado solo en navegación del agente.
- `crates/cdp/src/protocol.rs`: `Target.getTargetInfo` para el contrato ampliado.
- `crates/security/src/host_effect.rs`, `file_control.rs`, `policy.rs`, `lib.rs`, `crates/vault/src/lib.rs`: efectos durables del anfitrión con recibo y reconciliación sin doble envío; `SecretExecutor` con marca de uso y auditoría antes de resolver el valor y fallo cerrado con reconciliación; control de archivos (detección por contenido, límites, cuarentena, registro) y `credential_origin` HTTPS canónico sin userinfo.
- `crates/web-adapter/src/lib.rs`, `transport.rs`, `Cargo.toml`, `crates/web-adapter/tests/actions.rs`: `WebAdapter` sobre `LocalBackend` o `SocketBackend`; `act` rechaza secretos literales y autoriza `Action::Browser` por clase/riesgo; `credential` solo lee metadatos y usa `SecretExecutor`; `stage_path`/`stage_blob` con autoridad de archivos y `download` con control de archivos. Límite de la biblioteca: no registra herramientas del agente ni visor.
- `docs/development/browser.md` y `browser.es.md`: sección funcional de acciones, diálogos, archivos y credenciales en ambos idiomas, sin historia de errores ni etiquetas de hitos. `AGENTS.md`: comandos de verificación B4.

### Verificación

Pruebas locales y sintéticas; sin APIs de modelos de pago, cuentas reales, secretos revelados, servicios desplegados ni benchmarks completos. Chrome real instalado como usuario dedicado `excalibur-web`, con sandbox verificado; se requieren `openssl` y `certutil`. No se usó `EXCALIBUR_BROWSER_TESTS=skip`.

| Comprobación | Resultado |
|---|---|
| `cargo fmt --check` | Código 0 |
| `cargo clippy -p excalibur-browser -p excalibur-web-adapter -p excalibur-security -p excalibur-vault -p excalibur-cdp --all-targets -- -D warnings` | Código 0 |
| `cargo build -p excalibur-browser -p excalibur-web-adapter -p excalibur-security -p excalibur-vault` | Código 0 |
| `cargo test -p excalibur-browser --lib observation` | Código 0; 5 aprobadas (clases, límites de claves/peticiones, orígenes exactos HTTPS sin userinfo, nombres sugeridos, tipos por contenido) |
| `cargo test -p excalibur-web-adapter --test actions -- --nocapture` | Código 0; 1 aprobada + 1 filtrada (auxiliar `ignored` ejecutada por la prueba padre); 106 s con Chrome real y accesorio HTTPS local con CA temporal |
| `git diff --check` | Código 0 |

Evidencia en `target/test-evidence/browser/actions/`: `contract.json` y `runner.log`. Cubre cada acción sobre su control, botón tapado (`not_actionable` sin clic), botón en movimiento, escritura idempotente, arrastres de ratón y HTML5, alerta/confirmación, ventanas emergentes efímeras y bloqueadas en persistente, denegación fuera de `rw`, aprobación de `commit` antes del efecto con resultado repetido una sola vez, `commit` desconocido en persistente, contenido exacto de subida, rechazo de rutas protegidas/enlace fugitivo/inexistente sin llegar a webd, descargas no armada/armada/sobredimensionada, nombre `../../etc/passwd` aislado con GUID, relleno con redacción, rechazos entre orígenes/subdominio/HTTP/userinfo sin resolver, carrera de navegación entre comprobación y relleno, caída simulada del anfitrión tras la marca de uso sin reintento, y ausencia del valor del secreto en archivos, eventos, auditoría y salida. El aviso de pánico del worker `tokio-rt-worker` en el registro es el ruido esperado de la caída simulada; la prueba lo convierte en error de reconciliación y el estado de salida es correcto.

### Hallazgos pendientes

Ninguno bloqueante conocido en este alcance. Queda la auditoría ordinaria de Claude sobre el commit entregado.

### Entrega

- Commit local: `d8db5985b3181c1385cec8d8aa75e4e95738ab84` («Add browser input actions, dialogs, staged uploads, quarantined downloads and reference-only credential filling»).
- Rama: `browser`; remoto `origin/browser` verificado en `d8db5985b3181c1385cec8d8aa75e4e95738ab84` (avance `3a61e35..d8db598`).

## Nota de coordinación (2026-10-30, turno de propietario)

- La auditoría Claude del flujo anterior sobre `d8db598` (review `7a425b1c2aa64cfdbe3fc108031d5b89`, misma sesión `82792c3c…`) terminó por infraestructura (`You've hit your 5-hour limit`), sin veredicto y sin modificar este informe. No se toma como éxito ni como hallazgo.
- Re-auditoría solicitada por el puente nuevo con sesión independiente: `b4-reaudit` sobre el commit `d8db5985b3181c1385cec8d8aa75e4e95738ab84` (alcance B4). La rama `browser` permanece publicada y sin modificar hasta su veredicto.
- El reintento por el puente nuevo (`b4-reaudit`, sesión independiente) falló de inmediato por el mismo límite de cuenta (restablece 16:20 Europe/Berlin). Reintentar la auditoría B4 tras esa hora; mientras tanto la rama `browser` queda congelada en `d8db598` sin integraciones.
- Segundo reintento (`b4-reaudit2`, 14:50): mismo límite de cuenta hasta las 16:20. Sin veredicto; `browser` sigue congelado en `d8db598`.
- Aclaración 15:00: el puente marcó `b4-reaudit` como superado y relanzó `b4-reaudit2` con auditor independiente Codex/Meta (worktree `excalibur-review-b4-reaudit2`, en curso). Decisión del puente ante la falta de cuota Claude, no de este turno. Su veredicto, cuando llegue, se registrará como señal provisional: la aprobación vinculante de B4 sigue requiriendo auditoría Claude tras las 16:20. `browser` sigue congelado en `d8db598`.
- Integración posterior al veredicto provisional (15:10): la rama `browser` avanzó con dos commits que no tocan archivos B4: `53dd6a6` (fundación de recursos, verificada fmt/clippy/33 tests/build, push verificado) y `ff84849` (fundación del buscador rebasada, fmt/clippy/65 tests/build, `Cargo.lock` con las aristas de `excalibur-search`, push verificado). Base de B4 (`d8db598`) intacta para la auditoría vinculante Claude.
