
## Respuesta de Codex

Operación `flow-operation:d1fe9838d54145b1be5a6e6857504388`, corrección 0 (implementación). Alcance: B0 completo, «Preparación y comprobaciones del entorno», según 19.2 y referencias 3.2–3.3, filas de usuario/root de 3.5, 12.2, 13.1 y 13.5 y apéndice A. No se inicia el siguiente alcance ni auditoría.

El archivo de recomendaciones no existía al comenzar; no había hallazgos ni IDs previos que resolver. Esta sección conserva cualquier contenido que aparezca anteriormente en el archivo.

Cambios implementados:

- `Cargo.toml` y `Cargo.lock`: cinco miembros nuevos; bibliotecas vacías en `crates/{cdp,browser,search,web-adapter}` y binario/biblioteca `crates/webd`. Sin integrar herramientas en el host.
- `crates/webd/src/lib.rs` y `src/main.rs`: TOML dedicado `web.*`, valores predeterminados, validación estricta, cambio de usuario antes de recursos, rechazo de root y de capacidades retenidas, comprobación de imposibilidad de recuperar UID/GID 0 y eliminación de grupos suplementarios cuando arranca root. El usuario normal conserva su identidad.
- Detección de ejecutable y versión Chrome mediante `--version`, plazo de cinco segundos, comprobación de disco con `statvfs`, reserva mínima de 5 GiB y presupuesto buscador de 2 GiB. JSON y salida distinta de cero ante comprobaciones fallidas. Detección de cgroup v2 con disponibilidad, controladores, memoria observada y uso exclusivo/compartido explícito.
- `crates/webd/tests/preflight.rs`: fixtures locales de configuración, disco y versiones; procesos reales para rechazo de root, arranque como usuario normal y lectura de UID/GID/capacidades en `/proc`.
- `docs/development/browser.md`, `browser.es.md` y `AGENTS.md`: preparación, configuración y comandos de uso/verificación. La documentación funcional no lleva historial de errores ni etiquetas de hitos.

Preparación efectuada en esta máquina: Rust estable 1.99.1 con rustfmt/clippy; compiladores GCC 13.3 y Clang, pkg-config y cabeceras OpenSSL; bubblewrap 0.13.1 verificado en `/opt/excalibur-toolchain/bubblewrap-0.13.1/bin/bwrap` (enlace `/usr/local/bin/bwrap`), fuente oficial comprobada con SHA-256 `0e85c8702248b8dd64144f594147553650cdd0e4c951690345faace79c217c4d`; Google Chrome estable 156.0.8078.79 del paquete oficial. Creado usuario de sistema `excalibur-web`, UID 999/GID 988, y directorios `/var/lib/excalibur-web` y `/run/excalibur-web`, propios del usuario y modo 0700. No se ha desplegado webd ni alterado servicios ajenos. Restricción AppArmor de user namespaces conservada en 1; perfil `/etc/apparmor.d/chrome` presente y `chrome-sandbox` root:root 4755. La prueba aislada `bwrap --unshare-all --ro-bind / / --proc /proc --dev /dev --die-with-parent /bin/true` terminó con código 0.

Comprobación previa real: `target/debug/excalibur-webd --preflight /tmp/excalibur-web-preflight.toml`, código 0, JSON en `/tmp/excalibur-browser-preflight.json` (artefactos fuera de Git). Resultado observado:

```json
{
  "ready": true,
  "identity": {
    "uid": 999,
    "euid": 999,
    "gid": 988,
    "egid": 988,
    "supplementary_groups": [],
    "capabilities": "zero"
  },
  "chrome": {
    "binary": "/usr/bin/google-chrome-stable",
    "version": "Google Chrome 156.0.8078.79"
  },
  "disk_available_bytes": 198230503424,
  "disk_reserve_bytes": 5368709120,
  "search_budget_bytes": 2147483648,
  "search_admissible": true,
  "cgroup": {
    "v2": true,
    "path": "/sys/fs/cgroup/user.slice/user-0.slice/session-39.scope",
    "controllers": [
      "cpu",
      "memory",
      "pids"
    ],
    "exclusive": false,
    "memory_current": 7144460288,
    "fallback": "/proc process-tree accounting required when cgroup is shared or inaccessible"
  },
  "errors": [],
  "sandbox_verified": false
}
```

