# Ejecución no interactiva y evaluación

[English](noninteractive.md)

`excalibur exec` es un comando Rust ligero sobre `excalibur-eval`, el motor, seguridad, herramientas, pasarela y telemetría compartidos. Crea una ejecución raíz con función de coordinador; no inicia conversación del maestro, servidor HTTP, agenda ni canal interactivo de aprobaciones. Nunca pregunta. Los datos obligatorios declarados por el anfitrión bloquean la ejecución si faltan. Las operaciones que requieren una aprobación no concedida por la política fijada devuelven un resultado de herramienta denegado para que el modelo busque una alternativa autorizada.

```sh
cargo build -p excalibur-cli
# El anfitrión aporta EXCALIBUR_API_KEY por entorno; nunca es un argumento de CLI.
EXCALIBUR_MODEL=modelo-configurado target/debug/excalibur exec 'Tu petición' \
  --project /ruta/workspace --json --logs-dir /ruta/ensayo-nuevo
```

`EXCALIBUR_BASE_URL` o `--base-url` selecciona un endpoint compatible con Chat Completions de OpenAI. Por defecto se usa la API de OpenAI. `--model` prevalece sobre `EXCALIBUR_MODEL`; `--reasoning-effort` fija el ajuste de la pasarela. El anfitrión conserva las credenciales y registra la del entorno en el redactor compartido antes de generar solicitudes o artefactos. No se descubren automáticamente archivos de credenciales. `--replay` no necesita credencial ni inicializa un proveedor en vivo.

## Entrada, salida y límites

`excalibur --help` muestra opciones; `excalibur --version`, versión y commit compilados. `--project`, `--autonomy A0|A1|A2|A3`, `--output-schema`, `--json`, `--budget-usd`, `--max-steps`, `--max-tokens` y `--timeout` configuran una ejecución. Un presupuesto económico exige una `--price-table` del anfitrión que coincida con el modelo; un precio desconocido no equivale a inferencia gratuita. Se usan enteros en micro-USD. La cota conservadora de la pasarela puede detener la siguiente llamada antes del despacho si no cabe en los tokens/costo restantes. Véanse los límites de estimación y facturación en [contabilidad y réplica](replay-pricing.es.md).

`--required-input NOMBRE=VALOR` declara datos obligatorios; un valor vacío bloquea antes de inferir. `--criterion ARCHIVO` añade un `Criterion` backend en JSON: éxito de herramienta, valor exacto en un puntero de su salida o comprobación del anfitrión. El criterio predeterminado comprueba una respuesta final no vacía y, si corresponde, `--output-schema`. Acredita la sintaxis de entrega, no el significado arbitrario de una tarea: para verificar más, hay que declarar criterios observables. Los fallos de validación de esquema no se convierten en salidas finales correctas. La puerta acotada informa criterios pendientes en vez de aceptar una afirmación de cierre.

| Resultado raíz | Código de salida |
| --- | --- |
| Completada | 0 |
| Completada con criterios pendientes | 2 |
| Bloqueada / falta un dato obligatorio | 3 |
| Pausada por presupuesto o tiempo | 4 |
| Ejecución fallida / fallo de proveedor | 5 |
| Cancelada por SIGINT/SIGTERM | 130 |
| Error de argumentos, arranque o infraestructura del arnés | 1 |

`--json` escribe eventos JSONL redactados al confirmar acciones y termina con un registro `type=result`. Sin esa opción, stdout recibe el texto final y stderr el motivo/directorio de registros. El JSONL persistido y las suscripciones tienen cursores durables; desconectar un cliente no elimina eventos. La proyección durable del motor se vacía tras la admisión y antes de efectos posteriores. Si falla, se conservan eventos recuperables en el diario y no se avanza a un efecto sin registrar. Un timeout del modelo al agotar el plazo pausa la ejecución. La cancelación conserva su estado durable y detiene sus terminales.

Cada ejecución usa estado nuevo en `LOGS/excalibur`, privado del propietario en Unix, y rechaza un directorio de datos no vacío o artefactos primarios anteriores. Una reserva impide reutilizarlo simultáneamente. En modo proyecto los registros van por defecto a una ruta temporal única que sobrevive al proceso; en `eval`, a `/logs/agent`. Este comando no reanuda ni borra silenciosamente un episodio anterior. Las APIs compartidas de recuperación del motor/diario siguen disponibles para adaptadores del anfitrión.

## Contrato de evaluación del anfitrión

