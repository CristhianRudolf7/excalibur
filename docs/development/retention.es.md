# Retención y borrado a petición

`excalibur-retention` es el servicio del núcleo compartido para la sección 4.7 de la arquitectura: borrado a petición del usuario de una conversación, un documento o un recuerdo, más las clases de retención por tiempo. Es una biblioteca. Aquí no se añaden subcomandos de la CLI, rutas HTTP ni el diálogo de confirmación (20.5).

`MemoryStore::purge_memory` delega en este servicio; lo usa la llamada existente `memory.purge` de la puerta. El host del daemon entrega a la memoria su manejador de telemetría y llama a `resume_pending_purges` al arrancar.

## La purga como operación durable con clave

`PurgeService::open(principal, main, telemetry)` instala un esquema aditivo con suma de comprobación (`retention_schema` versión 1) en la base del principal. El registro gana una tabla aditiva `journal_payload_redirects` (`durable_schema` versión 2), y la telemetría gana `telemetry_expired` (`telemetry_schema` versión 2). No se modifica ninguna tabla existente.

`purge(PurgeRequest)` recibe una clave de operación, el ámbito, el tipo e ID del sujeto, un código de motivo y la hora de la solicitud.

**Planificación.** La planificación es una sola orden del escritor y no modifica nada hasta superar todas las comprobaciones:
1. Autoriza el sujeto y enumera sus miembros: los mensajes de la conversación, los fragmentos de todas las versiones del documento y los IDs que derivan los participantes.
2. Encuentra las **ejecuciones vinculadas** del sujeto (ver más abajo).
3. Rechaza la purga con `ActiveRun` mientras alguna ejecución durable vinculada no esté `completed`, `failed` o `cancelled`.
4. Valida estáticamente el grafo de claves foráneas y ejecuta un borrado de prueba acotado (hasta 16 miembros más la fila del sujeto) dentro de un punto de guardado que luego revierte. Un fallo devuelve `Rejected` y no registra nada.
5. Solo entonces registra las agujas, los miembros, los IDs de objetos vectoriales, las ejecuciones vinculadas y los hashes de blobs.

**Fases.** Cada fase confirma su avance en `retention_purges`:

1. **swept.** Se redactan, en lotes acotados del escritor, las filas de la base del principal que pertenecen al sujeto. Esto incluye las cargas del registro en línea, los blobs de carga del registro, y otras filas de ejecuciones vinculadas o filas que referencian a un miembro.
2. **base_deleted.** La primera orden vuelve a descubrir las ejecuciones vinculadas y bloquea las activas. Si aparecen nuevas ejecuciones terminadas, la purga vuelve al barrido. Esa misma orden ejecuta después los borrados de los participantes y elimina vectores (por búsqueda de clave), aristas y referencias a blobs. Los miembros se borran luego en órdenes de como mucho 64 filas, y una orden final borra la fila del sujeto. Cada borrado elimina también las filas que no pueden existir sin las borradas. Las referencias anulables desde filas ajenas pasan a NULL.
3. **telemetry_swept.** Una segunda pasada sobre la base del principal cubre las filas escritas o cambiadas después del primer barrido. Después se redactan las filas de telemetría de las ejecuciones vinculadas, se vuelven a sellar los desplazamientos de origen en el registro y se reexportan las ejecuciones.
4. **finalized.** Se escribe la lápida y se borran las agujas y los registros de hashes esperados.
5. **blobs_released.** Los archivos de blob sin referencias y sus metadatos se eliminan en el acto. El contenido que otro dueño todavía referencia se conserva (`blobs_shared`).
6. **completed.** Se ejecuta `wal_checkpoint(TRUNCATE)` en ambas bases.

**Replay y reanudación.** Repetir la misma clave, o el mismo sujeto con otra clave en el mismo ámbito, devuelve el recibo guardado. Una clave que nombra a otro sujeto en el mismo ámbito falla con `KeyConflict`. Una clave de otro ámbito o principal responde `NotFound`, igual que un sujeto inexistente.

