# Memoria y documentos

[English](memory.md)

`excalibur-memory` es una biblioteca compartida para la memoria a largo plazo por ámbitos, el Recuperador, la consolidación en segundo plano, los espacios de embeddings, los documentos de identidad y la biblioteca de documentos con citas deterministas. Instala un esquema aditivo con suma de comprobación sobre la base SQLite principal y mantiene cada consulta dentro de ámbitos explícitos. Los roles que usan modelos (extractor, decisor, clasificador barato, reordenador, reescritor de consultas, compactador de notas, extracción con visión, transcripción, frases de contexto) son traits que aporta el anfitrión; la biblioteca no inicia ningún modelo, servicio de red ni daemon.

La biblioteca todavía no está conectada al maestro conversacional, al runtime de herramientas del motor ni al compilador de contexto. Esos puntos de conexión se enumeran al final de esta página.

## Concesiones y ámbitos

Cada punto de entrada recibe un `MemoryGrant`: principal, entre 1 y 64 ámbitos explícitos, ejecución, conversación y tarea opcionales, zona horaria del usuario y un límite de confianza. Los ámbitos pedidos deben estar contenidos en la concesión; una concesión vacía se rechaza. Los candidatos léxicos, semánticos, relacionales y temporales se filtran por principal y ámbito en SQL y se comprueban otra vez antes de devolverlos. Los vectores viven en tablas `vec0` particionadas por ámbito. Las relaciones y el recorrido del grafo solo siguen aristas de los ámbitos pedidos. Los prompts de consolidación contienen turnos de un único ámbito y se registra el conjunto de ámbitos de cada prompt.

## Registros y admisión

Un recuerdo tiene tipo (`fact`, `preference`, `event`, `commitment`, `belief`, `relation`, `procedure`), enunciado, entidades, atributo opcional, tiempo de validez (`valid_from`, `valid_to`), tiempo de registro (`recorded_at`, `invalidated_at`), caducidad opcional, estado (`candidate`, `active`, `expired`, `superseded`, `disputed`, `retired`), confianza, importancia, fuentes y confianza de la fuente (`user`, `agent`, `untrusted_external`). Los enunciados pasan por el redactor de secretos compartido.

- `memory_write` guarda un recuerdo explícito y lo admite de inmediato.
- `memory_propose` guarda un candidato con su fuente; solo es recuperable en la conversación o tarea de origen hasta que la consolidación lo admite.
- La admisión compara un candidato con los registros del mismo ámbito que comparten entidad, coinciden léxicamente o son vecinos vectoriales. Una similitud ≥ 0,95 con entidad compartida fusiona la procedencia en el registro existente. Entre 0,80 y 0,95 clasifica el decisor y después el clasificador barato; si no hay ninguno, el registro queda como candidato. Un refinamiento reemplaza al registro anterior y cierra su validez. Misma entidad y atributo con otro enunciado es una contradicción: con un cambio temporal explícito (indicador o expresiones como "ahora" o "ya no") el nuevo reemplaza al anterior; si no, ambos quedan en disputa y se encola una pregunta de baja prioridad.
- El contenido de páginas externas se marca `untrusted_external`; una preferencia de esa fuente se guarda como hecho y nunca se propone para el perfil del usuario.
- Las actualizaciones nunca destruyen registros; se conserva cada cambio de estado. `purge_memory` es la única operación destructiva y elimina contenido, claves, vectores y detalle de admisión a petición del usuario.

## Recuperación

