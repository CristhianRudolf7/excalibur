# Entorno del navegador

`excalibur-cdp` implementa CDP asíncrono por tuberías nativas. `excalibur-browser`
lanza y supervisa instancias de Chrome, verifica el sandbox de renderizadores y
aplica la política de versiones, gestiona perfiles, sesiones y pestañas durables
y ofrece esperas de carga, observación, acciones de entrada, diálogos, descargas,
subidas y relleno de credenciales. `excalibur-web-adapter` es la biblioteca de
confianza del host que aplica a esos efectos la autorización del arnés, la
autoridad de archivos, el control de archivos y `SecretExecutor`.
`excalibur-webd` ofrece diagnósticos de entorno y un servicio JSON-RPC Unix local.
`excalibur-search` sigue vacío. El registro de herramientas web del agente, la
composición del host no están implementados; el visor de intervención lo sirve `excalibur-webd --serve` por TCP real (código de un solo uso, cookie `HttpOnly;SameSite=Strict`, comprobaciones exactas de `Origin`, cabeceras CSP estrictas y dibujo del lienzo), cubierto por `viewer_http` y `viewer_viewport` con fixtures sintéticas locales.

Instala Rust estable con rustfmt y clippy, un compilador C/C++, `pkg-config`,
cabeceras de desarrollo de OpenSSL y una compilación verificada de bubblewrap
(como mínimo 0.12). Los paquetes de compilación de Ubuntu son
`build-essential pkg-config libssl-dev`; usa el archivo, checksum y ruta de
instalación verificados de bubblewrap 0.13.1 en [Seguridad](security.es.md).
Compilarlo requiere `meson ninja-build libcap-dev`; las pruebas de sandbox
existentes también usan `curl` y compilación C/Rust local. Instala Google Chrome
estable desde su paquete Linux oficial. Conserva activo su sandbox; nunca uses
`--no-sandbox` ni desactives globalmente las restricciones de espacios de nombres.

Un administrador prepara una identidad dedicada y directorios privados escribibles:

```sh
sudo useradd --system --user-group --home-dir /var/lib/excalibur-web --shell /usr/sbin/nologin excalibur-web
sudo install -d -o excalibur-web -g excalibur-web -m 0700 /var/lib/excalibur-web /run/excalibur-web
```

Omite crear el usuario si ya existe. Recrea el directorio de ejecución después
de reiniciar. Estas comprobaciones no requieren instalar ni desplegar servicios.
En desarrollo se admite `os_user = "nobody"` con directorios escribibles preparados
por separado. Un usuario normal conserva su propia identidad.

Proporciona un archivo dedicado y confiable (distinto de la configuración del host):

```toml
[web]
os_user = "excalibur-web"
instance_class = "ephemeral"
chrome_args = []
chrome_binary = "/usr/bin/google-chrome-stable" # opcional: busca rutas Linux conocidas
data_dir = "/var/lib/excalibur-web"
run_dir = "/run/excalibur-web"
disk_reserve_bytes = 5368709120
search_budget_bytes = 2147483648
```

Las rutas deben ser absolutas. Se rechazan opciones desconocidas. La reserva
libre no puede reducirse por debajo de 5 GiB; el presupuesto del buscador incluye
1,5 GiB de índice y 0,5 GiB de estado. La identidad final debe poder escribir en
los directorios de datos y ejecución ya existentes, que deben pertenecer a su UID
y tener modo 0700. Se rechazan enlaces simbólicos en esas rutas de directorio. Las
entradas JSON `directories` informan ruta, UID propietario, modo numérico,
privacidad, acceso compartido configurado del socket y acceso de escritura. No se crean perfiles ni sockets.

```sh
cargo build -p excalibur-webd
sudo target/debug/excalibur-webd --preflight /ruta/absoluta/web.toml
```

El lanzador root borra grupos suplementarios, cambia GID y UID, verifica que no
puede recuperar root y rechaza capacidades efectivas, permitidas, heredables o
ambientales antes de comprobar Chrome o disco. Se rechazan destinos root y
usuarios inexistentes. El JSON incluye identidad final, ejecutable y versión
observada de Chrome, bytes libres, reserva y admisión del buscador, errores y
observaciones de cgroup v2. El código de salida es cero solo si pasan las
comprobaciones; los errores de argumentos, configuración e identidad también
producen JSON con `ready = false`.

`ready` en la comprobación previa significa que la preparación del entorno es
satisfactoria. Esa comprobación mantiene `sandbox_verified` en false porque no
lanza renderizadores. Su sonda de versión solo invoca Chrome con `--version` en
un grupo de procesos separado. El plazo total de cinco segundos
cubre el proceso y stdout, incluida la salida retenida por descendientes. Se mata
el grupo al terminar o ante un error; la salida se limita a 4096 bytes. Los errores
transitorios de ejecutable ocupado comparten el mismo plazo. La sonda también
vacía el entorno heredado. No se usan modelos
remotos ni cuentas.
Se informa explícitamente si el cgroup está compartido o es inaccesible; en ese
caso hará falta contabilidad por árbol de procesos al implementar los recursos.
El cgroup es opcional y su disponibilidad no implica límites de memoria activos.
La función de admisión de disco rechaza crecimiento que consuma la reserva; la
comprobación previa exige que quepan presupuesto del buscador más reserva en el
sistema de archivos de datos. No están implementadas la vigilancia continua ni
la aplicación de límites a futuras operaciones.

Verifica con fixtures backend locales y sintéticos:

```sh
(umask 0077; cargo test -p excalibur-webd --test preflight)
(umask 0022; cargo test -p excalibur-webd --test preflight)
cargo fmt --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo build --workspace
```

La suite backend cubre errores de configuración, límites exactos de disco,
fallos de ejecutable y versión, limpieza de descendientes, plazo total de sonda,
propiedad y permisos de directorios, fixtures independientes del umask, identidades
root rechazadas, arranque sin root,
retirada irreversible de privilegios y evidencia real de identidad y capacidades
en `/proc`. Los casos exclusivos de root se ejecutan si el lanzador de pruebas
es root. No existe frontend que revisar.


Ejecuta una comprobación real de instancia temporal después de la previa:

```sh
sudo target/debug/excalibur-webd --check-instance /ruta/absoluta/web.toml
```

Este diagnóstico retira privilegios antes de construir Tokio, prepara un perfil
privado único, inicia un listener loopback propio que solo acepta pares del UID web (mapeados en `/proc/net/tcp` y `/proc/net/tcp6`) y descarta las conexiones aceptadas del diagnóstico sin reenviarlas, lanza
Chrome headless, obtiene `Browser.getVersion`, verifica el árbol de renderizadores
y emite JSON antes de cerrar y borrar su perfil temporal. No expone endpoint RPC ni visor, y no
navega a sitios externos. `sandbox_verified` solo es true con una inspección de
renderizadores satisfactoria. `--preflight` conserva su significado de comprobación
del entorno.

La API compartida `Instance` exige un perfil existente del UID actual con modo
0700, un binario absoluto de Chrome y un proxy loopback IPv4 explícito. Rechaza
root, capacidades retenidas y perfiles que contengan `DevToolsActivePort`. Los
descriptores 3 y 4 del hijo son extremos bloqueantes de tuberías nativas; el padre
usa Tokio. Chrome tiene grupo de procesos propio y `PR_SET_PDEATHSIG(SIGKILL)`,
con comprobación de la carrera de muerte del padre. Su entorno se vacía y solo
recibe HOME privado, PATH/LANG controlados y los valores necesarios de pantalla o
bus de sesión. El cierre y la liberación limpian el grupo y los manejadores de
caídas observados, comprobando el instante de inicio del PID antes de señales
individuales. La enumeración incluye manejadores con doble fork identificados
por ejecutable y perfil privado, sin apropiarse de árboles de Chrome ajenos.

