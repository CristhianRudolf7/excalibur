# Vigilante durable de ejecuciones atascadas

Este documento describe el vigilante de ejecuciones durables atascadas (arquitectura 17.6, "Latidos y vigilancia", y el vigilante de ejecuciones atascadas de 22.5). Cubre solo el latido del motor compartido y el vigilante del orquestador. No es el watchdog `sd_notify` del sistema operativo ni un vigilante de RAM, disco, WAL, errores de proveedor o deriva de caché. Esos siguen siendo requisitos separados.

## Latido

Al comenzar cada admisión del conductor y en cada frontera de paso de una ejecución activa, el motor renueva `runs.heartbeat_at`. Esta columna ya existía en el esquema base y es una proyección, no un hecho del registro, así que no cambian la reproducción, las revisiones ni la proyección de eventos. Solo se renueva una ejecución que está autoritativamente en `running`, de modo que un conductor obsoleto no puede refrescar una ejecución pausada, bloqueada o terminal.

La hora de referencia del vigilante es la más reciente entre ese latido, un hecho confirmado de avance del conductor (`run_started`, `model_request`, `model_dispatch`, `model_response`, `model_failure`, `model_interrupted`, `tool_intent`, `tool_result`, `step_completed`, `checkpoint`, `compaction`, `directive_applied`, `wake_event`) y una transición confirmada a `running`. Las directivas recibidas, los mensajes entre agentes y las notas de recuperación del host son escrituras externas y no cuentan, así que no pueden ocultar un conductor colgado.

## Detección

`Orchestrator::watchdog_runs(at)` recibe un reloj UTC explícito, y `Orchestrator::tick` lo llama con el reloj del host. Solo examina ejecuciones cuyo conductor está vivo en este orquestador y cuyo estado es `running`. Los estados de espera, incluidos `waiting(tool)`, la espera de hijos, la del usuario y la de aprobación, nunca se examinan. Una ejecución está atascada cuando `at - referencia >= 600` segundos.

Una herramienta pendiente registrada, es decir, un `tool_intent` confirmado sin resultado, cuenta como espera larga declarada hasta la hora de su intención más el timeout de herramientas de la ejecución. El reloj de diez minutos vuelve a empezar en ese plazo.

Antes de la interrupción se escribe una fila de intención del host en `orchestration_run_watchdog`, con clave por ejecución y revisión observada del registro, dentro de una transacción que vuelve a comprobar la revisión y el latido. Después, el motor aplica un CAS de revisión exacta mediante `Engine::interrupt_stalled_revision`, sin tomar el candado de la ejecución, que un conductor colgado podría retener. Un hecho más reciente en el registro, un control del usuario o una transición terminal hacen fallar el CAS. Los efectos pendientes se evalúan en esa misma revisión:

- con una intención no pura sin resolver, la ejecución pasa a `blocked` con el motivo `watchdog: unresolved effect requires reconciliation`;
- en otro caso pasa a `paused` con el motivo `watchdog_stalled`.

El host también puede interrumpir una herramienta gestionada, sin garantía de éxito.

## Interrupción, reconciliación y decisión

Normalmente, un conductor vivo observa el control durable y se detiene por sí mismo. Una llamada al modelo colgada registra su interrupción y una contabilidad conservadora. Si la tarea del conductor sigue viva 5 segundos después de la detección según el reloj del vigilante, se aborta. La cancelación de la tarea no se trata como un fallo de la ejecución. Si 60 segundos después todavía no se ha detenido (por ejemplo, porque está atascada en código síncrono que no se puede cancelar), el incidente se informa sin reanudación automática.

Cuando el conductor antiguo ha desaparecido, la decisión reutiliza la evaluación de arranque de 17.3 (`assess_resume`, compartida con `recover_startup`). Una reanudación automática requiere todo lo siguiente:

- autonomía del host y autonomía congelada de la ejecución al menos autónomas;
- presupuesto restante suficiente;
- ninguna intención no pura sin resolver;
- ninguna aprobación pendiente;
- ninguna edición de archivos registrada;
- ninguna solicitud al modelo sin respuesta durable;
- ninguna reserva de hijo pendiente;
- ni recuperación de arranque ni modo seguro activos;
- ninguna cuarentena;
- ningún incidente anterior del vigilante que ya haya reanudado esta ejecución.

