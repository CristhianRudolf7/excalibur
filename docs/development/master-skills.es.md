# Sesiones de skills del maestro por conversación

El daemon compone `MasterSkillFastActions` por fuera de memoria y del adaptador autorizado de herramientas rápidas. Su delegado de esquemas solo ve definiciones `FastActions`. No dispone del runtime de trabajadores, herramientas directas de archivos/terminal ni del catálogo exclusivo del motor. `search_skills` y `load_skill` son los únicos nombres añadidos al maestro. Una skill que requiere un esquema asociado no disponible falla antes de activarse. Las herramientas rápidas diferidas opcionales conservan la autorización del adaptador diferido existente del maestro.

`MasterContextRequest` transporta el principal configurado, la conversación, el ámbito original y el ID de la fuente aceptada desde el compilador compartido del maestro. El daemon verifica esos campos contra el mensaje y la bandeja durables, y lee directamente la fuente literal. Los argumentos de herramientas no eligen el vínculo. Las claves del catálogo de conversación y del turno fuente son hashes SHA-256 con dominio separado sobre valores JSON tipados, conservando los límites entre campos.

El `SkillRuntime` compartido congela un snapshot original de catálogo por principal/conversación/ámbito. Cada turno fuente aceptado comienza desde ese snapshot con sus propias restricciones cargadas y hasta tres sugerencias léxicas congeladas derivadas de la fuente literal. Las sugerencias son pistas, nunca permisos ni sustitutos de instrucciones literales obligatorias. Volver a A después de B o General reutiliza el catálogo de A; otra fuente nunca hereda las restricciones del turno anterior. El vínculo, el snapshot original, la política cargada, las sugerencias y los recibos de skills por clave sobreviven a la reapertura de la base. Una skill modificada no se puede cargar desde el snapshot antiguo; el cuerpo ya cargado conserva su recibo original y genera un delta de cambio versionado.

El compilador añade el catálogo a L1, conservando identidad y memoria, y las sugerencias a L3 sin sustituir la fuente original. La migración v5 almacena actualizaciones L4 con vínculo inmutable a la fuente, ID estable, cuerpo y referencia de secuencia. Las actualizaciones repetidas reutilizan esa referencia; reutilizar un ID con contenido distinto falla. El ámbito original controla el contexto aunque cambie el ámbito activo de la conversación. Se mantienen los checks de fuentes literales y redacción existentes.

Los efectos rápidos ordinarios conservan la guarda compartida de admisión del runtime durante `invoke_source_scoped`; la activación de skills usa su guarda exclusiva. El runtime compartido vuelve a comprobar la política. El adaptador reenvía conversación, ámbito, fuente y clave originales a memoria/herramientas rápidas. `confirm_cancel` humano se reenvía directamente al puerto interno exclusivo del host, conservando su autorización humana durable y sin esquema para el modelo. Los comandos explícitos del usuario conservan su vía existente del host.

Comandos de verificación (solo fixtures locales deterministas):

```sh
cargo test -p excalibur-skills --test catalog --test runtime
cargo test -p excalibur-master --test context --test fast_tools --test cancel_confirmation
cargo test -p excalibur-daemon --test master_skills --test memory_host --test memory_history --test cancel_confirmation --test source_authority
cargo clippy -p excalibur-skills -p excalibur-master -p excalibur-daemon --all-targets -- -D warnings
cargo build -p excalibur-skills -p excalibur-master -p excalibur-daemon
```

Este checkpoint contiene implementación y fixtures de backend, pero todavía no se ha compilado, probado ni aprobado de forma independiente. La nueva fixture del daemon recorre el modelo/herramientas/contexto reales del maestro y reabre la base; las aserciones directas del adaptador cubren además replay, rechazo de ámbito/principal, esquemas exclusivos de trabajador/motor no disponibles y reenvío de confirmación exclusivo del host. Son comprobaciones previstas hasta registrar sus resultados de ejecución. Siguen incompletos las sugerencias híbridas, el enrutamiento experto, todos los puertos del decisor, estadísticas agregadas durables, evaluación del catálogo curado, promoción/aprendizaje y H11 global. No se afirma interfaz ni calidad de proveedores.