`ready` certifica solo estas comprobaciones del entorno. El cgroup está compartido: no se atribuye su memoria a webd ni se afirma que haya límites propios. `sandbox_verified: false` es explícito: el lanzamiento y verificación de renderizadores pertenecen al siguiente alcance. No se han abierto perfiles, sockets ni sesiones de navegador. No existe frontend en este alcance.

Verificación final (pruebas locales y sintéticas, sin APIs de modelos ni cuentas reales):

| Comando | Resultado | Evidencia fuera de Git |
|---|---|---|
| `cargo fmt --check` | Código 0 | `/tmp/excalibur-browser-fmt.log` |
| `cargo clippy --workspace --all-targets -- -D warnings` | Código 0 | `/tmp/excalibur-browser-clippy.log` |
| `cargo test -p excalibur-webd --test preflight` | Código 0; 6 pruebas aprobadas | `/tmp/excalibur-browser-backend.log` |
| `cargo test --workspace` | Código 101: una prueba de temporización falló, detalle debajo | `/tmp/excalibur-browser-tests.log` |
| `cargo test -p excalibur-eval --test noninteractive wall_timeout_during_inference_pauses_with_exit_4 -- --exact --nocapture` | Código 0 | `/tmp/excalibur-browser-timeout-check.log` |
| `cargo test --workspace -- --test-threads=1` | Código 0; suite completa y doc-tests | `/tmp/excalibur-browser-tests-serial.log` |
| `cargo build --workspace` | Código 0 | `/tmp/excalibur-browser-build.log` |
| `target/debug/excalibur-webd --preflight /tmp/excalibur-web-preflight.toml` | Código 0; identidad dedicada y `ready: true` | `/tmp/excalibur-browser-preflight-final.json` |
| `git diff --check` | Código 0 | Comprobación local |

Observación de verificación: la ejecución paralela falló en `excalibur-eval/tests/noninteractive.rs`, prueba `wall_timeout_during_inference_pauses_with_exit_4`, por la aserción de que el motivo contenga «wall time». La misma prueba pasó aislada, tanto mediante Cargo como ejecutando directamente el binario de pruebas del workspace, y pasó dentro de la suite completa secuencial. No se modificó el código de evaluación ni se omitió ninguna suite para obtener el resultado secuencial. El fallo paralelo se conserva como limitación observada, sin atribuirle una causa no demostrada.

La instalación de bubblewrap coincide con el runtime verificado exigido por `docs/development/security.md`; los checks no se rebajaron. Los proxies del toolchain Rust están disponibles en `/usr/local/bin`. No hay IDs de recomendaciones anteriores pendientes dentro del alcance asignado.

Evidencia directa del proceso real durante la comprobación con `excalibur-web`, capturada desde `/proc/<pid>/status` por el proceso padre, en `/tmp/excalibur-browser-process-identity.txt`:

```text
Uid:	999	999	999	999
Gid:	988	988	988	988
Groups:	 
CapInh:	0000000000000000
CapPrm:	0000000000000000
CapEff:	0000000000000000
CapAmb:	0000000000000000
```


Disco al cierre: `target` propio ocupa 28970007682 bytes; libres en el filesystem del repositorio 170811867136 bytes. La última comprobación previa midió 170811117568 bytes libres en datos. Se mantienen holgadamente la reserva de 5 GiB y el umbral de aviso de 10 GiB; no se han limpiado artefactos ni archivos ajenos.

Commit: `60233a08f2096c888f856afdba30300034df2173` — `Add unprivileged browser startup and environment checks`.

Push completado exclusivamente a `origin/browser`, sin force-push. Verificación posterior con `git ls-remote --heads origin browser`: `60233a08f2096c888f856afdba30300034df2173` en `refs/heads/browser`, idéntico a `git rev-parse HEAD`. Rama local `browser`; árbol de trabajo limpio. El informe externo, la configuración de esta máquina, los logs y los artefactos de comprobación no se han incluido en Git.

Entrega limitada al alcance asignado; no se ha iniciado el siguiente ni un auditor.

## Auditoría — revisión de `60233a08f2096c888f856afdba30300034df2173`

Alcance auditado: entrada B0 «Preparación y comprobaciones del entorno» de 19.2 y sus referencias (3.2, 3.3, filas de usuario y root de 3.5, 4.5 en lo relativo a detección de binario y versión, 12.2, 13.1, 13.5, 19.1 y apéndice A). No se exigen hitos posteriores (lanzamiento de Chrome, verificación de renderizadores, socket, política de versión, contabilidad de memoria). Hallazgos previos a verificar: ninguno (lista de IDs vacía). Rama `browser`, `HEAD` = `origin/browser` = `60233a08f2096c888f856afdba30300034df2173`, árbol limpio. No se ha modificado código de producto ni arquitectura; las pruebas manuales usaron solo archivos en `/tmp/audit-b0` y `/tmp/audit-b0-run`.

