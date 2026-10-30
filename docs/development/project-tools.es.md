# Herramientas de proyecto y concurrencia de edición

Las bibliotecas Rust compartidas aportan herramientas de proyecto mediante un catálogo diferido. El host controla rutas, comandos de servidores de lenguaje aprobados, identidades de tareas, permisos y ciclo de vida. El backend no interactivo conecta grafo, vigilancia y catálogo con su ejecución existente; no añade un servidor de frontend.

## Catálogo y permisos

`LocalTools` es el runtime físico. `project::deferred_catalog()` describe `file_history`, `revert_edit`, `workspace_snapshot`, `worktree`, `integration_queue`, `code_graph` y `lsp`. `DeferredTools` entrega sus esquemas mediante `search_tools` e invoca `call_tool` por el mismo delegado seguro que una llamada directa. Esos esquemas quedan fuera del prefijo inicial. [Recuperación de herramientas](tool-retrieval.es.md) describe BM25/vectores, roles, soporte nativo y validación de esquemas.

El host puede componer `DeferredTools(ExecutionTools(EpisodeTools(SecuredTools(LocalTools))))`. El envoltorio diferido resuelve el nombre real antes de autorizar. El historial y la restauración de entradas requieren autoridad sobre el directorio padre canónico; así, sustituir un enlace no sigue su destino actual para autorizar. Los destinos de restauración son explícitos; se rechazan renombrados fuera del conjunto autorizado. El modelo no elige identidad Git, autoridad de tarea ni ejecutor de pruebas de integración. `LocalTools::authorize_parent` instala esas capacidades del host. Estas herramientas no hacen push ni fusionan ramas de tarea con la rama del usuario.

## Ediciones durables, historial e instantáneas de comandos

Cada restauración reutiliza preparación, preimágenes durables, huellas, arrendamientos de escritura, publicación atómica y `file_versions`. `file_history` enumera las versiones registradas. `revert_edit` calcula una reversión a tres bandas contra el archivo actual: conserva cambios posteriores independientes y devuelve conflictos sin sobrescribir. La reapertura y reconciliación reutilizan preparación e identidad de versión.

Los pasos de una ejecución que usa exec toman una instantánea sombra al inicio del paso y otra al final; los exec intermedios reutilizan la instantánea inicial del paso en vez de recorrer el árbol de nuevo. Salir de la ejecución activa por pausa, cancelación, espera de aprobación o bloqueo también cierra el paso actual con una instantánea final, para que los pasos que terminan fuera del límite normal sigan siendo restaurables. Las ejecuciones que nunca tocan exec no registran instantáneas. Los blobs se resumen en un solo proceso por instantánea y el índice se escribe con terminadores NUL, para que los nombres hostiles viajen literales y no puedan corromper ni falsear entradas; el contenido excesivo o secreto se omite con un aviso en lugar de fallar la instantánea, y las fronteras de paso degradan a un aviso en el siguiente resultado exec en vez de fallar el paso. Cada árbol recibe una referencia Git inmutable antes de publicar su recibo SQLite. Antes de la recolección limitada de objetos también se protegen los recibos antiguos de todas las ejecuciones de esa raíz. El historial activo no tiene un corte arbitrario por cantidad de instantáneas. La caducidad temporal y limpieza de referencias huérfanas siguen pendientes de la retención H9; esta política conservadora puede conservar objetos adicionales. El mantenimiento automático de Git está desactivado en estos repositorios. No preparan archivos ni crean commits en el repositorio del usuario. `workspace_snapshot` enumera las instantáneas de la ejecución actual, muestra diferencias o restaura archivos seleccionados explícitamente. Se rechazan identificadores de otras ejecuciones. Un comando vivo puede abarcar varios pasos; cada uno registra el estado observado del sistema de archivos, sin afirmar que el comando terminó.

Las entradas pasan comprobaciones de raíz vinculadas a descriptores y de hardlinks privados. Se excluyen nombres de credenciales, metadatos sensibles, rutas protegidas del host, `.git`, `target`, `node_modules` y reglas de ignorados. El grafo recibe archivos normales UTF-8 autorizados; las instantáneas también conservan metadatos de ejecutable y enlaces donde el sistema de archivos los admite. Una restauración no sigue un enlace externo para leer o sobrescribir su destino. Los cambios de entrada no admitidos o conflictivos se informan explícitamente.

