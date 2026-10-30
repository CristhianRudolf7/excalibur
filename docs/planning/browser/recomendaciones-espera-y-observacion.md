
## Respuesta de Codex

Operación `flow-operation:0892d5f2da4946ebb8fbc5326e180a4c`. Corrección 0. Fecha UTC: 2026-10-30T09:31:06+00:00.

Implementada «Espera y observación» de 19.2, con sus verificaciones de 6.8 y 7.5 y los requisitos NB-06, NB-07, NB-08 y NB-25. No se implementan acciones de entrada, adaptador de herramientas del agente, envío a modelos visuales, broker de red ni visor.

### Cambios

- `crates/browser/src/observation/`: espera de navegación, red y DOM en mundos aislados; condiciones visibles por marco; detección determinista de bloqueos; perfiles de espera por sitio; resultado ready asociado a loaders; límite de captura, DOM/AX por sesión y coordenadas OOPIF. Las llamadas a Chrome se realizan fuera del cerrojo global del modelo y bajo exclusión por pestaña.
- Instantáneas con jerarquía, roles, visibilidad, habilitación, referencias estables y texto de hasta 16 KiB. Validación de referencias con motivos de obsolescencia y remapeo explícito, sin ejecutar entrada. Subárboles leídos del blob guardado. JPEG de calidad 60, página completa limitada y dimensiones CSS lógicas. Valores sensibles redactados antes de serializar y rectángulos enmascarados; se omite la imagen si falta DOM necesario para redactarla.
- Almacenamiento SHA-256/zstd del componente compartido, directorio privado y autorización de ventanas de blobs por ejecución en `webd.db`, durante 24 horas. Consola y red con 500 registros, filtros y secuencias; red sin cuerpos ni cabeceras de autorización/cookies y sin consultas al marcar contaminación. La contrapresión de consola está separada de los eventos de espera y supervisión. La instrumentación de red de oyentes se retira al terminar o cancelar la operación.
- `sessions.rs`, `sessions.sql` y `rpc.rs`: operaciones de observación, navegación con espera, emulación y lectura autorizada de blobs; navegación/emulación usan la reconciliación durable existente. Frontera interna de asentamiento de 250 ms y espera de 5 s para integración posterior, sin implementar acciones. Se conserva la identidad del navegador de perfiles persistentes.
- `crates/cdp/`: canales de eventos filtrados y catálogo ampliado, comprobado con Chrome real. Documentación funcional equivalente EN/ES y comandos de verificación en `AGENTS.md`.

### Verificación

Todas las pruebas se ejecutaron con datos locales y sintéticos. Chrome real `Chrome/156.0.8078.79`, usuario dedicado sin privilegios y sandbox habilitado. No se configuró `EXCALIBUR_BROWSER_TESTS=skip` ni se redujo `EXCALIBUR_LISTEN_TEST_SECONDS`. Los helpers marcados ignored son ejecutados por las pruebas padre.

| Comprobación | Resultado | Registro local |
|---|---|---|
| `cargo fmt --check` | Código 0 | `/tmp/observe-fmt.log` |
| `cargo clippy --workspace --all-targets -- -D warnings` | Código 0 | `/tmp/observe-clippy.log` |
| `umask 0077; cargo test --workspace --no-fail-fast` | Código 0; 142 resultados de suite correctos, 616 pruebas contabilizadas | `/tmp/observe-workspace-final.log` |
| `cargo build --workspace` | Código 0 | `/tmp/observe-build.log` |
| `cargo test -p excalibur-browser --test observe -- --nocapture` | Código 0; repetición final tras completar los casos de cierre | `/tmp/observe-final.log` |
| `cargo test -p excalibur-webd --test preflight`, con umask 0077 y 0022 | Código 0; 12 pruebas en cada ejecución | `/tmp/observe-preflight077.log`, `/tmp/observe-preflight022.log` |
| `git diff --check 99ab5c6d3714c8b8099bbc822de8ed19a79dda94 HEAD` | Código 0 | Comprobado sobre los commits |

