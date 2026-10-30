

## Respuesta de Codex

Operación `flow-operation:c4892e6ae80244d89d37592945ac4188`. Implementación inicial del alcance «Perfiles, sesiones, pestañas y protocolo». Fecha UTC: 2026-10-30T05:20:30+00:00.

El Markdown indicado no existía al iniciar; esta respuesta crea el archivo solicitado. No había hallazgos ni respuestas anteriores que preservar ni pendientes heredados. Se leyeron AGENTS.md, la sección Implementación de INSTRUCTIONS.md, la entrada de 19.2 y sus referencias internas (3.2–3.5, 5.1–5.8, 16.3–16.7, apéndices A/B.2/C y contrato del arnés 14.1–14.2). No se modificaron los documentos de arquitectura ni se avanzó al siguiente alcance.

### Implementación

- `crates/browser/src/sessions.rs` y `sessions.sql`: `webd.db` privado, WAL/FULL, escritor serializado en pool de bloqueo; tablas de perfiles, estado de sitios, sesiones, pestañas, operaciones y eventos. Bloqueo exclusivo del directorio de datos, validación de nombres/orígenes y directorios propios 0700.
- Perfiles persistentes con contexto por defecto, instancia compartida o por sitio; contextos efímeros independientes por ejecución, destrucción al cerrar/suspender y parada de la instancia vacía a los diez minutos. Variante compacta con contextos separados dentro de la instancia persistente. Preferencias de restauración de sesión y detección acotada de Secret Service; alternativa basic con aviso explícito.
- Sesiones y pestañas con propietario, etiqueta, ID lógico, URL/título y época de documento; estados y transiciones; ventanas propias, selección lógica por ejecución, límites 4/12 pestañas y 3 contextos. Exclusión por pestaña con plazo de treinta segundos; suspensión/reapertura y rechazo de suspender escucha. La admisión de entrada consulta la URL real del frame y rechaza orígenes human_only.
- Sitio en escucha con una pestaña compartida entre propietarios, flags de actividad y emulación de foco. Títulos y MutationObserver en mundo aislado para no leídos; eventos sin contenido, máximo uno por cinco segundos y sitio. Canal de ciclo de vida separado, reconciliación de salud/conteos y consumo acotado de eventos generales. Recuperación de renderizador conservando ID lógico y aumentando época.
- Recuperación persistente al reiniciar webd o caer Chrome, con URL restaurada, referencias previas invalidadas, backoff 1/5/30 s y circuito de cinco relanzamientos por diez minutos. Contextos efímeros anteriores pasan a lost. Solo se retira un SingletonLock cuyo PID esté inequívocamente ausente.
- `crates/browser/src/rpc.rs`: JSON-RPC Unix delimitado por líneas, límite 4 MiB, errores recuperables, socket 0660 y SO_PEERCRED (UID configurado o root). Cursores durables y reproducción paginada; límites de conexiones/trabajo y plazo de escritura. `web_ops` registra received/dispatched/completed/failed, hash canónico, unión al trabajo vivo, reproducción de resultados, conflicto de clave y uncertain tras despacho interrumpido. Conserva trabajo al desconectar y devuelve working para operaciones largas.
- `crates/webd/src/main.rs` y configuración: `--serve`, cambio de usuario antes de recursos, proxy propio que deniega toda red, arranque de gestor/socket y cierre ordenado con drenaje ante SIGTERM/SIGINT. Excepción explícita de directorio de socket 0710 para grupo compartido, sin relajar datos/perfiles privados. No se instaló ni desplegó un servicio permanente.
- Documentación funcional equivalente en `docs/development/browser.md` y `browser.es.md`, y órdenes de verificación en AGENTS.md. Sin historia de errores ni etiquetas de hitos en la documentación funcional.

### Verificación

| Comprobación | Resultado final |
|---|---|
| `cargo fmt --check` | Código 0; `/tmp/b2-fmt.log` |
| `cargo clippy --workspace --all-targets -- -D warnings` | Código 0; `/tmp/b2-clippy-verified.log` |
| `cargo test --workspace --no-fail-fast`, umask 0077 | Código 0; 141 resultados de suite ok, ninguno fallido; `/tmp/b2-workspace-final.log` |
| `EXCALIBUR_LISTEN_TEST_SECONDS=2 cargo test -p excalibur-cdp -p excalibur-browser -p excalibur-webd -- --nocapture`, umask 0077 | Código 0; 13 suites ok; comprobación rápida final con escucha abreviada explícita; `/tmp/b2-backend-verified.log` |
| `cargo test -p excalibur-webd --test preflight`, umask 0022 | Código 0; 12 pruebas; `/tmp/b2-preflight-022.log`. También pasa con 0077 dentro del workspace/backend |
| `cargo build --workspace` | Código 0; `/tmp/b2-build.log` |
| `git diff --check 8cb3341dc65ae7a7da4d64ce5e76fca1612a978f HEAD` | Código 0 sobre la entrega comprometida |

Chrome real: `Chrome/156.0.8078.79`. La ejecución prolongada del workspace mantuvo escucha durante **600 segundos**, recibió **30 eventos**, y contó **12026 ticks** con otra ventana activa. Se verificaron límite de frecuencia, ausencia de contenido de mensajes, MutationObserver/no leídos, recuperación del renderizador y del navegador, persistencia de cookie permanente, cookie de sesión, localStorage e IndexedDB, estado logged_in comunicado por el host, aislamiento de contextos, variantes compacta/por sitio, límites y espera real de treinta segundos.

`webd_boundary` verifica el servicio real: UID 999/GID 988, grupos vacíos y capacidades a cero, rechazo sin respuesta de UID 65534 aun con acceso al socket, dos instancias antes de SIGKILL y `remaining_live: []`, restauración persistente y pérdida efímera al reiniciar, socket retirado al cerrar. El contrato CDP incluye las 74 órdenes del catálogo, sin omitir Chrome. Evidencias: `target/test-evidence/browser/sessions/contract.json`, `webd_boundary/contract.json` y `cdp_contract/contract.json`.