Una fase que falla registra un `last_error` sin contenido y deja la purga reanudable. `resume_pending()` lleva adelante cada purga pendiente por separado y devuelve un `PendingOutcome` por clave, de modo que una purga atascada nunca bloquea a las demás. `resume(key, scopes)` reanuda una sola purga visible; `purge_memory` la usa tras una caída posterior al borrado de la fila del recuerdo.

## Qué pertenece a un sujeto

La redacción nunca busca coincidencias de contenido en toda la base. Una fila solo se redacta cuando se puede demostrar que pertenece al sujeto:

- **Ejecuciones vinculadas.** Una ejecución está vinculada al sujeto en cualquiera de estos casos:
  - se declaró con `PurgeService::link_run(kind, id, run)`;
  - una tabla lleva un `run_id` junto a una referencia a un miembro (`conversation_id`, `message_id`, `document_id`, `chunk_id`, `memory_id`), por ejemplo `memory_run_bindings` o `chunk_retrievals`;
  - está ligada al `origin_conversation` de un recuerdo o es una ejecución de su `origin_task`;
  - es hija de una ejecución vinculada, por `memory_run_bindings.parent_run_id` o por una asignación de presupuesto hijo en el registro.

  Sus filas del registro, sus blobs de carga, sus filas de telemetría y otras filas que llevan su `run_id` son candidatas.
- **Referencias a miembros.** Las filas cuya columna de referencia a un miembro contiene uno de los IDs de miembro del sujeto también son candidatas.
- **Nunca candidatas.** Las filas de otras ejecuciones, las filas heredadas sin vínculo y las filas de otros ámbitos nunca se tocan, aunque contengan el mismo texto. Las tablas con ámbito exigen además coincidencia exacta de ámbito (los nombres de ámbito nunca se comparan por subcadena; los NULL antiguos quedan con sujetos sin ámbito).

Dentro de las filas candidatas solo se reescriben las columnas de texto que no son identificadores, claves, hashes, enumeraciones, marcas de tiempo ni parte de una clave primaria, índice o clave foránea. Nunca se reescriben:
- el contenido de sujetos independientes: otros mensajes, recuerdos, documentos, fragmentos y documentos de identidad;
- las tablas de contenido de índices FTS5 de contenido externo;
- los metadatos estructurales, incluidos la tabla encadenada por hash `audit`, `durable_operations` y `task_observed_events`.

**Coincidencia.** Se usan los textos exactos del sujeto de al menos 12 caracteres, las frases o líneas de al menos 24 caracteres y sus formas escapadas en JSON. No distingue mayúsculas de minúsculas en ASCII.

**JSON.** Los valores que se interpretan como JSON se redactan solo en sus hojas de texto (reemplazo `[purged]`), de modo que el documento sigue siendo JSON válido. Una coincidencia situada en la estructura JSON (un objeto, claves o números) se deja en su sitio, se cuenta en `structured_matches` y se lista con `PurgeService::flags(key)`. El texto que parece JSON pero no se puede interpretar nunca se reescribe. El texto plano se reemplaza directamente.

## Registro, telemetría y otros almacenes

