
## Respuesta de Codex

Operación `flow-operation:8db9a13523f04779b5b78394b1734a7c`, corrección 0 (implementación). Alcance: entrada «Cliente CDP y gestor de instancias» de 19.2, referencias 3.2, 4.3–4.6, 12.2, filas de sandbox de 12.9, apéndice B.3 y evidencia de 18.1–18.2. Base: `7e217a0797fd7b9f2fdb0f0961759620079534b9`. No había archivo de recomendaciones al comenzar ni IDs pendientes heredados (`[]`). No se ha iniciado el siguiente alcance, una tercera corrección de la capacidad anterior ni un auditor.

### Implementación y requisitos comprobados

| Requisito | Implementación y evidencia |
|---|---|
| CDP por tubería, sin puerto de depuración | `crates/cdp/src/lib.rs`, `framing.rs` y `crates/browser/src/lib.rs`: tuberías nativas, extremos del hijo en FD 3/4, encuadre JSON terminado en NUL. Transporte probado fragmentado, concatenado y concurrente; Chrome real responde por esa tubería. |
| Correlación, cancelación y errores | IDs crecientes sin reutilización, guard de peticiones pendientes, error de protocolo tipado, cierre ante escritura parcial cancelada y EOF. Pruebas de respuestas invertidas, tardías, cancelación y errores de ruta sin sessionId. |
| Plazos y sesiones planas | 10 s por defecto, 15 s para captura y 20 s para DOMSnapshot; comprobación con reloj simulado. Suscripción por sesión y global. Autoattach con `flatten` y `waitForDebuggerOnStart`; OOPIF real adjuntado y reanudado tras verificar procesos. |
| Límite de 64 MiB y aislamiento | Descarte incremental hasta el NUL, identificación de sesión incluso al final de la trama, invalidación/desconexión de esa sesión y conservación del resto. Fixture sintético de transporte y una instantánea DOM real que contiene 70 MiB de texto; se comprueba `SessionCrashed` y que otra sesión y `Browser.getVersion` siguen respondiendo. |
| Tipos y contrato de órdenes | `crates/cdp/src/protocol.rs`: catálogo manual, estructuras comunes tipadas y parámetros JSON genéricos, tolerancia de campos adicionales en respuestas. Contrato instalado: 73 órdenes, 68 resultados normales, cuatro errores de estado esperables y cierre normal. Ningún `method not found`. |
| Flags y frontera del binario | Lista estricta de extras y flags gestionados de 4.4, incluidos proxy loopback sin excepciones, QUIC/UDP no proxificado desactivados, aislamiento de sitios y cachés 32/64 MiB. Se rechazan los flags prohibidos y sustituciones de transporte, proxy o perfil. Prueba del binario `webd --check-instance` con todos los flags prohibidos antes de crear perfiles. |
| Usuario y entorno | Lanzador rechaza UID 0 y capacidades retenidas. `webd` retira privilegios antes de Tokio. Entorno del navegador y de la sonda limpiado; un marcador sintético prueba que el entorno del host no entra en la sonda. Perfil existente privado, propietario del UID actual y modo 0700. |
| Ciclo de vida | Grupo propio, `PR_SET_PDEATHSIG(SIGKILL)` con comprobación de la carrera del padre, cierre/liberación con limpieza de grupo y PIDs observados contrastando instante de inicio. La API de cierre propaga errores de protocolo y limpia igualmente. SIGKILL del padre con dos instancias reales: `remaining_live: []` (procesos terminados o ya zombis no se consideran vivos). |
| Árbol y caídas | `crates/browser/src/process.rs`: parentesco de `/proc`, identidad PID/inicio, clasificación de renderers, zygotes, GPU, utility, helpers y crashpad con doble fork ligado al perfil. Caídas reales por SIGKILL de renderizadores y navegador generan eventos; la supervisión sigue operativa mientras hay órdenes pendientes. |
| Sandbox | Cada renderizador exige Seccomp 2, NoNewPrivs 1 y user/net namespace distintos del navegador. Arranque con al menos un renderer verificado; muestreo 50 ms y comprobación antes de reanudar targets nuevos. Inicialización acotada a un segundo por identidad nueva mientras el target está pausado; regresión de una identidad verificada falla inmediatamente. Pruebas negativas deterministas y copia real de Chrome sin perfil AppArmor/helper utilizable: rechazo `sandbox_unavailable`. No se usa `--no-sandbox` ni se cambia AppArmor. |
| Política de versión | `crates/browser/src/version.rs`: sonda instalada antes de abrir perfiles persistentes, `Browser.getVersion` al arrancar y refresco programado cada 24 h mediante la misma ruta comprobada con feeds locales y refresco explícito. 0/1 mayores por detrás normal; 2 aviso/evento; 3 o más o fecha conocida de más de 42 días bloquea persistentes y deja efímeras disponibles. Se comprueba que el perfil bloqueado permanece vacío, el aviso y el bloqueo tras cambiar metadatos. |
| Diagnóstico y dependencias reales | `webd --check-instance` es puntual, usa perfil propio temporal y proxy local de rechazo total, verifica realmente y elimina su perfil. No hay servidor RPC, sesiones de aplicación, visor ni herramientas del arnés. La integración del broker/red y gestión de perfiles de aplicación permanecen fuera del alcance; la API nativa requiere un proxy del host confiable. |
| Documentación | `docs/development/browser.md` y `browser.es.md` actualizados conjuntamente, sin historia de errores ni etiquetas de hitos. `AGENTS.md` añade los comandos reales. No se tocaron arquitecturas externas, main ni crates existentes ajenos al alcance. |