Veredicto: **cambios requeridos** (un hallazgo bloqueante, AUD-B0-01).

### Comprobaciones reproducidas

Entorno: rustc/cargo 1.99.1, clippy 0.1.99; `umask` de la sesión de auditoría 0077.

| Comprobación | Resultado | Evidencia |
|---|---|---|
| `cargo fmt --check` | Código 0, sin salida | `/tmp/audit-b0/fmt.log` |
| `cargo clippy --workspace --all-targets -- -D warnings` | Código 0 | `/tmp/audit-b0/clippy.log` |
| `cargo build --workspace` | Código 0 | `/tmp/audit-b0/build.log` |
| `cargo test -p excalibur-webd --test preflight -- --nocapture` (umask 0077) | **Código 101**: 5 aprobadas, falla `ordinary_user_starts_without_resolving_configured_root_identity` (`preflight.rs:205`, `left: Null, right: 65534`); fallo determinista en 6 de 6 ejecuciones | `/tmp/audit-b0/preflight.log` |
| `cargo test -p excalibur-webd --test preflight` (umask 0022) | Código 0; 6 aprobadas en 3 de 3 ejecuciones | Salida de terminal |
| `cargo test --workspace` (umask 0077) | **Código 101**; se detiene en `excalibur-webd --test preflight` | `/tmp/audit-b0/tests.log` |
| `cargo test --workspace --no-fail-fast` (umask 0077) | **Código 101**: 134 binarios aprobados; fallan `excalibur-webd --test preflight` (AUD-B0-01) y `excalibur-eval --test verifiable` (detenido tras 18 min colgado, AUD-B0-05) | `/tmp/audit-b0/tests-nff.log` |
| `cargo test -p excalibur-eval --test verifiable` aislada | Código 0 en 2 de 2 ejecuciones (≈24 s) | Salida de terminal |
| `cargo test --workspace --no-fail-fast` (umask 0022) | Código 0: 136 resultados `ok`, 0 fallidos, incluidos `preflight` (6/6), `verifiable` y doc-tests | `/tmp/audit-b0/tests-umask022.log` |
| `git diff --check 60233a0~1 60233a0` | **Código 2**: línea en blanco final en cuatro `Cargo.toml` (AUD-B0-04) | Salida de terminal |

### Informe real de comprobación previa en la VPS

Configuración de `browser.md` en `/tmp/audit-b0-run/web.toml`, lanzada como root: `target/debug/excalibur-webd --preflight /tmp/audit-b0-run/web.toml`, código 0, `ready: true`, identidad 999/999/988/988, `supplementary_groups: []`, `capabilities: "zero"`, Chrome `/usr/bin/google-chrome-stable` «Google Chrome 156.0.8078.79», 170 790 649 856 bytes libres, reserva 5 GiB, presupuesto del buscador 2 GiB admisible, cgroup v2 `system.slice/excalibur-browser-flow.service` compartido (`exclusive: false`), `sandbox_verified: false` (`/tmp/audit-b0/preflight-real.json`). Sin `chrome_binary` se detecta la misma ruta. `/var/lib/excalibur-web` y `/run/excalibur-web` siguen vacíos, `excalibur-web:excalibur-web` y modo 0700.

Fila de usuario de 3.5, leída de `/proc/<pid>/status` del proceso vivo (fixture que hace `sleep 1` y `exec` del Chrome real; `/tmp/audit-b0/proc-identity.txt`): `Uid: 999 999 999 999`, `Gid: 988 988 988 988`, `Groups:` vacío y `CapInh/CapPrm/CapEff/CapAmb` a cero.

Fila de root de 3.5 y casos negativos: `os_user = "root"` → `{"error":"root identity is forbidden","ready":false}`, código 1; usuario inexistente → código 1; `disk_reserve_bytes = 1` → error de configuración, código 1; presupuesto mayor que el disco → `search_admissible: false`, código 1. Ninguno crea archivos en los directorios configurados.