Evidencias en `target/test-evidence/browser/observation/`: página estática, datos diferidos, condiciones presentes y ausentes, selector en iframe, long-polling degradado y estricto, WebSocket continuo, DOM inestable, diálogo y captcha simulado. Captura de 300 controles más shadow DOM abierto/cerrado y dos iframes; coordenadas OOPIF y estados oculto/deshabilitado; redacción en texto, elementos, árbol e imagen; referencias estables, sustituidas, navegadas, cambiadas, eliminadas, reemplazadas, marco desaparecido e invalidación de intervención/reinicio. También subárbol inmutable después de cambiar el DOM, rechazo de blobs a otra ejecución, captura sin espera previa y en segundo plano, emulación móvil de 360 px y consulta de medios, métricas, cabecera Authorization ausente y límite de 500 mensajes.

La página de 5.000 controles conserva 10008 nodos en el blob; `omitted=4601`, `partial=true` y texto dentro del límite. La prueba exige plazo de captura de 3 s o señalización parcial. El contrato CDP recorrió 78 órdenes y registró `listeners=[]`.

La suite de sesiones conservó la escucha durante 600 segundos y registró 30 cambios de título. Pasaron reconciliación del oyente, aislamiento frente al renderizador en bucle y frontera del servicio. Como dependencia se verificó la entrega anterior `99ab5c6`; no se abre otra corrección de su alcance ni se presenta esta comprobación como su auditoría final. El fixture heredado espera ahora el documento y el estado ready antes de cambiar de pestaña: la primera ejecución completa detectó esa carrera; la repetición de cierre pasó íntegra. La comprobación de preparación pasó bajo ambos umasks. Quedan 130628456448 bytes libres, por encima de la reserva y del aviso de espacio.

### Entrega

Commit: `53fa55a96826b8a11afc7e3f8222ce01d934a28d` — `Add browser readiness checks and page observation`.

Push realizado exclusivamente a `origin/browser`, sin force-push. `git ls-remote origin refs/heads/browser` devuelve `53fa55a96826b8a11afc7e3f8222ce01d934a28d`, idéntico a HEAD. Árbol de trabajo limpio tras el commit. Este informe es externo al repositorio y conserva cualquier contenido anterior. Sin servicios desplegados, cuentas reales, APIs de modelos ni benchmarks pagados. No se ha iniciado el alcance siguiente.

## Auditoría — revisión de `53fa55a96826b8a11afc7e3f8222ce01d934a28d`

Alcance auditado: entrada B3 «Espera y observación» de 19.2 (NB-06, NB-07, NB-08 y NB-25) y sus referencias: 6.2–6.7 y 7.2–7.4, más los blobs. Las pruebas de cierre son 6.8 y 7.5. Las regresiones abarcan B0–B2 (sesiones, aislamiento del renderizador, contrato CDP y preflight). No se exigen alcances posteriores: acciones, `act.*`, envío al rol `vision`, broker, detectores completos de 9.1 ni visor. Hallazgos previos a verificar: ninguno (lista vacía).

Rama `browser`: `HEAD` = `origin/browser` = `53fa55a96826b8a11afc7e3f8222ce01d934a28d`, árbol limpio, `git diff --check 49ceeaa 53fa55a` con código 0. El rango incluye `99ab5c6` («Keep renderer probes outside the browser state lock»), entrega de B2 que no se reaudita aquí. No se ha modificado producto ni arquitectura.

Las pruebas manuales usaron el programa auxiliar externo `/tmp/audit-b3-scratch/src/main.rs`, que usa la API `Manager::observe` con Chrome real como `excalibur-web` y un proxy local sintético; su `target` ya está eliminado. Se ejecutaron solo cuando no corría ninguna suite. `/var/lib/excalibur-web` y `/run/excalibur-web` siguen vacíos y no quedan procesos de Chrome. `umask` de la sesión: 0077.

Veredicto: **cambios requeridos** (un hallazgo bloqueante, AUD-B3-01).

### Comprobaciones reproducidas

| Comprobación | Resultado | Evidencia |
|---|---|---|
| `cargo fmt --check` | Código 0 | `/tmp/audit-b3/fmt.log` |
| `cargo clippy --workspace --all-targets -- -D warnings` | Código 0 | `/tmp/audit-b3/clippy.log` |
| `cargo build --workspace` | Código 0 | `/tmp/audit-b3/build.log` |
| `cargo test -p excalibur-browser --test observe -- --nocapture` | Código 0 | `/tmp/audit-b3/observe.log`, `target/test-evidence/browser/observation/` |
| `cargo test -p excalibur-browser --test sessions other_sessions_stay_responsive_with_a_spinning_renderer -- --nocapture` | Código 0; latencias máximas: lease 23 ms, list 0,4 ms, status 22 ms, switch 29 ms, RPC Unix 73 ms con un renderizador en bucle | `/tmp/audit-b3/isolation.log`, `target/test-evidence/browser/renderer_isolation/contract.json` |
| `cargo test -p excalibur-browser --test cdp_contract -- --nocapture` | Código 0 | `/tmp/audit-b3/cdp_contract.log` |
| `(umask 0022; cargo test -p excalibur-webd --test preflight)` | Código 0 | `/tmp/audit-b3/preflight022.log` |
| `cargo test --workspace --no-fail-fast` (umask 0077) | Código 0; 142 resultados `ok`, 0 fallidos (incluida la escucha de 600 s) | `/tmp/audit-b3/tests.log` |