La sonda acotada existente se trasladó a `crates/browser/src/installation.rs` y se reexporta desde `excalibur-webd`, conservando su API, para reutilizarla en la admisión previa a Chrome sin duplicar lógica. El mismo límite de cinco segundos cubre los reintentos exclusivos de `ETXTBSY`; la condición se reprodujo de forma controlada y se prueba liberando un descriptor de escritura. No se reintentan errores de permisos ni versiones inválidas. Se han repetido las regresiones de preflight con ambos umask.

### Evidencia de Chrome instalado

Producto observado: `Chrome/156.0.8078.79`, protocolo `1.3`, revisión `@a21ce95c24ce3964416f09ae1862f53f319eb4d0`. Contrato y traza redactada por método/estado en `target/test-evidence/browser/cdp_contract/contract.json`; no se guardan los 70 MiB de contenido. Incluye 5 renderizadores verificados, OOPIF, ningún socket TCP de escucha de los PIDs del árbol en sus namespaces y ausencia de `DevToolsActivePort`. Los cuatro errores esperados corresponden a operaciones sin estado activo (descarga inexistente, diálogo no abierto o requestId de Fetch inexistente); no afirman implementar todavía esos flujos de aplicación.

`boundary.json` registra: browser_crash_detected, ephemeral_outdated_available, persistent_outdated_blocked, renderer_crash_detected, unprofiled_binary_rejected, version_refresh_blocks_persistent, version_warning_emitted. `parent-death.json` conserva las identidades de los dos árboles y la ausencia de procesos vivos restantes. Los dos helpers marcados `ignored` son ejecutados explícitamente por sus pruebas padre; no se ha usado `EXCALIBUR_BROWSER_TESTS=skip`.

Diagnóstico del binario final: `target/debug/excalibur-webd --check-instance /tmp/excalibur-browser-instance.toml`, código 0, `ready: true`, `sandbox_verified: true`, proxy `deny_all_diagnostic_proxy`, perfil propio eliminado. Evidencia en `target/test-evidence/browser/instance_diagnostic/instance.json` y `identity.txt`. Lecturas directas de `/proc` durante la ejecución:

```text
webd
Uid:	999	999	999	999
Gid:	988	988	988	988
Groups:	 
CapInh:	0000000000000000
CapPrm:	0000000000000000
CapEff:	0000000000000000
CapAmb:	0000000000000000
Chrome
Uid:	999	999	999	999
Gid:	988	988	988	988
Groups:	 
CapInh:	0000000000000000
CapPrm:	0000000000000000
CapEff:	0000000000000000
CapAmb:	0000000000000000
```

No se consultó una estable real por Internet ni se inventó la fecha de publicación instalada: el diagnóstico mantiene `stable_major: null`, `installed_release_unix_secs: null`, estado `unknown` y aviso de vigencia no verificada. La consulta pública optativa y la política de antigüedad se verifican con datos locales/sintéticos; no se ha esperado 24 h reales. La disponibilidad del sandbox sí se ha comprobado con Chrome real. No se afirma funcionamiento con Google, WhatsApp o Teams ni continuidad de cuentas.

### Checks ejecutados