`--profile eval` selecciona la política `eval@1` fijada dentro del binario. Usa UTC e inglés por defecto, permite como máximo `min(2, CPU de la tarea)` adaptadores de hijos activos, desactiva memoria persistente/perfil de usuario, conversación/aprobaciones, agenda, capacidades personales externas, inferencia local, aprendizaje y publicación, y registra los hashes del perfil, configuración efectiva, herramientas reales y skills congeladas. `--task-cpus` registra la asignación de la tarea sin cambiar recursos. `--task-timeout` usa como máximo el 90 % del plazo recibido; sin esa señal rige `--timeout`. No se cambia ningún multiplicador de tiempo o recursos del benchmark.

La ruta instalada necesita una declaración explícita del anfitrión de confianza:

```sh
EXCALIBUR_EVAL_CONTAINER=1 target/debug/excalibur exec 'Instrucción de la tarea' \
  --profile eval --container-runtime runtime-declarado --project /app \
  --logs-dir /logs/agent --task-network none --json
```

La declaración fija las identidades observadas de los namespaces Linux. **No verifica independientemente Docker ni Harbor.** El adaptador externo debe aportar un contenedor nuevo y aislado, recursos/red de la tarea, instalación protegida del arnés, montajes de solo lectura para metadatos sensibles existentes y ausencia de tests del verificador durante el turno del agente. El adaptador Linux aplica Landlock —ABI 3 o posterior— y seccomp sin necesitar namespaces de usuario anidados ni bubblewrap. No recurre a un shell ordinario sin restricciones. `--read-root` y `--write-root` añaden raíces canónicas del contenedor aportadas por el adaptador confiable. El ejecutable del arnés queda protegido; debe instalarse en un prefijo propio fuera de las raíces escribibles. Las raíces escribibles son techos explícitos del anfitrión, disjuntos del estado privado y las rutas del evaluador; conceder `/` o solapar rutas ocultas falla. Los metadatos protegidos existentes necesitan montajes reales de solo lectura. Puede exigir preparación del contenedor además de instalar el binario. El modo proyecto sigue necesitando su frontera bubblewrap validada.

`/tests`, `/logs/verifier`, `/solution`, los datos privados del arnés y la configuración sensible permanecen protegidos. El agente no busca, ejecuta ni altera verificadores del benchmark. `--task-network none` impide red en procesos; `open` usa el techo de red del contenedor declarado externamente. Es distinto del proxy vivo de project-net: no se anuncia proxy de dominios en eval. El adaptador nativo rechaza restricciones de dominios no soportadas. Las pruebas locales comprueban el aislamiento real del kernel Linux; no constituyen un ensayo OCI/Harbor.

Permiso de la política y disponibilidad de implementación son cosas distintas. El catálogo real incluye archivos/terminal, `task_db` del episodio, reloj/estado, búsqueda/llamada de herramientas, historial de la ejecución, observaciones fijadas con límites, continuación/directivas y búsqueda/carga de skills inmutables de programación, planificación y verificación. El episodio utiliza el almacén compartido de tareas durables, requisitos/criterios/comprobaciones canónicos, planes versionados, lotes atómicos, recibos idempotentes y puerta de cierre con evidencia vigente. La política del episodio sigue desactivando el registro de proyectos. Consulta [Trabajo verificable](verifiable-work.es.md). Se conservan como máximo ocho observaciones fijadas dentro de 4 KiB serializados, con desalojo determinista por secuencia de origen.

El daemon compartido proporciona orquestación de árboles de agentes; este adaptador exec aislado no arranca ese servicio implícitamente. El sistema completo de skills, los adaptadores de código/documentos/medios/web y los conectores alternativos ACP/externo pertenecen a sus propios alcances. Sus permisos admisibles y techo de hijos quedan registrados, mientras que las herramientas no registradas faltan del catálogo real y se rechazan al pedirlas. Los metadatos identifican explícitamente esas dependencias. Se sigue la regla arquitectónica de aplazar verificaciones de capacidades aún no construidas; una fila de política no se presenta como implementación. Un adaptador web registrado debe respetar la señal de red y las restricciones de fuentes; el decorador actual rechaza solicitudes a fuentes conocidas del benchmark antes de delegar. No se anuncia aquí un adaptador web ni detección de copias arbitrarias de soluciones en terceros.

`--memory-store`, un almacén de skills no declarado o `--credentials-store` aborta el arranque de evaluación. El catálogo de programación incluido es inmutable. Las skills de la tarea necesitan un `--profile-override` explícito que declare `/runtime/skills`; el cargador limita y calcula el hash de esa instantánea, sin instalar ni aprender skills. `--profile-override ARCHIVO` es JSON estricto: idioma soportado, declaraciones de roles del mismo modelo y el montaje de skills pasan a configuración/ATIF como desviaciones registradas. Un rol con otra pasarela necesita un adaptador registrado y se rechaza en este comando de modelo único. El contenido del workspace no puede anular denegaciones o techos.

## Artefactos y conexión