- **Registro.** Las cargas se redactan en el sitio. No cambian la secuencia, el tipo, la clave de idempotencia, el estado ni el número de filas, y los disparadores de solo-añadir se levantan únicamente dentro de la transacción de purga. Un blob de carga redactado recibe una nueva dirección de contenido; `journal_payload_redirects` asocia el hash antiguo, de modo que `Journal::context_observation(run, old_hash)` sigue resolviendo las observaciones de contexto archivadas.
- **Resellado.** Los hashes guardados sobre eventos del registro solo se vuelven a sellar si el hash guardado aún coincide con el contenido previo a la purga. Si no, el hash se deja como estaba, se cuenta en `hash_mismatches` y se registra una marca `task_hash_mismatch` o `telemetry_hash_mismatch`. Esto cubre `task_observed_events.payload_hash` y `telemetry_offsets.payload_hash` (vía `excalibur_telemetry::journal_offset_hash`).
- **Telemetría.** Se redactan las filas de las ejecuciones vinculadas en `telemetry.db`, y se reexporta cada ejecución vinculada con telemetría, de modo que los artefactos de `runs/<id>/` se regeneran.
- **FTS5.** Los borrados de la purga se ejecutan con `secure-delete` de FTS5 activado en cada índice de contenido externo. Las entradas salen del índice en el acto, sin una reconstrucción completa, y la opción se restaura después. Requiere SQLite 3.42 o posterior, que es la biblioteca incluida.
- **Vectores.** Las filas `vec0` se borran por clave para cada ID de miembro y `<prefijo>:<id>` (`memory`, `message`, `msg`, `chunk`, `key`, `document`, `conversation`), y sqlite-vec 0.1.9 pone a cero los bytes borrados.
- **Participante de memoria.** `excalibur_memory::MemoryPurgeParticipant` se ocupa de las filas propias de la memoria:
  - elimina las claves extraídas y sus embeddings, las admisiones, transiciones, vínculos con entidades, necesidades de relación, el historial de curación y los registros de recuperación de fragmentos;
  - cierra las preguntas abiertas.

  La purga se rechaza con `ParticipantMissing("memory")` cuando el esquema de memoria está instalado y este participante no se registró.

## Lápida y aviso de copias

`retention_tombstones` guarda solo `id, principal_id, scope_id, subject_kind, subject_id, reason, purged_at`. `reason` es un código (`user_request`, `retention_policy`, `administrative`). `PurgeService::tombstone(scope, kind, id)` solo responde dentro del ámbito de quien consulta.

Cada recibo incluye `BackupNotice { code: "backups_retain_until_rotation", max_backup_age_days: 28 }`: las copias existentes pueden conservar el dato hasta que roten (4.6). Las copias no se reescriben.

## Clases de retención (4.7)

`RetentionSweeper::open(main, telemetry, RetentionPolicy::default(), clock)` recibe un reloj inyectado en segundos Unix. Los valores por defecto son 30 días de registro completo, 30 días de salidas de herramientas, 90 días de telemetría detallada y 7 días de instantáneas sombra. `run_once()` hace lo siguiente:
- Borra la telemetría detallada de más de 90 días: los eventos con sus desplazamientos, las trazas y las métricas. Conserva `metric_daily` y reexporta las ejecuciones afectadas. También registra una marca de agua por ejecución en `telemetry_expired`, para que `Telemetry::reconcile` no vuelva a ingerir los eventos del registro caducados.
- Informa las ejecuciones terminadas cuyo registro tiene más de 30 días (`journal_compaction_due`), salvo las retenidas con `hold_run` (`journal_held`).
- Cuenta los blobs de carga del registro de esas ejecuciones que nada referencia (`tool_output_blobs_due`).

## Higiene y límites honestos

- **secure_delete, sin VACUUM.** Cada escritura de purga se ejecuta con `PRAGMA secure_delete=ON`. No se aplica `VACUUM`: renumeraría los rowid implícitos de los que dependen los índices FTS5 de contenido externo, y exige acceso exclusivo. El espacio libre que dejaron dentro de páginas actualizaciones anteriores sin secure_delete puede conservar bytes antiguos. La prueba de purga imprime las coincidencias en crudo de la base y del WAL en lugar de hacer una aserción sobre ellas; en la ejecución registrada no hubo ninguna, pero no está garantizado.
- **Fuera de SQLite.** Los bloques del sistema de archivos, el remapeo de SSD, las copias antiguas que deja el reemplazo atómico de archivos y las copias de seguridad quedan fuera del alcance de SQLite.
- **Qué no detecta la coincidencia.**
  - No se detectan el plegado de mayúsculas y la normalización Unicode (por ejemplo, acentos compuestos o descompuestos), los cambios de espacios o de saltos de línea, los fragmentos truncados ni codificaciones como base64.
  - Los textos de menos de 12 caracteres se borran de sus filas base, pero no se buscan en otros sitios.
  - No se detectan paráfrasis ni hechos derivados por el modelo, y los sujetos independientes no se borran en cascada.