| Comando | Resultado | Log/evidencia |
|---|---|---|
| `cargo fmt --check` | Código 0 | `/tmp/browser-format.log` |
| `cargo clippy --workspace --all-targets -- -D warnings` | Código 0 | `/tmp/browser-all-clippy.log` |
| `(umask 0077; cargo test -p excalibur-cdp -p excalibur-browser -p excalibur-webd)` | Código 0: transporte, política, contrato real y preflight | `/tmp/browser-backend-final.log`; copias en `target/test-evidence/browser/transport/` y `policy/` |
| `(umask 0022; cargo test -p excalibur-webd --test preflight)` | Código 0; 12 aprobadas | `/tmp/browser-preflight-final022.log` |
| `cargo test -p excalibur-browser --test cdp_contract` con el fixture DOM de 70 MiB | Código 0; 2 pruebas padre aprobadas, 2 helpers invocados por ellas | `/tmp/browser-cdp-contract-dom.log` |
| `cargo build --workspace` | Código 0 | `/tmp/browser-final-build.log` |
| `(umask 0077; cargo test --workspace --no-fail-fast)` | Primera ejecución de cierre con código 101 por una prueba de evaluación; repetición completa código 0 | `/tmp/browser-workspace-release.log`, `/tmp/browser-workspace-retry.log` y `.exit` |
| Binario exacto de `excalibur-eval --test noninteractive`, `--exact wall_timeout_during_inference_pauses_with_exit_4 --nocapture` | Código 0; 1 aprobada aislada | `/tmp/browser-eval-isolated.log` y `.exit` |
| `--preflight` y `--check-instance` del binario final | Código 0 ambos | `target/test-evidence/browser/instance_diagnostic/` |
| `git diff --cached --check` y `git diff --check 7e217a0797fd7b9f2fdb0f0961759620079534b9 be483f56fc61aef9845f87d26d60caaafcb6d578` | Código 0 | `/tmp/browser-commit-diff-check.log` para el rango de commits |

Observación de verificación fuera del alcance: la ejecución completa con código 101 falló en `excalibur-eval/tests/noninteractive.rs`, `wall_timeout_during_inference_pauses_with_exit_4`, por la aserción sobre el motivo de pausa. La prueba pasó aislada y la repetición exacta de la suite completa, sin cambiar eval, dio código 0. Se conserva el fallo observado y no se declara corregido ese riesgo temporal. No se omitió ninguna suite para obtener el resultado completo. La prueba específica posterior concreta el exceso de tamaño con DOMSnapshot real y también pasó.

Todas las pruebas y sitios son locales/sintéticos, sin APIs de modelos, cuentas reales ni benchmarks pagados. No se han desplegado servicios ni alterado servicios ajenos. La preparación conserva el usuario dedicado y Chrome estable instalados, bubblewrap verificado y la restricción de user namespaces de AppArmor.

Al cierre, `target` propio ocupa 32565088896 bytes y el filesystem del repositorio tiene 167160934400 bytes libres. Preflight midió 167157698560 bytes libres en datos; se respetan reserva de 5 GiB y aviso de 10 GiB. No se limpiaron artefactos ni archivos ajenos.

### Commit y remoto

Commit `be483f56fc61aef9845f87d26d60caaafcb6d578` — `Add a pipe-based CDP client and sandboxed Chrome supervision`.

Push exclusivamente a `origin/browser`, sin force-push. `git ls-remote --heads origin browser` devuelve `be483f56fc61aef9845f87d26d60caaafcb6d578` en `refs/heads/browser`, idéntico a HEAD. Rama local `browser`, árbol limpio. Configuración de máquina, coordinador, logs, evidencias e informe externo no se incluyeron en Git. Entrega finalizada sin avanzar al siguiente alcance.

## Auditoría — revisión de `be483f56fc61aef9845f87d26d60caaafcb6d578`

Alcance auditado: entrada B1 «Cliente CDP y gestor de instancias» de 19.2 y sus referencias: 4.3 (transporte y cliente), 4.4 (flags y prohibidos), 4.5 (política de versión), 4.6 (pruebas), 12.2 y filas de sandbox de 12.9, apéndice B.3, suite `cdp_contract` de 18.2 y principios de 18.1. Las partes de 3.2 que aplican son el lanzamiento de Chrome con PDEATHSIG y grupo propio, y el no-root. Hallazgos previos a verificar: ninguno (lista vacía). No se exigen hitos posteriores (sesiones, socket, broker, observación). Rama `browser`: `HEAD` = `origin/browser` = `be483f56fc61aef9845f87d26d60caaafcb6d578`, árbol limpio. No se ha modificado código de producto ni arquitectura. Las pruebas manuales usaron `/tmp/audit-b1`, `/tmp/audit-b1-run` y un programa auxiliar fuera del repositorio, `/tmp/audit-b1-scratch` (depende por ruta de `excalibur-browser`/`excalibur-cdp`, con `target` propio ya eliminado). `umask` de la sesión: 0077.

Veredicto: **cambios requeridos** (un hallazgo bloqueante, AUD-B1-01).