Una ejecución elegible recibe una `recovery_note` para su contexto L5 y pasa de `paused(watchdog_stalled)` a `ready` con el CAS con clave `watchdog-resume:<revisión>`. Después, el planificador ordinario la vuelve a admitir y el motor reconcilia sus herramientas puras pendientes como indica 17.2. En cualquier otro caso se mantiene la pausa y los impedimentos quedan registrados como motivo.

Una ejecución bloqueada mantiene el bloqueo hasta la reconciliación explícita existente mediante `resolve_uncertain`. El vigilante nunca ejecuta ni reintenta un efecto. Si un control posterior del usuario o una transición del motor se adueña de la ejecución, el incidente queda registrado como `superseded`.

## Alertas e idempotencia

Cada incidente con interrupción produce una única alerta en la tabla ordinaria `orchestration_notifications`, confirmada en la misma transacción que el recibo `notified` del incidente. La canalización de entrega del maestro la dirige a la conversación de origen del agente. La carga tiene `kind: "run_watchdog"`, la ejecución, la revisión, la acción, la decisión, el motivo, el ámbito y la conversación. Un resultado bloqueado o pausado es `urgent`, y uno reanudado es `normal`. El aviso de parada ordinario que el recolector existente emite cuando un conductor se detiene por sí mismo es independiente.

Las pasadas de un mismo proceso se serializan. Las de otros procesos o de bases de datos reabiertas son idempotentes gracias a:

- la fila de intención con clave;
- las claves de CAS de revisión exacta `watchdog-stalled:<n>` y `watchdog-blocked:<n>`;
- la reanudación con clave `watchdog-resume:<n>`;
- `UPDATE ... WHERE decision IS NULL`;
- los identificadores deterministas de las notificaciones.

Si un proceso se detiene después del CAS, el incidente continúa en la pasada siguiente a partir de la clave confirmada en el registro. La recuperación de arranque conserva `paused(watchdog_stalled)` como control existente. Una fila de intención cuyo CAS nunca se aplicó queda como `not_applied` y no genera alerta. Las alertas exigen tanto la fila del host como la clave confirmada del cambio de estado en la revisión `n + 1`.

Esquema: v1 (`run_watchdog.sql`, del punto de control no verificado `d203232`) se mantiene sin cambios. La v2 aditiva (`run_watchdog2.sql`) añade una sola vez `action`, `decision`, `reason` y `aborted`, y se verifica con checksum. La tabla de cursor de v1 no se usa.

## Verificación

```sh
cargo test -p excalibur-orchestrator --test run_watchdog
cargo test -p excalibur-orchestrator --test startup_recovery
cargo test -p excalibur-engine --test crash --test end_to_end
```

La suite ejecuta conductores reales del motor con modelos y herramientas simulados locales, una base de datos SQLite temporal real y un reloj del vigilante inyectado. Ninguna prueba espera diez minutos reales ni llama a APIs de pago. Cubre:

- el límite de 599/600 segundos y la renovación del latido;
- la interrupción cooperativa de una llamada al modelo colgada, con alerta urgente;
- pasadas concurrentes y la reapertura de la base de datos sin interrupciones ni alertas duplicadas;
- el aborto de un conductor colgado fuera de cualquier punto de control, seguido de una única reanudación que termina a partir de la respuesta del modelo ya registrada;
- la exención de una herramienta pendiente registrada hasta su timeout;
- el bloqueo de un efecto no idempotente incierto sin volver a ejecutarlo;
- el control del usuario y una revisión obsoleta prevaleciendo sobre el vigilante;
- el esquema aditivo verificado con checksum.

## Límites

- La detección cubre las ejecuciones despachadas por el orquestador cuyo conductor vive en el proceso actual. No se vigilan los turnos conversacionales del maestro ni las ejecuciones que están en `running` pero solo esperan admisión en la cola.
- Tokio no puede cancelar un conductor bloqueado en código síncrono. Se informa del caso, pero el conductor sigue en el proceso.
- La interrupción de herramientas depende del ciclo de vida de herramientas y del adaptador del host existentes. Aquí no se inventarían los procesos huérfanos del sistema operativo.
- La reanudación automática es conservadora: las ejecuciones con ediciones de archivos registradas se quedan pausadas hasta una recuperación explícita. Una llamada al modelo interrumpida reserva de forma conservadora su cargo desconocido del proveedor. Sin un límite de llamada declarado, eso puede agotar el presupuesto de tokens y dejar la ejecución pausada, como en la prueba del modelo colgado.
- Las alertas van al maestro y no a canales externos. Los demás vigilantes de salud siguen sin implementarse, y H9 no está completo.
