# Entorno del navegador

`excalibur-cdp` implementa CDP asíncrono por tuberías nativas. `excalibur-browser`
lanza y supervisa instancias de Chrome, verifica el sandbox de renderizadores y
aplica la política de versiones. `excalibur-webd` ofrece diagnósticos puntuales de
entorno e instancia. `excalibur-search` y `excalibur-web-adapter` siguen vacíos.
No hay sesiones de aplicación, servidor RPC, herramientas web ni frontend visor.

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
privacidad y acceso de escritura. No se crean perfiles ni sockets.

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
privado único, inicia un proxy loopback propio que rechaza toda conexión, lanza
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
No están implementadas la integración del broker ni la gestión de perfiles de
aplicación; el argumento del proxy es una frontera del host confiable, no una
afirmación de aislamiento completo de salida.

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
