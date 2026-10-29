# Visión general de la arquitectura

[English](overview.md)

Este documento describe la arquitectura prevista. Existen una biblioteca Rust parcial de proveedores y bibliotecas de base/almacenamiento; el agente y las interfaces aún no están implementados. Consulta la [cobertura de proveedores](provider-implementation.es.md).

## Núcleo compartido e interfaces

El núcleo en Rust contiene la lógica del agente y de los proveedores. La CLI puede llamar directamente al núcleo sin un servidor HTTP. La futura interfaz web se comunicará mediante una API HTTP, que llamará al mismo núcleo; la lógica de la aplicación no debe duplicarse en la CLI ni en la API.

```text
CLI ──────────────────┐
                      ├── Núcleo compartido ── Agente y proveedores
Web ── API HTTP ──────┘
```

Inicialmente, el desarrollo y las pruebas se realizarán mediante la CLI y el núcleo compartido. Queda por decidir si la CLI también usará la API cuando sea necesario compartir sesiones activas entre interfaces.

Consulta el [soporte previsto de proveedores](providers.es.md) para las integraciones y otras modalidades.