### Comprobaciones reproducidas

| Comprobación | Resultado | Evidencia |
|---|---|---|
| `cargo fmt --check` | Código 0 | `/tmp/audit-b1/fmt.log` |
| `cargo clippy --workspace --all-targets -- -D warnings` | Código 0 | `/tmp/audit-b1/clippy.log` |
| `cargo build --workspace` | Código 0 | `/tmp/audit-b1/build.log` |
| `cargo test -p excalibur-cdp -p excalibur-browser` | Código 0 | `/tmp/audit-b1/cdp-browser.log` |
| `cargo test -p excalibur-browser --test cdp_contract -- --nocapture` | Código 0; 2 pruebas padre aprobadas, 2 helpers invocados por ellas; sin `EXCALIBUR_BROWSER_TESTS=skip` | `/tmp/audit-b1/cdp_contract.log`, `target/test-evidence/browser/cdp_contract/` |
| `cargo test -p excalibur-webd --test preflight` con umask 0077 y 0022 | Código 0 en ambos | `/tmp/audit-b1/preflight077.log`, `preflight022.log` |
| `cargo test --workspace --no-fail-fast` (umask 0077) | Código 0; 139 resultados `ok`, 0 fallidos | `/tmp/audit-b1/tests.log` |
| `git diff --check 7e217a0 be483f5` | Código 0 | Salida de terminal |

### Evidencia real con Chrome instalado

- Contrato (`contract.json` de la ejecución de auditoría): Chrome/156.0.8078.79, protocolo 1.3. 73 órdenes: 68 `ok`, 4 `expected_state_error` (`Browser.cancelDownload`, `Page.handleJavaScriptDialog`, `Fetch.continueRequest`, `Fetch.failRequest`, todas por falta de estado) y 1 `clean_shutdown` (`Browser.close`). Ningún `method not found`. Las 69 órdenes de B.3 están en el catálogo (`protocol.rs`), más `DOM.getDocument`, `DOM.querySelector`, `Target.detachFromTarget` y `Runtime.runIfWaitingForDebugger`. `listeners: []`, `oopif`, `oversized_dom_snapshot` y `oversized_session_isolated` a true. `boundary.json` registra a true las caídas de navegador y de renderizador, el bloqueo persistente, la efímera disponible, el binario sin perfil rechazado, el refresco que bloquea y el aviso emitido. `parent-death.json`: `remaining_live: []`.
- `target/debug/excalibur-webd --check-instance /tmp/audit-b1-run/web.toml` lanzado como root: código 0, `ready: true`, `sandbox_verified: true`, `network: deny_all_diagnostic_proxy`, estado de versión `unknown` con aviso explícito. Los tres renderizadores tienen `Seccomp 2`, `NoNewPrivs 1` y `user:[4026532284]`/`net:[4026532290]`, distintos del navegador (`user:[4026531837]`/`net:[4026531840]`). El perfil temporal se elimina: `/var/lib/excalibur-web` queda vacío. Durante la ejecución, `/proc` de webd y de Chrome muestra `Uid 999×4`, `Gid 988×4`, `Groups` vacío y `Cap*` a cero, y no existe `DevToolsActivePort` (`/tmp/audit-b1/identity.txt`).
- Flags prohibidos y sustituciones en `chrome_args` (`--no-sandbox`, `--remote-debugging-port=9222`, `--disable-web-security`, `--single-process`, `--enable-automation`, `--disable-features=site-per-process`, `--user-data-dir=…`, `--proxy-server=direct://`): todos salen con código 1 y «unapproved Chrome flag» antes de crear perfiles.
- Copia de `/opt/google/chrome` sin `chrome-sandbox` y fuera de la ruta del perfil AppArmor: `{"error":"sandbox_unavailable: startup failed before verification: …","ready":false}`, código 1, sin procesos residuales.
- Política de versión con `stable_major` configurado: persistente con 3 mayores de diferencia → bloqueada (código 1); efímera con 3 de diferencia → `ready: true`, estado `blocked_persistent`; persistente con 2 → `warning` con aviso; persistente con fecha de publicación de más de 42 días → bloqueada.
- Disco al cierre: `target` 32 637 666 958 bytes; 167 019 728 896 bytes libres, por encima de la reserva de 5 GiB y del aviso de 10 GiB.

### Requisitos