### Evidencia de cierre (6.8 y 7.5) en `contract.json` de la ejecución de auditoría

- 7.5: página estática `ready` en 561 ms. La SPA (datos 800 ms después de `load`, abierta con `nav.go`) llega a `ready` a los 1342 ms. Long-polling: `ready` con `degraded: ["network_busy"]`; con `strict_network`, `timeout`. WebSocket continuo: `ready`. DOM que muta sin parar: `timeout` con `criteria_met.dom = false`. `alert()`: `blocked{kind: dialog}`. reCAPTCHA simulado: `blocked{kind: captcha}`. Instantánea sin espera previa: `final_wait` incluido.
- 6.8: página de 5000 controles con 10 008 nodos en el blob, `omitted: 4601` y `partial: true`. Hay controles con shadow DOM abierto y cerrado, iframes del mismo sitio y fuera de proceso, estados de visibilidad y habilitación, y referencias estables o en estado `stale_ref` (`superseded` con `remapped_ref`, `navigated`, `node_detached`, `node_gone`/`changed`, `takeover`, `browser_restarted`). La consola está limitada a 500 entradas y la red no guarda cabecera `Authorization` ni cuerpos. Con `small`, la captura tiene 360 px de ancho lógico y la página responde a la consulta de medios.

### Pruebas adversariales propias (Chrome real)

- **Redacción**: una página con contraseña cuyo valor se teclea por JS (sin atributo), contraseña en shadow DOM **cerrado**, contraseña en un **OOPIF** de `two.test`, `autocomplete=cc-csc` y contraseña desplazada con `transform: translateX(450px)`. Ninguno de los cinco valores aparece en la respuesta de la instantánea ni en el blob del árbol. En la captura JPEG, las cinco regiones tienen brillo medio 0 (enmascaradas) frente a 255 en la región de control, `partial: false` y `missing: []`. Es correcto.
- **Ráfaga de red** (3000 `fetch` en la carga): la instancia y la espera siguen operativas; el anillo de red informa `dropped: 1992`, `event_gap: false`. Esta prueba reveló AUD-B3-01.

### Requisitos

| Requisito (fuente) | Estado | Evidencia |
|---|---|---|
| Espera: compuerta de navegación, DOM estable en mundo aislado, latido, condición visible por marco, diálogo/selector de archivos, `ready`/`degraded`/`timeout`/`blocked` (7.2) | Verificado | `contract.json` (filas de 7.5) |
| Contador de red de las peticiones del documento, con exclusiones y `long_lived` a 3 s (7.2.2) | **No cumplido** | AUD-B3-01: ciego a las peticiones anteriores a la primera observación |
| Perfiles de espera por sitio y contabilidad de esperas por época (7.3–7.4) | Verificado | La instantánea sin espera previa ejecuta la predeterminada |
| Captura DOMSnapshot/AX por sesión, OOPIF con coordenadas, presupuesto de 3 s o `partial` (6.2) | Verificado | `frames.json`, `huge` |
| Interactivos, visibilidad y habilitación (6.2.4–5) | Verificado | `nodes.json`, `snapshot.json` |
| Redacción en texto, lista, blob y captura (6.2.7) | Verificado | Suite y pruebas adversariales |
| Formato con tope de 16 KB, `omitted`, `*new` y `untrusted` (6.3) | Verificado | `snapshot.json`, `huge` |
| Referencias estables y `stale_ref` con motivos y `remapped_ref` sin actuar (6.4) | Verificado | Suite `observe` |
| Capturas JPEG q60, página completa limitada y blob privado (6.5) | Verificado | Suite y sonda |
| Subárbol desde el blob (6.6) | Verificado | Subárbol inmutable tras cambiar el DOM |
| Consola, red y rendimiento acotados; emulación con presets; rechazo de cambio de UA en persistentes (6.7) | Verificado | `network`, `console_count`, `small`; `access.rs` deniega `user_agent` fuera de efímeras |
| Blobs autorizados por ejecución durante 24 h | Verificado | Rechazo de blobs de otra ejecución en la suite |
| Llamadas a Chrome fuera del cerrojo global (`AGENTS.md`) | Verificado | `renderer_isolation` |
| Documentación EN/ES y `AGENTS.md` | Verificado con reserva | Equivalentes; no advierten de la limitación de AUD-B3-01 |
| Sin regresiones de B0–B2 | Verificado | Workspace, `cdp_contract`, preflight, aislamiento |

