# Skills y decisiones acotadas

La biblioteca compartida `excalibur-skills` valida la cabecera Agent Skills, informa entradas inválidas, resuelve la precedencia proyecto/usuario/instaladas/integradas, calcula hashes de recursos y construye índices BM25 y vectoriales opcionales proporcionados por el anfitrión. Las raíces instaladas permanecen en cuarentena. Las skills integradas cubren los siete grupos integrados; los grupos curados tienen definiciones pero no se distribuyen como contenido auditado.

`SkillSession` congela un catálogo por ejecución. El llamador aporta su tokenizador; L1 cabe en el menor valor entre el 2% de la ventana y 4.000 tokens. Los grupos pertinentes reciben descripciones cuando caben; los demás reciben cabeceras. Presupuestos muy pequeños pueden omitir cabeceras. Las sugerencias son léxicas sobre la instantánea congelada, como máximo tres, con filtrado de resultados negativos por modelo. `SkillIndex::search` también admite recuperación vectorial híbrida mediante un adaptador de embeddings acotado.

La carga devuelve el cuerpo congelado como resultado de herramienta después de que una función del anfitrión resuelva las herramientas diferidas declaradas. La carga nunca reescribe el catálogo. No se pueden cargar entradas modificadas o no disponibles desde una sesión obsoleta. Las skills activas intersectan `allowed-tools` con la autoridad existente; los llamadores deben aplicar esta intersección al ejecutar herramientas. Los deltas identifican cambios en las skills cargadas. `SkillWatcher` usa eventos de archivos y un reescaneo de recuperación cada 30 minutos; destruirlo aborta su tarea.

La biblioteca `excalibur-decider` ofrece respuestas probabilísticas tipadas, respaldo acotado, abstención explícita, umbrales conservadores sin calibrar y un registro SQLite etiquetado. La calibración informa Brier/ECE/MCE y cobertura a partir de etiquetas locales suministradas. Los veredictos finales de corrección no tienen puerto de decisión. El enrutador de roles vincula modelos a roles y permite consultas aisladas al experto mientras mantiene estable la vinculación del ejecutor.

Estas bibliotecas todavía no están conectadas al ciclo completo de contexto y ejecución del daemon. Quedan por integrar la aplicación automática de restricciones, la proyección de skills al contexto, los ocho puertos reales de decisión, las estadísticas durables de skills, la auditoría/promoción de skills curadas y el aprendizaje. No se afirma calidad del proveedor ni precisión calibrada en producción. Las pruebas usan dobles locales deterministas y nunca llaman a APIs de pago.

Verificación:

```sh
cargo test -p excalibur-skills --test catalog
cargo test -p excalibur-decider --test contract
cargo clippy -p excalibur-skills -p excalibur-decider --all-targets -- -D warnings
```