Los flags seleccionan tubería, headless por defecto, aislamiento de sitios,
extensiones y servicios de fondo desactivados, cachés de disco acotadas, proxy
incluida loopback y desactivación de resolución DNS anticipada, QUIC y UDP de
WebRTC fuera del proxy. Los flags de fondo para escucha se restringen a instancias
persistentes en escucha. Los flags adicionales tienen lista estricta: se rechazan
desactivación del sandbox, depuración TCP, evasión de certificados, automatización
y sustitución de flags gestionados. El host solo puede seleccionar el almacén de
Secret Service tras verificar ese servicio; en otro caso `basic` lleva un aviso
explícito frente a lectores locales. El diagnóstico usa un perfil vacío y `basic`.
La gestión de perfiles de
aplicación no está implementada. `--serve` arranca el broker de salida (`HostProxy`)
y reenvía a su socket Unix solo las conexiones loopback del mismo UID, cerrando
los pares de otro UID sin reenviarlos; el argumento del proxy sigue siendo una
frontera del host confiable, no una afirmación de aislamiento completo de salida.

La verificación exige `Seccomp: 2`, `NoNewPrivs: 1` y espacios de nombres de usuario
y red distintos del navegador. El arranque exige al menos un renderizador
verificado en cinco segundos. Los targets nuevos y OOPIF se adjuntan con sesiones
planas y `waitForDebuggerOnStart`; solo se reanudan tras verificar sus procesos.
Las identidades de proceso nuevas tienen hasta un segundo para terminar la
inicialización del sandbox mientras sus targets siguen pausados. Un fallo después
de que un renderizador haya pasado es inmediato. Un temporizador independiente
muestrea procesos cada 50 ms y usa `spawn_blocking` para inspeccionar `/proc` y el
estado del hijo. Los eventos de consola, red y página no provocan recorridos de
procesos. Un fallo cierra la instancia con `sandbox_unavailable`; las caídas de
renderizadores generan eventos de target.

La supervisión consume un canal acotado separado con notificaciones de adjuntar,
desconectar, caída de target y ciclo de vida del transporte. Un observador normal
puede recibir `Lagged` al llenarse su cola acotada; eso no cierra CDP ni mata
Chrome. Si se retrasa el propio canal de ciclo de vida, el supervisor reconcilia
las sesiones adjuntas activas desde la caché de estado actual independiente del
lector, verifica los targets pausados antes de reanudarlos y emite
`events_resynchronized` con la cantidad omitida. Es reconciliación de estado, no
un registro histórico sin pérdidas. El muestreo de procesos continúa aparte y la
terminación del transporte se conserva en un canal watch separado aunque se
pierdan notificaciones.

`instance_crashed` significa salida/desaparición inesperada del navegador o fallo
del transporte CDP: EOF, JSON/envoltorios de protocolo o datos de adjunto inválidos,
sesión de respuesta incorrecta, trama excesiva del navegador/sin identidad o
escrituras interrumpidas/fallidas. El retraso de un observador no es una caída.
La imposibilidad de inspeccionar procesos desde el worker de bloqueo bloquea la
supervisión sin afirmar que el navegador se haya caído. `Instance::close` y la
liberación explícita generan `instance_stopped`; cerrar el cliente CDP crudo fuera
de ese ciclo de vida es un fallo de transporte. El proceso principal del navegador
permanece en los espacios de nombres del host;
aquí no se añade un espacio de nombres de red exterior.

CDP ofrece un catálogo de métodos escrito a mano, estructuras tipadas comunes de
petición/respuesta y parámetros JSON genéricos. Maneja tramas NUL fragmentadas o
concatenadas, peticiones concurrentes con números crecientes, errores de protocolo
y eventos por sesión plana. Los plazos cubren espera de bloqueo/escritura/respuesta:
diez segundos por defecto, quince para capturas y veinte para instantáneas DOM.
La cancelación y el plazo liberan las entradas pendientes; no se reutilizan IDs.
Cancelar una escritura parcial cierra el transporte. Los mensajes entrantes se
limitan a 64 MiB; una trama excesiva de target se descarta hasta el delimitador,
su sesión CDP se invalida y desconecta, y las demás continúan. Una trama excesiva
sin identidad o del navegador, JSON inválido o EOF cierra la conexión.

La versión se comprueba con la sonda compartida y acotada del binario instalado
antes de que Chrome abra un perfil persistente, y con `Browser.getVersion` tras
arrancar. Las opciones de política se configuran en el TOML dedicado:

```toml
[web.version]
# stable_major se puede aportar desde metadatos de versión confiables.
# installed_release_unix_secs es la publicación conocida, no el mtime del archivo.
# La consulta pública es optativa; omite la clave para trabajar sin red:
# stable_metadata_url = "https://googlechromelabs.github.io/chrome-for-testing/last-known-good-versions.json"
```

Frente a la versión estable aportada, cero o una mayor por detrás es normal, dos
emiten aviso y tres o más bloquean instancias persistentes, manteniendo disponibles
las efímeras. Una publicación conocida de más de 42 días también bloquea las
persistentes. La antigüedad desconocida se informa como null; no se inventa desde
la fecha de instalación. Sin metadatos de estable, la vigencia es explícitamente
desconocida. La consulta pública optativa tiene límites de cinco segundos y 64 KiB,
no sigue redirecciones y rechaza URLs con credenciales y admite fixtures HTTP locales. La
política se refresca cada 24 horas y mediante `Instance::refresh_version`; avisos
y bloqueos son eventos observables. Si falla la consulta, se emite aviso y se
conserva el último dato aportado, sin afirmar que se haya refrescado. No hay
actualizador ni descarga automática del binario.

Verifica transporte, política y contrato con el navegador instalado:

```sh
cargo test -p excalibur-cdp -p excalibur-browser
cargo test -p excalibur-browser --test cdp_contract -- --nocapture
```

El contrato corre como usuario dedicado sin privilegios, con perfil nuevo y proxy
HTTP sintético local. Ejercita cada orden del catálogo contra Chrome instalado,
incluidas las experimentales; registra errores explícitos de estado ausente para
operaciones como cancelar una descarga inexistente. Comprueba OOPIF reales, las
cuatro condiciones del sandbox, oyentes TCP del árbol y ausencia de
`DevToolsActivePort`, una instantánea DOM real de 70 MiB aislada a una sesión, aviso,
bloqueo y refresco de versión, un binario copiado sin sandbox utilizable, caídas de
renderizador/navegador y SIGKILL del padre con dos instancias. Una ráfaga real de
10.000 eventos de consola desborda deliberadamente un observador normal mientras
la instancia sigue Running/Degraded, responde a `Browser.getVersion`, conserva
renderizadores verificados y adjunta otro target. Los fixtures de transporte
comprueban también la entrega filtrada de ciclo de vida y recuperación de 400
sesiones adjuntas activas tras un retraso de ese canal. Guarda resúmenes
redactados de método/resultado y evidencia de procesos bajo
`target/test-evidence/browser/cdp_contract/`. Los helpers de subproceso están
marcados como ignorados para la invocación directa, pero los ejecutan sus pruebas
padre. La falta de Chrome o confinamiento hace fallar la suite; solo
`EXCALIBUR_BROWSER_TESTS=skip` explícito excluye Chrome real y debe constar en el
informe. No se usan APIs de modelos, cuentas reales ni benchmarks pagados.


## Sesiones y protocolo local

Ejecuta `target/debug/excalibur-webd --serve /ruta/absoluta/web.toml` en primer plano
bajo un supervisor, o permite que un host confiable use `setsid`. El proceso cambia
de usuario antes de crear runtime, base de datos, socket o perfiles. SIGTERM/SIGINT
cierran la admisión de clientes, esperan las operaciones despachadas y cierran
Chrome. La orden no instala ni despliega un servicio. Su proxy propio deniega toda
la red; el broker de red y el adaptador del arnés son capacidades separadas. Las
pruebas inyectan un proxy sintético local mediante las opciones Rust del gestor.

El servicio abre `<run_dir>/webd.sock` en modo `0660` y autentica cada conexión con
`SO_PEERCRED`. Solo admite `web.host_uid` (por defecto `0`) y root; cierra sin
respuesta las conexiones rechazadas. `web.socket_gid` usa por defecto el GID
primario final. Un host con otro UID necesita pertenecer al grupo compartido y
atravesar el directorio de ejecución: un administrador puede prepararlo en modo
`0710`, del UID web y del GID configurado del socket. La comprobación previa lo
informa como `shared_socket_directory`, no como directorio privado. Datos y
perfiles siguen en `0700`; el grupo del socket debe estar disponible para la
identidad final. Ni acceder al socket ni un run ID autorizan a un programa no
confiable a actuar como host.

Ejemplo de configuración confiable (orígenes HTTP(S) exactos):