### Hallazgos

#### AUD-B3-01 — Bloqueante (severidad media): la espera declara la red tranquila sin ver las peticiones iniciadas antes de la primera observación

- Evidencia (código): `Network.enable` solo se envía en `Observation::initialize` (`crates/browser/src/observation/wait.rs`, al empezar `wait`/`devtools`/instantánea). `new_tab_locked` y la apertura de sesión con URL del sitio navegan solo con `Page.enable`. Las peticiones que el documento lanzó antes no generan `requestWillBeSent` y no cuentan en `inflight`, de modo que `network` (`active <= max_inflight && quiet && long == 0`) sale verdadero.
- Evidencia (Chrome real, modo `slow` de la sonda): página `http://one.test/slowdata` que en la carga hace `fetch('/slow')` (5 s) y entonces escribe `DATA_ARRIVED`.
  - Con `tab.new` y la URL, seguido de `observe.wait_ready`: `status: "ready"`, `criteria_met.network: true`, `degraded: []`, `inflight_sample: []` a los 563 ms. La instantánea inmediata aún contiene `LOADING_MARKER` y no `DATA_ARRIVED`.
  - La misma página con `nav.go`: `ready` con `degraded: ["network_busy"]`, `network: false` e `inflight_sample: ["http://one.test/slow"]`, que es lo que exige 7.2.
  - En la ráfaga de 3000 `fetch`, la primera espera dio igualmente `network: true` con miles de peticiones pendientes.
- Impacto: incumple 7.2 paso 2 y NB-08 («esperar la carga antes de capturar o actuar, con criterio verificado») para el primer documento de cada pestaña abierta con URL (`tab.new`, `session.open` de un sitio, restauraciones) y para la espera predeterminada de la primera instantánea (7.4). El informe afirma un criterio de red que no ha medido. La fila SPA de 7.5 pasa porque usa `nav.go`.
- Recomendación: habilitar `Network` (con los búferes de 6.7), el ciclo de vida y el script de estabilidad en el adjunto de cada pestaña no oyente **antes** de navegar. Cuando la red se habilite sobre un documento ya cargándose (pestañas en escucha o adjuntos tardíos), marcar el criterio de red como desconocido (por ejemplo `degraded: ["network_unobserved"]`) o reconciliar con `performance.getEntriesByType('resource')` desde el mundo aislado, en lugar de declararlo tranquilo. Añadir una prueba con Chrome real de `tab.new` con una URL que tenga una petición lenta en la carga.

#### AUD-B3-02 — Informativo (no bloqueante): `observe.subtree` sin referencia devuelve el árbol completo sin límite propio

- Evidencia: `Observation::subtree` (`access.rs`) devuelve todos los nodos seleccionados del blob (hasta 128 MiB descomprimidos). En la página de 5000 controles el blob ya ocupa unos 376 KB comprimidos con 10 008 nodos, y una página mayor superará la trama de 4 MiB del RPC, que el servidor sustituye por `response_too_large`.
- Recomendación: paginar u ofrecer el subárbol como referencia a blob cuando supere el límite del protocolo.

### Observación sobre el alcance anterior

`renderer_isolation` muestra latencias de decenas de milisegundos con un renderizador en `while(true){}`, lo que indica que `99ab5c6` corrige el problema descrito en AUD-B2-05 (informe de B2). Lo registro solo como regresión verificada; no es una reauditoría de B2.

### Hallazgos previos

No había IDs previos que verificar.

Disco al cierre: `target` 69 349 089 238 bytes; 129 820 127 232 bytes libres, por encima de la reserva de 5 GiB y del aviso de 10 GiB.


## Respuesta de Codex