## Servicios de lenguaje y grafo de código

`excalibur-code-intelligence` indexa Rust, Python, JavaScript, TypeScript/TSX, JSON y Markdown con gramáticas tree-sitter reales. Los hashes evitan parseos redundantes y los árboles en memoria permiten ediciones incrementales. Nodos y aristas persistidos representan símbolos, imports, llamadas/referencias, tests, secciones documentales y citas. Las consultas distinguen resolución sintáctica por nombre y enriquecimiento LSP. Los vínculos explícitos con requisitos conservan su referencia de evidencia del host. `impacted_tests` recorre dependencias indexadas; las relaciones incompletas o dinámicas no se presentan como hechos demostrados.

`ProjectIntelligence` conecta el grafo con herramientas y vigilancia. Una edición devuelve errores de sintaxis nuevos y errores LSP de la versión actual, sin repetir diagnósticos anteriores. El parser comprueba también puntuación ausente. Archivos binarios, protegidos o demasiado grandes se informan como no indexados y sus nodos anteriores se retiran. El límite del indexador es de 8 MiB por archivo; herramientas de archivos e instantáneas aplican sus propios límites de bytes.

`LspManager` registra servidores sin iniciarlos, comparte una sesión por proyecto/lenguaje y las cierra tras diez minutos de inactividad. Ofrece definición, referencias, símbolos del documento/espacio de trabajo, diagnósticos y renombrado. El adaptador Linux usa sandbox de solo lectura, home privado, red cerrada y entorno explícito del host. Rechaza peticiones `workspace/applyEdit` del servidor. Los renombrados son propuestas hasta validar hashes, versiones, rangos Unicode UTF-16, solapes y destinos en el pipeline normal. Se rechazan operaciones de recursos del servidor que no se hayan negociado.

Los TOML generales/de usuario y de proyecto pueden declarar `language_servers.<lenguaje>` con `command`, `args`, `enabled` e `initialization_options`. Los comandos se resuelven respecto al archivo propietario, independientemente del cwd. Los archivos de ámbito no pueden cambiarlos; los del proyecto no pueden redirigir almacenamiento ni reducir durabilidad. Cargar configuración no ejecuta servidores: el host debe aprobarla y construir un `ServerConfig` confinado. `ExecOptions.language_servers` recibe esos servidores aprobados; un mapa vacío no inicia ninguno.

Un diagnóstico de versión coincidente no demuestra que el servidor haya terminado todo su análisis de fondo. Puede publicar primero una lista vacía y después errores de la misma versión. Comprobaciones frescas y diagnósticos de sintaxis se distinguen de un build/test aprobado.

## Arrendamientos, partición e integración

`excalibur-workspace` combina arrendamientos durables por ruta canónica, mapa en memoria y bloqueos del sistema operativo entre procesos que cooperan. Caducan tras diez minutos sin latido; las ejecuciones activas los renuevan durante herramientas largas. El bucle de renovación reintenta al titular fallido en el siguiente ciclo en vez de detenerse, y sigue renovando mientras quede vivo cualquier gestor. Esperas humanas, pausa, bloqueo y estados terminales los liberan. Una colisión espera hasta treinta segundos e informa titular y actividad. Un efecto no interrumpible ya admitido conserva su guarda SO hasta terminar aunque se libere el arrendamiento durable. Las huellas detectan también cambios del usuario, que no pide arrendamientos.

El host reenvía `ToolRuntime::execution_state`, `begin_step` y `end_step` por los decoradores. Las herramientas independientes sin notificaciones del motor liberan sus arrendamientos después de cada llamada. Las ejecuciones gestionadas los conservan hasta la transición correspondiente. Admisión y renovación comprueban el estado durable; un aviso obsoleto no reactiva una ejecución terminal.

El padre puede asignar conjuntos de archivos con dependencias directas del grafo; los solapes se persisten y notifican al padre. Las consultas grandes informan su límite en lugar de devolver una partición aparentemente completa. Sin grafo, la API de bajo nivel puede registrar conjuntos explícitos del host.