Estado del entorno: `excalibur-web` UID 999/GID 988 con shell `nologin`; Chrome 156.0.8078.79; `/opt/google/chrome/chrome-sandbox` root:root 4755; `/etc/apparmor.d/chrome` presente; `kernel.apparmor_restrict_unprivileged_userns = 1` (no desactivada); bubblewrap 0.13.1 en `/usr/local/bin/bwrap`. Disco: `target` 28 970 007 682 bytes; 170 784 727 040 bytes libres en `/`, por encima de la reserva de 5 GiB y del aviso de 10 GiB.

### Requisitos

| Requisito (fuente) | Estado | Evidencia |
|---|---|---|
| Crates vacíos `cdp`, `browser`, `search`, `web-adapter` y `webd` como miembros del workspace (3.3) | Verificado | `Cargo.toml`, `crates/*/src/lib.rs`; solo se tocan archivos nuevos, `Cargo.toml`, `Cargo.lock`, `AGENTS.md` y `browser*.md`; sin dependencias nuevas fuera de las del lock (`toml` 0.9.12 ya existía) |
| Configuración `web.*` en TOML (3.2, 13.5, apéndice A) | Verificado | `crates/webd/src/lib.rs:12-60`: `deny_unknown_fields`, rutas absolutas, reserva ≥ 5 GiB, presupuesto > 0; prueba `configuration_and_disk_admission_fail_closed` |
| Cambio de usuario antes de recursos y rechazo de root (3.2, 12.2) | Verificado | `lib.rs:71-125`: `setgroups(0)`, `setgid`, `setuid`, intento fallido de `setuid(0)`/`setgid(0)`, capacidades a cero, rechazo de UID/GID 0; reproducido en VPS |
| Fila 3.5 «Lanzar webd como root con `web.os_user`» | Verificado | `/tmp/audit-b0/proc-identity.txt` |
| Fila 3.5 «UID efectivo final 0 → error antes del socket» | Verificado | Código 1 con `os_user = "root"`; directorios vacíos |
| Detección del binario de Chrome y su versión (4.5, B0) | Verificado con reserva menor (AUD-B0-03) | Real 156.0.8078.79; fixtures de salida inválida, código ≠ 0 y no ejecutable |
| Comprobación de disco con reserva de 5 GiB y presupuesto del buscador (13.5) | Verificado | `admits_growth` con límites exactos; caso real y caso de presupuesto excesivo |
| Comprobación de cgroup v2, exclusivo/compartido (13.1) | Verificado | Informe real con `exclusive: false` y `memory_current` sin atribuir a webd |
| Informe JSON y código de salida (B0) | Verificado | Código 0 solo con `ready: true`; errores tempranos también en JSON |
| Documentación bilingüe y orden en `AGENTS.md` (19.1) | Verificado | `docs/development/browser.md` y `browser.es.md` equivalentes; no describen como implementado lo que no existe |
| Medición de disco y `target` (19.1, 13.5) | Verificado | Cifras arriba |
| Suite de cierre `preflight` reproducible y checks obligatorios en verde | **No cumplido** | AUD-B0-01 |
| Sin regresiones en el resto del workspace | Verificado (ver AUD-B0-05) | El commit no modifica código de crates existentes; con umask 0077 los 134 binarios restantes pasan salvo el cuelgue intermitente de `verifiable`, que pasa aislada; con umask 0022 la suite completa pasa |

### Hallazgos

#### AUD-B0-01 — Bloqueante (severidad media): la prueba de usuario normal depende del `umask`

- Evidencia: `ordinary_user_starts_without_resolving_configured_root_identity` (`crates/webd/tests/preflight.rs:176-207`) escribe `config.toml` con `fs::write` y copia el binario con `fs::copy` dentro de un directorio 0755, y ejecuta la copia como UID/GID 65534. Con `umask 0077`, `config.toml` queda 0600 root:root y webd responde `{"error":"Permission denied (os error 13)","ready":false}`; el `report["identity"]["uid"]` es `Null` y la aserción de la línea 205 falla. Con `umask 0022` pasa. Reproducción manual en un directorio 0755 con `setpriv --reuid=65534 --regid=65534 --clear-groups <copia> --preflight c.toml`: con `c.toml` 0600, `Permission denied`; con 0644, identidad 65534. El producto se comporta correctamente al rechazar una configuración ilegible: el defecto está en el fixture.
- Impacto: la suite de cierre del hito y `cargo test --workspace` fallan en esta VPS bajo el `umask` de la sesión, por lo que el check obligatorio `tests` no se puede dar por pasado.
- Recomendación: fijar explícitamente los modos del fixture (`config.toml` 0644 y, por robustez, la copia del binario 0755 con `set_permissions`), o crear los archivos con `OpenOptions::mode`, de modo que la prueba no dependa del `umask` heredado; volver a ejecutar la suite con `umask 0077` y `0022`.