```toml
[web.profiles.personal]
class = "persistent"
isolation = "shared_instance" # alternativa: per_site

[[web.profiles.personal.sites]]
name = "chat"
rw = ["https://chat.example.org"]
login = ["https://login.example.org"]
listen = true
listen_probe = { unread_selector = "" } # vacío: solo cambios de título
wait = { selector = "main", strict_network = false }
```

El perfil implícito `ephemeral` crea un BrowserContext desechable por ejecución,
con un máximo de tres contextos vivos y una instancia efímera compartida. Cerrar
o suspender destruye el contexto; sus cookies y almacenamiento no se reutilizan.
La instancia se detiene tras diez minutos sin contextos vivos y borra su perfil.
`web.compact = true` coloca esos contextos aislados en una instancia persistente
compartida configurada; comparte deliberadamente el proceso del navegador.
Los perfiles persistentes usan el contexto por defecto y una instancia por perfil,
o una por sitio con `per_site`. Los nombres no admiten rutas ni el separador
reservado `--`. Se activa la restauración de sesión de Chrome; el contrato contra
el Chrome instalado registra si conserva las cookies de sesión al reiniciar.
Se conservan cookies persistentes, localStorage, IndexedDB y el estado de inicio
de sesión comunicado por el host. El gestor consulta el Secret Service del usuario sin recuperar secretos, con un
plazo de dos segundos; si responde usa `gnome-libsecret`. En otro caso usa `basic`,
con aviso en el estado: lo protegen los permisos, no un cifrado frente a quien
pueda leer el perfil.
No existe RPC de CDP arbitrario ni de cookies.

Las sesiones tienen ejecución propietaria, perfil, sitio, modo, referencias a
credenciales y estado. Las pestañas conservan propietario/etiqueta de subtarea,
URL/título, época de documento e ID lógico estable. Cada pestaña usa
`newWindow: true`. Los límites son cuatro pestañas vivas por ejecución y doce
globales; la admisión devuelve `tab_limit` o `capacity_exceeded` cuando se exceden.
Un sitio en escucha comparte una sola pestaña entre sus sesiones. Sus operaciones
usan la misma exclusión por pestaña, esperan hasta treinta segundos y después
devuelven `tab_busy`. Otras pestañas pueden mantener exclusiones independientes.
La selección activa lógica pertenece a cada ejecución; cambiarla llama a
`Page.bringToFront`. Suspender guarda URL/título, cierra targets vivos y reabre en
la siguiente operación de pestañas. Una sesión en escucha devuelve
`listener_cannot_suspend`.

El gestor representa opening, ready, busy, blocked, human_control, suspended,
recovering, lost, closing y closed, y valida las transiciones. Headless es el modo por defecto. `headed` nativo requiere DISPLAY o
WAYLAND_DISPLAY; una instancia que ya vive en otro modo devuelve
`mode_unavailable`. Sin pantalla, `headed` también devuelve ese error hasta
disponer de intervención. La admisión de documentos del agente acepta solo los orígenes de
lectura/escritura configurados; los de login son exclusivos de personas. La
exclusión para entrada comprueba la URL real del frame y devuelve
`denied(human_only_origin)` en un origen de login. La detección de login y la
intervención humana se exponen por RPC mediante `takeover.start` y
`takeover.end`, que llevan una sesión bloqueada a `human_control` y de vuelta
(véase la sección de intervención).

Los bloqueos de origen conservan `blocked_reason` (`needs_login` o
`document_origin`). La reconciliación lee el frame superior real, incluidas las
páginas de error de red, para todos los propietarios de una escucha compartida.
Cuando todas las pestañas vivas vuelven a orígenes permitidos, levanta esos
bloqueos, emite `session_state` y vuelve a admitir exclusiones del agente.
Un cambio de URL no levanta bloqueos de seguridad/versión ni control humano.
Los cambios de ciclo de vida usan la tabla de transiciones validada; volver a un
origen permitido no afirma por sí solo que haya una cuenta autenticada.

La reconciliación usa eventos de navegación y URL de frames verificadas en caché.
La consulta de targets del navegador se ejecuta fuera del cerrojo del modelo y en
paralelo entre instancias. Las sondas de frame/conteo del renderizador también
quedan fuera, con un presupuesto total de 500 ms, una sonda pendiente por pestaña
y doce globales. Un timeout conserva el último estado conocido; no declara una
caída. Se descartan resultados si cambió el target, la sesión CDP o la época del
documento. Así siguen disponibles las otras ejecuciones y los eventos en escucha
cuando una página ejecuta código sin fin. La admisión de entrada y la suspensión
leen frames actuales bajo sus propias exclusiones de pestaña, fuera del cerrojo
del modelo, y vuelven a comprobar la identidad antes de aplicar el resultado.

Las instancias en escucha usan los tres flags contra la retención en segundo
plano y emulación de foco. No reciben suspensión, congelación, descarte ni presión
de memoria. El gestor reconcilia periódicamente los metadatos de targets; los
cambios de título producen `listener_activity` sin contenido de mensajes, como
máximo una vez cada cinco segundos por sitio. Los cambios dentro de esa ventana
se agrupan en una notificación pendiente que se emite al cerrar la ventana,
aunque la página ya no cambie. Conserva el último recuento; `kinds` identifica
actividad de título y/o no leídos y `kind` sigue siendo `unread_count` si cambió el
recuento, o `title_changed` en otro caso. No copia títulos ni mensajes al evento.
Un selector opcional instala un
MutationObserver en un mundo aislado y comunica solo el número de no leídos por
un binding limitado a ese mundo. Un canal de ciclo de vida separado y sondas
periódicas de salud/conteo reconcilian la contrapresión de eventos generales. Tras caer un renderizador en escucha, se sustituye
el target, restaura foco y sonda y aumenta la época; conserva el ID lógico.
La recuperación valida la URL guardada antes de crear una página y usa un origen
configurado de lectura/escritura si encuentra login o un documento de error.
Si falla la preparación, cierra el target candidato. Un fallo de recuperación de
una pestaña no interrumpe el resto de pestañas o instancias.
Las caídas de una instancia persistente pasan a recovering con esperas de 1, 5 y
30 segundos y hasta cinco relanzamientos en diez minutos. Agotarlos produce
`instance_failed` y sesiones lost. Reiniciar explícitamente el servicio limpia
ese circuito. Las sesiones efímeras pasan a lost. Las operaciones de pestañas
esperan hasta treinta segundos a que termine la recuperación.

`webd.db` es SQLite privado en WAL, sincronización FULL y un escritor serializado
en el pool de bloqueo. Guarda perfiles, estado de sitios, sesiones, pestañas,
operaciones y eventos con cursor. Un bloqueo de archivo impide gestores simultáneos
en los mismos datos; otro protege la sustitución del socket. Reiniciar marca lost
las sesiones efímeras, restaura inmediatamente las URL persistentes y aumenta las
épocas (`stale_ref(browser_restarted)`). Solo retira un SingletonLock de perfil si
su PID está inequívocamente ausente. Las sesiones CDP y las instantáneas previas
en memoria dejan de ser válidas.

JSON-RPC 2.0 usa objetos UTF-8 delimitados por salto de línea, de hasta 4 MiB.
Los mensajes inválidos o demasiado grandes producen errores tipados sin cerrar
la conexión. Las peticiones incluyen `run_id`, `deadline_ms` (1–300000) y `op_key`
para efectos. Métodos implementados: `session.open/close/suspend/list`,
`tab.new/list/switch/close`, `status.get`, `events.list`, `op.get` y la operación
de metadatos del host `site.set_login_state`. Esta última registra una afirmación
del host, no verifica un inicio de sesión. Cada operación comprueba el propietario
de su sesión/pestaña.

```json
{"jsonrpc":"2.0","id":1,"method":"session.open","params":{"op_key":"run:step:call","run_id":"run","deadline_ms":30000,"profile":"ephemeral"}}
```