| Requisito (fuente) | Estado | Evidencia |
|---|---|---|
| CDP por tubería FD 3/4, JSON terminado en NUL, sin puerto (4.3) | Verificado | `lib.rs` (`pre_exec` con `dup2` a 3/4, `--remote-debugging-pipe`); `listeners: []`; sin `DevToolsActivePort` |
| Correlación, IDs crecientes sin reutilización y plazos 10/15/20 s con `cdp_timeout` (4.3) | Verificado | `crates/cdp/src/lib.rs`, `protocol.rs:11`; suite `transport` |
| Sesiones planas, `setAutoAttach` con `waitForDebuggerOnStart` y OOPIF (4.3) | Verificado | `oopif: true`; el monitor reanuda los targets tras verificar el sandbox |
| Límite de 64 MiB por sesión, con el resto de sesiones intactas (4.3, 4.6) | Verificado | DOMSnapshot real de 70 MiB: `oversized_session_isolated: true` |
| EOF del descriptor 4 → `instance_crashed` (4.3) | Verificado | `boundary.json` `browser_crash_detected` |
| Flags de 4.4 y lista de prohibidos (4.4, 4.6) | Verificado | `LaunchOptions::arguments`, `validate_args`; reproducción manual |
| PDEATHSIG, grupo de procesos y SIGKILL del padre con dos instancias (3.2, 18.2) | Verificado | `parent-death.json` `remaining_live: []` |
| Árbol de procesos con crashpad de doble fork (13.1 como base, B1) | Verificado | `process.rs`; evidencia `processes` |
| Detección de caídas (B1) | **No cumplido** | AUD-B1-01: falsa caída provocada por contenido de la página |
| Verificación del sandbox de cada renderizador y bloqueo `sandbox_unavailable` (12.2, 12.9 filas 1–2) | Verificado | `check-instance` real; copia sin sandbox rechazada |
| Política de versión: `Browser.getVersion` al arrancar y cada 24 h, aviso con 2, bloqueo persistente con ≥3 o >6 semanas, efímera disponible (4.5, 4.6) | Verificado | Reproducción manual y `boundary.json`; la consulta pública es optativa |
| Contrato de cada orden de B.3 contra la versión instalada (4.6, `cdp_contract`) | Verificado | 73 órdenes, sin `method not found`, con la versión registrada |
| Chrome real obligatorio; omisión solo explícita (18.1) | Verificado | La suite falla cerrada; no se usó `skip` |
| Documentación EN/ES y `AGENTS.md` (19.1) | Verificado con reserva | Equivalentes; no describen el efecto de AUD-B1-01 |
| Sin regresiones | Verificado | Workspace completo en verde; `preflight` con ambos umask |

### Hallazgos

#### AUD-B1-01 — Bloqueante (severidad alta): una ráfaga de eventos CDP de la página se trata como caída y mata la instancia entera

- Evidencia (código): el monitor de `Instance::launch` (`crates/browser/src/lib.rs:380-421`) consume `client.subscribe_all()`, un `broadcast` de capacidad 1024 (`crates/cdp/src/lib.rs:184`), y ante cualquier `Err` de `recv()`, incluido `RecvError::Lagged`, ejecuta `inner.stop(Status::Crashed, "instance_crashed", "event stream lagged or closed")`, que hace `SIGKILL` al grupo. Además, cada iteración del bucle, también la de cada evento, hace `try_wait` y dos recorridos completos de `/proc` (`tree_for_profile`), lo que limita el ritmo al que el monitor consume eventos.
- Evidencia (reproducción con Chrome real, como `excalibur-web`): instancia efímera, `Target.createTarget about:blank`, `attach_page`, `Runtime.enable` (orden del catálogo de B.3) y `Runtime.callFunctionOn` con un bucle de `console.log`. Con 200 mensajes el estado sigue `Degraded` y `Browser.getVersion` responde. Con 1500, 5000 y 20 000 mensajes (3 de 3 ejecuciones): estado `Crashed`, evento `instance_crashed: event stream lagged or closed`, y después `Browser.getVersion` devuelve `Closed("EOF on descriptor 4")`. Chrome sigue sano hasta que lo mata webd. Programa: `/tmp/audit-b1-scratch/src/main.rs`.
- Impacto: cualquier página no confiable que genere unos pocos miles de eventos (consola, red, ciclo de vida, targets) en cuanto un consumidor active esos dominios provoca la muerte de todo el navegador. En la instancia persistente arrastra también a las sesiones en escucha (5.7, 13.3) y a las demás pestañas. Esto contradice la detección de caídas de B1 («fin de flujo en el descriptor 4 = navegador terminado») y la documentación, que solo atribuye `instance_crashed` a EOF o a la muerte del navegador.
- Recomendación: no tratar `Lagged` como caída. El monitor solo necesita `Target.attachedToTarget`, los eventos de caída e `instance_crashed`; conviene darle un canal propio filtrado y sin pérdida (o acotado con contabilidad de pérdidas que no mate la instancia), o reconciliar con `Target.getTargets` y `/proc` tras un `Lagged`. Separar el muestreo de `/proc` (tick de 50 ms, idealmente en `spawn_blocking`) del consumo de eventos. Añadir a `cdp_contract` una prueba con Chrome real que emita ≥ 10 000 eventos de consola y compruebe que la instancia sigue `Running`/`Degraded` y responde, y documentar el comportamiento ante contrapresión.

