# Daemon, maestro y subagentes

El daemon aloja el motor Rust compartido existente. `excalibur-master` gestiona la recepción durable y los turnos acotados; `excalibur-orchestrator`, los árboles de agentes, buzones, admisión, control y avisos; `excalibur-gateway` adapta esos servicios a JSON-RPC autenticado. `excalibur-daemon` aporta el ciclo de vida del anfitrión y la CLI sigue siendo un adaptador de proceso.

## Ejecución y desarrollo

```sh
cargo build --workspace
# Configurar por entorno un token privado aleatorio de al menos 32 caracteres.
excalibur daemon start --project /ruta/absoluta/proyecto --data-dir /ruta/absoluta/datos-privados --listen 127.0.0.1:3791
excalibur daemon stop --data-dir /ruta/absoluta/datos-privados --listen 127.0.0.1:3791
```

Ambos comandos requieren `EXCALIBUR_DAEMON_TOKEN`. El arranque permanece en primer plano; un supervisor puede gestionar el proceso. La propiedad exclusiva de la base del principal impide otro propietario. Las rutas relativas de datos se resuelven una vez respecto al directorio inicial. SIGINT, SIGTERM y `daemon.stop` autenticado inician el cierre. Esto no implica instalador, registro de servicios del sistema ni despliegue.

Sin modelo, el daemon recibe mensajes durables y atiende estado/control deterministas; el trabajo en lenguaje natural queda pendiente con un aviso de disponibilidad. Se configura un modelo explícito con `--model` u `OPENAI_MODEL` y su clave por `OPENAI_API_KEY`; `--base-url` elige un endpoint compatible. El anfitrión compartido también acepta una pasarela de respaldo y `MasterConfig` explícito. El maestro usa el compilador L0–L5 con un techo de 32 768 tokens que incluye un margen de seguridad del 10 %, 256 KiB y hasta dieciséis turnos activos. El historial se selecciona por ámbito antes de leer los textos; los originales durables permanecen completos. Una fuente que no cabe genera `context_blocked`, sin llamar al proveedor ni saltarla con un mensaje posterior. El anfitrión puede invocar `retry_context` tras ajustar la configuración, o detener ese turno. La compilación y tokenización van al pool de fondo. Los secretos se registran en el redactor vivo y no se incluyen en eventos para clientes. La suite backend determinista no necesita llamadas a modelos externos.

Los comandos explícitos autenticados `/approve [id]` y `/deny [id]` usan la misma decisión durable de alcance único que `approval.respond`, sin llamar al modelo. El ID debe pertenecer a una ejecución originada en esa conversación; sin ID solo se elige una solicitud pendiente y vigente única. Si hay varias, se pide el ID exacto sin adivinar. La selección y su recibo quedan ligados a la operación; reenviar el mensaje no repite la decisión. Solo se despierta una ejecución en espera de aprobación y se conservan las pausas del usuario. No existe una herramienta de concesión de aprobaciones invocable por el modelo.

El adaptador independiente escucha en loopback. El alojamiento remoto necesita un proxy inverso TLS autenticado de confianza; el router compartido acepta Origins explícitos del anfitrión. Los tokens del navegador van en Authorization, nunca en una URL. Los [contratos de la puerta](../../crates/gateway/README.es.md) describen los métodos reales, autoridad sobre blobs y cursores. Los métodos futuros de memoria, skills y automatización indican indisponibilidad, sin simular resultados.

## Planificación y autoridad

El anfitrión mantiene un runtime interactivo de dos hilos, otro de trabajo y un pool de CPU de fondo acotado. Las ejecuciones del motor son tareas Tokio; las herramientas conservan su sandbox y grupo de procesos. Un pánico en un turno, ejecución o trabajo de CPU se convierte en error de tarea. Un error del servicio de fondo del daemon inicia un cierre controlado.

