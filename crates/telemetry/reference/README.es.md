# Validadores de referencia fijados

[English](README.md)

`harbor/` contiene los modelos oficiales sin modificar de trayectorias y AgentContext de Harbor 0.25.0, commit `509b119db0d97b141dcd9c80f4f7ab861c47c2d6`, bajo la LICENSE Apache-2.0 incluida. `harbor/PIN.json` guarda el SHA-256 de cada fuente. `atif-v1.8.schema.json` se generó con `Trajectory.model_json_schema()` y Pydantic 2.12.3; el validador contrasta el esquema con esas fuentes exactas antes de validar las fixtures.

`genai/` conserva las convenciones oficiales Development del exportador OTLP JSON local, commit `3fe84604a581b3d50dd5b4bd29383b462d2a5018` de `open-telemetry/semantic-conventions-genai`, con su LICENSE Apache-2.0 y hashes. `validate.py` verifica atributos/kinds y analiza el mensaje generado `ExportTraceServiceRequest` de `opentelemetry-proto==1.45.1`. OTLP exige IDs hexadecimales; el script los convierte a base64 únicamente para el parser JSON genérico de protobuf.

Instalar `requirements.txt` en un entorno Python temporal y ejecutar `python reference/validate.py` desde el crate, o indicar directorios/archivos de artefactos. La validación es offline. Las fixtures son salidas sintéticas de las pruebas Rust, no resultados de benchmarks. Repositorios: [Harbor](https://github.com/harbor-framework/harbor/tree/509b119db0d97b141dcd9c80f4f7ab861c47c2d6), [convenciones GenAI](https://github.com/open-telemetry/semantic-conventions-genai/tree/3fe84604a581b3d50dd5b4bd29383b462d2a5018), [distribución oficial Python de protobuf OpenTelemetry](https://pypi.org/project/opentelemetry-proto/1.45.1/).

`check-platform-adapters.py` crea un manifiesto Cargo temporal aislado que importa el archivo real `src/resources.rs` con tipos mínimos de error. Comprueba únicamente ese módulo para los targets Rust Windows/macOS instalados. No enlaza/ejecuta un programa ni acredita una compilación cruzada de todo el crate/workspace. Las comparaciones nativas Linux y pruebas cgroup sintéticas están en `tests/resources.rs`.