Operación `flow-operation:162ea468a03f4118a2d63357c60c897d`. Corrección 1. Fecha UTC: 2026-10-30T10:27:17+00:00.

### Hallazgos atendidos

**AUD-B3-01 — Resuelto.** Las pestañas no oyentes instalan la observación de red, ciclo de vida y estabilidad del DOM sobre el target vacío antes de navegar. La preparación se aplica tanto a creación/apertura de sitio como a restauración. El contador recibe las peticiones iniciales aunque no se haya invocado antes ninguna operación de observación.

Los adjuntos tardíos y las reanudaciones de recogida de los oyentes conservan la incertidumbre por sesión/documento: `network_unobserved` mantiene `criteria_met.network=false`. Una espera no estricta puede devolver ready degradado tras comprobar DOM/condición y la ventana adicional; la estricta devuelve timeout. Una navegación nueva observada permite verificar de nuevo la red. La limpieza de Network invalida el ready guardado antes de deshabilitar el dominio; la instantánea mantiene la recogida activa durante la admisión de ese ready para evitar una desactivación intermedia.

Prueba con Chrome real y un fetch de 5 segundos en la carga (`initial_network.json`):

- `tab.new` seguido de espera estricta de 1200 ms: `timeout`, red false y `/slow` en `inflight_sample`.
- Apertura de sitio y restauración: ambas devuelven `timeout` con red false y la petición pendiente identificada.
- Primera instantánea tras `tab.new`, sin espera explícita previa: la espera automática termina con los datos `DATA_ARRIVED` presentes.
- Adjunto tardío de un oyente: ready degradado con `network_unobserved` y red false; la espera estricta devuelve timeout. Tras una navegación observada, ready con red true y sin esa degradación.

**AUD-B3-02 — Resuelto.** El subárbol se entrega en línea solo cuando sus nodos serializados ocupan hasta 1 MiB. Si supera ese límite, devuelve `delivery: blob`, `node_count`, `root_ref`, `source_blob` y `nodes` vacío. El blob completo ya está autorizado para la ejecución y se lee por ventanas mediante `blob.read`; no se recaptura la página ni se pierde la selección solicitada, indicada por `root_ref`.

El contrato JSON-RPC con el árbol de 10008 nodos devuelve una respuesta de 579 bytes en la evidencia formateada, muy por debajo de los 4 MiB. La prueba comprueba además la lectura del blob y sus 5000 controles; los subárboles pequeños siguen siendo inline e inmutables.

### Rutas y documentación

En `/root/proyectos/Excalibur/excalibur`: `crates/browser/src/sessions.rs`, `crates/browser/src/observation/{mod,wait,tree,access}.rs` y `crates/browser/tests/observe.rs`. Se actualizaron `AGENTS.md` y la documentación funcional equivalente `docs/development/browser.md` / `browser.es.md`, sin etiquetas de hitos ni historia de incidencias. Se conserva el alcance de espera y observación; no se implementa el siguiente.

### Verificación

| Comando | Resultado | Registro |
|---|---|---|
| `cargo fmt --check` | Código 0 | `/tmp/b3r1-fmt.log` |
| `cargo clippy --workspace --all-targets -- -D warnings` | Código 0 | `/tmp/b3r1-clippy.log` |
| `cargo build --workspace` | Código 0 | `/tmp/b3r1-build.log` |
| `umask 0077; cargo test --workspace --no-fail-fast` | Código 0; 142 resultados de suite correctos, 616 pruebas contabilizadas, sin fallos | `/tmp/b3r1-workspace.log` |
| `cargo test -p excalibur-browser --test observe -- --nocapture` | Código 0; repetición final tras proteger la admisión del ready guardado | `/tmp/b3r1-observe-final.log` |
| `cargo test -p excalibur-webd --test preflight` con umask 0077 y 0022 | Código 0; 12 pruebas en cada ejecución | `/tmp/b3r1-preflight077.log`, `/tmp/b3r1-preflight022.log` |
| `git diff --check 53fa55a96826b8a11afc7e3f8222ce01d934a28d HEAD` | Código 0 | Comprobado sobre commits |

El workspace incluye CDP real, aislamiento frente al renderizador en bucle, reconciliación del oyente y frontera del servicio. La escucha se ejecutó durante 600 segundos y registró 30 cambios de título; no se acortó. Los helpers ignored se ejecutan mediante sus pruebas padre; Chrome no se omitió. Las pruebas permanecen locales y sintéticas, sin cuentas reales, APIs de modelos ni benchmarks pagados. Evidencias en `target/test-evidence/browser/observation/`, en especial `initial_network.json` y `subtree_delivery.json`, además de los directorios de regresión existentes.