El planificador por proveedor tiene cuatro carriles y cubos de solicitudes/tokens con reposición continua por minuto. P0 reserva un hueco de concurrencia y el diez por ciento del cupo configurado de solicitudes/tokens por minuto; los inferiores no consumen esa reserva. Cada carril conserva FIFO y cede ante solicitudes superiores. El mantenimiento también cede cuando el anfitrión marca actividad del usuario. Las reservas usan el máximo declarado por el adaptador cuando existe; de lo contrario, una estimación de la solicitud serializada. Los techos del anfitrión se reducen con las cabeceras numéricas `x-ratelimit-limit-requests` y `x-ratelimit-limit-tokens` observadas por el adaptador compatible; los valores inválidos se ignoran. No se infieren cupos de respuestas sin esas cabeceras ni se modela tráfico externo a esta instalación. El contrato de cabeceras se contrasta con la [documentación oficial de OpenAI](https://developers.openai.com/api/docs/guides/rate-limits). Los decoradores conservan métricas observadas y metadatos de caché/réplica.

En Linux se lee cada segundo el RSS del daemon y sus descendientes desde `/proc`. El controlador compartido detiene la admisión, solicita descargar recursos inactivos, pausa ejecuciones de prioridad baja por recursos y puede detener una herramienta como último recurso. La admisión vuelve tras sesenta segundos por debajo del ochenta por ciento del límite; las pausas del usuario permanecen. Este adaptador declara indisponible el RSS nativo en otras plataformas. Las herramientas Linux usan nice 10 y prioridad de E/S best-effort 7; un cgroup delegado sigue perteneciendo al anfitrión. Las pruebas distinguen las prioridades observadas del despliegue de cgroups no comprobado.

`HostOptions` aporta autoridad sobre raíces canónicas y proyectos registrados. Cada raíz debe quedar dentro de esa autoridad. `DaemonActions` restringe rutas y ámbitos de los trabajadores de proyecto; `Security::register_run` verifica cada token real de hijo respecto al padre antes de ejecutarlo. El maestro solo expone coordinación, estado, preguntas y hora acotados. Los archivos y terminal se delegan a una ejecución de nivel uno. Un trabajador de nivel dos no crea hijos, aunque intente invocar directamente la herramienta oculta. El catálogo del perfil se fija en el contexto durable; la autorización de ejecución es independiente de la visibilidad.

El ámbito `general` excluye los archivos de los proyectos registrados, también en el verificador. Para trabajar en el proyecto inicial se envía `scope: "project:default"` en `conversation.send` o se selecciona `/project default`. Registrar metadatos de un proyecto no amplía el techo de permisos: los ámbitos de ejecución y raíces autorizados se aprovisionan en `HostOptions`. Los perfiles personalizados registrados conservan un rol base obligatorio; nombre, icono e identidad no añaden permisos.

El host puede configurar `GitOptions` con una identidad y `IntegrationChecks` reales; no se inventan valores por defecto. El padre crea el worktree registrado y la delegación del hijo incluye `worktree-owner:<owner>` en `inputs`. El anfitrión comprueba tarea del padre, ámbito, raíz canónica y propiedad única, fija el token efectivo y enruta archivos/procesos a ese árbol. El checkpoint y la cola Git del hijo usan su autoridad específica; integrar sigue correspondiendo al padre. A2 puede requerir aprobación real para Git; A3 necesita `full_acknowledged` explícito. Los comandos de la CLI no inventan una identidad ni comprobaciones de integración. `ScopedPlanning` reutiliza las transacciones y recibos de `EpisodeTools`; la puerta de cada ejecución y el verificador independiente usan su raíz efectiva.

## Coordinación durable

La recepción persiste antes del acuse y deduplica por identidad de cliente y `client_msg_id`; rechaza una reutilización contradictoria. La bandeja del cliente puede reenviar mensajes sin acuse y retomar eventos desde el último cursor confirmado. Las conversaciones avanzan independientemente; cada una serializa sus turnos, limita a ocho pasos de modelo e inyecta mensajes en puntos seguros. Una generación temprana aún no mostrada puede reiniciarse con ambos mensajes. El fallo del proveedor conserva la bandeja y mantiene disponibles estado/control; los reintentos de disponibilidad son acotados y una señal explícita de salud reinicia el circuito.

La delegación estructurada transporta objetivo, entrega, ámbito, rutas mediante el token, referencias de entrada, instrucciones literales, skills, presupuesto y bifurcación explícita opcional del contexto. El hijo normalmente no recibe razonamiento ni historial del padre. Cada tarea conserva una ficha durable con los mensajes literales de origen y sus IDs, interpretación, instrucciones, suposiciones y un criterio observable inicial. Las reservas de presupuesto usan el motor durable existente. `task_db` y `task.batch` comparten una bandeja durable que propaga solo las correcciones confirmadas a las ramas afectadas; el registro de tareas invalida sus evidencias conforme a la revisión. Los padres en espera liberan huecos, permitiendo avanzar a hijos de varios árboles bajo el límite global. Las herramientas de coordinación del modelo y la puerta de finalización usan el mismo orquestador que la API.

Los buzones tienen identificadores durables, entrega al menos una vez y acuses explícitos. Las directivas persisten antes de entregarse, se propagan solo a ramas/ámbitos afectados y conservan su revisión. Los contratos de cancelación del motor y las herramientas determinan el punto seguro. Estado muestra datos durables, intenciones de modelo/herramientas pendientes, salida viva del anfitrión, uso, cola e informes; no revela generación privada del modelo ni avance interno de una herramienta silenciosa.

Las órdenes de control se serializan y son durables. La pausa se propaga con `paused_by` ligado a la orden; reanudar solo restaura esos descendientes. Interrumpir conserva los descendientes ajenos a la orden. Las pausas/reanudaciones automáticas comprueban motivo y revisión mediante CAS para conservar las pausas del usuario. La cancelación es terminal y exige confirmación autenticada; un booleano generado por el modelo no es confirmación del usuario en el adaptador del maestro. Redirigir lleva la instrucción nueva y puede reanudar la ejecución pausada indicada. Deshacer sigue usando historial e instantáneas; una orden no revierte un efecto externo ya enviado.

Los avisos conservan origen, clase, intentos y acuse. Los urgentes evitan la agrupación normal; los normales esperan cinco segundos y respetan silencio. La conversación de origen conserva su aviso, un urgente puede enlazar otra conversación activa y los intentos por un canal externo preferido permanecen pendientes hasta que un adaptador confirme un envío idempotente. El anfitrión independiente publica avisos de conversación; no afirma tener Telegram ni canal móvil instalado. Un acuse en cualquier canal suprime intentos posteriores en otros.

## Comprobaciones

```sh
cargo test -p excalibur-master
cargo test -p excalibur-orchestrator
cargo test -p excalibur-gateway
cargo test -p excalibur-daemon
cargo test -p excalibur-cli --test daemon
cargo test -p excalibur-tools --test process_priority
cargo fmt --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo build --workspace
```

Las pruebas usan respuestas sintéticas, bases temporales aisladas, sockets/procesos locales reales y muestras temporales explícitas o reloj Tokio pausado. Disponibilidad, redirección, control y avisos se comprueban en backend. La evidencia de plataformas y los pendientes heredados pertenecen al informe de entrega separado; una prueba Linux no certifica otro sistema operativo.

El progreso del registro de trabajadores comparte el cursor durable de eventos de conversación. Las entradas exponen solo ejecución, agente, ámbito, secuencia del registro y clase de evento; el estado detallado se consulta mediante estado autenticado. Las proyecciones repetidas se deduplican por ejecución y secuencia. Cada trabajador usa la misma proyección de telemetría que la ejecución embebida y exporta trayectorias ATIF y métricas por ejecución en `daemon-logs/runs/<run_id>`. Los intentos del maestro conservan metadatos reales del proveedor principal/respaldo, costo y caché conocidos y valores pendientes desconocidos.


## Acciones explícitas por proyecto

Una delegación del maestro puede indicar `scope`; `scopes` registra una tarea transversal explícita. El ámbito principal debe figurar en ese conjunto. Cambiar el destino exige una mención inequívoca del proyecto en los mensajes originales congelados y debe respetar el token del anfitrión. El recibo durable conserva IDs y texto originales: un cambio posterior del selector no puede redirigir el reenvío. Los trabajos de proyectos distintos reciben tareas distintas. Una tarea transversal conserva un ámbito principal y los ámbitos y raíces de proyecto concedidos en su capacidad; no puede adquirir un proyecto no mencionado. Los worktrees Git siguen perteneciendo a un solo proyecto.

Las operaciones `task_db` del maestro pueden indicar ámbitos autorizados diferentes. El adaptador valida todos los grupos antes del primer efecto, ejecuta una transacción idempotente por ámbito y devuelve resultados `scopes` etiquetados. No es una transacción atómica entre ámbitos; el reintento usa los recibos de cada grupo. Las menciones desconocidas o ambiguas requieren aclaración en lugar de una escritura adivinada.

Para autorizar escrituras, los alias ASCII de uno o dos caracteres usan la forma explícita `project:<id>`; un artículo corriente no concede acceso a un proyecto.


Los trabajadores transversales fijan un límite positivo del sistema de archivos a la unión exacta de raíces registradas. Lecturas y ediciones, búsquedas, capturas sombra y procesos confinados comparten ese límite; el verificador independiente lo conserva en solo lectura. Los proyectos anidados no concedidos y los archivos futuros fuera de la unión siguen siendo inaccesibles. Las capturas históricas con rutas ajenas al límite actual no pueden listarse ni compararse desde esa ejecución. `ToolRuntime::filesystem_confinement` es una acreditación del anfitrión, ausente por defecto: los runtimes genéricos siguen necesitando permiso para todo el workspace. Cada raíz acreditada se comprueba contra el token actual; los argumentos del modelo no pueden crear esa acreditación.


Los informes informativos dirigidos al maestro entran en la misma ruta de avisos. `NotificationPolicy.progress` admite omisión/null (activado por defecto para tareas N3), true (activado) o false (desactivado). Una `notification_policy` de tarea válida prevalece sobre los campos de conversación; las sustituciones malformadas conservan la política de conversación. Siguen aplicándose silencio y acuses. Las respuestas humanas obligatorias, pausas por aprobación y detenciones por presupuesto son urgentes; las esperas ordinarias de hijos/herramientas no generan avisos repetidos.


Al arrancar con el anfitrión detenido, `DaemonHost::open` usa `Orchestrator::rebind_master_at_startup` para aplicar el techo confiable de `HostOptions`, conservando el ID del maestro y el token congelado de cada descendiente existente. `register_master` sigue siendo estricto. Las fábricas y concesiones usan los proyectos provisionados actuales; todas las raíces registradas durables, incluidos los proyectos omitidos, siguen siendo límites de privacidad para General y los ámbitos solapados. Una ejecución cuyo ámbito ya no está provisionado se bloquea antes de inferencia o efectos y emite un aviso urgente y un mensaje al padre. El backend no concede raíces ni ámbitos en caliente.
