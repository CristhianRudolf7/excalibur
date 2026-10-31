# Retención y borrado a petición

`excalibur-retention` es el servicio del núcleo compartido para la sección 4.7 de la arquitectura: borrado a petición del usuario de una conversación, un documento o un recuerdo, más las clases de retención por tiempo. Es una biblioteca. Aquí no se añaden subcomandos de la CLI, rutas HTTP ni el diálogo de confirmación (20.5); `MemoryStore::purge_memory`, que usa la llamada existente `memory.purge` de la puerta, ahora delega en este servicio.

## La purga como operación durable con clave

`PurgeService::open(principal, main, telemetry)` instala un esquema aditivo con suma de comprobación (`retention_schema` versión 1) en la base del principal. No se modifica ninguna tabla existente. `purge(PurgeRequest)` recibe una clave de operación, el ámbito, el tipo e ID del sujeto, un código de motivo y la hora de la solicitud. Ejecuta estas fases, y cada una confirma su avance en `retention_purges`:

1. **planned.** Se autoriza el sujeto y se registran los IDs de sus miembros, las agujas de búsqueda, el conjunto de ámbitos, las marcas de agua de rowid por tabla y los hashes de blobs. Cuentan como miembros los mensajes de la conversación, los fragmentos de todas las versiones del documento y los IDs que derivan los participantes.
2. **main_swept.** Se redactan las copias derivadas en la base del principal.
3. **journal_blobs_swept.** Se reescriben las cargas del registro de más de 64 KiB que viven en blobs.
4. **telemetry_swept.** Se barre la base de telemetría, se vuelven a sellar los desplazamientos y se reexportan los artefactos de las ejecuciones afectadas.
5. **base_deleted.** Una sola transacción del escritor hace varias cosas. Barre las filas escritas después de las marcas de agua, invoca los borrados de los participantes, elimina vectores, aristas y referencias a blobs, borra las filas base junto con toda fila que no puede existir sin ellas, reconstruye los índices FTS5 afectados, escribe la lápida y borra las agujas.
6. **blobs_released.** Los archivos de blob sin referencias y sus metadatos se eliminan en el acto, sin el margen de 24 horas de la recolección de basura. El contenido que otro dueño todavía referencia se conserva y se cuenta como `blobs_shared`.
7. **completed.** Se ejecuta `wal_checkpoint(TRUNCATE)` en ambas bases y se registran la higiene y el código del aviso de copias.

Repetir la misma clave, o el mismo sujeto con otra clave, devuelve el `PurgeReceipt` guardado. Reutilizar una clave para otro sujeto falla con `KeyConflict`. `resume_pending()` termina las operaciones interrumpidas, por ejemplo al arrancar el host. Cada fase es idempotente: las filas ya redactadas dejan de coincidir, la liberación de blobs acepta archivos que ya no existen y la fase base comprueba su fase dentro de la misma transacción.

El contenido necesario para encontrar copias (las agujas) solo se guarda en `retention_purge_needles` hasta que se borran las filas base, en la misma transacción. Una caída antes de ese punto reanuda con las mismas agujas. Una caída posterior ya no las necesita.

## Qué se elimina y dónde

- **Filas base.** Conversaciones con sus mensajes, documentos con sus versiones y fragmentos, y recuerdos. El borrado sigue las claves foráneas reales de `pragma_foreign_key_list`. Las filas que no pueden existir sin el sujeto se borran, por ejemplo `client_messages` y las tablas de buzón y turno del maestro. Las referencias anulables desde filas ajenas se vacían en su lugar. Por ejemplo, el `reply_to` de un mensaje ajeno pasa a NULL, y la lápida explica la referencia rota.
- **Copias derivadas.** Se recorre cada tabla normal de las bases del principal y de telemetría, salvo las siguientes:
  - contenido de sujetos independientes: otros mensajes, recuerdos, documentos, fragmentos y documentos de identidad;
  - metadatos estructurales (tablas de blobs, esquema, migraciones, índices vectoriales y códigos cortos);
  - la tabla encadenada por hash `audit`;
  - las tablas virtuales y sus tablas sombra.

  Esto cubre el registro durable, avisos, aprobaciones, claves y colas de memoria, eventos de telemetría, metadatos de ejecuciones y cualquier tabla que añadan otros crates. La coincidencia usa los textos exactos del sujeto de al menos 12 caracteres, las frases o líneas de al menos 24 caracteres y sus formas escapadas en JSON. Los valores JSON conservan su estructura: solo cambian los valores de texto (y las claves), y el reemplazo es `[purged]`. Si un valor redactado choca con una clave única, se usa en su lugar una marca propia de la fila.