Espacio libre al cierre: 129517920256 bytes, superior a la reserva y al aviso de espacio.

### Entrega

Commit: `3a61e355614165228609848983db5401f25b8711` — `Track initial page requests and bound subtree responses`.

Push realizado exclusivamente a `origin/browser`, sin force-push. SHA remoto comprobado con `git ls-remote origin refs/heads/browser`: `3a61e355614165228609848983db5401f25b8711`, idéntico a HEAD. Árbol de trabajo limpio. Informe externo añadido preservando íntegramente las respuestas y la auditoría anteriores. No se desplegaron servicios ni se inició otra etapa o auditoría.

## Auditoría — revisión de `3a61e355614165228609848983db5401f25b8711`

Alcance: la misma entrada B3 de 19.2 (NB-06, NB-07, NB-08 y NB-25) y sus referencias (6.2–6.7, 7.2–7.4 y blobs), con cierre 6.8 y 7.5, más los hallazgos AUD-B3-01 y AUD-B3-02. Rama `browser`: `HEAD` = `origin/browser` = `3a61e355614165228609848983db5401f25b8711`, árbol limpio, `git diff --check 53fa55a 3a61e35` con código 0. El diff solo toca `crates/browser/src/{observation/*,sessions.rs}`, `tests/observe.rs`, `AGENTS.md` y `browser*.md`. Desde `768a497`, la rama no modifica ningún crate fuera de `browser`, `cdp`, `search`, `web-adapter` y `webd`. No se ha modificado producto ni arquitectura.

Volví a ejecutar el programa auxiliar externo `/tmp/audit-b3-scratch/src/main.rs` contra este commit, con Chrome real como `excalibur-web` y sin suites en paralelo; su `target` ya está eliminado. `/var/lib/excalibur-web` y `/run/excalibur-web` siguen vacíos y no quedan procesos de Chrome. `umask` de la sesión: 0077.

Veredicto: **aprobado**.

### Comprobaciones reproducidas

| Comprobación | Resultado | Evidencia |
|---|---|---|
| `cargo fmt --check` | Código 0 | `/tmp/audit-b3r1/fmt.log` |
| `cargo clippy --workspace --all-targets -- -D warnings` | Código 0 | `/tmp/audit-b3r1/clippy.log` |
| `cargo build --workspace` | Código 0 | `/tmp/audit-b3r1/build.log` |
| `cargo test -p excalibur-browser --test observe -- --nocapture` | Código 0 (78 s) | `/tmp/audit-b3r1/observe.log` |
| `cargo test -p excalibur-browser --test sessions other_sessions_stay_responsive_with_a_spinning_renderer -- --nocapture` | Código 0; latencias máximas lease 54 ms, list 0,2 ms, status 51 ms, switch 35 ms, RPC 74 ms | `/tmp/audit-b3r1/isolation.log` |
| `cargo test -p excalibur-browser --test cdp_contract -- --nocapture` | Código 0 | `/tmp/audit-b3r1/cdp_contract.log` |
| `(umask 0022; cargo test -p excalibur-webd --test preflight)` | Código 0; 12 aprobadas | `/tmp/audit-b3r1/preflight022.log` |
| `cargo test --workspace --no-fail-fast` (umask 0077), 1.ª ejecución | Código 101: 141 `ok`; único fallo `excalibur-eval --test verifiable` (AUD-B3-04, fuera de alcance); todas las suites del navegador en verde | `/tmp/audit-b3r1/tests.log` |
| `cargo test --workspace --no-fail-fast` (umask 0077), repetición | **Código 0**; 142 `ok`, 0 fallidos | `/tmp/audit-b3r1/tests2.log` |

### Verificación de hallazgos previos

