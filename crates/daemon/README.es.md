# Anfitrión del daemon Excalibur

Alojamiento reutilizable del motor compartido, maestro conversacional, orquestador y puerta autenticada. `RuntimeSet` separa trabajo interactivo, tareas del motor y CPU de fondo acotada. `LaneScheduler` reserva concurrencia/cupo del proveedor para el maestro. `DaemonHost::open` conecta raíces del anfitrión, permisos efectivos, tareas durables y puertas de finalización; `close` cierra los componentes y el almacenamiento.

`HostOptions::project` aporta un proyecto inicial. Los anfitriones embebidos pueden registrar varios dentro del techo de raíces canónicas, un decisor de recepción acotado opcional y un catálogo rápido ya autorizado. Son ajustes del anfitrión de confianza; JSON-RPC no sustituye ese techo. `control_actions` expone el adaptador de coordinación compartido; `recover_task_directives` procesa su bandeja durable de correcciones.

El adaptador síncrono de CLI implementa `daemon start` en primer plano y `daemon stop` autenticado; el dominio permanece en crates compartidos. [Daemon, maestro y subagentes](../../docs/development/daemon.es.md) detalla arranque, variables, contratos reales, límites de adaptadores nativos y comandos de verificación. Las pruebas no necesitan inferencia de producción ni despliegue de canales externos.