- **Registro durable.** Las cargas se redactan en el sitio. No cambian la secuencia, el tipo, la clave de idempotencia, el estado ni el número de filas. Los disparadores de solo-añadir se eliminan y se recrean únicamente dentro de la transacción de purga. Los hashes guardados sobre eventos del registro se vuelven a sellar con la carga redactada, de modo que `Telemetry::reconcile` y la reobservación de tareas siguen siendo idempotentes: `telemetry_offsets.payload_hash` usa la nueva `excalibur_telemetry::journal_offset_hash`, y `task_observed_events.payload_hash` se actualiza cuando esa tabla existe.
- **Telemetría.** Las filas de `telemetry.db` se barren igual, y cada ejecución afectada se reexporta, de modo que los artefactos de `runs/<id>/` se regeneran a partir de los datos redactados.
- **Índices y vectores.** Se reconstruyen los índices FTS5 de contenido externo sobre las tablas modificadas. Esto cubre mensajes, recuerdos, fragmentos y claves de memoria, incluidos los índices trigrama, para que los términos borrados salgan de los segmentos y no queden solo sus marcas de borrado. Se borran las filas `vec0` cuyo ID de objeto es un ID de miembro o `<prefijo>:<id de miembro>`, y sqlite-vec 0.1.9 pone a cero los bytes del vector borrado en su almacenamiento por bloques. El participante de memoria también elimina las entradas de `memory_vector_objects` y `memory_index_queue`.
- **Participante de memoria.** `excalibur_memory::MemoryPurgeParticipant` se ocupa de las filas propias de la memoria. Elimina las claves extraídas (y sus embeddings) de mensajes y recuerdos purgados, las admisiones, transiciones, vínculos con entidades, necesidades de relación e historial de curación, cierra las preguntas abiertas y borra los registros de recuperación de fragmentos. La purga se rechaza con `ParticipantMissing("memory")` cuando el esquema de memoria está instalado y este participante no se registró.

## Lápida y aviso de copias

`retention_tombstones` guarda solo `id, principal_id, scope_id, subject_kind, subject_id, reason, purged_at`. `reason` es un código (`user_request`, `retention_policy`, `administrative`), nunca texto libre. `PurgeService::tombstone(scope, kind, id)` solo responde dentro del ámbito de quien consulta.

Cada recibo incluye `BackupNotice { code: "backups_retain_until_rotation", max_backup_age_days: 28 }`. Su mensaje indica que las copias existentes pueden conservar el dato hasta que roten. Las copias semanales de 4.6 se guardan cuatro semanas, y las diarias y previas a actualizaciones rotan antes. Las copias no se reescriben.

## Ámbito y principal

El servicio se abre para un principal, en línea con la disposición física de un archivo por principal. Un sujeto de otro ámbito o principal responde `NotFound`, igual que uno inexistente. Los barridos solo tocan filas cuyo `scope_id` es NULL o uno de los ámbitos del propio sujeto, y cuyo `principal_id` es NULL o el principal del servicio. Las copias con ámbito que pertenecen a otros ámbitos nunca se modifican.

## Clases de retención (4.7)

`RetentionSweeper::open(main, telemetry, RetentionPolicy::default(), clock)` recibe un reloj inyectado en segundos Unix. Los valores por defecto son 30 días de registro completo, 30 días de salidas de herramientas, 90 días de telemetría detallada y 7 días de instantáneas sombra. `run_once()` hace lo siguiente:

- Borra la telemetría detallada de más de 90 días, es decir, los eventos con sus desplazamientos, las trazas y las métricas. Conserva `metric_daily` y reexporta las ejecuciones afectadas.
- Informa como `journal_compaction_due` las ejecuciones terminadas cuyo registro tiene más de 30 días. Las ejecuciones retenidas con `hold_run(run, Evaluation | Replay | SkillCandidate | SkillEvaluation)` se informan en cambio como `journal_held`.
- Cuenta los blobs de carga del registro de las ejecuciones vencidas que ninguna evidencia, documento, recuerdo o mensaje referencia (`tool_output_blobs_due`).

## Higiene y límites honestos

