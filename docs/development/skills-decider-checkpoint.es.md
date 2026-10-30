# Punto de continuación H11 e integración pendiente

Base: `4888773` (fundación conservada de decisiones/enrutamiento), descendiente de `6e7ed6f`.
Este punto completa un bloque revisable de bibliotecas compartidas, no H11 ni H13.

| Requisito de arquitectura | Superficie implementada | Trabajo pendiente |
| --- | --- | --- |
| 12.1 formato | Validación del manifiesto y diagnósticos | Contrastar cambios del formato oficial al actualizar |
| 12.2 índice | Precedencia de raíces, conflictos, hashes de recursos, búsqueda léxica/vectorial, eventos y reescaneos periódicos | Persistir estadísticas; conectar el ciclo de vida y los ámbitos de archivos con proyectos del daemon |
| 12.3 catálogo | Catálogo agrupado congelado con tokenizador del anfitrión y límite estricto | Compilar en L1 real; informar explícitamente el desbordamiento si no caben cabeceras |
| 12.4 carga/sugerencia | Máximo tres sugerencias léxicas por sesión, carga de cuerpo, función de resolución de herramientas, intersección de autoridad y deltas | Conectar `search_skills`/`load_skill` nativas, esquemas diferidos, sugerencias L3, deltas y aplicación de autoridad al motor; usar sugerencias híbridas en sesiones congeladas |
| 12.5 catálogo | 24 skills integradas en siete grupos integrados; 22 definiciones de grupo | Contenido curado, auditoría independiente y evaluación antes de activar |
| 19 enrutamiento por roles | Configuración de roles, disparadores deterministas de experto y ficha aislada acotada | Integración del enrutamiento/respaldo anfitrión, contabilidad y estado durable por ejecución |
| 19 decisor | Respuestas tipadas, abstención/respaldo, etiquetas SQLite y calibración | Conectar los ocho puertos, abstención al turno ordinario, telemetría y configuración de despliegue |
| H13 | Raíces instaladas en cuarentena | Auditoría de paquetes, aprobación, cuarentena por cambio de hash tras promoción, réplicas A/B, canario/reversión y aprendizaje de preferencias |

El adaptador OpenAI Decisions se conserva del commit base. Las pruebas de este punto ejercitan dobles deterministas locales, sin confirmar un contrato de proveedor en vivo ni hacer solicitudes pagadas. Las pruebas de roles no demuestran integración con el daemon. Las estadísticas de skills viven en RAM y el filtrado de resultados negativos es una regla observacional, no una estimación aleatorizada de beneficio causal.

Los comandos de verificación y contratos están en [Skills y decisiones acotadas](skills-decider.es.md). Formato, clippy, pruebas, build del workspace y auditoría independiente del commit integrado siguen siendo requisitos de entrega. No emitir una señal de finalización desde este punto.

Verificación del punto: `cargo fmt --all --check` pasó. `cargo test -p excalibur-skills --test catalog -p excalibur-decider --test contract -p excalibur-models --test routing` pasó las 15 pruebas locales (9 catálogo, 4 decisiones, 2 enrutamiento). Las pruebas cubren el límite de la ficha con escapes JSON y distribuciones contradictorias, además de los contratos del índice/sesión. La verificación usa el bloqueo global de compilaciones pesadas, dos trabajos y un target separado sin símbolos de depuración.