Los efectos se registran como received, dispatched antes de ejecutar el gestor y
completed/failed con su resultado. Repetir clave y hash se une al trabajo vivo o
reproduce el resultado guardado; cambiar la petición da `idempotency_conflict`.
El hash excluye el plazo para poder consultar con otro presupuesto de espera.
Tras dos segundos (o la espera menor solicitada), un efecto vivo devuelve
`working{op_key}` y continúa aunque se desconecte el cliente. `op.get` informa de
trabajo vivo, resultados o `uncertain` para trabajo despachado en una vida anterior
del servicio. Los efectos inciertos nunca se repiten. Operaciones terminadas y
eventos tienen una retención de 24 horas, depurada al arrancar. Los eventos llegan
como notificaciones `event` con cursores monótonos durables; `events.list{after}`
reproduce hasta 256 por página. Un cliente que no lee se cierra tras diez segundos
de espera de escritura y puede reconectar/reproducir. Un cursor no confirma que
el host haya procesado el evento.

```sh
cargo test -p excalibur-browser --test sessions -- --nocapture
cargo test -p excalibur-webd --test webd_boundary -- --nocapture
```

Estas suites con Chrome real prueban cookies persistentes/IndexedDB, cookies de
sesión, contextos aislados, instancias compactas/por sitio, propietarios, límites,
colas exclusivas, reproducción/incertidumbre durable, mensajes y permisos del
socket, reinicio del servicio y SIGKILL con dos instancias. El contrato de escucha
mantiene una página sintética de long-polling durante diez minutos con otras
ventanas activas, comprueba eventos de título y temporizadores y mata su
renderizador para verificar recuperación. `EXCALIBUR_LISTEN_TEST_SECONDS` permite
acortar una ejecución de desarrollo; solo los 600 segundos por defecto satisfacen
la escucha prolongada. Evidencias en
`target/test-evidence/browser/{sessions,webd_boundary}/`. El lienzo del visor, la autenticación y las cabeceras de seguridad los sirve `excalibur-webd --serve` para revisión visual, cubiertos por `viewer_http` (TCP real) y `viewer_viewport` (presets pequeño/grande de Chrome) con evidencias en `target/test-evidence/browser/viewer/`.


`cargo test -p excalibur-browser --test sessions listener_events_and_document_recovery -- --nocapture`
ejecuta el fixture de ráfagas/recuperación con Chrome real sin la escucha prolongada.
Comprueba la entrega del recuento final tras cambios dentro de cinco segundos,
recuperación de login/red para propietarios compartidos y contabilidad de targets
tras caer una escucha en login. Guarda evidencia en
`target/test-evidence/browser/listener_reconciliation/reconciliation.json`.
El fixture de long-polling desactiva la caché, consume el cuerpo de cada respuesta
y registra los cambios reales de la página junto al número de notificaciones.
El fixture del servicio espera el bloqueo por documento de error que corresponde
a su proxy de rechazo total; los fixtures conservan explícitamente el ID de la
sesión viva.


`cargo test -p excalibur-browser --test sessions other_sessions_stay_responsive_with_a_spinning_renderer -- --nocapture`
usa un renderizador real en bucle infinito junto a un contexto sano del mismo
navegador y una escucha persistente. Acota a un segundo list/status, la exclusión
y el cambio de otra pestaña y las lecturas RPC Unix; comprueba los intervalos de
actividad del oyente, la sonda de entrada acotada y el cierre. Son comprobaciones
sintéticas locales, guardadas en
`target/test-evidence/browser/renderer_isolation/contract.json`.

## Espera y observación de páginas

La biblioteca y el protocolo Unix ofrecen `observe.wait_ready`,
`observe.snapshot`, `observe.subtree`, `observe.validate_ref`, `blob.read`,
`devtools.console`, `devtools.network`, `devtools.performance`, `emulate.viewport`
y `nav.go|back|forward|reload`. Son operaciones del backend compartido. El registro
de herramientas del agente, el envío al modelo visual y el visor de intervención
son componentes separados. Las acciones de entrada se describen en la sección
siguiente.

Cada operación de pestaña requiere `run_id`, `session_id`, `tab_id` y
`deadline_ms`. Navegación y emulación requieren además `op_key` y usan el
protocolo durable. Las consultas devuelven `state: completed` con el valor en
`result`. Por ejemplo, `observe.wait_ready` acepta:

```json
{"run_id":"local-run","session_id":"SESSION","tab_id":"TAB","deadline_ms":15000,"criteria":{"selector":"main.loaded","network_idle_ms":500,"dom_stable_ms":500,"timeout_ms":10000,"max_inflight":0,"strict_network":false}}
```

La espera combina el compromiso de navegación y la carga del documento, actividad
de red del marco principal y sesiones OOPIF, condiciones visibles de selector o
texto, y un MutationObserver con latido de animación en mundos aislados. Los
valores predeterminados son 500 ms de quietud de red y estabilidad del DOM y un
plazo de 10 s. WebSocket, EventSource, ping, medios y prefetch no cuentan. Las
peticiones de más de 3 s pasan a larga duración; una página estable puede devolver
`ready` con `network_busy` tras una ventana adicional de 500 ms + 2 s. El modo
estricto devuelve timeout. `timeout` incluye `criteria_met` y URL de diagnóstico.
Los marcos de desafío visibles, diálogos JavaScript, selectores de archivos,
campos de verificación vacíos e indicadores de acceso producen `blocked`; observar no los resuelve. Los sitios pueden proporcionar los
mismos criterios en su configuración opcional `wait`. El resultado ready se
recuerda junto a los loaders de los marcos. Las pestañas no oyentes habilitan
red, ciclo de vida y el script aislado de estabilidad mientras siguen vacías,
antes de navegar inicialmente o restaurarse. En adjuntos tardíos u oyentes que
reanuden la recogida, `network_unobserved` indica que no se midieron las peticiones
anteriores: `criteria_met.network` sigue siendo false. La espera no estricta puede
devolver ready con esa degradación tras cumplir DOM/condición y la ventana adicional
de quietud; la estricta devuelve timeout. Un nuevo documento observado elimina
esa incertidumbre. Desactivar la recogida del oyente invalida también su ready
guardado. La navegación invalida esa coincidencia;
una instantánea sin resultado vigente espera automáticamente.

`observe.snapshot` acepta `options` con `dom: interactive|full`,
`extent: viewport|fullpage`, `screenshot: bool` y criterios `wait` opcionales.
Combina DOMSnapshot y árboles de accesibilidad dentro de sus propias sesiones
CDP, incluidos marcos del mismo proceso y OOPIF, con 3 s de presupuesto de
recogida. Los componentes ausentes aparecen en `missing` con `partial: true`;
el DOM disponible se conserva aunque la accesibilidad quede incompleta. La imagen
se omite si falta DOM necesario para enmascarar los campos sensibles. Las
cajas del iframe propietario trasladan las coordenadas a la vista superior.
El árbol normalizado contiene jerarquía, atributos filtrados, roles, nombres,
geometría, visibilidad, habilitación y motivos de interactividad. El texto ocupa
como máximo 16 KiB y abarca la vista más una pantalla arriba y abajo; `omitted`
cuenta controles conservados solo en el blob completo. Las regiones y encabezados
mantienen el contexto. Texto de página, consola y red son datos no confiables.

La captura se guarda como blob JPEG de calidad 60 tras traer la pestaña al frente.
La altura de página completa se limita a 16.384 píxeles CSS. Los valores de
contraseña, código de un solo uso y tarjeta se redactan en texto, elementos y
árbol; sus rectángulos se enmascaran en la imagen. El host puede registrar secretos
en el redactor compartido del baúl mediante `manager.observation.redactor`,
aplicado antes de serializar. La instantánea incluye `vision_recommended` si se
pide imagen o si la vista es principalmente visual y tiene menos de 20 nodos
útiles de accesibilidad. No envía imágenes a un modelo.

Las referencias (`eN` o `fKeN` en marcos) usan identidad de target, sesión CDP,
marco, loader y nodo backend. Un nodo vivo conserva su referencia entre
instantáneas; los números no se reciclan. Solo otra instantánea sustituye la
vigente. `observe.validate_ref` acepta `snapshot_id` y `ref` y comprueba la
instantánea, loader, marco, nodo vivo, conexión y huella de rol/nombre. Devuelve
`valid` o `stale_ref` con `superseded`, `navigated`, `frame_gone`, `node_gone`,
`node_detached`, `changed`, `takeover` o `browser_restarted`. Una clave sustituida
puede incluir `remapped_ref`; validar no envía entrada ni elige otro nodo. El host
puede invalidar una pestaña al intervenir mediante
`manager.observation.invalidate(tab_id, "takeover").await`.
`observe.subtree` lee el árbol comprimido guardado por `snapshot_id` y `ref`
opcional, sin volver a capturar la página. `delivery: inline` devuelve los nodos
si ocupan hasta 1 MiB serializados. Las selecciones mayores devuelven
`delivery: blob`, `node_count`, `root_ref` y `source_blob`, con `nodes` vacío.
Se lee el blob completo autorizado mediante `blob.read` y se selecciona la raíz
indicada localmente; la respuesta queda bajo el límite de trama de 4 MiB.