Durante desarrollo se corrigieron comprobaciones del fixture que asumían una única página de eventos o que la primera línea recibida era una respuesta y no una notificación. La primera escucha larga falló por el recorte del lector de pruebas; la repetición completa de 600 s pasó. El lector de notificaciones ajustado se volvió a comprobar en la suite backend final. No se usó `EXCALIBUR_BROWSER_TESTS=skip`; los helpers marcados ignored son ejecutados explícitamente por sus pruebas padre. Pruebas locales y sintéticas, sin APIs de modelos, cuentas reales ni benchmarks pagados.

### Límites y dependencias

La red del ejecutable sigue denegada hasta integrar el broker; solo las pruebas inyectan un proxy local sintético mediante la API Rust. No se afirma disponibilidad de Internet ni de cuentas iniciadas. El estado de login persistido es una afirmación del host, no un detector de autenticación. Herramientas del agente/adaptador, observación, acciones, visor y toma de control pertenecen a alcances posteriores; el rechazo human_only se verifica en la compuerta de entrada compartida, sin implementar anticipadamente browser_act. La máquina no tiene pantalla y las verificaciones son headless; no hay frontend propio que revisar visualmente. Los metadatos de perfiles reservan tamaño en disco para la contabilidad posterior. Los límites de recursos completos y la integración con disparadores del arnés no se presentan como disponibles.

Disco al cierre: `target` ocupa **61463443962 bytes**; quedan **138145796096 bytes** disponibles, por encima de la reserva de 5 GiB y del aviso de 10 GiB. No se limpiaron directorios ajenos.

### Entrega

Commit: `f6ef7cd81def7621a09596db883569443f5e8042` — **Add durable browser sessions and the local webd protocol**.

Push realizado exclusivamente a `origin/browser`, sin force. `git ls-remote origin refs/heads/browser` devolvió **`f6ef7cd81def7621a09596db883569443f5e8042`**, idéntico a HEAD. Árbol de trabajo limpio. Este informe permanece fuera del repositorio. No se inició auditor ni el siguiente alcance.

## Auditoría — revisión de `f6ef7cd81def7621a09596db883569443f5e8042`

Alcance auditado: entrada B2 «Perfiles, sesiones, pestañas y protocolo» de 19.2 (NB-03, NB-04, NB-14, NB-26) y sus referencias: 3.2–3.5, 5.1–5.8, 16.3–16.6, B.2 y apéndice C. Las pruebas de cierre son 3.5 y 5.8. No se exigen hitos posteriores: observación, acciones, `act.fill_secret`, detectores, visor, broker ni adaptador. Hallazgos previos a verificar: ninguno (lista vacía). Rama `browser`: `HEAD` = `origin/browser` = `f6ef7cd81def7621a09596db883569443f5e8042`, árbol limpio, `git diff --check 8cb3341 f6ef7cd` con código 0. No se ha modificado producto ni arquitectura.

Las pruebas manuales usaron `/tmp/audit-b2*` y un programa auxiliar externo, `/tmp/audit-b2-scratch/src/main.rs`, que usa la API `Manager` con Chrome real como `excalibur-web` y un proxy local sintético (su `target` ya eliminado). También usaron el servicio real `excalibur-webd --serve` con `/var/lib/excalibur-web` y `/run/excalibur-web`; los artefactos que creé allí se eliminaron y los directorios quedan vacíos como estaban. `umask` de la sesión: 0077.

Veredicto: **cambios requeridos** (dos hallazgos bloqueantes, AUD-B2-01 y AUD-B2-02, y uno de reproducibilidad de la suite de cierre, AUD-B2-03).

### Comprobaciones reproducidas

| Comprobación | Resultado | Evidencia |
|---|---|---|
| `cargo fmt --check` | Código 0 | `/tmp/audit-b2/fmt.log` |
| `cargo clippy --workspace --all-targets -- -D warnings` | Código 0 | `/tmp/audit-b2/clippy.log` |
| `cargo build --workspace` | Código 0 | `/tmp/audit-b2/build.log` |
| `cargo test -p excalibur-browser --test sessions -- --nocapture` (1.ª, con sondas mías en paralelo) | **Código 101**: `sessions.rs:655`, 18 `listener_activity` en 600 s (se esperaban ≥ 28) | `/tmp/audit-b2/sessions.log` |
| Ídem, dentro de `cargo test --workspace --no-fail-fast` | **Código 101**: `sessions.rs:623`, `Fault { kind: "session_closed" }` a los 50 s (AUD-B2-03) | `/tmp/audit-b2/tests.log` |
| Ídem, aislada y sin otra carga | Código 0; 3 aprobadas en 653,7 s; 30 eventos en 600 s; cookie persistente y de sesión, IndexedDB y localStorage restaurados | `/tmp/audit-b2/sessions-quiet.log`, `target/test-evidence/browser/sessions/contract.json` |
| `cargo test -p excalibur-webd --test webd_boundary -- --nocapture` | 1.ª ejecución **código 101** (`webd_boundary.rs:211`, `left: "blocked"`, `right: "ready"`); 5 de 5 repeticiones y la del workspace con código 0 | `/tmp/audit-b2/webd_boundary.log` |
| `cargo test -p excalibur-browser --test cdp_contract -- --nocapture` | Código 0 | `/tmp/audit-b2/cdp_contract.log` |
| `(umask 0022; cargo test -p excalibur-webd --test preflight)` | Código 0 | `/tmp/audit-b2/preflight022.log` |
| `cargo test --workspace --no-fail-fast` (umask 0077) | **Código 101**: 140 resultados `ok`; único fallo `excalibur-browser --test sessions` (AUD-B2-03) | `/tmp/audit-b2/tests.log` |

El fallo de la línea 655 se produjo mientras mis sondas de Chrome y una compilación auxiliar corrían en paralelo, y no se repitió aislado, así que no lo atribuyo a un defecto del producto. El de la línea 623 es determinista según el orden de UUID (AUD-B2-03).

### Servicio real (`excalibur-webd --serve`, lanzado como root, `host_uid = 0`)

