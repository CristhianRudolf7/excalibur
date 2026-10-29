# Excalibur

[English](README.md)

Excalibur será un agente de inteligencia artificial personal capaz de conversar con el usuario, comprender sus necesidades y ayudarlo a planificar y ejecutar tareas. Podrá trabajar en varios proyectos, escribir y modificar código, ejecutar herramientas y pruebas, buscar información, recordar preferencias y decisiones, y recuperar el contexto relevante de conversaciones anteriores. También podrá delegar trabajos a subagentes especializados, coordinar su progreso y ampliar sus capacidades mediante skills.

## Estado

El repositorio contiene documentación y bibliotecas Rust compartidas de proveedores, almacenamiento, motor durable mínimo, seguridad, baúl y sandbox Linux. El agente personal completo, la CLI interactiva, la interfaz web y la tienda siguen previstos. Consulta la [cobertura y los límites de proveedores](docs/architecture/provider-implementation.es.md).

Consulta [Base y almacenamiento](docs/development/storage.es.md) para la implementación y sus comprobaciones. Revisa [Motor durable mínimo](docs/development/durable-engine.es.md) para el ciclo embebido, recuperación de efectos, contexto, pasarela y herramientas locales.

Para preparar las herramientas y consultar las comprobaciones disponibles, revisa el [entorno de desarrollo](docs/development/environment.es.md).

Consulta [Seguridad obligatoria](docs/development/security.es.md) para los contratos de autorización, secretos y sandbox del backend.

## Organización prevista

- `apps/`: aplicaciones de línea de comandos y web.
- `crates/`: componentes del backend y del agente en Rust.
- `packages/`: paquetes compartidos del frontend.
- `store/`: catálogo, instalación y ejemplos previstos para herramientas y flujos.
- `docs/`: documentación de arquitectura, desarrollo, usuarios, tienda e investigación.
- `tests/`: pruebas de integración y benchmarks.
- `scripts/`: scripts de automatización del proyecto.

Consulta [ROADMAP.es.md](ROADMAP.es.md), [AGENTS.md](AGENTS.md), [CHANGELOG.es.md](CHANGELOG.es.md) y [SECURITY.es.md](SECURITY.es.md). Excalibur tiene una licencia de código disponible con restricciones comerciales; consulta [LICENSE](LICENSE).

La ejecución no interactiva `excalibur exec`, el perfil de evaluación, las métricas/ATIF y la réplica están descritos en [Ejecución no interactiva y evaluación](docs/development/noninteractive.es.md). El adaptador concreto de Harbor lo aporta el usuario.
