# Excalibur

[English](README.md)

Excalibur será un agente de inteligencia artificial personal capaz de conversar con el usuario, comprender sus necesidades y ayudarlo a planificar y ejecutar tareas. Podrá trabajar en varios proyectos, escribir y modificar código, ejecutar herramientas y pruebas, buscar información, recordar preferencias y decisiones, y recuperar el contexto relevante de conversaciones anteriores. También podrá delegar trabajos a subagentes especializados, coordinar su progreso y ampliar sus capacidades mediante skills.

## Estado

El repositorio contiene una estructura y documentación iniciales y una biblioteca Rust parcial de proveedores. El agente, la CLI, la interfaz web y la tienda aún no están implementados. Consulta la [cobertura y los límites de proveedores](docs/architecture/provider-implementation.es.md).

Para preparar las herramientas y consultar las comprobaciones disponibles, revisa el [entorno de desarrollo](docs/development/environment.es.md).

## Organización prevista

- `apps/`: aplicaciones de línea de comandos y web.
- `crates/`: componentes del backend y del agente en Rust.
- `packages/`: paquetes compartidos del frontend.
- `store/`: catálogo, instalación y ejemplos previstos para herramientas y flujos.
- `docs/`: documentación de arquitectura, desarrollo, usuarios, tienda e investigación.
- `tests/`: pruebas de integración y benchmarks.
- `scripts/`: scripts de automatización del proyecto.

Consulta [ROADMAP.es.md](ROADMAP.es.md), [AGENTS.md](AGENTS.md), [CHANGELOG.es.md](CHANGELOG.es.md) y [SECURITY.es.md](SECURITY.es.md). Excalibur tiene una licencia de código disponible con restricciones comerciales; consulta [LICENSE](LICENSE).