#### AUD-B0-02 — Menor (severidad baja, no bloqueante): la comprobación previa no verifica la privacidad de los directorios

- Evidencia: `preflight` (`lib.rs:271-282`) solo exige que `data_dir` y `run_dir` sean directorios con `W_OK|X_OK` para la identidad final. Con un directorio 0777 de root, o 0777 de `nobody`, el informe da `ready: true` y `errors: []` (`/tmp/audit-b0-run/ww`, `/tmp/audit-b0-run/other`). 12.2 exige directorios 0700 del usuario web y la documentación ordena prepararlos como «privados», pero el informe no lo comprueba.
- Recomendación: comprobar en la comprobación previa que ambos directorios pertenecen al UID final y no son escribibles por grupo u otros (al menos para `data_dir`; `run_dir` admitirá después el grupo compartido del socket de 3.2), e informarlo en el JSON; o, si se pospone, indicarlo explícitamente en `browser.md`/`browser.es.md`.

#### AUD-B0-03 — Menor (severidad baja, no bloqueante): el plazo de cinco segundos de la sonda de versión no es total

- Evidencia: `detect_chrome` (`lib.rs:145-164`) espera a que el hijo directo termine y después llama a `wait_with_output`, que lee hasta EOF. Un ejecutable configurado que deja un descendiente con la salida abierta (`sleep 12 &` y `echo` de versión) mantiene la comprobación 12,0 s con `ready: true`; uno que se cuelga se corta a 5,02 s, pero `kill` solo alcanza al hijo directo y el `sleep 30` del fixture quedó huérfano. La documentación afirma un plazo de cinco segundos.
- Recomendación: lanzar la sonda en su propio grupo de procesos y matar el grupo al vencer el plazo, leyendo la salida con el mismo plazo total; añadir un fixture con descendiente.

#### AUD-B0-04 — Informativo (no bloqueante): espacio en blanco en manifiestos

- Evidencia: `git diff --check 60233a0~1 60233a0` devuelve código 2: «new blank line at EOF» en `crates/{browser,cdp,search,web-adapter}/Cargo.toml`. La tabla de la respuesta registra `git diff --check` con código 0, comprobación hecha sobre el árbol de trabajo, no sobre el commit.
- Recomendación: eliminar la línea final y comprobar con `git diff --check <base> <commit>`.

#### AUD-B0-05 — Informativo, fuera del alcance (no bloqueante): cuelgue intermitente en `excalibur-eval --test verifiable`

- Evidencia: en la ejecución `--no-fail-fast`, `five_requirement_task_launches_a_read_only_verifier_and_charges_its_budget` estuvo más de 18 min en espera (estado `S`, 11 hilos) y se detuvo con `kill`; en la ejecución anterior de la misma sesión y en dos ejecuciones aisladas pasó en 23–25 s. El commit no toca `crates/eval` ni ningún crate del que dependa, y ningún crate existente depende de los nuevos, por lo que no se atribuye a este cambio. Junto con el fallo paralelo intermitente que registra la respuesta (`wall_timeout_during_inference_pauses_with_exit_4`), apunta a sensibilidad temporal preexistente en `excalibur-eval`.
- Recomendación: registrarlo como riesgo de la suite del arnés fuera de este flujo; no bloquea B0.

### Hallazgos previos

No había IDs previos que verificar.


## Respuesta de Codex

### Corrección 1 — preparación y comprobaciones del entorno

Operación `flow-operation:00ba2f74cfc04485ab287b89d8f89303`. Revisión atendida: `60233a08f2096c888f856afdba30300034df2173`. Se conserva íntegra la respuesta anterior y la auditoría. Alcance limitado a la entrada asignada de 19.2 y sus referencias; no se ha iniciado el siguiente alcance ni un auditor.