Worktrees y ramas de integración viven en el directorio de datos del host fuera del proyecto. Proyectos sin Git o con submódulos usan árbol compartido. Solo un padre con autoridad explícita de tarea puede crear, guardar checkpoints o retirar worktrees de hijos y procesar la cola. Los checkpoints preparan entradas capturadas autorizadas con índice privado, verificaciones repetidas de contenido/metadatos y CAS de la referencia; conservan lo preparado anteriormente para archivos no seleccionados. Un worktree sucio no puede entrar en la cola.

La integración es serial y durable: rebase sobre la rama de integración de tarea, ambos diffs e intención conservados ante conflictos, pruebas impactadas del host y avance rápido solo de la rama de integración cuando pasan. Fallos y conflictos devuelven la rama para resolverla. Reintentar una entrada devuelta o en conflicto termina el rebase en curso, exige un árbol limpio y adopta atómicamente el checkpoint actual del hijo como nueva fuente de la cola; la entrada se vuelve a comprobar desde ahí, mientras que un reintento sin estado nuevo se rechaza. No se inventa una comprobación aprobada si falta el ejecutor de pruebas. Hooks, includes y drivers de ejecución Git del proyecto no se ejecutan por estas operaciones; las configuraciones de drivers personalizados sin adaptador confinado se rechazan. El modo compartido no crea commits en la rama activa del usuario.

## Cambios externos y ciclo de vida

`ProjectMonitor` controla un vigilante nativo y una cola durable. Los avisos de archivos calculan hashes solo de los candidatos admisibles afectados; cambios de directorios/ignorados o desbordamiento provocan una reexploración, y `.git/info/exclude` siempre llega al vigilante aunque el resto de `.git` nunca se explora. El host configura las rutas de proyección cuya sección generada se canoniza antes de la comparación semántica. El contenido que el agente produjo con `exec` se atribuye a Excalibur: los cambios observados con un paso exec abierto no se marcan externos, la instantánea de cierre registra el contenido producido y las filas pendientes de un análisis que compitió con el cierre se corrigen. Se reintenta trabajo pendiente sin releer periódicamente todo el árbol. Los cambios invalidan evidencia del ámbito, actualizan/retiran nodos del grafo y envían una directiva durable al titular del arrendamiento. Se conservan las preimágenes de lectura para mostrar el diff de un conflicto.

El host conserva un monitor mientras el proyecto está activo, consulta `last_error` y llama a `stop` al cerrarlo. El trabajo interrumpido permanece en la cola. El backend no interactivo ejecuta una exploración final, detiene vigilancia y servidores de lenguaje y cierra grafo y bases.

La publicación de salida de terminal ejecuta la barrera `sync_all` del spool sin retener el mutex de estado de salida. Estado, contabilidad de recursos y plazos de espera async pueden consultar la sesión en ejecución mientras la sincronización está pendiente; `exited`, código de salida y estado de timeout se publican juntos cuando retorna la barrera. Se conserva la política existente de sincronización de mejor esfuerzo. La prueba backend determinista `slow_spool_sync_keeps_status_readable_and_exit_unpublished` bloquea esa barrera y comprueba disponibilidad del mutex y orden de publicación. Esto demuestra el defecto del mutex y su corrección, no la causa de todos los timeouts históricos HCC-1.

## Verificación

```sh
cargo test -p excalibur-workspace --test concurrency --test shadow_entries --test typed_checkpoint
cargo test -p excalibur-code-intelligence
cargo test -p excalibur-tools --lib slow_spool_sync_keeps_status_readable_and_exit_unpublished
cargo test -p excalibur-tools --test terminal
cargo test -p excalibur-tools --test tool_retrieval --test terminal_vs_tools
cargo test -p excalibur-tools --test project_tools --test project_monitor --test project_monitor_races --test child_integration
cargo test -p excalibur-models --test deferred --test replay
cargo test -p excalibur-tasks --test verification
cargo fmt --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo build --workspace
```

Las pruebas backend usan repositorios sintéticos, protocolos locales, procesos confinados reales, SQLite, locks SO e interrupciones. Grafo/LSP incluyen parsers reales y una comprobación rust-analyzer identificada por separado en Linux. No acreditan ejecución en Windows o macOS. La precisión de recuperación y la comparación con terminal describen sus fixtures sintéticos, sin benchmarks pagados ni garantías generales de rendimiento. Proveedores externos, clientes MCP e instalación/creación de paquetes son capacidades separadas: solo se registran runtimes realmente implementados.
