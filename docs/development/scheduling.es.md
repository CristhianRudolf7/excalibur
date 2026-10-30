# Base de temporizadores durables

`excalibur-scheduler` ofrece temporizadores UTC de una vez y de intervalo fijo por ámbito sobre el escritor compartido de almacenamiento. Registra atómicamente un disparo en una bandeja de entregas y avanza el temporizador. El ID de disparo codifica ámbito, ID de temporizador y hora UTC prevista; los reintentos y ticks concurrentes reutilizan ese ID. Los IDs de temporizador son inmutables: recrear la definición exacta funciona; cambiarla exige otro ID.

Los ticks de recuperación aplican `RunAll`, `RunOnceNow` o `Skip`. Un tick acotado de `RunAll` conserva el resto pendiente para ticks posteriores. `RunOnceNow` agrupa los intervalos perdidos en la última hora prevista y `Skip` avanza hasta la siguiente hora futura. Un disparo que vence exactamente al recuperar se entrega. Los ticks normales registran todas las ocurrencias vencidas, hasta el límite de la página.

Las entregas pendientes sobreviven al cierre y reapertura del almacenamiento. La confirmación es idempotente y está acotada al ámbito. Los consumidores deben usar el ID del disparo como clave externa de idempotencia: la bandeja por sí sola no garantiza un único efecto externo si el consumidor cae entre enviar y confirmar. El anfitrión autenticado aporta el ámbito; las cargas de las programaciones no pueden ampliarlo.

Verificación: `cargo test -p excalibur-scheduler` cubre políticas de recuperación, acumulación acotada, reapertura, ticks concurrentes, conflictos de definición, aislamiento de ámbitos y desbordamiento de fechas. Las pruebas son locales y deterministas.

Es preparación para H9, todavía sin despachador conectado al daemon. Quedan pendientes el cálculo de hora local IANA/cron, avisos por cambio de zona, dependencias/ventanas de actividad, ciclo de recordatorios y entrega por canales, modo seguro de arranque, cuarentena, retención y actualizaciones firmadas. H9 no está completo.