Los blobs usan SHA-256 y zstd del almacenamiento existente dentro del directorio
privado `observations/`. Un registro en webd autoriza cada hash para la ejecución
solicitante durante 24 horas. `blob.read` requiere `sha256` y acepta `offset` sin
comprimir y `length` (1–1.048.576); devuelve una ventana verificada en base64 y
`total_bytes`. Otra ejecución no puede leerlo con solo conocer el hash.

Los búferes de consola y red guardan hasta 500 registros cada uno. Admiten filtros
`since_seq`, `limit`, `level` y `url_contains`; devuelven secuencia siguiente y
recuento de descartes. La contrapresión de consola está aislada de la espera y
supervisión. La pérdida de eventos de espera se informa y no produce un ready
normal. Los resúmenes de red incluyen URL, método, estado, tipo, tamaño y tiempos,
sin cuerpos ni cabeceras de autorización o cookies. `tainted: true` elimina también
consulta y fragmento; es permanente para la pestaña. Los búferes de red se limitan
a 2 MiB totales y 512 KiB por recurso; las pestañas en escucha solo recogen red
durante operaciones explícitas, con limpieza al cancelar. El host dispone además de `settle_observation`: una
ventana de navegación de 250 ms seguida de una espera de 5 s que informa
`settled` sin enviar entrada. Rendimiento devuelve
las métricas de Chrome.

Los tamaños son `small` (360×740, DPR 3, móvil), `tablet` (768×1024), `desktop`
(1280×900) y `large` (1440×900). `preset: custom` acepta `width`, `height`,
`device_scale_factor` y `mobile`. Las imágenes usan dimensiones CSS lógicas.
`user_agent` opcional solo se acepta en sesiones efímeras; las cuentas persistentes
conservan su identidad de navegador.

Ejecuta `cargo test -p excalibur-browser --test observe -- --nocapture` con Chrome
real y páginas locales sintéticas. Comprueba esperas, bloqueos, 300 controles,
shadow DOM abierto y cerrado, marcos y coordenadas OOPIF, 5.000 controles,
redacción, referencias, subárboles guardados, blobs privados, captura en segundo
plano, filtrado de cabeceras, límite de consola, métricas y tamaño móvil. También
comprueba peticiones lentas iniciales con `tab.new`, la espera automática de la
primera instantánea, adjuntos tardíos de oyentes y entrega por blob de subárboles
grandes.
Las evidencias quedan en `target/test-evidence/browser/observation/`.

## Acciones, diálogos, archivos y credenciales

Las acciones usan dos llamadas del protocolo Unix. `act.prepare` (una consulta)
acepta `request` con `action` (`click`, `type`, `select`, `hover`, `scroll`,
`press`, `upload`, `drag` o `dialog`), `snapshot_id`, `ref`, `value`, `keys`,
`target_ref`, `files` y `options` (`button`, `click_count`, `modifiers`,
`key_events`, `html5`, `direction`, `amount`, `accept`, `prompt_text`). Valida la
referencia y el origen del documento del marco del elemento y devuelve un
`plan_id` válido durante 60 segundos junto con `action_class`, `effect`, `risk`,
origen y nombre del elemento, calculados de forma determinista. `act.perform`
recibe `plan_id` y `op_key`, usa el protocolo durable de operaciones y solo se
acepta para la misma ejecución, sesión y pestaña. No se acepta JavaScript
arbitrario, órdenes CDP ni rutas del host.

Antes de la entrada, webd toma la exclusión de la pestaña, rechaza cualquier
acción distinta de `dialog` mientras hay un `confirm`/`prompt` abierto
(`dialog_open`), vuelve a validar la referencia (`stale_ref` sin efecto), vuelve
a comprobar el origen de lectura/escritura del documento del marco del elemento
(`denied(origin_not_writable)`) y recalcula la clase; si cambia, devuelve
`authorization_changed`. La accionabilidad exige un elemento conectado, visible,
habilitado (y editable para `type`), con la caja igual durante dos fotogramas de
animación y que reciba la prueba de impacto en su centro. Los reintentos esperan
0, 20, 100, 100 y 500 ms y alternan la alineación `center`, `end` y `start`. La
prueba de impacto atraviesa raíces shadow del agente de usuario y del autor y
los propietarios de marcos. Si falla, devuelve `not_actionable` con
`covered_by`; nunca se fuerza la entrada. Solo entonces la operación pasa a
`dispatched` en `web_ops`; si a partir de ahí se pierde la confirmación de CDP,
se informa `uncertain` y no se repite nunca.

`click` y `hover` envían eventos de ratón en el centro del elemento; `type`
enfoca, selecciona todo e inserta el texto, por lo que repetirlo es idempotente,
y `key_events: true` envía eventos de tecla (como máximo 2.048 caracteres).
`select` elige una opción por valor o etiqueta en un mundo aislado y emite
`input` y `change`. `press` acepta teclas con nombre, letras, dígitos y F1–F12
con `Control`, `Alt`, `Meta` y `Shift`. `scroll` sin referencia envía un evento de
rueda de `amount` pantallas en `direction`. `upload` asigna archivos preparados a
un `input[type=file]`. `drag` mueve el ratón en cinco pasos hasta `target_ref`;
con `html5: true` intercepta los datos del arrastre y reproduce `dragEnter`,
`dragOver` y `drop`, y rechaza datos de archivos nativos. `dialog` acepta o
descarta el diálogo actual, con `prompt_text` opcional.

El resultado contiene `result` (`ok` o `dialog_open`), `navigated`, `url`,
`new_tabs`, `popup_blocked`, `dialog`, `alerts`, `download`, `file_chooser`,
`settled`, `wait`, `dom_changed`, `suggest_snapshot`, `action_class` y
`elapsed_ms`. El asentamiento usa una ventana de 250 ms y una espera de carga de
5 s. `dom_changed` y `suggest_snapshot` son conservadores y siempre valen true.
Los resultados se redactan y se marcan como no confiables.

Las clases son `observe` (hover, scroll), `input` (type, select),
`navigate_link` (un enlace fuera de un formulario sin manejador de clic),
`commit`, `upload`, `drag` y `credential`. `commit` cubre botones de formulario,
Enter en un formulario o en un sitio en escucha, aceptar un diálogo en un perfil
persistente y nombres accesibles con el léxico de compromiso (enviar, send,
submit, publicar, post, comprar, buy, pagar, pay, confirmar, confirm, eliminar,
delete, borrar, transferir, transfer, firmar, sign, reservar, book, suscribir,
subscribe, aceptar, accept). En perfiles persistentes un clic sin clasificar es
`commit`. El riesgo es bajo/medio en sesiones efímeras y medio/alto en
persistentes; subidas y credenciales son siempre de riesgo alto. Los ajustes de
clase por sitio son una tabla por sitio que consulta `classify` (arq. 8.3):
WhatsApp afina Enter en el compositor a `commit` y los clics fuera de
formulario en la superficie de chats sin léxico de compromiso a
`navigate_link`; los controles de formulario y los aciertos de léxico nunca se
degradan. El resto de sitios mantiene el valor conservador (clic desconocido
en persistente sigue siendo `commit`).

Un `alert` abierto durante una acción se acepta y su mensaje redactado se
devuelve en `alerts`. `beforeunload` solo se acepta en una navegación del agente
o en una acción que navega. `confirm` y `prompt` quedan abiertos: la acción
devuelve `dialog_open` con tipo y mensaje, porque la página queda suspendida
hasta que se atiende el diálogo. Las páginas abiertas por la pestaña que actúa se
registran con el mismo propietario y aparecen en `new_tabs`. En sesiones
persistentes, una ventana emergente fuera de los orígenes de lectura/escritura se
cierra y se informa en `popup_blocked`; los límites de pestañas también la
cierran.

