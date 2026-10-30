# Historial por ámbito y de ejecución

Cuando hay memoria y el trabajador tiene un vínculo durable, `history_search` conserva el esquema y dispatcher del adaptador de memoria por ámbito. Busca mensajes y acciones solo dentro de su concesión. `run_history_search` siempre consulta el diario acotado de la ejecución y las referencias de observaciones archivadas. Sin propietario de memoria, `history_search` mantiene su alias de diario anterior.

El hook exclusivo del anfitrión `ToolRuntime::overrides_builtin` selecciona el propietario antes de compilar el primer contexto y al planificar, preparar y ejecutar llamadas. Los decoradores lo reenvían sin apropiarse de herramientas. La validación de la semilla inicial elimina nombres de utilidades duplicados; el contexto real usa la definición del propietario vinculado. Una definición no amplía permisos. `run_history_search` pasa por la misma auditoría y sanitización que el alias anterior.

Las observaciones enmascaradas ya indican `get_tool_output` con un hash durable; no encaminan la recuperación del diario al historial de mensajes por ámbito. Se conserva esa indicación inequívoca.

`cargo test -p excalibur-daemon --test memory_history` ejercita ambos dispatchers reales con modelo simulado y base local. Las pruebas de utilidades del motor comparan el alias con `history_search`. La verificación final está pendiente en este punto; no se declara completo el workspace ni H11.