| ID | Estado tras la corrección | Cambio y evidencia |
|---|---|---|
| AUD-B0-01 | Resuelto | `crates/webd/tests/preflight.rs`: modos explícitos 0644 para la configuración que lee el usuario normal y 0755 para la copia del ejecutable. Los directorios de fixtures que usa el proceso tras retirar privilegios son 0700 y pertenecen al UID final. Las nueve pruebas backend y la suite completa del workspace pasan tanto con umask 0077 como con 0022. |
| AUD-B0-02 | Resuelto | `crates/webd/src/lib.rs`: ambos directorios deben pertenecer al UID efectivo final, tener modo exacto 0700 y acceso de escritura/búsqueda; se rechaza un enlace simbólico en la ruta final. JSON `directories` informa ruta, propietario, modo numérico, privacidad y acceso. Fixtures negativos con 0777/0750 y propietario incorrecto; positivo con 0700 del usuario final. |
| AUD-B0-03 | Resuelto | `probe_version` lanza únicamente la sonda `--version` en un grupo propio; lectura no bloqueante con plazo total de cinco segundos y límite de 4096 bytes. El guard de proceso mata el grupo y espera al hijo directo al terminar o fallar. Pruebas con padre salido y stdout retenido por descendiente, padre esperando a su descendiente y salida excesiva; los descendientes dejan de estar vivos tras el plazo (ausentes o zombis pendientes de recogida por su padre adoptivo). Cada sonda de descendiente vence alrededor de cinco segundos; el fixture comprueba un límite superior de nueve segundos y la terminación del descendiente. |
| AUD-B0-04 | Resuelto | Eliminadas las líneas en blanco finales de `crates/{browser,cdp,search,web-adapter}/Cargo.toml`. Comprobaciones sobre commits: `git diff --check 60233a08f2096c888f856afdba30300034df2173 7e217a0797fd7b9f2fdb0f0961759620079534b9` y `git diff --check 768a49723f9b3a49f082588968cbad9713eca202 7e217a0797fd7b9f2fdb0f0961759620079534b9`, ambas código 0. |
| AUD-B0-05 | Conservado como fuera de alcance | Riesgo intermitente de temporización de `excalibur-eval` observado en la auditoría. No se ha modificado ese crate ni se declara corregido. `five_requirement_task_launches_a_read_only_verifier_and_charges_its_budget` y `wall_timeout_during_inference_pauses_with_exit_4` pasan en ambas ejecuciones completas de esta entrega. |

Documentación funcional actualizada en paralelo: `docs/development/browser.md` y `browser.es.md`, sin historial de errores ni etiquetas de hitos. `AGENTS.md` recoge las verificaciones de permisos, descendientes y ambos umask. Los crates vacíos continúan vacíos; no hay sesiones, CDP, socket, frontend ni herramientas web implementados.

### Verificación ejecutada

Todas las pruebas son locales y sintéticas, sin APIs de modelos, cuentas reales ni benchmarks pagados. No se han desplegado servicios ni modificado servicios ajenos.

| Comando | Resultado | Evidencia fuera de Git |
|---|---|---|
| `(umask 0077; cargo test -p excalibur-webd --test preflight)` | Código 0; 9 aprobadas | `/tmp/browser-correction1-preflight077.log` |
| `(umask 0022; cargo test -p excalibur-webd --test preflight)` | Código 0; 9 aprobadas | `/tmp/browser-correction1-preflight022.log` |
| `cargo fmt --check` | Código 0 | `/tmp/browser-correction1-fmt.log` |
| `cargo clippy --workspace --all-targets -- -D warnings` | Código 0 | `/tmp/browser-correction1-clippy.log` |
| `cargo build --workspace` | Código 0 | `/tmp/browser-correction1-build.log` |
| `(umask 0077; cargo test --workspace --no-fail-fast)` | Código 0; suite completa, ejecución paralela normal y doc-tests | `/tmp/browser-correction1-workspace077.log` y `.exit` |
| `(umask 0022; cargo test --workspace --no-fail-fast)` | Código 0; suite completa, ejecución paralela normal y doc-tests | `/tmp/browser-correction1-workspace022.log` y `.exit` |
| `target/debug/excalibur-webd --preflight /tmp/browser-correction1-web.toml` | Código 0; `ready: true` | `/tmp/browser-correction1-preflight-real.json` |
| Comprobaciones `git diff --check` entre commits indicadas en AUD-B0-04 | Código 0 | `/tmp/browser-correction1-diff-check.log` |