#### AUD-B1-02 — Informativo (no bloqueante): la detección de caídas por desbordamiento no está en la documentación

- Evidencia: `browser.md`/`browser.es.md` dicen «EOF or browser death produces `instance_crashed`» y no mencionan la causa «event stream lagged or closed» de `lib.rs:421`.
- Recomendación: tras corregir AUD-B1-01, documentar qué causas producen `instance_crashed` y cómo se tratan la contrapresión y la pérdida de eventos.

### Hallazgos previos

No había IDs previos que verificar.


## Respuesta de Codex

### Corrección 1 — cliente CDP y gestor de instancias

Operación `flow-operation:1dfcf6d95eb0431da993e81001fa8ad2`. Auditoría atendida: `be483f56fc61aef9845f87d26d60caaafcb6d578`. Se conservan íntegramente la respuesta anterior y la auditoría. Trabajo limitado a detección/supervisión de instancias y sus referencias de 19.2, 4.3–4.6 y 12.2; no se inicia el siguiente alcance ni un auditor.

| ID | Estado | Corrección y evidencia |
|---|---|---|
| AUD-B1-01 | Resuelto | `crates/cdp/src/lib.rs`: canal acotado propio para adjuntos, desconexiones y notificaciones de caída; consola/red no lo ocupan. Caché independiente de sesiones adjuntas activas actualizada antes de notificar; `Lagged` permite reconciliarla y no cierra el transporte. `crates/browser/src/lib.rs`: tareas separadas para eventos y muestreo; `/proc` y estado del hijo se inspeccionan con `spawn_blocking` solo en el temporizador, más las verificaciones necesarias para adjuntar targets. La terminación real de CDP se conserva mediante watch independiente. Si se retrasa el canal de ciclo de vida, se recuperan los adjuntos activos y se emite `events_resynchronized`, sin SIGKILL por contrapresión. |
| AUD-B1-02 | Resuelto | `docs/development/browser.md` y `browser.es.md` describen causas de `instance_crashed`, cierre explícito, contrapresión de observadores, reconciliación de ciclo de vida y fallo de supervisión, sin historia de errores ni etiquetas de hitos. `AGENTS.md` incorpora la verificación de ráfagas y reconciliación. |

### Regresión y evidencia

- `crates/browser/tests/cdp_contract.rs`: Chrome real con `Runtime.enable` y 10.000 llamadas `console.log` en una página local sintética. Un observador general queda deliberadamente sin consumir y recibe `Lagged`; la instancia sigue `degraded`, responde a `Browser.getVersion`, conserva el sandbox verificado y adjunta un nuevo target que ejecuta una función y devuelve 42. No se recibe `instance_crashed`.
- `crates/cdp/tests/transport.rs`: 12.000 eventos sintéticos de consola no desplazan las tres notificaciones de ciclo de vida ni impiden correlacionar una respuesta. Otro caso fuerza `Lagged` del propio canal de ciclo de vida con 400 adjuntos y recupera los 400 desde la instantánea, sin cerrar CDP. Los productores de estos fixtures permanecen abiertos hasta una confirmación explícita del test, sin depender de un retraso temporal para comprobar liveness.
- La instantánea de adjuntos mantiene estado activo, no una cola histórica ilimitada. Puede haber notificaciones históricas omitidas, que se declaran mediante `events_resynchronized`; la reconciliación recupera los adjuntos activos para verificarlos y reanudarlos. Las muertes reales se detectan por el proceso y el watch de transporte, no por pérdida de notificaciones de un observador.
- Se conserva el contrato real completo: órdenes de B.3, OOPIF, DOMSnapshot de 70 MiB aislado a su sesión, flags prohibidos, sandbox, política de versión, caídas reales y SIGKILL del padre con dos instancias. Los helpers `ignored` siguen siendo invocados por las pruebas padre; no se ha utilizado `EXCALIBUR_BROWSER_TESTS=skip`.

Resultado de la regresión real, `target/test-evidence/browser/cdp_contract/console-backpressure.json` (además contiene las lecturas de sandbox de los procesos):