- Identidad del proceso: `Uid 999×4`, `Gid 988×4`, `Groups` vacío y `CapEff` a cero. El socket `webd.sock` es `srw-rw----` de `excalibur-web`.
- JSON inválido → `-32700 invalid_json_or_request`; línea de 4 MiB + 10 bytes → `message_too_large`; en ambos casos la misma conexión responde después a `status.get`.
- Un peer con UID 999 (no autorizado) recibe `ConnectionResetError` sin ningún byte de respuesta.
- `session.open` efímera, repetida con la misma `op_key` → la misma `session_id`; con la misma clave y otro parámetro → `idempotency_conflict`. La quinta pestaña de la ejecución → `tab_limit` («4 per run»).
- Con dos instancias (efímera y persistente, 27 PID en sus árboles), `SIGKILL` a webd deja 0 procesos vivos. Al reiniciar: la sesión efímera pasa a `lost` (las operaciones sobre ella devuelven `session_lost`), la persistente vuelve a `ready` y la `op_key` completada se reproduce con el resultado guardado. Con `SIGTERM` sale con código 0 y retira el socket.

### Requisitos

| Requisito (fuente) | Estado | Evidencia |
|---|---|---|
| `webd.db` privado, WAL/FULL y un escritor (apéndice C) | Verificado | `sessions.sql`, `Database::call` |
| Perfil persistente y contextos efímeros aislados; variantes compacta y por sitio (5.1–5.2) | Verificado | `contract.json` (`ephemeral_isolated`, restauración) |
| Máquina de estados (5.3) | **No cumplido** | AUD-B2-02: `blocked` no vuelve a `ready` al desaparecer la condición |
| Pestañas en ventana propia, propietarios, límites 4/12/3 y exclusión de 30 s (5.6) | Verificado | Servicio real y `contract.json` (`tab_limits`, `queue_timeout_seconds: 30`, `owner_denied`) |
| Escucha: ventana propia, focus emulation, flags y sin congelar (5.7.1–3) | Verificado | `sessions` aislada: 600 s con otra ventana activa |
| `listener_activity` sin contenido, como máximo uno cada 5 s por sitio, uno por cambio (5.7.4, 5.8) | **No cumplido** | AUD-B2-01: el límite descarta el último cambio |
| Recuperación de renderizador y navegador, backoff 1/5/30 s y circuito 5/10 min (5.7.5, 16.6) | Verificado con reserva | `renderer_recovered`; AUD-B2-04 |
| JSON-RPC por socket, 4 MiB, `SO_PEERCRED` (3.2, 3.4, 3.5) | Verificado | Servicio real |
| `web_ops` received/dispatched/completed/failed, hash, conflicto, incierto y retención de 24 h (16.4) | Verificado | Servicio real y `operation_contract` |
| Recuperación al reiniciar webd: efímeras `lost`, persistentes restauradas, épocas avanzadas (16.6) | Verificado | Servicio real y `webd_boundary` |
| 3.5 filas de usuario, root, UID no autorizado, SIGKILL con dos instancias, 4 MiB/JSON inválido | Verificado | Arriba; filas de B0 ya auditadas |
| 3.5 fila `act.fill_secret` | Fuera de alcance | `act.*` y el baúl son de B4; el protocolo no expone esa orden |
| 5.8 filas 1–6 (cookie/IndexedDB, cookie de sesión, aislamiento efímero, instancia única, límites, cola de 30 s) | Verificado | `contract.json` de la ejecución aislada |
| 5.8 fila 7 (escucha 10 min con un evento por cambio) | Verificado parcialmente | Cambios cada 20 s sí; cambios más próximos que 5 s se pierden (AUD-B2-01) |
| 5.8 fila 8 (matar el renderizador en escucha) | Verificado | `renderer_recovered: true` |
| 5.8 fila 9 (`human_only`) | Verificado en la compuerta compartida | `lease(input)` devuelve `denied(human_only_origin)`; `browser_act` es de B4 |
| Suites de cierre reproducibles | **No cumplido** | AUD-B2-03 |
| Documentación EN/ES y `AGENTS.md` | Verificado con reserva | Equivalentes; la descripción de transiciones y del límite de frecuencia no refleja AUD-B2-01/02 |

### Hallazgos

#### AUD-B2-01 — Bloqueante (severidad media): el límite de frecuencia de `listener_activity` descarta cambios en lugar de agruparlos

- Evidencia (código): `Manager::activity` (`crates/browser/src/sessions.rs`, función `activity`) devuelve `Ok(())` sin emitir nada si hubo un evento del sitio hace menos de 5 s. Mientras tanto `m.titles` y `m.counts` ya se han actualizado, así que el cambio no se vuelve a detectar. La clave `profile/site` la comparten `title_changed` y `unread_count`.
- Evidencia (Chrome real, programa auxiliar en modo `rapid`): página en escucha con `unread_selector = ".unread"` que a los 8, 9 y 10 s cambia el título a «chat 1», «chat 2» y «chat 3» y añade un `.unread` en cada paso. En 25 s solo hay dos eventos: `unread_count` 0 (inicial) y `unread_count` 1 (a los 8 s). No llega ningún `unread_count` 2 o 3 ni ningún `title_changed`, aunque la página queda estable 15 s y el título final de la pestaña es «chat 3». El arnés se queda con un recuento obsoleto (1 frente a 3) y sin aviso posterior.
- Impacto: NB-26 y 5.8 («un `listener_activity` por cambio, con el límite de frecuencia respetado»). En sitios de mensajería es habitual recibir varios mensajes en pocos segundos; el último estado no se notifica nunca.
- Recomendación: agrupar en lugar de descartar. Guardar el último estado pendiente por sitio y emitir un único evento al cerrarse la ventana de 5 s, con el recuento y el tipo vigentes. Añadir una prueba con Chrome real con varios cambios en menos de 5 s que compruebe un evento final con el último recuento.

#### AUD-B2-02 — Bloqueante (severidad media): una sesión `blocked` no vuelve a `ready` cuando desaparece la condición

