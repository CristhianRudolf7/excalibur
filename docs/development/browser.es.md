# Entorno del navegador

El workspace reserva `excalibur-cdp`, `excalibur-browser`, `excalibur-search` y
`excalibur-web-adapter` como bibliotecas vacías. `excalibur-webd` implementa
únicamente comprobaciones del entorno Linux. No atiende peticiones ni lanza
sesiones de navegador y no está registrado en las herramientas del host.

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

`ready` significa que la preparación del entorno es satisfactoria.
`sandbox_verified` permanece en false: no están implementados el lanzamiento del
navegador ni la verificación del sandbox de renderizadores. Solo se invoca Chrome
con `--version` en un grupo de procesos separado. El plazo total de cinco segundos
cubre el proceso y stdout, incluida la salida retenida por descendientes. Se mata
el grupo al terminar o ante un error; la salida se limita a 4096 bytes. No se usan modelos remotos ni cuentas.
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