```json
{
  "console_events": 10000,
  "false_instance_crashed": false,
  "new_target_after_flood": true,
  "ordinary_observer_lagged": true,
  "status_after": "degraded",
  "version_after": "Chrome/156.0.8078.79"
}
```

### Checks ejecutados

| Comando | Resultado | Evidencia |
|---|---|---|
| `cargo fmt --check` | Código 0 | `/tmp/browser-b1-correction1-fmt.log` |
| `cargo clippy --workspace --all-targets -- -D warnings` | Código 0 | `/tmp/browser-b1-correction1-clippy.log` |
| `(umask 0077; cargo test -p excalibur-cdp -p excalibur-browser -p excalibur-webd)` | Código 0; incluye contrato con 10.000 eventos, 9 pruebas de transporte, 6 de política y preflight | `/tmp/browser-b1-correction1-backend-final.log` |
| `(umask 0022; cargo test -p excalibur-webd --test preflight)` | Código 0; 12 aprobadas | `/tmp/browser-b1-correction1-preflight022.log` |
| `(umask 0077; cargo test --workspace --no-fail-fast)` | Código 0; 139 resúmenes `ok`, sin targets fallidos, incluidos el contrato definitivo y doc-tests | `/tmp/browser-b1-correction1-workspace.log` |
| `cargo build --workspace` | Código 0 | `/tmp/browser-b1-correction1-build.log` |
| `target/debug/excalibur-webd --check-instance /tmp/excalibur-browser-instance.toml` | Código 0; `ready: true`, `sandbox_verified: true`, `Chrome/156.0.8078.79` | `/tmp/browser-b1-correction1-instance.json` |
| `git diff --cached --check` y `git diff --check be483f56fc61aef9845f87d26d60caaafcb6d578 8cb3341dc65ae7a7da4d64ce5e76fca1612a978f` | Código 0 | `/tmp/browser-b1-correction1-diff-check.log` para el rango entre commits |

Todos los checks de esta corrección han pasado; no se modificó `eval` ni se ocultaron suites. Pruebas locales y sintéticas, sin APIs de modelos, cuentas reales ni benchmarks pagados. No hay frontend nuevo que revisar. No se desplegaron servicios, no se alteraron servicios ajenos y no se modificaron las arquitecturas externas ni main.

Disco al cierre: `target` propio 33016236484 bytes; filesystem del repositorio 166632747008 bytes libres, por encima de reserva de 5 GiB y aviso de 10 GiB. No se limpiaron archivos ajenos.

### Commit y comprobación remota

Commit `8cb3341dc65ae7a7da4d64ce5e76fca1612a978f` — `Separate Chrome supervision from page event backpressure`.

Push exclusivamente a `origin/browser`, sin force-push. Verificación posterior con `git ls-remote --heads origin browser`: `8cb3341dc65ae7a7da4d64ce5e76fca1612a978f` en `refs/heads/browser`, idéntico a HEAD. Rama local `browser`, árbol de trabajo limpio. Este informe, los logs, la configuración de máquina y las evidencias permanecen fuera de Git. No quedan hallazgos pendientes dentro del alcance asignado y no se avanzó al siguiente.

## Auditoría — revisión de `8cb3341dc65ae7a7da4d64ce5e76fca1612a978f`

Alcance: la misma entrada B1 de 19.2 y sus referencias (4.3–4.6, 12.2, filas de sandbox de 12.9, B.3, `cdp_contract` de 18.1–18.2 y lanzamiento de 3.2), más los hallazgos AUD-B1-01 y AUD-B1-02. Rama `browser`: `HEAD` = `origin/browser` = `8cb3341dc65ae7a7da4d64ce5e76fca1612a978f`, árbol limpio. El diff `be483f5..8cb3341` toca `crates/cdp/src/lib.rs`, `crates/browser/src/lib.rs`, sus pruebas `transport.rs`/`cdp_contract.rs`, `AGENTS.md` y `browser*.md`. No se ha modificado producto ni arquitectura. Las pruebas manuales usaron `/tmp/audit-b1r1`, `/tmp/audit-b1-run` y el programa auxiliar externo `/tmp/audit-b1-scratch/src/main.rs`, recompilado contra este commit (su `target` ya eliminado). `umask` de la sesión: 0077.

Veredicto: **aprobado**.

### Comprobaciones reproducidas