- Evidencia (código): `tick` pone `SessionState::Blocked` directamente, sin `transition()`, cuando la URL de una pestaña sale de los orígenes `rw` (login, `document_origin` o página de error de red). Ningún código devuelve `Blocked` a `Ready`; `restore` lo hace solo al relanzar. `lease` devuelve `blocked` mientras dure ese estado.
- Evidencia (Chrome real, modo `back`): sesión en escucha en `http://chat.test`. La página redirige a `http://login.test/signin` → `blocked` (`needs_login`). El login vuelve a `http://chat.test/?back=1` 2 s después. Durante los 12 s siguientes la sesión sigue `blocked` con la URL ya permitida, y `lease` devuelve `Fault { kind: "blocked" }`. Un fallo de red temporal tiene el mismo efecto (página de error → `blocked`), y explica la intermitencia observada en `webd_boundary.rs:211`, donde la sesión restaurada en `denied.test` aparece `blocked` o `ready` según quién llegue antes.
- Impacto: incumple 5.3 («blocked → ready: condición desaparece») y 16.6 («pérdida de red: sesiones en escucha intactas»). Tras un inicio de sesión o una caída de red, la sesión queda inutilizable para el agente hasta reiniciar webd.
- Recomendación: reevaluar en cada reconciliación las sesiones `blocked` por `document_origin`/`needs_login` y volver a `ready` (con evento `session_state`) cuando la URL de nivel superior vuelva a un origen `rw`. Pasar todas las asignaciones de estado por `transition()`. Probar el retorno tras login y tras error de red con Chrome real.

#### AUD-B2-03 — Bloqueante para el check `tests` (severidad media): la suite de cierre `sessions` no es determinista

- Evidencia: `crates/browser/tests/sessions.rs:613-614` toma `m.list("rpc")[0]`, pero la sesión `rpc` abierta antes se cierra en la línea 470, y `Manager::list` recorre un `BTreeMap` con clave UUID v4 aleatoria. Cuando la cerrada queda primera, `new_tab` falla con `session_closed`. Así ocurrió en la ejecución de `cargo test --workspace` (`sessions.rs:623`), mientras que la ejecución aislada pasó. Además, `webd_boundary.rs:211` asume `ready` para una sesión cuyo destino falla siempre con el proxy de rechazo total (ver AUD-B2-02).
- Recomendación: conservar el `session_id` de la sesión viva en lugar de indexar la lista, y en `webd_boundary` usar un destino que cargue de verdad o esperar el estado final esperado. Repetir las suites varias veces para comprobar la estabilidad.

#### AUD-B2-04 — Menor (severidad baja, no bloqueante): `revive_tab` deja un target huérfano si la URL guardada ya no es admisible

- Evidencia (Chrome real, modo `login`): pestaña en escucha redirigida al origen de login (sesión `blocked` `needs_login`) y renderizadores matados con `SIGKILL`. `revive_tab` crea y adjunta un target nuevo y después falla en `document_allowed(old.url)`: el registro muestra `browser lifecycle: denied` y el número de páginas sube de 1 a 2 y se mantiene en 2 durante 30 s. La pestaña lógica queda `crashed`, la página nueva queda fuera de la contabilidad de 4/12 y el `tick` se interrumpe en ese ciclo para el resto de instancias.
- Recomendación: validar la URL antes de crear el target (o cerrarlo al fallar), restaurar a un origen `rw` del sitio cuando la URL guardada no sea admisible y no abortar el `tick` completo por el error de una pestaña.

### Hallazgos previos

No había IDs previos que verificar.

Disco al cierre: `target` 61 571 918 296 bytes; 137 957 355 520 bytes libres, por encima de la reserva de 5 GiB y del aviso de 10 GiB.


## Respuesta de Codex

**Corrección 1 — flow-operation:348f864dbffa40aa999b2b5c9e7d7459**. Fecha UTC: 2026-10-30T06:53:55+00:00. Base auditada: `f6ef7cd81def7621a09596db883569443f5e8042`. Se conservan íntegramente las respuestas y la auditoría anteriores.

### Hallazgos resueltos

| ID | Cambio y comprobación |
|---|---|
| **AUD-B2-01** | Resuelto. `Manager::activity` conserva actividad pendiente por perfil/sitio; el tick la publica al terminar la ventana de cinco segundos aunque la página quede estable. Agrupa título y no leídos, mantiene el recuento más reciente y añade `kinds` sin cambiar los valores admitidos de `kind`. La prueba real produce recuentos 1 y 3, separados cinco segundos, con ambos tipos de actividad en el último evento y sin contenido de mensajes. |
| **AUD-B2-02** | Resuelto. `blocked_reason` distingue bloqueos de documento de otros bloqueos. La reconciliación consulta el frame superior real, incluidos documentos de error de red, y devuelve a ready/busy las sesiones bloqueadas por needs_login/document_origin cuando todas sus pestañas vuelven a orígenes permitidos. Emite session_state y actualiza todos los propietarios de una escucha compartida. Los cambios de estado pasan por la tabla de transiciones. Las pruebas reales comprueban login → rw y error de red → rw; la prueba unitaria asegura que no se levantan bloqueos de versión/seguridad ni control humano. Volver a rw no afirma un login autenticado. |
| **AUD-B2-03** | Resuelto. El fixture conserva el ID de la sesión rpc viva y selecciona la instancia por ID, sin depender del orden de UUID/HashMap. Espera eventos y ejecución real del documento en lugar de plazos fijos de recuperación. webd_boundary espera blocked/document_origin, el estado final correcto para su destino bajo el proxy de rechazo total. Se verificaron tres repeticiones cortas de sessions y diez de webd_boundary; después del ajuste final del long-polling pasaron la batería backend final y la suite completa con 600 s. |
| **AUD-B2-04** | Resuelto. `revive_tab` valida la URL antes de crear el target y usa un origen rw configurado si la URL guardada es de login o de error. Cierra el candidato si falla la preparación o no puede retirar el target anterior. El supervisor de cada pestaña registra el fallo con reintento acotado sin abortar el resto del tick. La prueba mata los renderizadores en login: quedan una página antes y una después, se conserva el ID lógico, ambos propietarios vuelven a ready y otra instancia sigue utilizable. |

No quedan hallazgos de esta auditoría pendientes en esta entrega. No se inició otro alcance ni un auditor.

### Rutas y contrato

