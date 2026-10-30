# Fundamento de recursos del navegador

[English](browser-resources.md)

`excalibur-browser-resources` implementa el fundamento independiente de
recursos del documento externo `arquitectura-navegador.md` (sección 13 con
el procedimiento de medición 18.3). El crate es autocontenido y sin
dependencias: no toca el ciclo de vida del navegador, el intermediario de
red ni la contabilidad del host. Todas las comprobaciones usan fixtures
locales y sintéticas más una lectura propia de `/proc/self/status`; no hay
acceso a modelos, rastreos en vivo, sesiones reales ni despliegue.

## Mapa de módulos

| Módulo | Responsabilidad |
|---|---|
| `sampling` | Muestreo por niveles: nivel A (`memory.current - inactive_file`, 2 s), nivel B (`RssAnon + VmSwap` por `/proc/<pid>/status`, 10 s), nivel C (`Pss` por `smaps_rollup`, 60 s), nivel D (eventos del cgroup, PSI `some avg10` > 10 %); enumeración desde una raíz con forma de `/proc` ordenada por PID con clasificación `--type=` (`browser`, `zygote`, `gpu-process`, `utility`, `renderer`, `crashpad_handler`, `webd`); totales de PSS y conjunto de trabajo por tipo; `oom_score_adj` deseado (renderizadores efímeros 1000, persistentes 300, GPU/utilidad 200, navegador 0) con regla de subida del mismo usuario; un único enum `InstanceKind` compartido con desalojo (`InstanceClass` es alias) |
| `budgets` | Constantes explícitas de RAM y disco: global blando 1,5 GiB, duro 2,0 GiB, `memory.max` del cgroup 2,5 GiB, aviso por pestaña en escucha 700 MiB, recarga en vivo con 1 GiB durante 10 min y 2 min de calma, aviso efímero 500 MiB y cierre 800 MiB, máximo 3 contextos y 3 pestañas efímeras, 4 pestañas por ejecución y 12 globales, estimación por defecto 200 MiB (400 MiB pesadas) más 200 MiB de margen, admisión con PSI bajo 20 %, webd 60 MiB RSS, caché de Chrome 32 MiB efímera y 64 MiB persistente, perfil con aviso 1 GiB y limpieza permitida 1,5 GiB, reserva libre 5 GiB, umbral de disco bajo 7 GiB, datos de webd 2,5 GiB (instantáneas 500 MiB LRU 24 h, caché de lecturas 200 MiB, cuarentena 300 MiB con rechazo), buscador `minimo` 1,5 GiB de índice más 0,5 GiB de estado y `ampliado` 6 GiB más 1 GiB |
| `admission` | Estimador de media móvil por origen (200 MiB por defecto, 400 MiB clase WhatsApp/Teams); las decisiones deniegan límites de pestañas, PSI desde 20 % y crecimientos que rompan la reserva, admiten si estimación más margen cabe bajo el blando y si no esperan hasta 120 s antes de `capacity_exceeded` con el desglose |
| `eviction` | Clases LIVE, ACTIVE (ventana 60 s), IDLE (TTL 5 min) y HTTP; pasos ordenados 1-7 (cachés de webd, IDLE LRU incluidas pestañas persistentes no escuchadas, GC efímero más presión moderada solo en la instancia efímera, caché de red efímera, cierre de una pestaña ACTIVE efímera sobre su límite con `tab_evicted`, reinicio efímero, recarga tranquila LIVE); las pestañas en escucha nunca se congelan, descartan, presionan ni suspenden; `suspend_session` es ayuda de decisión que devuelve `listener_cannot_suspend` en escucha sin aprobación (se exige en `sessions::Manager`); operaciones prohibidas (`prepareForLeakDetection`, congelar, `memory.high` estricto sin swap) |
| `disk` | Puertas de admisión (`disk_admission_allowed`, `search_start_allowed` con presupuesto más reserva), acciones por tamaño de perfil, limpieza con disco bajo solo en cachés efímeras y `cache_storage`/`shader_cache` persistentes de orígenes no escuchados, estado de presupuestos de webd y presupuesto LRU genérico por antigüedad |
| `telemetry` | `BrowserResourceSample` con `browser_working_set_bytes`, `browser_pss_bytes`, `browser_instances`, `browser_tabs` y sus picos, informados aparte de la RAM del arnés y del contenedor |
| `measure` | Resumen del procedimiento reproducible: instancia en blanco, sitio estático local, diez pestañas efímeras pesadas, 30 s de reposo, lecturas A/B/C con PSS por tipo, exactamente 3 repeticiones con mediana/mínimo/máximo más metadatos de Chrome, kernel y flags |

## Verificación

Ejecuta la suite backend aislada de este alcance (directorio de destino
propio):