Cada instancia y contexto arranca con las descargas denegadas. Una descarga
iniciada por una acción se cancela y se informa como `download_blocked` con el
nombre sugerido y el origen. El método `download` (con clave `op_key`) acepta
`trigger: {snapshot_id, ref}` o `{url}` y `timeout_ms` de hasta 120 s. Serializa
las descargas por contexto, permite una sola descarga desde los marcos de la
pestaña en un directorio privado nuevo `quarantine/<sesión>/<uuid>/`, usa el GUID
como nombre de archivo, rechaza orígenes fuera de la sesión y cancela y borra
todo lo que supere 200 MiB. Al completar, calcula SHA-256, tamaño y tipo por
bytes mágicos, guarda los bytes como blob autorizado para la ejecución, borra la
copia de la cuarentena y restaura la política de denegación. El nombre sugerido
se sanea y nunca se usa como ruta.

`act.stage_upload` recibe fragmentos base64 de hasta 2 MiB (`file_name`,
`staged_id` opcional, `offset`, `final`) y escribe
`staging/<sesión>/<uuid>/<nombre saneado>` con modo `0600` en directorios
privados, hasta 200 MiB por archivo y 128 archivos preparados. Los preparados
pertenecen a la ejecución y a la sesión, se borran al cerrar la sesión y caducan
a la hora. Chrome solo recibe estas rutas de preparación.

`act.check_credential_target` y `act.fill_secret` implementan el relleno de
credenciales por referencia. La sesión debe declarar la referencia `secret://`.
El origen esperado debe ser HTTPS canónico sin `userinfo`. El origen de
seguridad comprometido y la URL del marco superior y del marco del elemento deben
coincidir exactamente con él; un iframe solo se acepta si el sitio incluye su
origen exacto en `credential_frames`. El elemento debe ser un
`input[type=password]` visible, habilitado y accionable en un origen de
lectura/escritura. `act.fill_secret` repite todas las comprobaciones bajo la
exclusión de la pestaña justo antes y después de enfocar, confirma el foco,
inserta el valor y pone a cero el búfer de la petición y la cadena decodificada.
El elemento rellenado se redacta en instantáneas posteriores. Las capturas de esa
pestaña se retienen (`vision_suppressed: true`) durante 5 s y mientras siga el
mismo documento. El método no se guarda en `web_ops`; el `SecretExecutor` del
host aporta la marca durable de uso y la reconciliación de cierre seguro.

`excalibur-web-adapter` ofrece `WebAdapter` sobre un `LocalBackend` en proceso o
el `SocketBackend` Unix. `act` rechaza argumentos que contengan un secreto
registrado, prepara el plan y lo autoriza como `Action::Browser` (acción externa
`browser.<clase>` y riesgo según la clase) o como el ámbito de datos
`browser.observe` mediante el registro durable de efectos del host, de modo que
una aprobación o una clave repetida nunca envían dos veces. `credential` solo lee
metadatos del baúl, exige un `WebsitePassword` con URL, comprueba el objetivo en
webd sin el secreto y después llama a `SecretExecutor::execute`, que registra el
uso y la auditoría antes de resolver el valor. `stage_path` resuelve la ruta con
un descriptor de directorio de capacidades dentro del espacio de trabajo de la
ejecución, rechaza rutas protegidas, nombres de credenciales y enlaces simbólicos
que salen de la raíz, y exige autorización de lectura; `stage_blob` exige el
ámbito de datos `blob:<sha256>`. No se prepara contenido que contenga un secreto
registrado. `download` autoriza un efecto de navegador de riesgo alto, lee el
blob en cuarentena en ventanas de 1 MiB y lo entrega al control de archivos del
host: los ejecutables, los archivos demasiado grandes y los que contienen
secretos quedan en cuarentena, se ejecuta un `FileScanner` opcional y los
archivos admitidos pasan a ser blobs del host no confiables con registro de
auditoría. Son puntos de entrada de biblioteca; no se registra ninguna
herramienta del agente.

Ejecuta `cargo test -p excalibur-browser --lib observation` y
`cargo test -p excalibur-web-adapter --test actions -- --nocapture`. La segunda
usa Chrome real con sandbox como usuario dedicado y un servidor HTTPS local
sintético con una CA temporal, que requiere `openssl` y `certutil`
(`libnss3-tools`). Comprueba cada acción sobre su control, una capa
superpuesta que devuelve `not_actionable` sin clic, un botón en movimiento, la
escritura idempotente, arrastre con ratón y HTML5, `alert`/`confirm`, ventanas
emergentes efímeras y bloqueadas, denegación en un marco ajeno, aprobación de un
`commit` antes del efecto con un único resultado repetido, clics desconocidos
persistentes como `commit`, contenido exacto de la subida, fuentes de subida
rechazadas que nunca llegan a webd, descargas sin armar, armadas y demasiado
grandes, relleno y redacción de credenciales, rechazo en marco de otro origen,
subdominio y HTTP sin resolver el secreto, una navegación entre comprobación y
relleno, una caída simulada del host tras la marca de uso que no se reintenta y
la ausencia del valor secreto en archivos de webd y del host, eventos, auditoría
y salida de las pruebas.
Las evidencias quedan en `target/test-evidence/browser/actions/`.

## Red, contenido, bloqueos e intervención

`--serve` escucha el proxy de Chrome en `127.0.0.1:<aleatorio>` y comprueba
cada conexión aceptada contra `/proc/net/tcp` y `/proc/net/tcp6`: solo un par
del UID web alcanza al broker de salida (`HostProxy` con DNS fijado, sin
respuestas privadas/loopback, solo puertos web) por su socket Unix
(`excalibur_webd::loopback::{accept_owned, forward_to_unix}`); cualquier otro
par se cierra sin reenvío, así el listener no sirve como proxy abierto.
`net::check_loopback_uid` interpreta ambas tablas (IPv4 little-endian, IPv6
con palabras invertidas) y falla cerrado ante pares desconocidos. El
diagnóstico `--check-instance` aplica la misma comprobación de UID y descarta
las conexiones aceptadas sin reenviarlas.

La admisión de documentos compara orígenes comprometidos de frames, nunca
cadenas (`net::document_origin`: `esquema://host[:puerto]` exacto, sin
userinfo). La navegación superior a `file:`, `chrome:`, `chrome-extension:`,
`devtools:`, `javascript:` o `data:` se deniega
(`net::is_forbidden_scheme`). Los perfiles persistentes solo admiten los
orígenes configurados: la lectura acepta `rw` o `ro`, la entrada exige `rw`
(`denied(origin_not_writable)`) y los orígenes `login` quedan para personas
(`denied(human_only_origin)`). `credential_frames` deben ser orígenes HTTPS
exactos y los orígenes `ro` no pueden solaparse con `rw` ni `login`. Las
sesiones efímeras registran cada origen nuevo navegado
(`Manager::note_ephemeral_origin`); la comprobación determinista C4
(`net::exfiltration_check` vía `Manager::navigation_risk`) marca POST entre
orígenes y consultas o fragmentos de más de 128 caracteres (o más de 4,5 bits
de entropía de Shannon por carácter) hacia un origen nuevo, para que una
ejecución contaminada pida aprobación antes de confirmar.

Cada contexto nuevo deniega quince permisos sensibles con
`Browser.setPermission` antes de navegar (`permissions::denial_params`:
geolocalización, notificaciones, cámara, micrófono, MIDI, lectura y escritura
de portapapeles, acelerómetro, giroscopio, magnetómetro, detección de
inactividad, acceso a red local, captura de pantalla, gestión de ventanas y
NFC); si una denegación falla, la apertura falla y ningún contexto corre sin
ellas (los nombres simples `clipboard`, `sensors` y `shared-storage` los
rechaza el Chrome actual y no se usan). La configuración del sitio puede
conceder permisos concretos después.

La detección de bloqueos es determinista. La sonda de readiness corre en un
mundo aislado e informa `{kind, vendor, signals, confidence, url}` para
`blocked_captcha`, `blocked_by_site`, `needs_login` y `needs_2fa`, incluidos
frames de reto, marcadores invisibles de reCAPTCHA, páginas de bloqueo
perimetrales y orígenes de identidad, con señales de corroboración en
título/texto. `wait_ready` solo acepta la sonda con
`observation::detectors::classify_probe_blocked` (tipos desconocidos,
confianza baja y señales vacías nunca bloquean) y puede elevar la confianza
media con un estado de documento corroborante (`elevate_with_status`); las
señales de baja confianza quedan como `degraded`. La señal de bloqueo por
cabecera vive atada a su documento: cada navegación descarta el bloqueo de
cabecera guardado de la página anterior antes de confirmar la nueva, así
`wait` ni informa un bloqueo rancio ni pierde el nuevo.

