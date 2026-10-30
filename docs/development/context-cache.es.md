# Contexto y caché

[English](context-cache.md)

La biblioteca compartida `excalibur-context` aporta compilación determinista y una `ContextSession` serializable. Antes de cada solicitud nueva al modelo, el motor prepara esa sesión desde el historial confirmado, los hechos de la tarea en el backend y las instrucciones literales. Las solicitudes confirmadas se reutilizan al recuperar. El registro conserva los recibos de preparación, enmascarado y compactación, junto con las referencias al historial completo. La telemetría proyecta un recibo compacto y el hash/puntero del estado; la sesión íntegra permanece en el diario principal.

## Capas y actualizaciones

L0 contiene instrucciones del rol, seguridad y herramientas núcleo ordenadas por nombre; L1, identidad/catálogo fijados; L2, ámbito estable; L3, tarea literal; L4, actividad y actualizaciones añadidas; L5, hora, presupuesto, continuación e informes transitorios. Las herramientas permanecen en el catálogo cuando se deshabilitan; la pasarela cambia `tool_choice`. Se sugieren hasta tres skills. El host proporciona recuerdos/preferencias seleccionados; la biblioteca no inicia un Recuperador de memoria.

`ContextSession::update_layer_changed` añade una actualización versionada sin reescribir la base estable. El motor lo aplica a requisitos, estados de criterios y plan micro vigentes. Los cambios de instrucciones literales tienen sus propias entradas versionadas. Una compactación vuelve a renderizar L2/L3 actuales una vez. Cada preparación registra hashes por capa, hash de prefijo estable, estimaciones y deriva; el detector informa como defecto un cambio de L0/L1 dentro de la sesión. Herramientas, formato de salida y razonamiento quedan fijados en la especificación de la ejecución.

`InstructionStore` es una API Rust del host sobre la tabla existente `instructions`, con metadatos aditivos comprobados, revisiones optimistas, historial inmutable y recibos. Conserva texto exacto, origen, principal, ámbito (general/proyecto/tarea/conversación) y estado activo/retirado. El modelo no registra instrucciones mediante herramientas. Los mensajes de origen deben pertenecer al mismo principal. Los vínculos explícitos de ejecución y delegación seleccionan instrucciones sin importar contenido ajeno. El motor detecta revisiones del registro y rechaza llamadas todavía no admitidas de respuestas anteriores. El verificador independiente recibe los mismos registros literales y revisiones del ámbito. La finalización adquiere una guarda compartida breve: una revisión distinta exige una respuesta nueva y las mutaciones del registro quedan ordenadas tras la transición terminal del diario. Ninguna guarda abarca inferencia. Los adaptadores del host clasifican los fragmentos duraderos; el contenido conversacional ordinario no obtiene autoridad automáticamente.

## Enmascarado y compactación

Las observaciones sin enmascarar que superan 24K tokens estimados disparan un lote hacia 12K. Las diez últimas, las fijadas y los errores recientes quedan literales; el conjunto protegido puede superar el objetivo. El vencimiento del TTL permite adelantar el lote. Las llamadas del agente permanecen intactas. Cada marcador incluye hash y referencia utilizable de blob para `get_tool_output`. Las observaciones JSON confirmadas, también las pequeñas, se archivan sin cambiar sus bytes originales; las referencias participan en el protocolo existente de GC y durabilidad. `history_search` es acotado y limitado a la ejecución actual, con consulta/cursor e intervalo opcional de secuencias.

El umbral es `min(500000, compaction_cap, floor(0.85 * model_window) - output_reserve)`, con techo de 500K que un perfil de modelo puede bajar. Las decisiones incluyen un margen del 10 %. `SessionPolicy` configura ventana, reserva de salida, TTL y ventanas recientes. Sus 128K predeterminados son un presupuesto de planificación configurado, no una afirmación sobre la capacidad anunciada de un modelo desconocido; el host debe indicar los límites reales del modelo elegido. `ExecOptions.context_policy` transmite la configuración al motor compartido y la semilla durable la conserva.

La compactación deriva del backend petición original literal, instrucciones activas, requisitos, estados frescos de criterios, plan, rutas/operaciones/hashes de artefactos, los últimos veinte comandos con su salida, texto exacto de errores abiertos, hijos y decisiones. Retiene los últimos 20K tokens de actividad sin partir grupos llamada/resultado. Las observaciones fijadas y los grupos indivisibles pueden superar el objetivo; el recibo lo indica y una vista que no cabe falla explícitamente. Un error de herramienta se considera resuelto solo tras éxito de la misma operación con argumentos iguales; no se infiere una resolución de cambios distintos. El registro conserva los comandos y resultados anteriores. No se trunca información obligatoria en silencio.

El extractor opcional, configurado mediante `Engine::with_context_extractor`, devuelve un resumen incremental estricto de solo el tramo descartado. El motor exige rol extractor y consumo acotado, registra admisión y finalización, respeta control/timeout, contabiliza uso y utiliza el estado estructurado si falta el extractor o hay fallo del proveedor, salida inválida o interrupción. Fusiona hallazgos sin reescribirlos y repara mecánicamente los ID abiertos ausentes. El máximo de una solicitud de extractor pendiente queda reservado y no permite repartirlo a hijos antes de reconciliarlo. Un recibo interrumpido se contabiliza conservadoramente y no se reenvía a ciegas. Resúmenes y observaciones citadas siguen siendo datos no confiables y no sustituyen estado ni instrucciones literales.

El modo conversación retiene 12K recientes y actualiza el resumen al superar 24K o al cruzar un límite de sesión por TTL vencido. Lo omitido sigue consultable. Este modo compartido está disponible para el [daemon y el maestro conversacional](daemon.es.md); la compilación del contexto sigue siendo independiente de canales y planificación de agentes.

## Tokenizadores, caché y comprobaciones

Los modelos con correspondencia conocida usan vocabularios locales integrados de `tiktoken-rs`. Los demás usan estimación por caracteres con corrección entera de media móvil, aprendida solo del uso de entrada conocido del proveedor, por modelo/sesión. El uso ausente o estimado no calibra. El margen del 10 % es una reserva de política, no una garantía universal de precisión sobre datos nuevos. Las pruebas sintéticas miden error en sus fixtures declarados.

Los [contratos de caché de proveedores](provider-cache.es.md) describen campos oficiales verificados, marcas/TTL de Anthropic, espacio de claves opaco de OpenAI, caché implícita y calentamiento acotado de hermanos tras el primer delta. Las claves derivan del hash de principal/rol/ámbito; no envían identidad personal literal. Los contadores observados de entrada, lectura y escritura se distinguen de la reutilización teórica. Los ausentes quedan desconocidos. El estado opaco nativo de reenvío se conserva en el historial y no se trata como instrucción.

```sh
cargo test -p excalibur-context
cargo test -p excalibur-models --test cache
cargo test -p excalibur-journal --test context_instructions
cargo test -p excalibur-engine --test context_compaction --test context_session_races
cargo test -p excalibur-telemetry --test context_summary
cargo fmt --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo build --workspace
```

Los fixtures backend usan datos sintéticos, HTTP/SSE local, SQLite/archivos reales e interrupción en subprocesos aislados. La suite de caché compara reutilización esperada con contadores devueltos por fixtures; no afirma tasas reales en proveedores ni ahorros de producción. La evidencia Linux no implica ejecución Windows/macOS. No requiere credenciales, inferencia pagada, GPU ni pruebas frontend.
