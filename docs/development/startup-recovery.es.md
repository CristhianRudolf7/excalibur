# Arranque del daemon y admisión ante bucles de caídas

El host compartido mantiene un registro durable de arranques en la base del principal. `DaemonHost::open` abre el esquema aditivo con checksum y registra una encarnación antes de inicializar servicios. El almacenamiento ya comprueba la integridad al abrir la base. Un arranque anterior sin recibo de cierre limpio se registra como reinicio no limpio; los errores y reintentos ordinarios del proveedor no crean entradas de arranque o caída.

`DaemonHost::close` drena maestro, orquestador y recursos nativos antes de registrar una frontera limpia. El comando foreground aporta el resultado de sus servicios: un fallo de supervisión o limpieza no obtiene recibo limpio. El marcador es la última escritura durable de aplicación antes del cierre de la base, no una prueba de que toda finalización del sistema operativo haya tenido éxito.

Tres detecciones de reinicio no limpio en 600 segundos activan el modo seguro hasta 1.800 segundos después de la última caída detectada. Otra caída durante ese periodo lo prolonga. Las marcas de caída son las de su detección en el siguiente arranque, porque SIGKILL no puede escribir su salida. El mantenimiento interactivo actualiza el latido del arranque independientemente del trabajo. Un retroceso del reloj impide la admisión automática de recuperación.

## Admisión y control reales

`Orchestrator::tick` sigue recogiendo tareas y atendiendo buzones y controles, pero no despacha workers durante recuperación inicial o modo seguro. Otra comprobación durable ocurre antes de vincular el runtime y registra la asociación arranque/ejecución antes de lanzar el motor. La cuarentena se comprueba en esa misma frontera. Recepción de conversación, turnos del maestro, estado autenticado, pausa/cancelación y cierre del daemon siguen disponibles. El trabajo nuevo recibido en modo seguro queda listo en la cola durable.

Cada arranque conserva las ejecuciones realmente admitidas por su encarnación. En el siguiente arranque, sus estados autoritativos y entradas confirmadas `step_completed` determinan el paso activo. Tres caídas con la misma ejecución y paso activos ponen esa ejecución en cuarentena. Salir del modo seguro no libera la cuarentena. Un error de proveedor o una ejecución solo en cola no establecen atribución de caída.

Métodos RPC autenticados del host:

- `daemon.recovery.status`: arranque, fase, plazo del modo seguro, caídas recientes, interrupción, IDs en cuarentena y las primeras 100 decisiones del arranque actual. SQLite conserva todas las decisiones y sus agregados.
- `daemon.recovery.exit_safe_mode`, con `operation`: sale del modo global. Repetir la misma operación no sale de una generación posterior.
- `daemon.recovery.release_quarantine`, con `operation` y `run`: libera esa cuarentena y deja la ejecución bloqueada en pausa de recuperación. Sigue siendo necesario un control ordinario explícito de reanudación. No son herramientas del modelo ni concesiones de capacidades.

La expiración o salida manual solo liberan pausas propiedad del modo seguro. El host vuelve a comprobar aprobaciones pendientes, presupuesto, ediciones registradas y límite de interrupción de seis horas antes de reanudar una pausa apta. Las pausas del usuario conservan su propiedad. Una ejecución que ya no cumple queda pausada por recuperación. Se mantiene el orden existente por prioridad y pesos de proyecto.

## Decisiones conservadoras de arranque

La recuperación automática exige autonomía autónoma o superior tanto en el host actual como en la configuración de seguridad inmutable de la ejecución, interrupción inferior a seis horas más de un paso restante, tokens restantes positivos y más de 30 segundos restantes. Una autonomía congelada desconocida se trata como supervisada. Se conservan controles de usuario/padre/recursos y esperas de aprobación. Las intenciones pendientes de herramientas no puras bloquean antes de inferencia o despacho. Reservas de hijos pendientes, ediciones registradas y solicitudes de modelo sin respuesta durable pausan para recuperación explícita en lugar de repetirse a ciegas.

El motor existente conserva la reconciliación de efectos. Este bloque no acredita reconciliación completa del mundo al arrancar: reconsulta de tareas durables de capacidades externas, comparación exhaustiva de archivos tras ediciones registradas, inventarios de sesiones de terminal y recibos de todos los posibles despachos de proveedor en vuelo siguen pendientes. Una edición registrada impide admisión automática hasta reconciliación del host/usuario. El arranque no concede ni consume aprobaciones.

Cada agente registrado no terminal recibe una decisión y un hecho de recuperación durables. El motor compartido reproduce el último hecho en la continuación L5 sin sobrescribir la nota del worker ni convertirlo en instrucción del usuario. Ese hecho no despierta controles existentes. El arranque encola una notificación urgente real por cada nueva decisión bloqueada, vinculada al arranque, ejecución, motivo, conversación original y ámbito. Otras notificaciones `recovery_summary` informan conteos y detalles acotados de ejecución/decisión/motivo del ámbito activo de cada conversación, una vez por conversación y arranque. La canalización existente del maestro aplica urgencia y horario silencioso; las rutas urgentes a otra conversación solo exponen un enlace, no los detalles de la decisión. El esquema aditivo v2 confirma cada recibo junto a su notificación: reabrir y repetir la publicación no pierde ni duplica el aviso. El siguiente arranque publica decisiones bloqueadas anteriores aún sin recibo con su identidad original. Los resúmenes del host conservan el ámbito vinculado al recibo durante la entrega, aunque no exista agente en ese proyecto, sin suplantar un worker. El almacenamiento conserva todas las decisiones; el resumen no sustituye a las alertas urgentes individuales.

## Verificación backend y resto de H9

```sh
cargo test -p excalibur-orchestrator --test startup_recovery
cargo test -p excalibur-cli --test startup_recovery
cargo test -p excalibur-daemon --test service_supervision
```

La suite del orquestador usa SQLite temporal real, relojes deterministas y modelos/herramientas simulados. Cubre umbral, cuarentena por paso y liberación, frontera real de despacho, expiración/controles manuales, replay, autonomía/seis horas, hechos de aprobación conservados, intenciones pendientes y deduplicación de avisos urgentes con ámbito tras reapertura real de la base. El fixture CLI de Linux lanza el daemon foreground real, lo mata tres veces con SIGKILL, consulta HTTP autenticado, recibe conversación del maestro durante modo seguro, comprueba un resumen durable, sale del modo y verifica reinicio limpio. No configura modelos de proveedor ni llama a APIs pagadas.

Es una base conectada de arranque/admisión, no H9 completo. Sesiones de proyectos largos, watchdog de diez minutos, reconciliación completa, despacho de agenda, retención/purga, instaladores y supervisores del SO, actualizaciones firmadas/drenaje/reversión y ejecución nativa multiplataforma siguen siendo requisitos pendientes. El registro de arranque foreground no implica servicio del SO ni actualización implementados.
