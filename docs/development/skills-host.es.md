# Sesiones de skills de trabajadores

El `SkillRuntime` compartido envuelve el runtime autorizado existente. El daemon vincula cada ejecución de trabajador a su ámbito efectivo tras completar los vínculos de workspace, seguridad, planificación y memoria. Cada proyecto indexa solo sus directorios `.agents/skills`, las raíces de usuario/instaladas configuradas y las skills integradas; General no tiene raíces de proyectos. Los paquetes instalados mantienen la cuarentena.

Una ejecución guarda su instantánea validada original, los cuerpos completos, el catálogo agrupado inmutable y hasta tres sugerencias léxicas. El contador del proveedor elegido y la ventana del anfitrión limitan el catálogo a min(ventana/50, 4000) tokens. Se rechaza cambiar el ámbito vinculado. La sesión serializada tiene un digest SHA-256 comprobado y sobrevive a otro proceso daemon con su catálogo y restricciones cargadas originales. El redactor del anfitrión comprueba el material congelado antes de guardarlo. Las estadísticas agregadas de sugerencias/cargas siguen en RAM.

El motor obtiene el contexto de sesión del anfitrión antes de compilarlo por primera vez: añade el catálogo a L1 sin reemplazar identidad/memoria, lleva las sugerencias a L3 y añade cambios identificados de skills cargadas como actualizaciones L4. La carga devuelve el cuerpo y los esquemas núcleo/diferidos resueltos de esa ejecución como resultado de herramienta; no modifica el catálogo inicial ni el catálogo diferido nativo del proveedor. `search_skills(query, group?)` busca en el índice congelado. `load_skill(name)` rechaza entradas cuyo hash actual cambió. Los recursos siguen sujetos a las herramientas normales de archivos y a sus permisos.

La activación y su resultado con clave se confirman en una transacción. Repetir una clave devuelve el resultado original; llamadas distintas no pueden reutilizarla. Los errores de resolución de esquemas o restricciones activas incompatibles no activan la skill. Las listas `allowed-tools` activas se intersectan y se aplican al preparar, ejecutar trabajo ya preparado, reconciliar y auditar utilidades internas del motor. El transporte diferido `call_tool` comprueba su objetivo y no concede autoridad. Todos los permisos de archivos, memoria, planificación y seguridad subyacentes siguen aplicándose. Las definiciones estables se conservan para la caché; la visibilidad no autoriza.

El escaneo de archivos, el tokenizador y la decodificación se descargan a workers bloqueantes. El vigilante agrupa eventos de creación/modificación/borrado y reescanea cada 30 minutos; las notificaciones de acceso no provocan nuevos escaneos. El cierre detiene los vigilantes. `SkillsOptions` puede aportar una ventana conocida por el anfitrión; en su defecto se usa la política del proveedor o el valor predeterminado del contexto. El conteo de modelos desconocidos sigue siendo estimado.

Comandos de verificación (mocks locales, sin proveedores pagados):

```sh
cargo test -p excalibur-skills --test catalog --test runtime
cargo test -p excalibur-daemon --test skills_host --test memory_host
cargo test -p excalibur-engine --test context_compaction --test context_session_races
cargo clippy -p excalibur-skills -p excalibur-daemon -p excalibur-engine --all-targets -- -D warnings
```

Las suites finales y la revisión independiente del commit fijo siguen pendientes en este punto. Este bloque no cierra H11: siguen requeridas las sesiones del maestro por conversación/ámbito, sugerencias híbridas congeladas, los ocho puertos del decisor, enrutamiento experto, estadísticas agregadas durables, evaluación del catálogo curado y promoción/aprendizaje H13. Las sugerencias del padre son pistas opcionales, nunca sustituyen instrucciones obligatorias fijadas.