Comprobación real en la VPS: Rust estable 1.99.1, GCC 13.3, OpenSSL 3.0.13 mediante pkg-config y bubblewrap 0.13.1 disponibles. Chrome detectado: `Google Chrome 156.0.8078.79`. Usuario dedicado UID 999/GID 988, directorios `/var/lib/excalibur-web` y `/run/excalibur-web` 0700 del usuario, sin archivos de sesiones creados. `chrome-sandbox` conserva root:root 4755 y la restricción AppArmor de user namespaces sigue en 1.

JSON observado de directorios (448 decimal equivale a 0700 octal):

```json
[
  {
    "path": "/var/lib/excalibur-web",
    "owner_uid": 999,
    "mode": 448,
    "private": true,
    "writable": true
  },
  {
    "path": "/run/excalibur-web",
    "owner_uid": 999,
    "mode": 448,
    "private": true,
    "writable": true
  }
]
```

Identidad leída del proceso vivo en `/proc/<pid>/status`, evidencia `/tmp/browser-correction1-proc.txt`:

```text
Uid:	999	999	999	999
Gid:	988	988	988	988
Groups:	 
CapInh:	0000000000000000
CapPrm:	0000000000000000
CapEff:	0000000000000000
CapAmb:	0000000000000000
```

El informe real registra 170237923328 bytes libres, reserva 5368709120 y presupuesto 2147483648, con `search_admissible: true`. Cgroup v2 compartido, `exclusive: false`; no se atribuye su memoria al servicio. `sandbox_verified: false` permanece explícito: verificar renderizadores pertenece al siguiente alcance.

Disco al cierre: `target` propio 29490801176 bytes; libres 170258022400 bytes, por encima de la reserva de 5 GiB y del aviso de 10 GiB. No se han limpiado archivos ajenos.

Commit: `7e217a0797fd7b9f2fdb0f0961759620079534b9` — `Harden browser preflight permissions and process cleanup`.

Push realizado exclusivamente a `origin/browser`, sin force-push. Comprobación posterior `git ls-remote --heads origin browser`: `7e217a0797fd7b9f2fdb0f0961759620079534b9` en `refs/heads/browser`, idéntico al HEAD local. Rama `browser`, árbol limpio. Configuración de máquina, logs, evidencias e informe externo permanecen fuera de Git. No quedan hallazgos abiertos dentro del alcance asignado; AUD-B0-05 conserva su condición de fuera de alcance.

## Auditoría — revisión de `7e217a0797fd7b9f2fdb0f0961759620079534b9`

Alcance: la misma entrada B0 de 19.2 y sus referencias (3.2, 3.3, filas de usuario y root de 3.5, 4.5 en lo relativo a la detección del binario y su versión, 12.2, 13.1, 13.5, 19.1 y apéndice A), más los hallazgos AUD-B0-01 a AUD-B0-05. Rama `browser`: `HEAD` = `origin/browser` = `7e217a0797fd7b9f2fdb0f0961759620079534b9`, árbol limpio. El diff `60233a0..7e217a0` solo toca `crates/webd/{src/lib.rs,tests/preflight.rs}`, los cuatro `Cargo.toml` vacíos, `AGENTS.md` y `browser*.md`. No se ha modificado producto ni arquitectura; las pruebas manuales usaron `/tmp/audit-b0r1` y `/tmp/audit-b0r1-run`. `umask` de la sesión de auditoría: 0077.

Veredicto: **aprobado**.

### Comprobaciones reproducidas

| Comprobación | Resultado | Evidencia |
|---|---|---|
| `cargo fmt --check` | Código 0, sin salida | `/tmp/audit-b0r1/fmt.log` |
| `cargo clippy --workspace --all-targets -- -D warnings` | Código 0 | `/tmp/audit-b0r1/clippy.log` |
| `cargo build --workspace` | Código 0 | `/tmp/audit-b0r1/build.log` |
| `(umask 0077; cargo test -p excalibur-webd --test preflight)` | Código 0; 9 aprobadas | `/tmp/audit-b0r1/preflight077.log` |
| `(umask 0022; cargo test -p excalibur-webd --test preflight)` | Código 0; 9 aprobadas | `/tmp/audit-b0r1/preflight022.log` |
| `(umask 0077; cargo test --workspace --no-fail-fast)` | Código 0; 136 resultados `ok`, 0 fallidos, doc-tests incluidos; `verifiable` 4/4 | `/tmp/audit-b0r1/tests077.log` |
| `git diff --check 60233a0~1 7e217a0` | Código 0 | Salida de terminal |

### Comprobación previa real y filas de 3.5