- **Qué no cubre el vínculo.** Las copias en ejecuciones sin vínculo de procedencia con el sujeto no se redactan, por diseño (RP-1): la purga no puede demostrar que pertenecen al sujeto. Los hosts deben vincular las ejecuciones que inician en nombre de un sujeto.
- **La estructura se informa, no se reescribe.** Las coincidencias dentro de la estructura JSON se quedan en su sitio y se informan para su revisión.
- **No cubiertos:**
  - la tabla encadenada por hash `audit`, `durable_operations.seed_hash`, y los blobs de salida de evidencias, de versiones de archivos y de instantáneas de herramientas;
  - la base separada del registro del decisor y `codegraph.db`;
  - la purga de proyectos, un sujeto de 4.7 que no está implementado.
- **Efectos de fallo seguro.** Reenviar la carga original de un evento del registro con una clave redactada falla como conflicto de clave, y volver a registrar los metadatos originales de una ejecución en telemetría se rechaza. Reconciliar la telemetría mientras una purga está entre sus fases de registro y telemetría informa un conflicto de identidad hasta que la purga termina.
- **No se ejecuta.** La compactación del registro, el borrado de salidas de herramientas a los 30 días y la caducidad de instantáneas sombra se informan, pero no se ejecutan.
- **Plataforma.** La verificación solo cubre Linux.

## Verificación

`cargo test -p excalibur-retention` ejecuta lo siguiente:

- **`purge`.** Siembra marcadores únicos en tablas base, índices FTS5 (incluido trigrama), vectores sonda `vec0`, blobs de adjuntos, documentos y registro, una observación de contexto archivada, copias del registro en línea y en mayúsculas, avisos, evidencias, claves de memoria, una tabla hija con clave foránea, eventos y metadatos de ejecuciones de telemetría y archivos exportados. Cada sujeto está vinculado a su ejecución por una procedencia real. Tras purgar la conversación, el documento y el recuerdo, una búsqueda exhaustiva en cada tabla, tabla sombra y archivo en crudo o descomprimido con zstd no encuentra nada. También comprueba FTS MATCH, los bytes del vector puestos a cero, las lápidas, el aviso, el replay, los hashes resellados, la redirección de la observación de contexto y la copia de seguridad. La suite incluye además estas pruebas de regresión:
  - `rp1`: el registro, la telemetría, las evidencias y los avisos del ámbito B y de una ejecución heredada sin vínculo quedan idénticos byte a byte pese a compartir frases;
  - `rp2`: un mensaje con forma de JSON y un mensaje que es un ID de ejecución mantienen válidas todas las cargas e intactos todos los IDs;
  - `rp3`: `resume_pending` aísla una purga que falla;
  - `rp4`: órdenes de borrado base acotadas, con borrados FTS incrementales;
  - `rp6`: el bloqueo por ejecuciones activas y las filas tardías;
  - `rp8`: `purge_memory` se reanuda tras una caída y cubre la telemetría;
  - `rp9`: los hashes manipulados se marcan, no se vuelven a sellar.
- **`crash`.** Envía SIGKILL en diez fronteras, incluida la transacción final de borrado base sin confirmar, y después reanuda con las mismas claves y comprueba la convergencia.
- **`policy`.** Recorre las clases de caducidad con un reloj manual y comprueba que la telemetría caducada no se vuelve a ingerir.

`cargo test -p excalibur-memory`, `cargo test -p excalibur-gateway --test shared_backend` y `cargo test -p excalibur-daemon --test memory_host` cubren la purga de memoria delegada y la conexión con el host. Los fixtures son sintéticos y locales.