Las salidas primarias son `/logs/agent/trajectory.json` y `agent-context.json`. `/logs/agent/excalibur` contiene bases, exportación redactada del diario, eventos, metadatos, métricas, muestras de recursos, OTLP JSON opcional y exportaciones/manifiestos por ejecución. Los intercambios confirmados producen `replay.json`; grabaciones incompletas/sin respuestas reciben un estado explícito en `replay-export.json`. Los archivos se publican incrementalmente y una interrupción deja artefactos parciales válidos. [Telemetría](telemetry.es.md) explica reparación de generaciones mezcladas y validación oficial ATIF/AgentContext fijada.

`--trial-id`, `--task-id`, `--task-digest`, `--benchmark` y `--harbor-version` reciben la identidad del ensayo del anfitrión. También se registran modelo/proveedor, razonamiento, configuración/catálogos, commit/estado de fuentes compiladas y entorno. Identidad del benchmark, tarifas o contadores ausentes quedan desconocidos. Un binario con cambios sin commit o metadatos externos incompletos no acredita una comparación válida. Solo el adaptador externo confiable adjunta recompensa/salida mediante `Telemetry::record_verifier` después del turno; el agente no lee archivos del verificador. No se publican resultados.

`--cgroup DIR --cgroup-scope task_children|task_container|shared` aporta un cgroup v2 existente y delegado. El lanzador nativo incorpora el proceso antes del código del usuario; un fallo impide ese comando. El muestreador distingue RSS/CPU del arnés de pico de memoria, CPU, PIDs y eventos OOM del grupo. No crea cgroups ni cambia límites. Sin un grupo exclusivo utilizable, los contadores de hijos/contenedor quedan explícitamente ausentes o compartidos. No se afirman experimentos OOM nativos ni ejecución Windows/macOS; la compilación aislada de adaptadores de recursos se documenta aparte.

El usuario aporta el adaptador concreto de agente instalado de Harbor: instalar/configurar binario y contrato del contenedor, pasar instrucción literal y metadatos/presupuestos, invocar `exec`, recoger código/ATIF/AgentContext y adjuntar después el verificador. Este trabajo no instala servidor ACP ni adaptador Harbor.

`--seed` y `--temperature` son solicitudes opcionales explícitas de muestreo del anfitrión; no se presume soporte del modelo ni determinismo. Esos valores solicitados y el esfuerzo de razonamiento entran en metadatos y contexto compilado, por lo que una réplica con muestreo distinto se detiene al divergir. `--metadata ARCHIVO` aporta metadatos estructurados del ensayo, incluido el muestreo solicitado opcional. Los límites del proveedor están en [contabilidad y réplica](replay-pricing.es.md). El modelo puede declarar un dato obligatorio ausente mediante `status`, `action=block` y un motivo; el motor confirma ese resultado y bloquea sin otra inferencia.

Las ejecuciones instaladas fallidas publican además `excalibur/failure.json`, con clase interna segura y una etiqueta `exception_type` de la [política de errores fijada de Harbor 0.25.0](https://github.com/harbor-framework/harbor/blob/509b119db0d97b141dcd9c80f4f7ab861c47c2d6/src/harbor/models/trial/error_policy.py). Se distinguen cuota, límite de frecuencia, sobrecarga, ventana de contexto y autenticación cuando el proveedor aporta un código/estado reconocido. Los casos desconocidos conservan una clasificación prudente. El adaptador externo consume el descriptor; el comando Rust no eleva excepciones Python ni reintenta errores de cuota.

## Verificación backend

```sh
cargo test -p excalibur-cli -p excalibur-eval -p excalibur-telemetry -p excalibur-models
cargo test -p excalibur-engine --test event_projection
cargo fmt --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo build --workspace
```

Las pruebas usan tareas sintéticas, HTTP/SSE locales, efectos reales de archivos/procesos temporales, comprobaciones de ámbitos, aislamiento real Linux, señales controladas e interrupciones. Cubren denegaciones que vuelven al modelo, esquemas/criterios, códigos de salida, ensayos nuevos, rechazo de perfiles/almacenes, fuentes, recuperación incremental, réplica y contabilidad de recursos. Los proveedores se simulan. No se usan modelos pagados, credenciales de inferencia, GPU, despliegues ni benchmark completo.

La verificación del árbol congelado superó las cuatro comprobaciones del workspace con 338 pruebas aprobadas y cero fallos; once auxiliares de subproceso ignorados son invocados por sus pruebas padre activas. Una comprobación acotada del comando instalado usó tres respuestas HTTP locales simuladas, escritura real, un comando confinado y esquema final comprobado. El validador oficial fijado aceptó sus artefactos reales junto a los de referencia: cinco documentos ATIF, cuatro AgentContext y tres OTLP. Es validación local sintética, no benchmark ni despliegue OCI.