Una sesión bloqueada puede entrar en intervención por RPC: `takeover.start`
pasa `Blocked` a `human_control` con instante de inicio, mientras
`takeover.end{resolved}` devuelve una intervención resuelta a `Ready`,
avanzando la época documental de cada pestaña e invalidando referencias
previas como `stale_ref(takeover)`; una intervención sin resolver o vencida
(15 minutos) vuelve a `Blocked` con `blocked_unresolved`. Pruebas
deterministas de ciclo de vida fijan el límite del plazo (899 s resuelve,
901 s caduca), el salto de época y el estado `blocked_unresolved`.

El visor lo sirve `--serve` en `127.0.0.1:<aleatorio>` (`webd::viewer`):
`viewer.issue` acuña un código de 8 caracteres de un solo uso por el socket
Unix con credenciales de par mientras la sesión está en `human_control`;
`POST /auth` lo canjea una vez por una cookie `HttpOnly; SameSite=Strict`
ligada a la pestaña y al plazo, y `GET /`, `/viewer.js` y `/style.css` exigen
esa cookie. La mejora a WebSocket exige además coincidencia exacta de
`Origin` antes de responder `101`; cualquier otro caso (código desconocido,
reutilizado o caducado, cookie ausente, origen ajeno) falla cerrado con
`403`. El código nunca se lee de la URL: las query strings se ignoran por
construcción. Las respuestas llevan
`Content-Security-Policy: default-src 'none'; img-src data: blob:; script-src
'self'; style-src 'self'; connect-src 'self'; frame-ancestors 'none';
base-uri 'none'; form-action 'self'; require-trusted-types-for 'script'` más
cabeceras `nosniff`, sin referrer y sin caché; el script de cliente solo toca
el DOM con `textContent`, imágenes de canvas y `document.title`, y la hoja de
estilos apila controles de ≥44 px en viewports pequeños manteniendo el canvas
1:1 hasta 1280 px en grandes. El streaming de screencast tras la trama `101`
sigue pendiente de la integración del arnés.

Los informes de bloqueo se traducen al resultado de tarea del arnés sin juicio
de modelo (`browser::takeover::intervention_outcome`): los llamantes
interactivos aparcan la ejecución en `input_required` con el bloqueo e
instrucciones, mientras los no interactivos (`excalibur exec`, perfil de
evaluación) fallan de inmediato con `blocked_requires_human`. La navegación
efímera conserva el conjunto de orígenes C2: cada navegación confirmada
registra orígenes nuevos (`note_ephemeral_origin` en `nav.go` y `tab.new`),
los esquemas prohibidos siguen denegados y el riesgo C4 (`navigation_risk`:
cargas entre orígenes largas o entrópicas, POST entre orígenes) se informa
antes de confirmar; la autorización de dominios pertenece al arnés, fuera del
límite de este crate.

Ejecuta `cargo test -p excalibur-browser --lib` y
`cargo test -p excalibur-webd --lib`. Cubren tablas proc y UID, orígenes,
entropía y exfiltración, las quince denegaciones y su forma CDP, clasificación de
detectores y páginas de control, códigos de intervención y visor de un solo uso
con caducidad, comprobaciones de cookie/página/socket del visor, cabeceras CSP
y escape de marcado, el límite de la intervención, la compuerta documental
efímera y el mapeo `input_required`/`blocked_requires_human`.
`cargo test -p excalibur-webd --test viewer_http` ejercita el endpoint servido
por TCP real (canje único, códigos caducados/reutilizados/desconocidos, código
en URL ignorado, cookie ausente, origen ajeno, handshake de mejora y cabeceras
estrictas). `cargo test -p excalibur-browser --test block_matrix -- --nocapture`
corre las matrices 9.5 con Chrome real sobre páginas sintéticas locales (frame
y marcadores de captcha, páginas de bloqueo perimetrales, cabecera
`cf-mitigated`, contraseña en sitio con sesión iniciada, campos de un solo uso
y grupos OTP, elevación por corroboración, denegación de origen de login solo
humano, más páginas de control con insignia invisible, ruta inyectada y texto
plano)
con la matriz en `target/test-evidence/browser/block_matrix/`.
`cargo test -p excalibur-webd --test viewer_viewport -- --nocapture` dibuja el
shell servido con Chrome real en presets pequeño (360×740) y grande (1440×900)
solo con páginas sintéticas, comprobando ancho, ausencia de scroll horizontal,
objetivos táctiles y cabecera de origen, con capturas en
`target/test-evidence/browser/viewer/`. Quedan pendientes: la elevación por
estado de documento conectada a la red, tipos de detector fuera del
vocabulario fijo 9.1 (variantes de muro de cookies/geo-bloqueo sin marcadores
9.1 no bloquean), la puesta a cero de la copia de relleno del adaptador
(F-B4-03) y la unificación de magias ZIP (F-B4-04), ambas fuera del conjunto de
ficheros B5.

## Lectura ligera y citas

`excalibur_browser::{extract, read, cite}` con `excalibur_webd::read::Service`
forman la ruta backend de `web_read` y el almacén de `cite`. No se registra
ninguna herramienta de agente; el render de respaldo del navegador se inyecta
como HTML ya esperado (contexto efímero más `observe.wait_ready` más
`DOM.getOuterHTML`), así el servicio sigue determinista sin Chrome real.

La validación acepta solo `http`/`https` sin userinfo y como máximo 2.048
caracteres; los fragmentos se quitan para la identidad de caché. `fetch_http` (valor seguro por defecto) usa plazos explícitos de 10 s de
conexión y 30 s totales, sigue como máximo cinco redirecciones de forma
manual con revalidación por salto (loopback/rangos privados/IP de metadatos/
sufijos sensibles) más rechazo de bajada HTTPS, consume el cuerpo por trozos
con contador y aborta pasados 10 MiB (`body_too_large`), no guarda ni reenvía
cookies (sin jar; `FetchPolicy::isolate_cookies`) y aplica la lista de tipos
texto/HTML/XML/feed/JSON. `FetchPolicy::test_loopback` admite literales
loopback solo para fixtures sintéticas locales. `extract::extract_document`
produce título, Markdown y fragmentos `pN` con desplazamientos en bytes UTF-8
y rutas de encabezados tras quitar bloques `script`, `style`, `nav`, `footer`
y `aside`, con enlaces en línea como texto. La heurística HTTP-primero escala
al navegador con cáscaras SPA (poco texto visible y varios scripts),
proporción texto/marcado bajo 0,02, montajes vacíos (`#root`, `#app`,
`__next`, `ng-app`) o `<noscript>` que pide JavaScript; `__NEXT_DATA__`,
`__NUXT__` o JSON-LD incrustados evitan la escalada. Sin render inyectado se
devuelve `needs_browser:<motivo>`.

La captura exige una espera `ready` vigente para la época de la pestaña
(`WaitGate::check`, regla 7.4-1): épocas no listas o caducadas devuelven
`wait_required` o `wait_stale_epoch` antes de tocar el DOM. Las esperas no
estrictas permiten degradaciones `network_busy` y `network_unobserved`; el
modo estricto las convierte en `timeout`, como el contrato long-poll. Los
resultados llevan `title`, Markdown en línea hasta 50 KiB recortado en la frontera UTF-8 anterior con `truncated`,
longitud total, `content_hash` (`sha256:`), `source_id` (`w_<16 hex>`), fecha,
estado HTTP, `render` (`http`/`browser`), etiqueta del extractor y metadatos
de fragmentos con `untrusted: true`. `slice_fragments` sirve rangos `pN` o
`pA-pB` desde el Markdown completo por hash. La caché en memoria conserva 24 h
con bypass `fresh` que preserva la fecha original.

`cite::locate` antepone la coincidencia exacta y, si no, usa búsqueda
normalizada (espacios/mayúsculas) mapeada al tramo exacto de bytes originales
que cubre las palabras (alineado a frontera de carácter); normalizar el tramo
reproduce la cita normalizada. `cite` crea
registros `Citation` (`source_id`, URL, título, fragmento, desplazamientos,
enlace `#:~:text=`, fecha, hash) y `format_citation` los muestra sin inventar
páginas. `webd::read::Service` guarda páginas por hash para fragmentos y
validación de citas.