- `crates/browser/src/sessions.rs`: agrupación de actividad, motivos y transiciones de estado, reconciliación de documentos y recuperación con limpieza de targets. El nuevo motivo es opcional al leer registros anteriores; no requiere modificar el esquema SQL existente.
- `crates/browser/tests/sessions.rs`: ráfagas con Chrome real, recuperación de login/red para propietarios compartidos, contabilidad de páginas, esperas observables y selección explícita de IDs. El long-polling desactiva caché y consume cada respuesta; registra ciclos reales y los contrasta con notificaciones.
- `crates/webd/tests/webd_boundary.rs`: espera del estado final del documento de error del proxy.
- `docs/development/browser.md`, `browser.es.md` y AGENTS.md: contratos y órdenes de comprobación equivalentes EN/ES, sin historia de errores ni etiquetas de hitos en la documentación funcional.

### Verificación final

| Orden / comprobación | Resultado |
|---|---|
| `cargo fmt --check` | Código 0; `/tmp/b2c1-fmt-final.log` |
| `cargo clippy --workspace --all-targets -- -D warnings` | Código 0; `/tmp/b2c1-clippy-final.log` |
| `cargo build --workspace` | Código 0; `/tmp/b2c1-build.log` |
| `cargo test --workspace --no-fail-fast`, umask 0077 | **Código 0, 141 resultados de suite ok y ninguno fallido**; `/tmp/b2c1-workspace-final.log` |
| `EXCALIBUR_LISTEN_TEST_SECONDS=2 cargo test -p excalibur-cdp -p excalibur-browser -p excalibur-webd -- --nocapture`, umask 0077 | Código 0; 13 suites ok; `/tmp/b2c1-backend-final.log`. Esta ejecución corta no sustituye a los 600 s del workspace |
| Tres repeticiones cortas de `cargo test -p excalibur-browser --test sessions -- --nocapture`, umask 0077 | Tres códigos 0; cuatro pruebas padre/lógica aprobadas por ejecución; `/tmp/b2c1-repeat-sessions-{1,2,3}.log` |
| Diez repeticiones de `cargo test -p excalibur-webd --test webd_boundary -- --nocapture`, umask 0077 | Diez códigos 0; `/tmp/b2c1-repeat-boundary-{1..10}.log` |
| `cargo test -p excalibur-webd --test preflight`, umask 0022 | Código 0; doce pruebas; `/tmp/b2c1-preflight-022.log`. También pasa con 0077 en el workspace y backend |
| `EXCALIBUR_LISTEN_TEST_SECONDS=85 cargo test -p excalibur-browser --test sessions durable_sessions_and_protocol -- --nocapture` | Código 0; cuatro ciclos reales y cuatro notificaciones de título; `/tmp/b2c1-listen-85-verified.log` |
| `git diff --check f6ef7cd81def7621a09596db883569443f5e8042 HEAD` | Código 0 sobre el commit entregado |

Chrome real: `Chrome/156.0.8078.79`. La escucha final registra **600 segundos, 30 ciclos reales de long-polling y 31 notificaciones con actividad de título**, incluida la actualización inicial de la página restaurada. Temporizadores: **12011 ticks**. Se mantienen cookies persistentes y de sesión, IndexedDB, aislamiento, límites, colas, permisos del socket y terminación de procesos ya verificados. No se usó `EXCALIBUR_BROWSER_TESTS=skip`; los helpers ignored se invocan explícitamente desde sus pruebas padre.

Evidencias locales: `target/test-evidence/browser/listener_reconciliation/reconciliation.json`, `sessions/contract.json`, `webd_boundary/contract.json` y `cdp_contract/contract.json`. La evidencia de ráfagas conserva los eventos con recuentos 1/3 y los indicadores `login_return_ready`, `network_return_ready`, `shared_owners_recovered` y `other_instance_usable`, todos verdaderos; páginas antes/después: 1/1.

Durante el cierre se identificó por qué el fixture largo podía cambiar más despacio: una ventana de 85 s solo completaba dos respuestas de long-polling. Se corrigió el consumo/caché del fixture y se añadieron contadores, manteniendo la exigencia de cadencia y entrega. La versión final produjo cuatro ciclos en 85 s y treinta en 600 s. Una ejecución previa también encontró el cuelgue intermitente preexistente de `excalibur-eval --test verifiable` (sin cambios en ese crate); se detuvo tras más de siete minutos. El test pasó aislado en 10,46 s y volvió a pasar en el workspace final, cuya suite verifiable terminó en 20,77 s. No se presenta esa ejecución previa fallida como aprobada.

Todo se probó localmente con sitios sintéticos, sin APIs de modelos, cuentas reales ni benchmarks pagados. El proxy del servicio sigue denegando la red; no se desplegaron servicios permanentes ni se implementaron herramientas, visor o broker de alcances posteriores. No hay frontend propio que revisar visualmente.

Disco al cierre: `target` ocupa **62037568721 bytes**; disponibles **137467097088 bytes**, por encima de la reserva de 5 GiB y del aviso de 10 GiB. No se limpiaron directorios ajenos.

### Entrega

Commit **`49ceeaab366cea13e9c0e1eecfc23181920cb2cb`** — **Coalesce listener updates and recover blocked browser sessions**.

Push exclusivamente a `origin/browser`, sin force. SHA remoto comprobado mediante `git ls-remote origin refs/heads/browser`: **`49ceeaab366cea13e9c0e1eecfc23181920cb2cb`**, idéntico a HEAD. Árbol de trabajo limpio. El informe se conserva fuera de Git, en este mismo Markdown.

## Auditoría — revisión de `49ceeaab366cea13e9c0e1eecfc23181920cb2cb`

Alcance: la misma entrada B2 de 19.2 y sus referencias (3.2–3.5, 5.1–5.8, 16.3–16.6, B.2 y apéndice C), más los hallazgos AUD-B2-01 a AUD-B2-04. Rama `browser`: `HEAD` = `origin/browser` = `49ceeaab366cea13e9c0e1eecfc23181920cb2cb`, árbol limpio, `git diff --check f6ef7cd 49ceeaa` con código 0. El diff toca `crates/browser/src/sessions.rs`, `tests/sessions.rs`, `crates/webd/tests/webd_boundary.rs`, `AGENTS.md` y `browser*.md`. No se ha modificado producto ni arquitectura.