`retrieve` hace un análisis determinista de la consulta: fechas y expresiones relativas en inglés y español resueltas en la zona horaria del usuario, nombres y alias de entidades conocidas como semillas relacionales, frases entre comillas e identificadores como coincidencias obligatorias y señales de consulta histórica. "Qué era cierto en X" usa el tiempo de validez; "qué creíamos en X", el de registro. Se generan cuatro listas de hasta 50 candidatos: léxica (BM25 de FTS5 con plegado de tildes, más índices trigram para identificadores, sobre recuerdos, mensajes, claves enriquecidas y fragmentos de documentos), semántica (KNN en cada espacio de embeddings vivo), relacional (uno o dos saltos) y temporal. La fusión Reciprocal Rank Fusion usa k = 60. La caducidad se aplica en la lectura; los registros reemplazados, caducados y retirados solo aparecen en consultas históricas. Las vidas medias de decaimiento son 7 días para eventos, 90 para preferencias y compromisos y 365 para el resto; importancia, confianza y fuente ajustan la puntuación. Un reordenador opcional reordena líderes muy próximos y se usa su orden directamente. Los avisos indican evidencia insuficiente, contradicciones, disputas, ambigüedad entre ámbitos, búsqueda semántica no disponible y reescritura de la consulta. Solo hay una reescritura barata cuando no hay resultados. El empaquetado agrupa por ámbito con id, enunciado, validez y fuente dentro de un presupuesto de tokens. Los hechos extraídos se indexan como claves adicionales que apuntan a su turno original.

`history_search` pagina mensajes pasados por texto, fechas, conversación, ámbito y rol.

## Embeddings

`EmbeddingProvider` es la interfaz común para embeddings locales y remotos. Los vectores se truncan a 512 dimensiones, se normalizan y se guardan en int8; el texto original se conserva siempre. Los espacios se versionan en `embedding_spaces`. Si un proveedor no está disponible, las escrituras entran en una cola de indexado y la recuperación sigue con las listas léxica, temporal y relacional informando de la degradación. `begin_migration` crea un espacio nuevo y encola cada objeto indexado; las lecturas usan ambos espacios hasta que `finish_migration` comprueba que no falta nada y retira el espacio anterior. `LazyLocalEmbedder` carga un modelo local en el primer uso y lo libera tras 10 minutos de inactividad; un fallo de carga se convierte en indexado encolado. `EmbeddingProfile` describe los modelos por defecto; los runtimes de modelos locales no se enlazan en este crate.

## Documentos de identidad y bloc de notas

El perfil del agente, el perfil del usuario, el bloc de notas y las personas se guardan como fragmentos con ruta de encabezados, orden, texto, versión y hash. `doc_read` devuelve el índice de encabezados, una sección o los fragmentos que coinciden. `doc_edit` reemplaza, inserta o borra por id de fragmento y hash esperado; si el hash no coincide no cambia nada y se devuelve el fragmento actual, y el segundo desajuste seguido del mismo actor sobre el mismo fragmento pide escalar. Cada versión se conserva y puede restaurarse. Los cambios de perfil exigen el mensaje que los motiva; salvo que el usuario los pida explícitamente, quedan como propuestas y se aplican cuando la evidencia repetida llega a dos fuentes. Las entradas del bloc pertenecen a un ámbito. Las personas copian el perfil del agente y no tocan la memoria del usuario. `agent_core` devuelve una instantánea acotada y con hash para un prefijo de prompt estable.

## Consolidación

`close_segment` extrae y admite los turnos nuevos de un ámbito (los 30 minutos de inactividad se detectan con `segment_idle`). `plan_review` usa un modo ligero durante los primeros 5 días con actividad (03:30 local tras 2 horas de inactividad) y después elige la ventana contigua más tranquila de un histograma horario de 14 días con mensajes del usuario y actividad informada por el anfitrión, recalculado semanalmente. `run_review` trabaja por ámbito con un presupuesto de tokens, guarda un punto de control tras cada lote, cede cuando la señal de actividad indica al usuario y retoma desde su punto de control. Sus pasos son extracción y admisión, caducidad, verificación de procedencia, resolución de contradicciones (gana la marca temporal explícita más reciente; si no, disputa y pregunta), enlaces deterministas a entidades, compactación del bloc con línea de tiempo de solo añadir (se rechazan reescrituras que eliminen más del 25 % de las entradas), propuestas de perfil a partir de preferencias repetidas del usuario, candidatos de aprendizaje de skills y un informe guardado. Los tipos de relación aprendidos se proponen tras 5 hechos sin tipo adecuado en 28 días y pasan a activos tras un uso consistente o la confirmación del usuario; los tipos pueden fusionarse.