```sh
cargo fmt --check --manifest-path crates/browser-resources/Cargo.toml
CARGO_BUILD_JOBS=2 CARGO_TARGET_DIR=/tmp/browser-resources-target cargo clippy --manifest-path crates/browser-resources/Cargo.toml --all-targets -- -D warnings
CARGO_BUILD_JOBS=2 CARGO_TARGET_DIR=/tmp/browser-resources-target cargo test --manifest-path crates/browser-resources/Cargo.toml
CARGO_BUILD_JOBS=2 CARGO_TARGET_DIR=/tmp/browser-resources-target cargo build --manifest-path crates/browser-resources/Cargo.toml
```

La suite cubre la clasificación `--type=` de cada clase de Chrome, el
análisis de nivel B y C, la aritmética del nivel A con `inactive_file`,
umbrales de eventos y PSI, enumeración de un árbol sintético de seis
procesos con totales coincidentes, la regla de subida de OOM, los valores
por defecto de RAM y disco, estimaciones pesadas con media móvil, rutas de
admisión/espera/denegación con espera de 120 s y desglose, denegaciones por
pestañas/PSI/reserva, orden de desalojo con LRU de IDLE, presión solo
efímera y cierre por límite, recarga tranquila LIVE, rechazo de suspensión,
protección de orígenes en escucha con disco bajo, puertas de arranque del
buscador, orden LRU, picos de telemetría y medianas de tres repeticiones.
El crate no tiene dependencias, así que no necesita cambios de lock ni del
manifiesto raíz.

## Cableado en webd (B6)

`excalibur-webd` aplica el fundamento mediante
`webd::resources::ResourceGate` (arquitectura 13.2-13.6): admisión con la
media móvil por origen bajo el presupuesto blando de la puerta, plan de
desalojo 1-7 con protección LIVE, puertas de reserva de disco, limpieza con
disco bajo que respeta los orígenes en escucha y campos de telemetría.
`serve` retiene la puerta, rechaza crecer en disco en los directorios de
datos y ejecución al arrancar cuando el libre rompería la reserva
configurada, admite la operación de arranque contra lecturas vivas de
memoria/PSI/disco (`admit_live`) y pliega muestras vivas en la telemetría de
arranque/cierre. `AdmissionRequest::with_estimator` rellena la estimación desde
el estimador del núcleo y toma el presupuesto blando explícito de la puerta
para no fijar valores en webd y no ignorar ningún campo en silencio. La
espera informa `capacity_exceeded` solo tras expirar los 120 s de desalojo. El crate no trae marcador `[workspace]` y se resuelve como
dependencia de ruta de `webd` dentro del workspace principal; las suites
aisladas anteriores siguen con `--manifest-path` y directorio propio.

```sh
cargo test -p excalibur-webd --test resources_gates -- --nocapture
cargo test -p excalibur-webd --test resources_measure -- --nocapture
```

`resources_gates` cierra la arquitectura 13.6: constantes, puertas de
admisión con media móvil, orden de desalojo 1-7 con protección LIVE, puertas
de disco, `listener_cannot_suspend` y picos de telemetría, más las filas
vivas — cgroup real nivel A con `memory.high` escriturable probado solo en
lectura (forzado aislado en fixtures sintéticas), subida no privilegiada de
`oom_score_adj` en proceso hijo, llenado con pestañas pesadas y continuidad
del temporizador LIVE en Chrome real (conjuntos reducidos con la misma orden
13.3), espera real de 120 s hasta `capacity_exceeded` con desglose,
suspensión por la vía de sesión, LIVE con más de 1 GiB y `logged_in`
preservado, puerta con sistema de archivos pequeño (`statvfs` real más
comprobaciones sintéticas), microbench de coste y admisión viva por operación
en la ruta serve.

## Mediciones (18.3)

`resources_measure` ejecuta la forma del procedimiento reproducible solo con
fixtures sintéticas: instancia en blanco, página estática local, diez
pestañas efímeras pesadas, lecturas A/B/C con PSS por tipo de proceso,
exactamente 3 repeticiones con mediana/mínimo/máximo más metadatos de
Chrome, kernel y flags. Los niveles B y C enumeran árboles sintéticos con
forma de `/proc`; una lectura propia en vivo de `/proc/self/status` ancla el
nivel B a la máquina real. Los 30 s de reposo constan como constante del
procedimiento sin espera real para que la suite sea rápida; una medición VPS
real espera el retardo completo. El informe JSON queda en
`target/test-evidence/browser/resources/measure.json`. Las cifras con sesión
de WhatsApp/Teams (18.4, 1 h/24 h) siguen pendientes del usuario y nunca se
afirman aquí.

## Aún pendiente

Las señales de presión del intermediario, las llamadas CDP de
GC/presión/caché, los recorridos de perfiles con
`Storage.getUsageAndQuota`, las mediciones en la VPS y la aceptación completa
de B6 más allá de las filas cerradas pertenecen al carril principal. Una
decisión de recursos sigue siendo una entrada de admisión y desalojo: solo
el servicio cableado la aplica, con `serve` exigiendo las puertas de reserva
de disco que le son propias más la admisión viva por operación al arrancar.
Las cifras de sesión de WhatsApp/Teams de 1 h/24 h (18.4) siguen pendientes
del usuario y nunca se afirman aquí.