| Comprobación | Resultado | Evidencia |
|---|---|---|
| `cargo fmt --check` | Código 0 | `/tmp/audit-b1r1/fmt.log` |
| `cargo clippy --workspace --all-targets -- -D warnings` | Código 0 | `/tmp/audit-b1r1/clippy.log` |
| `cargo build --workspace` | Código 0 | `/tmp/audit-b1r1/build.log` |
| `cargo test -p excalibur-cdp -p excalibur-browser` | Código 0 | `/tmp/audit-b1r1/cdp-browser.log` |
| `cargo test -p excalibur-browser --test cdp_contract -- --nocapture` | Código 0; 2 pruebas padre aprobadas y 2 helpers invocados por ellas, sin `skip` | `/tmp/audit-b1r1/cdp_contract.log` |
| `(umask 0022; cargo test -p excalibur-webd --test preflight)` | Código 0 | `/tmp/audit-b1r1/preflight022.log` |
| `cargo test --workspace --no-fail-fast` (umask 0077) | Código 0; 139 resultados `ok`, 0 fallidos | `/tmp/audit-b1r1/tests.log` |
| `git diff --check be483f5 8cb3341` | Código 0 | Salida de terminal |

### Verificación de hallazgos previos

| ID | Estado | Evidencia |
|---|---|---|
| AUD-B1-01 | Resuelto | La supervisión se divide en `supervise_processes` (tick de 50 ms con `MissedTickBehavior::Skip`; `/proc` y `try_wait` en `spawn_blocking`) y `supervise_events`, que consume `subscribe_lifecycle()`: un canal acotado (256) solo con attach/detach, caídas de target e `instance_crashed`, que la consola ya no puede desplazar. Ante `Lagged` reconcilia con `attached_sessions()` (caché de adjuntos que mantiene el lector) y emite `events_resynchronized`, sin matar Chrome. El cierre del transporte se lee del `watch` persistente. Reproducción con Chrome real como `excalibur-web` y el mismo procedimiento que fallaba: 1500, 20 000 y 50 000 `console.log` → estado `Degraded` (versión sin metadatos), sin eventos `instance_crashed`, `Browser.getVersion` responde, un target nuevo evalúa `6*7 = 42`, `verify_sandbox` ve 5 renderizadores verificados y `close` deja `Stopped`. Con 5000 mensajes más 300 workers dedicados: el último worker responde 41→42 (fue reanudado tras la verificación), 605 sesiones adjuntas activas, sin caída. No quedan procesos de Chrome del usuario web. `cdp_contract` añade 10 000 eventos reales (`console-backpressure.json`: `ordinary_observer_lagged: true`, `status_after: degraded`, `new_target_after_flood: true`, `false_instance_crashed: false`). |
| AUD-B1-02 | Resuelto | `browser.md`/`browser.es.md` describen el canal de supervisión separado, el `Lagged` de los observadores sin caída, la reconciliación y `events_resynchronized` (aclarando que es reconciliación de estado, no un registro sin pérdidas) y la lista de causas de `instance_crashed`. Las versiones EN y ES son equivalentes; `AGENTS.md` está actualizado. La reconciliación tras `Lagged` del propio canal de ciclo de vida se prueba con fixtures de transporte (400 adjuntos) y la documentación lo dice así. |

### Regresiones y requisitos

- Contrato con Chrome/156.0.8078.79: 73 órdenes (68 `ok`, 4 `expected_state_error`, 1 `clean_shutdown`), sin `method not found`; `oopif`, `oversized_session_isolated` y `oversized_dom_snapshot` a true; `listeners: []`. En `boundary.json` las siete comprobaciones están a true. `parent-death.json`: `remaining_live: []`.
- `excalibur-webd --check-instance` real: código 0, `ready` y `sandbox_verified` a true, proxy de rechazo total. Todos los renderizadores tienen `Seccomp 2`, `NoNewPrivs 1` y user/net ns distintos del navegador. `/proc` de webd y de Chrome muestra 999/988 sin grupos ni capacidades (`/tmp/audit-b1r1/identity.txt`). El perfil temporal se elimina.
- `--no-sandbox` en `chrome_args` → código 1, «unapproved Chrome flag». Copia de Chrome sin `chrome-sandbox` ni perfil AppArmor → `sandbox_unavailable`, código 1. Persistente con 3 mayores de diferencia → bloqueado, código 1. Sin procesos residuales y `/var/lib/excalibur-web` vacío.
- Todos los requisitos aplicables de B1 quedan verificados, incluida la detección de caídas, que estaba pendiente: EOF/salida del navegador → `instance_crashed` (`boundary.json`), caídas de renderizador → eventos de target, y la contrapresión de eventos ya no se interpreta como caída.
- Disco al cierre: `target` 33 039 559 289 bytes; 166 605 729 792 bytes libres, por encima de la reserva de 5 GiB y del aviso de 10 GiB.

Hallazgos nuevos: ninguno.