- Cada escritura de purga se ejecuta con `PRAGMA secure_delete=ON`, de modo que las celdas y páginas liberadas se ponen a cero. **No** se aplica `VACUUM`. Renumeraría los rowid implícitos de los que dependen los índices FTS5 de contenido externo, y exige acceso exclusivo. Un `wal_checkpoint(TRUNCATE)` reinicia ambos archivos WAL cuando ningún lector retiene una instantánea, y el recibo informa `main_wal_truncated` y `telemetry_wal_truncated`.
- La garantía cubre las consultas ordinarias sobre todas las tablas, todas las tablas sombra y los archivos de blobs. El espacio libre que quedó dentro de páginas *antes* de la purga, por actualizaciones anteriores hechas sin secure_delete, puede conservar bytes antiguos. La prueba de purga imprime los bytes en crudo de la base y del WAL que encuentra, en lugar de hacer una aserción sobre ellos. En la ejecución registrada tampoco contenían marcadores, pero ese resultado depende de estos fixtures y no está garantizado. Los bloques del sistema de archivos, el remapeo de SSD y las copias previas que deja el reemplazo atómico de archivos (como los archivos de exportación antiguos) quedan fuera del alcance de SQLite.
- Las copias de seguridad conservan el contenido hasta que rotan. Una prueba demuestra que la copia previa a la purga todavía lo contiene.
- Los textos del sujeto de menos de 12 caracteres se borran de sus filas base, pero no se buscan en almacenes derivados. Así se evita reescribir valores estructurales cortos. No se detectan paráfrasis, resúmenes ni hechos derivados por el modelo que no sean copias literales. Los sujetos independientes (como otro recuerdo que afirma el mismo hecho) no se borran en cascada.
- Las filas derivadas sin ámbito (la mayoría de las filas del registro y de los eventos de telemetría no llevan ámbito) se emparejan por contenido. Por eso una frase larga compartida literalmente con un sujeto de otro ámbito también se redacta en esas copias sin ámbito. Las filas con ámbito de otros ámbitos nunca se tocan.
- La tabla encadenada por hash `audit` no se reescribe. Guarda eventos ya redactados.
- No se reescriben `durable_operations.seed_hash`, los blobs de versiones de archivos, los blobs de salida de evidencias ni otros blobs ajenos al registro. Reenviar la carga original de un evento del registro con una clave redactada falla como conflicto de clave, y volver a registrar los metadatos originales de una ejecución en telemetría se rechaza. Ambos casos fallan de forma segura.
- `MemoryStore::purge_memory` abre el servicio sin telemetría (`telemetry_covered=false`). Los hosts que tienen el manejador de telemetría deben usar `PurgeService` directamente para cubrirla.
- Las filas escritas en la telemetría entre su barrido y el borrado base no se vuelven a barrer. Las copias que los agentes en ejecución aún tienen en memoria pueden escribirse después de la purga.
- La compactación del registro, el borrado de salidas de herramientas a los 30 días y la caducidad de instantáneas sombra se informan, pero no se ejecutan. El formato de compactación y su dueño no se implementan aquí.
- La verificación solo cubre Linux. No se ha probado en macOS, Windows ni móvil el comportamiento de secure_delete, el borrado de archivos ni los puntos de control.

## Verificación

`cargo test -p excalibur-retention` ejecuta lo siguiente:

- **`purge`.** Siembra marcadores únicos en tablas base, índices FTS5 (incluido trigrama), vectores sonda `vec0`, blobs de adjuntos, documentos y registro, eventos del registro en línea, avisos, claves de memoria, una tabla hija con clave foránea, eventos y metadatos de ejecuciones de telemetría y archivos exportados de telemetría. Comprueba que cada categoría existe y después purga la conversación, el documento y el recuerdo. Una búsqueda exhaustiva lee entonces cada tabla y tabla sombra de ambas bases, más los archivos de blobs y de exportación en crudo y descomprimidos con zstd. No encuentra nada. FTS MATCH no encuentra nada, los bytes del vector sonda desaparecen, la lápida y el aviso están presentes y el replay es idempotente. Las secuencias, tipos y claves del registro no cambian, los hashes de reconciliación y de tareas se vuelven a sellar, la copia de seguridad conserva el dato y se mantiene el aislamiento por ámbito y principal.
- **`crash`.** Envía SIGKILL a un subproceso en cada una de nueve fronteras: planificado, tras un lote de barrido, base principal barrida, blobs del registro barridos, telemetría barrida, dentro de la transacción de borrado base sin confirmar, base borrada, blobs liberados y completado. Después reanuda con las mismas claves y comprueba la convergencia: ningún marcador, tres lápidas, `quick_check=ok` y disparadores de solo-añadir restaurados.
- **`policy`.** Recorre las clases de caducidad con un reloj manual.

`cargo test -p excalibur-memory` mantiene en verde las suites de memoria con la purga delegada. Los fixtures son sintéticos y locales.
