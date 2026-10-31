# Visión general de la arquitectura

[English](overview.md)

Este documento describe la arquitectura prevista. Existen bibliotecas Rust de proveedores, base/almacenamiento y motor durable mínimo; la aplicación completa y las interfaces siguen previstas. Consulta la [cobertura de proveedores](provider-implementation.es.md).

La implementación y la verificación se centran actualmente en Linux; el soporte para otros sistemas operativos (incluidos Windows, macOS y perfiles móviles) está previsto para una fase posterior.

## Núcleo compartido e interfaces

El núcleo en Rust contiene la lógica del agente y de los proveedores. La CLI puede llamar directamente al núcleo sin un servidor HTTP. La futura interfaz web se comunicará mediante una API HTTP, que llamará al mismo núcleo; la lógica de la aplicación no debe duplicarse en la CLI ni en la API.

```text
CLI ──────────────────┐
                      ├── Núcleo compartido ── Agente y proveedores
Web ── API HTTP ──────┘
```

Inicialmente, el desarrollo y las pruebas se realizarán mediante la CLI y el núcleo compartido. Queda por decidir si la CLI también usará la API cuando sea necesario compartir sesiones activas entre interfaces.

Consulta el [soporte previsto de proveedores](providers.es.md) para las integraciones y otras modalidades.

Consulta [Motor durable mínimo](../development/durable-engine.es.md) para el contrato actual y sus pruebas backend.

Los contratos compartidos de tareas, planificación y evidencia se describen en [Trabajo verificable](../development/verifiable-work.es.md).

La compilación de contexto, las sesiones durables y la caché de proveedores se describen en [Contexto y caché](../development/context-cache.es.md).

Las herramientas de proyecto y el aislamiento de edición se implementan en bibliotecas compartidas; consulta [Herramientas de proyecto y concurrencia de edición](../development/project-tools.es.md).

La memoria por ámbitos, la recuperación, la consolidación y la biblioteca de documentos son una biblioteca compartida aún no conectada al maestro; consulta [Memoria y documentos](../development/memory.es.md).

El daemon, la puerta autenticada, los turnos conversacionales y los árboles de agentes: [contrato](../development/daemon.es.md).