## Biblioteca y citas

`library_ingest` pasa los archivos por un `IngestGuard`, guarda el original como blob versionado y extrae texto: de forma nativa para texto y Markdown, mediante el contrato `VisionExtractor` para PDF e imágenes y mediante el contrato `Transcriber` para audio. Los formatos Office y el vídeo informan que no están soportados hasta conectarse. Los fragmentos conservan documento, versión, página, párrafo, desplazamientos de caracteres, ruta de secciones, rango de líneas o rango temporal. Los documentos marcados como biblioteca reciben una frase de contexto única para los fragmentos ambiguos. `library_search` devuelve fragmentos citables y los registra para la ejecución que llama. `validate_citations` resuelve `[[doc:ID#cN]]` contra los fragmentos recuperados por la ejecución que responde o sus descendientes y los presenta como "Título, p. 12, párr. 3", o sección, párrafo y líneas con la indicación explícita "sin paginación", o un rango temporal. Las citas inválidas se eliminan y la frase se marca sin fuente. Las fuentes web se construyen solo con páginas registradas mediante `record_source_read` en el mismo árbol de la tarea y citadas en la respuesta.

## Herramientas

`memory_tool_definitions` y `call_memory_tool` exponen `memory_write`, `memory_propose`, `memory_search`, `history_search`, `doc_read`, `doc_edit`, `library_ingest` y `library_search`. Los argumentos pueden restringir los ámbitos pero nunca ampliarlos, la confianza está limitada por la concesión y es el anfitrión quien indica si una edición de perfil fue pedida explícitamente.

## Verificación

```sh
cargo test -p excalibur-memory
cargo test -p excalibur-memory --test mem_scope --test mem_time --test mem_conflict --test mem_cite
cargo fmt --check
cargo clippy --workspace --all-targets -- -D warnings
```

`mem_scope`, `mem_time`, `mem_conflict` y `mem_cite` cubren el aislamiento, las respuestas bitemporales y la caducidad, la admisión y los conflictos, y las citas. Otras suites cubren recuperación, embeddings, documentos de identidad y consolidación. Las pruebas usan bases SQLite temporales y dobles locales deterministas de `excalibur_memory::testing`; no llaman a modelos ni a la red. No se han ejecutado LongMemEval-S ni el subconjunto de conflictos de MemoryAgentBench.

## Conexiones pendientes y límites

- Maestro y motor: registrar las herramientas de memoria en el runtime de herramientas, vincular concesiones a conversaciones, ejecuciones y tareas, cerrar segmentos desde el maestro y llamar a `validate_citations` antes de enviar una respuesta.
- Compilador de contexto: el núcleo del perfil del agente en la capa 1, las preferencias pertinentes y el contexto del proyecto en la capa 2 y la recuperación automática al iniciar una tarea.
- Planificador: ejecutar `plan_review` y `run_review` en el carril P3 en segundo plano con la actividad real del usuario, las API por lotes de los proveedores y los roles de modelo del enrutador.
- Seguridad: la inspección completa de archivos de la capa de seguridad detrás de `IngestGuard`; el guardián incluido solo comprueba tamaño, firma de tipo y UTF-8.
- Árbol de ejecuciones: `TaskRunLinkTree` lee los enlaces padre de las tareas; la vista del árbol de agentes del daemon aún debe adaptarse a `RunTree`.
- No implementado: runtimes locales ONNX/candle, índice HNSW externo por encima de 500 000 vectores por ámbito, extracción de Office y vídeo, y filtros de herramienta y estado en `history_search`.