| ID | Estado | Evidencia |
|---|---|---|
| AUD-B3-01 | Resuelto | `prepare_tab` habilita `Network`, el ciclo de vida, `Runtime`/`Log`, la vinculación y el script de estabilidad mientras el target sigue en `about:blank`, antes de `Page.navigate`, tanto en `new_tab_locked` como en `revive_tab`. Cuando la red se habilita sobre un documento ya cargado (oyentes o adjuntos tardíos) se marca `network_unobserved`: `criteria_met.network` se mantiene en false y solo se admite `ready` degradado tras la ventana adicional, o `timeout` con `strict_network`. Repetí el modo `slow` con Chrome real: `tab.new` a `/slowdata` + `wait_ready` da `ready` a los **5517 ms**, ya con `DATA_ARRIVED` y sin `LOADING_MARKER` (antes: 563 ms con la petición pendiente). `nav.go` se comporta igual. La suite `observe` añade casos de `tab.new` lento, primera instantánea automática y oyente tardío. |
| AUD-B3-02 | Resuelto | `subtree` serializa la selección y, si supera 1 MiB, devuelve `delivery: "blob"`, `node_count`, `root_ref` y `source_blob` con `nodes: []`; en otro caso `delivery: "inline"`. La suite lo comprueba por RPC sobre la página de 5000 controles (`delivery == "blob"`). Documentado en EN/ES. |

### Regresiones y pruebas adversariales repetidas

- Redacción con Chrome real (valor tecleado por JS, shadow DOM cerrado, OOPIF, `cc-csc` y `transform`): ningún valor en la instantánea ni en el blob y las cinco regiones de la captura en negro (brillo 0 frente a 255 del control), sin `partial` ni `missing`.
- Ráfagas: con 100 y 300 `fetch` en la carga, `wait_ready` da `ready` en 0,68 s y 1,13 s, la instantánea funciona y las esperas posteriores durante 30 s también. Con 3000 `fetch` ver AUD-B3-03.
- Se mantienen los requisitos de la revisión anterior (6.2–6.7 y filas de 6.8/7.5 en `contract.json`), el aislamiento del renderizador, el contrato CDP, preflight y la escucha de 600 s dentro del workspace.

### Hallazgos nuevos

#### AUD-B3-03 — Informativo (no bloqueante): con una ráfaga extrema, la espera devuelve `cdp_timeout` antes de su plazo

- Evidencia: página que lanza 3000 `fetch` simultáneos en la carga (con la red ya instrumentada desde el inicio). `observe.wait_ready` con `timeout_ms: 15000` y la instantánea fallan con `Fault { kind: "cdp_error", detail: "cdp_timeout" }` a los ~0,53 s, en 3 de 3 intentos. Cinco segundos después la espera vuelve a dar `ready` en todos los intentos durante 30 s. La causa es que la primera llamada `frames()` (`Page.getFrameTree` con 500 ms) y las de `initialize` propagan el error con `?` antes del bucle, que en cambio sí degrada a `renderer_unresponsive`. Con 100 y 300 peticiones no ocurre. Es un error tipado (`cdp_timeout` está en B.2), no publica datos sin esperar y no es una regresión de corrección (antes la espera devolvía `ready` sin ver esas peticiones).
- Recomendación: tratar los plazos de esas consultas iniciales como en el bucle (`degraded: ["renderer_unresponsive"]`) y seguir esperando hasta `timeout_ms` antes de devolver `timeout`.

#### AUD-B3-04 — Informativo, fuera de alcance (no bloqueante): intermitencia preexistente en `excalibur-eval --test verifiable`

- Evidencia: `five_requirement_task_launches_a_read_only_verifier_and_charges_its_budget` (`crates/eval/tests/verifiable.rs:226`) acabó en `blocked` («k3: an admitted check has no confirmed outcome») en la primera ejecución del workspace y en 1 de 3 ejecuciones aisladas (unos 141 s en vez de 25 s). La rama no modifica ningún crate del arnés desde `768a497`, y ya se observó en la auditoría de B0 (AUD-B0-05). La repetición completa del workspace pasa.
- Recomendación: registrarlo como riesgo temporal de la suite del arnés fuera de este flujo.

### Requisitos

Verificados todos los requisitos aplicables de B3: espera con navegación, red instrumentada desde el inicio o marcada `network_unobserved`, DOM estable, condición, diálogos/captcha y resultados `ready`/`degraded`/`timeout`/`blocked` (7.2–7.4); captura DOM/AX por sesión con OOPIF, interactivos, visibilidad, redacción, formato limitado, referencias y `stale_ref`, capturas, subárbol acotado, devtools y emulación (6.2–6.7); blobs autorizados; llamadas fuera del cerrojo global. No hay regresiones atribuibles a la rama.

Disco al cierre: `target` 69 578 032 177 bytes; 129 586 503 680 bytes libres, por encima de la reserva de 5 GiB y del aviso de 10 GiB.