Las pruebas manuales usaron el programa auxiliar externo `/tmp/audit-b2-scratch/src/main.rs` compilado contra este commit y, para comparar, la misma fuente contra `f6ef7cd`, extraído con `git archive` en `/tmp/audit-b2-old` sin tocar el repositorio. Los dos `target` auxiliares ya están eliminados. Ninguna prueba se ejecutó en paralelo con las suites. `/var/lib/excalibur-web` y `/run/excalibur-web` siguen vacíos y no quedan procesos de Chrome del usuario web. `umask` de la sesión: 0077.

Veredicto: **cambios requeridos** (regresión bloqueante nueva, AUD-B2-05).

### Comprobaciones reproducidas

| Comprobación | Resultado | Evidencia |
|---|---|---|
| `cargo fmt --check` | Código 0 | `/tmp/audit-b2r1/fmt.log` |
| `cargo clippy --workspace --all-targets -- -D warnings` | Código 0 | `/tmp/audit-b2r1/clippy.log` |
| `cargo build --workspace` | Código 0 | `/tmp/audit-b2r1/build.log` |
| `cargo test -p excalibur-browser --test sessions listener_events_and_document_recovery -- --nocapture` | Código 0 (46 s) | `/tmp/audit-b2r1/listener.log`, `target/test-evidence/browser/listener_reconciliation/reconciliation.json` |
| `cargo test -p excalibur-browser --test sessions -- --nocapture` | Código 0; 4 aprobadas en 680,5 s | `/tmp/audit-b2r1/sessions.log` |
| `cargo test -p excalibur-webd --test webd_boundary` ×5 | Código 0 en las 5 | `/tmp/audit-b2r1/webd_boundary{1..5}.log` |
| `cargo test -p excalibur-browser --test cdp_contract -- --nocapture` | Código 0 | `/tmp/audit-b2r1/cdp_contract.log` |
| `(umask 0022; cargo test -p excalibur-webd --test preflight)` | 1.ª ejecución **código 101** (AUD-B2-06, fuera de alcance); 5 de 5 repeticiones con 0022 y 5 de 5 con 0077 con código 0 | `/tmp/audit-b2r1/preflight022.log` |
| `cargo test --workspace --no-fail-fast` (umask 0077) | Código 0; 141 resultados `ok`, 0 fallidos; `sessions` con 31 `listener_activity` en 600 s | `/tmp/audit-b2r1/tests.log`, `target/test-evidence/browser/sessions/contract.json` |

### Verificación de hallazgos previos

| ID | Estado | Evidencia |
|---|---|---|
| AUD-B2-01 | Resuelto | `activity` acumula en `pending_activity` y `flush_activity` emite al cerrarse la ventana de 5 s con el recuento vigente y `kinds`. Repetí el mismo procedimiento de la ráfaga 8/9/10 s con Chrome real y ahora emite `unread_count` 0, después 1 (`kinds: [title_changed, unread_count]`) y, 5 s más tarde, **3**, el estado final. La suite nueva lo confirma (`burst_events`, `final_unread_count: 3`). |
| AUD-B2-02 | Resuelto | `reconcile_documents` usa la URL real del frame, bloquea solo por `needs_login`/`document_origin` (`blocked_reason`) y vuelve a `ready` (`document_condition_cleared`) cuando todas las pestañas visibles están en orígenes `rw`, sin tocar bloqueos de seguridad o versión ni `human_control` (prueba unitaria `only_document_blocks_clear_when_the_origin_is_allowed`). Las asignaciones de estado pasan por `set_state`/`transition`. Repetí el modo `back` con Chrome real: `ready` → `blocked` (login) → `ready` en 2 s al volver a `chat.test/?back=1`, y `lease` funciona. La suite añade `login_return_ready`, `network_return_ready` y `shared_owners_recovered`. |
| AUD-B2-03 | Resuelto | `sessions.rs` usa la `session_id` devuelta en lugar de `list("rpc")[0]`; `webd_boundary` espera el estado `blocked`/`document_origin` final. `webd_boundary` 5 de 5 y `sessions` 2 de 2 (aislada y dentro del workspace) con código 0. |
| AUD-B2-04 | Resuelto | `revive_tab` valida la URL antes de crear el target, restaura al primer origen `rw` cuando la guardada no es admisible y cierra el target nuevo si falla. `revive_listener` limita los reintentos a uno cada 5 s y no aborta el `tick`. Repetí el modo `login` con Chrome real: tras matar los renderizadores, el número de páginas se mantiene en 1 durante 30 s, sin huérfanas, y la pestaña se restaura a `chat.test` (`renderer_restarted`). La suite registra `pages_before_crash` y `pages_after_recovery` a 1. |

### Hallazgos nuevos

#### AUD-B2-05 — Bloqueante (severidad alta): regresión; una página en bucle bloquea todo el gestor durante ~10 s en cada ciclo

- Evidencia (código): en `Manager::tick` (`crates/browser/src/sessions.rs:1838-1846`), la corrección añade para **cada** pestaña con target, en cada ciclo de 200 ms, un `Page.getFrameTree` que resuelve el renderizador y tiene el plazo por defecto de 10 s (`protocol.rs:11`). Se ejecuta mientras se mantiene el `Mutex` global `model`, del que dependen `list`, `status`, `open_session`, `new_tab`, `lease`, `end_session`, el RPC y la emisión de `listener_activity`. Antes solo había una consulta de este tipo, cada 5 s y solo en pestañas en escucha.
- Evidencia (Chrome real, programa auxiliar en modo `spin`): sesión en escucha más una sesión efímera de otra ejecución con una pestaña `http://spin.test/` que ejecuta `while(true){}` a los 1,5 s. Con `49ceeaa`, `Manager::list` tarda 8,0–9,5 s y `Manager::status` unos 10,0 s en cada muestra durante 14 s, y el programa no llega a terminar en 150 s (`/tmp/audit-b2r1/spin.log`). La misma fuente contra `f6ef7cd` responde en 0,08–0,39 ms durante 20 s y termina con normalidad (`/tmp/audit-b2r1/spin-old.log`).
- Impacto: el contenido no confiable de una sola pestaña efímera congela durante ~10 s de cada ciclo las operaciones de todas las ejecuciones y retrasa en la misma medida los eventos de las sesiones en escucha. Contradice la exclusión por pestaña de 5.6 (las demás pestañas tienen arrendamientos independientes) y la continuidad de 5.7. El RPC también lo sufre, con colas de 30 s y `deadline_ms` de lectura.
- Recomendación: no hacer consultas al renderizador bajo el cerrojo global. Usar la URL de `Target.getTargets`/`targetInfoChanged` o `Page.frameNavigated` (del proceso del navegador) para la reconciliación, o lanzar las consultas por pestaña fuera del cerrojo, en paralelo y con un plazo corto, y aplicar después el resultado. Añadir una prueba con Chrome real con una pestaña en `while(true){}` que exija latencias de `list`/`status`/`lease` de otra pestaña por debajo de un umbral y que los eventos del oyente sigan llegando a tiempo.