Ejecuta `cargo test -p excalibur-browser --test read_cite -- --nocapture`,
`cargo test -p excalibur-browser --lib` y
`cargo test -p excalibur-webd --lib read`. Cubren extracción estática y
desplazamientos, SPA con espera antes de capturar y render inyectado,
long-poll degradado frente a timeout estricto, citas exactas/normalizadas/
ausentes, corte de fragmentos con tope de 50 KiB en frontera de carácter,
rechazo por trozos tras 50 MiB, decisiones HTTP-primero, caché loopback local
y caducidad TTL, más las filas 14.4 del lado navegador (rechazo de
loopback/privadas/metadatos, rechazo de redirección a privada, detección de
bajada, robots permitir/denegar, gancho de índice meta-noindex, no reenvío de
cookies). Quedan pendientes la salida completa por broker (resolución DNS,
obtención/caché de `robots.txt`, encolado real al índice, registro de
visor/RPC), los extractores de producción `dom_smoothie`/`htmd`, PDF/feeds/
juegos de caracteres y render efímero real con contexto efímero (carril B9).

## Recursos, presupuestos, admisión y medición

`webd::resources::ResourceGate` conecta el fundamento de
`excalibur-browser-resources` con el servicio (arquitectura 13.1-13.6, 18.3;
NB-17, NB-18). La admisión usa la media móvil por origen (200 MiB por
defecto, 400 MiB clase pesada) más 200 MiB de margen bajo el presupuesto
blando de 1,5 GiB, con denegación inmediata por límites de pestañas (4 por
ejecución, 12 globales), PSI `some avg10` desde 20 % y crecimientos que
rompan la reserva de 5 GiB; sin sitio tras el desalojo, espera hasta 120 s
antes de `capacity_exceeded` con el desglose. El desalojo sigue los pasos
1-7 (cachés de webd, IDLE LRU, GC efímero más presión moderada solo en la
instancia efímera, caché efímera, cierre de una pestaña ACTIVE efímera sobre
su límite con `tab_evicted`, reinicio efímero, solo recarga tranquila LIVE);
las pestañas en escucha nunca se congelan, descartan, presionan ni
suspenden, y `browser_suspend_session` en escucha devuelve
`listener_cannot_suspend` sin aprobación. Con disco bajo (menos de 7 GiB
libres o presupuesto de webd excedido) solo se vacían cachés efímeras y
`cache_storage`/`shader_cache` persistentes de orígenes no escuchados.
`serve` rechaza crecer bajo la reserva configurada. La telemetría añade
`browser_working_set_bytes`, `browser_pss_bytes`, `browser_instances` y
`browser_tabs` con picos, aparte de la RAM del arnés y del contenedor.

```sh
cargo test -p excalibur-webd --test resources_gates -- --nocapture
cargo test -p excalibur-webd --test resources_measure -- --nocapture
```

`resources_gates` verifica solo el subconjunto de decisión de las filas 13.6
con fixtures sintéticas (constantes, admisión con media móvil, desalojo 1-7
con protección LIVE, puertas de disco, `listener_cannot_suspend`, picos de
telemetría); el cierre completo de 13.6 queda ABIERTO para el carril
principal. `resources_measure` ejecuta la forma del procedimiento 18.3
(instancia en blanco, página estática local, diez pestañas efímeras pesadas,
niveles A/B/C con PSS por tipo, 3 repeticiones con mediana/mínimo/máximo) con
informe en `target/test-evidence/browser/resources/measure.json`. Las cifras
con sesión de WhatsApp/Teams (18.4, 1 h/24 h) siguen pendientes del usuario y
nunca se afirman. Pendientes para el 13.6 completo: cgroup real con
`memory.high`, llenado con pestañas pesadas y continuidad LIVE, espera real
hasta `capacity_exceeded`, suspensión por la vía de sesión, LIVE con más de
1 GiB y `logged_in`, puerta con sistema de archivos pequeño y microbench de
coste (nunca afirmados). `serve` retiene la `ResourceGate`, exige la reserva
de disco configurada en los directorios de datos y ejecución al arrancar con
telemetría de arranque/cierre, y deja la admisión de memoria por operación
como pendiente del carril principal (serve no media la creación de pestañas).
La espera informa `capacity_exceeded` solo tras expirar los 120 s de desalojo;
`listener_cannot_suspend` se exige en `sessions::Manager`, y las ayudas de
desalojo y webd solo calculan la decisión. Ver [Fundamento de
recursos](browser-resources.es.md).

## Integración con el arnés

`excalibur-web-adapter::harness` aporta el `ToolRuntime` del arnés, el
catálogo de herramientas y los manifiestos de capacidad (arq. 16). Las doce
herramientas `browser_*` se registran como diferidas (fuera del prefijo
ansioso, vía `search_tools`/`call_tool`); `web_search` y `web_read` son
núcleo (arq. 14.3). Los metadatos estáticos siguen 16.2 (efecto,
idempotencia, riesgo, `timeout_ms`, fast, clase de cancelación);
`browser_act` degrada su efecto por llamada en `prepare` (`observe` a
`Pure`, `input` a `Idempotent`, las clases de commit quedan
`NonIdempotent`).

La reconciliación sigue 16.3: las lecturas puras reintentan,
`browser_navigate` sondea la URL de la pestaña (`Done` si ya muestra el
documento destino, `Retry` si no), las herramientas con clave reenvían con
la misma clave, y las clases de commit consultan `op.get(op_key)`
(`completed` a `Done` con el resultado guardado, `received` a `Retry`,
`failed`/`dispatched`/reiniciado a `Uncertain`, sin repetir a ciegas). La
cancelación sigue 16.5: `immediate` detiene el sondeo del cliente,
`graceful` y `after_completion` además cierran las sesiones efímeras
rastreadas de la ejecución al cancelar, mientras las persistentes siguen
vivas; `execution_state` libera el rastreo en pausa y estados terminales.
El perfil `eval` (16.8) deniega perfiles persistentes, bloquea fuentes de
benchmark en `web_read`/`browser_navigate`/resultados de búsqueda y
convierte los bloqueos humanos en fallos inmediatos. El registro en la
composición del host (cadena
`DeferredTools(ExecutionTools(SecuredTools(...)))` en
`crates/eval/src/runner.rs`) solo ocurre con el arnés habilitado
explícitamente vía `EXCALIBUR_WEB_HARNESS=1` con `EXCALIBUR_WEB_SOCKET`
apuntando al socket de webd y el flag de red del host permitiendo
herramientas web; la ruta por defecto no cambia. Un RPC
`op.cancel` dedicado para operaciones ya despachadas sigue pendiente en
webd, así que su cancelación depende del paro del sondeo más la limpieza
efímera. La intervención sigue siendo un envoltorio fino: `takeover_start`
llama al RPC existente `takeover.start` y `viewer_issue` al existente
`viewer.issue` con las mismas comprobaciones de propiedad; no se añade
autorización nueva. Conectar el comando `excalibur browser takeover
<sesión>` en el binario CLI sigue pendiente (el CLI hoy solo implementa
`exec`).

```sh
cargo test -p excalibur-web-adapter
cargo test -p excalibur-cdp -p excalibur-browser -p excalibur-search -p excalibur-web-adapter
cargo test -p excalibur-webd --test web_integration -- --nocapture
cargo fmt --check
cargo clippy --workspace --all-targets -- -D warnings
```

`web_integration` cubre las filas 16.10 solo con fixtures sintéticas
locales: recorrido efímero del arnés (apertura, navegación a URL local,
espera, captura, acción de entrada, `web_read`) con eventos de fuente,
reconciliación tras caída a `Done` sin reenvío, reinicio de webd que vuelve
`Uncertain` un commit despachado sin reenviar, re-lanzamiento de escucha,
cancelación que cierra sesiones efímeras y conserva persistentes,
recuperación de `search_tools` con `browser_snapshot`/`browser_act` entre
los cinco primeros con hash de catálogo estable, denegaciones `eval` y
conflicto por clave reutilizada. Las filas con caídas del demonio real y
los recorridos manuales 18.4 con sitios reales siguen pendientes del
usuario y nunca se afirman.