- `target/debug/excalibur-webd --preflight /tmp/audit-b0r1-run/web.toml` (configuración de `browser.md`, lanzado como root): código 0, `ready: true`; identidad 999/999/988/988, sin grupos suplementarios y sin capacidades. Chrome `/usr/bin/google-chrome-stable` «Google Chrome 156.0.8078.79». 170 245 550 080 bytes libres, reserva de 5 GiB y presupuesto de 2 GiB admisible. Cgroup v2 `system.slice/excalibur-browser-flow.service`, `exclusive: false`. `sandbox_verified: false`. `directories`: ambos con propietario 999, `mode` 448 (0700), `private` y `writable` a true. Evidencia: `/tmp/audit-b0r1/preflight-real.json`. Sin `chrome_binary` se detecta la misma ruta (código 0).
- Fila de usuario: `/proc/<pid>/status` del proceso vivo muestra `Uid: 999×4`, `Gid: 988×4`, `Groups:` vacío y `CapInh/CapPrm/CapEff/CapAmb` a cero (`/tmp/audit-b0r1/proc-identity.txt`).
- Fila de root: `os_user = "root"` → `{"error":"root identity is forbidden","ready":false}` con código 1; un usuario inexistente también sale con código 1.
- Disco al cierre: `target` 29 490 801 176 bytes; 170 256 678 912 bytes libres, por encima de la reserva de 5 GiB y del aviso de 10 GiB.

### Verificación de hallazgos previos

| ID | Estado | Evidencia |
|---|---|---|
| AUD-B0-01 | Resuelto | El fixture fija la configuración en 0644 y la copia del binario en 0755 (`preflight.rs:198-199`); los directorios usados tras la retirada de privilegios son 0700 y pertenecen al UID final. La suite pasa 9/9 con umask 0077 y con 0022, y `cargo test --workspace` pasa con 0077. |
| AUD-B0-02 | Resuelto | `check_directory` (`lib.rs`) exige un directorio que no sea enlace simbólico (`symlink_metadata`), cuyo propietario sea el euid final, con modo exacto 0700 y `W_OK\|X_OK`; el resultado se informa en `directories`. Reproducido con el binario real: 0777 de root, 0700 de `nobody`, 0750 y 2700 del usuario web y un enlace simbólico a un directorio válido dan `ready: false` con código 1; 0700 del usuario web da `ready: true`. |
| AUD-B0-03 | Resuelto | `probe_version` usa `process_group(0)`, lectura no bloqueante con plazo total de 5 s y límite de 4096 bytes; el guard `Probe` mata el grupo en `Drop`. Reproducido: el fixture con `sleep 12 &` que antes tardaba 12,0 s con `ready: true` termina ahora a 5,03 s con «Chrome version check timed out»; el colgado termina a 5,01 s. En ambos casos queda 0 `sleep` residual del usuario web. Las pruebas `chrome_probe_bounds_total_time_and_stops_descendants` y `chrome_probe_rejects_excessive_output_without_blocking` pasan. |
| AUD-B0-04 | Resuelto | Ya no hay línea en blanco final en los cuatro `Cargo.toml`; `git diff --check 60233a0~1 7e217a0` da código 0. |
| AUD-B0-05 | Fuera de alcance, sin reaparición | `excalibur-eval --test verifiable` pasó 4/4 dentro de la suite completa; el commit no toca `crates/eval`. Se mantiene como riesgo preexistente de temporización del arnés. |

### Requisitos

Verificados todos los requisitos aplicables de B0: crates vacíos y miembros del workspace (3.3); configuración `web.*` estricta; cambio de usuario irreversible antes de cualquier recurso y rechazo de root (3.2, 12.2); detección del binario de Chrome y de su versión con sonda acotada; reserva de disco de 5 GiB más presupuesto del buscador (13.5); cgroup v2 informado como compartido (13.1); informe JSON con código de salida coherente; directorios privados 0700 (12.2); documentación EN/ES equivalente que no describe como implementado lo que no existe, y orden en `AGENTS.md`; medición de disco y `target` (19.1); filas de usuario y root de 3.5 e informe real en la VPS. No se observan regresiones.

Observación no bloqueante: `mode` se serializa en decimal (448 equivale a 0700); la documentación lo describe como «modo numérico», lo cual es exacto. Mostrarlo en octal facilitaría la lectura, pero no es un defecto.

Hallazgos nuevos: ninguno.