#### AUD-B2-06 — Informativo, fuera de alcance (no bloqueante): carrera en una prueba de `preflight` de B0

- Evidencia: `chrome_probe_bounds_total_time_and_stops_descendants` (`crates/webd/tests/preflight.rs:263-270`) solo acepta `NotFound` al leer `/proc/<pid>/status` de un descendiente que se está recogiendo. En una de 12 ejecuciones la lectura devolvió `ESRCH` («No such process (os error 3)») y la prueba entró en pánico; las otras 11, con ambos umask, pasaron. El descendiente sí desapareció (el comportamiento del producto es correcto). Este commit no modifica ese archivo.
- Recomendación: tratar `ESRCH` igual que `NotFound` en el fixture.

### Regresiones y requisitos

- `sessions` y `webd_boundary` mantienen los requisitos verificados en la revisión anterior: perfiles persistentes con cookie de sesión, IndexedDB y localStorage restaurados; aislamiento efímero; límites y cola de 30 s; 600 s de escucha con 30–31 eventos; recuperación de renderizador y navegador; JSON-RPC con `SO_PEERCRED`; `web_ops`; reinicio y muerte del padre.
- Requisitos de B2 pendientes por AUD-B2-05: exclusión por pestaña independiente (5.6) y continuidad de las sesiones en escucha frente a contenido no confiable (5.7).
- Disco al cierre: `target` 62 061 220 232 bytes; 137 367 810 048 bytes libres, por encima de la reserva de 5 GiB y del aviso de 10 GiB.


## Respuesta de Codex

**Corrección 2 — flow-operation:3714b44d73514100aa8b18c961602b8a**. Fecha UTC: 2026-10-30T08:18:48+00:00. Base auditada: `49ceeaab366cea13e9c0e1eecfc23181920cb2cb`. Se conservan íntegramente respuestas y auditorías anteriores. Se ha mantenido el alcance «Perfiles, sesiones, pestañas y protocolo», sin iniciar otro alcance ni un auditor.

### Hallazgos

- **AUD-B2-05: resuelto y comprobado con Chrome real.** Las consultas de targets se ejecutan fuera del cerrojo del modelo y en paralelo por instancia. Las consultas de frame/no leídos son tareas independientes fuera del cerrojo, con presupuesto total de 500 ms, una sonda pendiente por pestaña y doce globales. El tick solo recoge resultados terminados; un timeout no se interpreta como caída. Los eventos de navegación conservan la URL real del documento y la caché de frames evita sobrescribirla con una URL solicitada que terminó en error. Antes de aplicar un resultado se comprueban target, sesión CDP y época del documento; los resultados de páginas sustituidas, navegadas o cerradas se descartan.
- La misma separación se aplica a las comprobaciones de entrada y a la lectura del frame antes de suspender: se mantiene la exclusión de sus pestañas, se libera el modelo durante la consulta y se vuelve a validar la identidad. Así no se introduce la espera de un renderizador en el acceso de otras ejecuciones al modelo.
- **AUD-B2-06: atendido con un ajuste mínimo del fixture.** La lectura de `/proc/<pid>/status` considera ESRCH equivalente a NotFound para un descendiente desaparecido. No se modifica el producto de preparación del entorno ni se rehace aquella entrega. Los directorios temporales de las pruebas unitarias del gestor fijan explícitamente 0700, con independencia del umask.
- **AUD-B2-01 a AUD-B2-04: se mantienen resueltos.** Pasan las pruebas de agrupación de ráfagas, retorno desde login/error de red para los propietarios compartidos, selección explícita de IDs, recuperación de renderizador y contabilidad de targets. La evidencia de reconciliación vuelve a registrar recuento final 3 y páginas 1/1.

### Cambios y rutas

- `crates/browser/src/sessions.rs`: descubrimiento fuera del cerrojo, tareas de sondas con límites y cancelación, caché de frames, rechazo de resultados tardíos y captura de frames para admisión/suspensión fuera del modelo. Las tareas pertenecen al gestor y se cancelan al destruirlo.
- `crates/browser/tests/sessions.rs`: nuevo fixture `other_sessions_stay_responsive_with_a_spinning_renderer`. Confirma por timeout de CDP que el renderizador está realmente bloqueado, comprueba otras operaciones y lecturas RPC Unix, continuidad del oyente, sonda de entrada acotada, ausencia de caída falsa y cierre normal.
- Prueba unitaria `late_probe_results_cannot_modify_a_replaced_navigated_or_closed_tab`: un resultado antiguo no cambia URL, recuento ni caché de un target sustituido, de otra época o cerrado. Se conserva la prueba que impide levantar bloqueos de seguridad/versión y control humano.
- `crates/webd/tests/preflight.rs`: tratamiento de ESRCH solo en el fixture.
- `docs/development/browser.md` y `browser.es.md`: comportamiento y límites de las sondas y comando de verificación, equivalentes EN/ES, sin historia de errores ni etiquetas de hitos. AGENTS.md documenta el nuevo comando y sus evidencias.

### Prueba real del renderizador en bucle

La página `spin.test` ejecuta `while(true){}`. Otra ejecución usa un contexto sano del **mismo navegador efímero**, mientras un perfil persistente mantiene una escucha con cambios de título/no leídos. La prueba mide list/status, la exclusión y el cambio de la pestaña sana, y solicitudes reales por socket Unix, con límite de un segundo por comprobación. Se ejecutó inicialmente, en el backend completo, en el workspace y en **tres repeticiones específicas**, todas aprobadas.

Última evidencia en `target/test-evidence/browser/renderer_isolation/contract.json`: **66 muestras durante 16 s**, sin caída falsa de la instancia. Máximos observados:

| Operación | Latencia máxima observada |
|---|---|
| `lease` | 34.161 ms |
| `list` | 0.630 ms |
| `status` | 32.013 ms |
| `switch` | 54.476 ms |
| `unix_rpc` | 176.485 ms |

El oyente continuó publicando recuentos `[6, 11, 16]` con intervalos de cinco segundos. La comprobación explícita de entrada sobre la pestaña bloqueada terminó por timeout sin bloquear el estado global. No se acumularon solicitudes CDP pendientes y el gestor terminó dentro del plazo de cierre de la prueba. Estas son medidas sintéticas locales, no una promesa de latencia bajo cualquier carga del sistema.

### Verificaciones finales

| Comprobación | Resultado |
|---|---|
| `cargo fmt --check` | Código 0; `/tmp/b2c2-fmt-final.log` |
| `cargo clippy --workspace --all-targets -- -D warnings` | Código 0; `/tmp/b2c2-clippy-final.log` |
| `cargo build --workspace` | Código 0; `/tmp/b2c2-build.log` |
| `cargo test --workspace --no-fail-fast`, umask 0077 | **Código 0: 141 resultados de suite ok, ninguno fallido**; `/tmp/b2c2-workspace.log` |
| `EXCALIBUR_LISTEN_TEST_SECONDS=2 cargo test -p excalibur-cdp -p excalibur-browser -p excalibur-webd -- --nocapture`, umask 0077 | Código 0: 13 suites ok; `/tmp/b2c2-backend.log`. La escucha corta no sustituye a los 600 s del workspace |
| Prueba específica del renderizador en bucle ×3, umask 0077 | Tres códigos 0; `/tmp/b2c2-spin-repeat-{1,2,3}.log` |
| `cargo test -p excalibur-browser --lib`, umask 0022 | Código 0: tres pruebas; `/tmp/b2c2-unit-022.log` |
| `cargo test -p excalibur-webd --test preflight`, umask 0022 | Código 0: doce pruebas; `/tmp/b2c2-preflight-022.log`. También pasa con 0077 en backend/workspace |
| `git diff --check 49ceeaab366cea13e9c0e1eecfc23181920cb2cb HEAD` | Código 0 sobre el commit entregado |

Chrome real: `Chrome/156.0.8078.79`. La escucha prolongada mantiene **600 s, 30 ciclos reales y 30 notificaciones de título**, con **12029 ticks**. Las pruebas de socket/permisos, idempotencia, aislamiento, cookies/IndexedDB, recuperación y muerte del padre se incluyen en el backend y el workspace. No se usó `EXCALIBUR_BROWSER_TESTS=skip`; los helpers ignored se invocan desde sus pruebas padre.

Evidencias: `target/test-evidence/browser/renderer_isolation/contract.json`, `sessions/contract.json`, `listener_reconciliation/reconciliation.json`, `webd_boundary/contract.json` y `cdp_contract/contract.json`. Durante desarrollo se detectó que las pruebas unitarias debían preparar sus directorios en 0700 con umask 0022; se ajustaron sus fixtures y ambas variantes de umask pasan en el cierre final. No se presenta esa ejecución de desarrollo fallida como una verificación aprobada.

Pruebas locales y sintéticas, sin APIs de modelos, cuentas reales ni benchmarks pagados. Sin despliegue de servicios permanentes. El broker, las herramientas y el visor de alcances posteriores siguen sin implementarse; no hay frontend propio para revisión visual. No quedan hallazgos de esta auditoría pendientes en esta entrega y no se inicia una tercera corrección ni el siguiente alcance.

Disco al cierre: `target` **62507713886 bytes**; disponibles **136907796480 bytes**, por encima de la reserva de 5 GiB y del aviso de 10 GiB. No se limpiaron datos ajenos.

### Entrega

Commit **`99ab5c6d3714c8b8099bbc822de8ed19a79dda94`** — **Keep renderer probes outside the browser state lock**.

Push exclusivamente a `origin/browser`, sin force. SHA remoto comprobado mediante `git ls-remote origin refs/heads/browser`: **`99ab5c6d3714c8b8099bbc822de8ed19a79dda94`**, idéntico a HEAD. Árbol de trabajo limpio. Se añadió esta respuesta al mismo Markdown externo, preservando todos sus bytes anteriores; el informe queda fuera de Git.


<!-- flow-transition:771aa634cf8946858025d0790ba9c00e:3:99ab5c6d3714c8b8099bbc822de8ed19a79dda94 -->
## Estado del coordinador

Se completaron dos correcciones. Por instrucción del usuario se continúa al siguiente alcance SIN auditoría final. Esta entrega NO se marca aprobada; los hallazgos de la revisión anterior permanecen como referencia pendiente de verificación.

Veredicto: cambios requeridos para 49ceeaa. Los cuatro hallazgos previos están resueltos y los verifiqué con Chrome real: la ráfaga se agrupa y llega el recuento final 3 (01); una sesión blocked vuelve a ready al regresar al origen permitido (02); sessions y webd_boundary son ya deterministas, 2 de 2 y 5 de 5 (03); no quedan targets huérfanos tras una caída en un origen de login (04). format, clippy, build y el workspace (141 ok) pasan. Sin embargo, la corrección introduce una regresión bloqueante, AUD-B2-05: tick llama a Page.getFrameTree para cada pestaña cada 200 ms mientras mantiene el cerrojo global. Una pestaña efímera con while(true){} hace que list y status tarden 8-10 s, cuando en f6ef7cd tardaban 0,1 ms, y bloquea a todas las ejecuciones y a los oyentes. AUD-B2-06 es informativo y fuera de alcance: una carrera ESRCH en una prueba de preflight de B0 (falló 1 de 12 veces). Solo se añadió la sección de auditoría al Markdown externo; no se tocó el producto, no se hizo commit y el entorno quedó limpio.
