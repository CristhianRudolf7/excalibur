# Contratos de capacidades

[English](capabilities.md)

`excalibur-capabilities` es la biblioteca compartida para la arquitectura §14.1, §14.8 y §14.11. Cubre el manifiesto de la capacidad, el ciclo durable de operaciones largas (`working`, `input_required`, `completed`, `failed`, `cancelled`), efectos con clave, cancelación cooperativa que se vuelve a consultar, sondeo seguro ante reinicios con retroceso y reloj inyectado, deltas de estado de aplicación §14.8 con puertas de aceptación, el filtro de divulgación §14.11 y un proveedor simulado determinista. No realiza E/S de red y no está conectado al daemon; los transportes MCP/ACP y las capacidades concretas siguen pendientes.

## Manifiesto

`CapabilityManifest::validate` revisa identidad y versión acotadas, 1–512 operaciones con nombres únicos y esquemas objeto, claves de idempotencia para toda operación con efectos, plataformas y perfiles acotados, `max_concurrency` 1–1024, límites de sesiones positivos, estados únicos que incluyen `working` y al menos un estado terminal, y un límite de 1 MiB. Registrar dos veces la misma identidad y versión solo es idempotente con una definición idéntica byte a byte.

## Tareas durables

`CapabilityTasks` usa el ámbito de la autoridad autenticada del anfitrión, nunca contenido del modelo o de la capacidad. El identificador durable es `cap_<32 hex>` sobre ámbito y clave de operación, así que reenviar la misma clave devuelve la misma tarea y otra clave u otro ámbito nunca colisiona.

Cada avance sigue reclamo, llamada remota y aplicación guardada:

1. una transacción de reclamo elige la acción (iniciar, entregar respuesta, cancelar o consultar), cuenta intentos y reserva la tarea;
2. la llamada al proveedor ocurre fuera de toda transacción;
3. una transacción de aplicación registra el resultado solo si la versión del reclamo sigue vigente, así que un conductor obsoleto no sobrescribe estado nuevo.

Una caída entre reclamo y aplicación deja un reclamo visible. `recover_interrupted` lo libera al arrancar el anfitrión y el siguiente avance reenvía la misma clave (`<task>:start`, `<task>:input:<seq>`, `<task>:cancel`). La simulación aplica cada clave una vez y cuenta recepciones y aplicaciones, así que las pruebas demuestran entrega al menos una vez con efectos como máximo una vez.

Sondeo, entrada y cancelación:

- `next_poll_at` sigue el `poll_interval_ms` remoto limitado por la política; los errores transitorios usan retroceso exponencial (`backoff_base_ms << (n-1)`, con tope `backoff_max_ms`) y fallan tras `max_transient_failures`. Un plazo previo al inicio o una cancelación explícita marca `cancelled` sin contactar al remoto. Un inicio confirmado que sigue fallando queda `effect_uncertain` para no reenviarlo con otra clave en silencio.
- `input_required` expone la petición como `ExternalOutput` no confiable, guarda primero la respuesta del anfitrión y la entrega con su clave. Una respuesta rechazada vuelve a pedir con nueva secuencia; la respuesta nunca se escribe en la auditoría.
- la cancelación es cooperativa: `request_cancel` es idempotente, la cancelación con clave se entrega una vez y el sondeo continúa. Una cancelación ignorada que igual termina marca `cancel_unhonored`; un `cancelled` tardío aún gana. Cada transición, despacho, error transitorio, rechazo, plazo, aplazamiento por concurrencia, expiración y descarte obsoleto queda en `capability_task_events`.

La concurrencia y el aislamiento se aplican en el reclamo: un escritor SQLite serializa conductores, varios conductores envían un solo inicio, los resultados obsoletos se descartan por secuencia, `max_concurrency` difiere inicios extra y otros ámbitos no pueden leer ni conducir la tarea.

## Divulgación y estado de aplicación

`filter_outbound` aplica la política §14.11 por contacto y propósito. Por defecto niega: lo no autorizado se retiene y pide permiso del usuario. Las credenciales nunca se comparten diga lo que diga la política, y los compromisos (pago, contrato, cita) exigen autorización explícita. Los segmentos retenidos solo conservan índice, categoría y motivo; su texto nunca se repite.

`AppSnapshot`, `delta`, `gate_allows` y `gate_blockers` cubren §14.8. El anfitrión guarda una foto por conversación `app:<id>`, incluye solo el delta en L3 cuando cambia y trata ediciones manuales y del agente con el mismo paso `rev + 1`. `delta` rechaza instancias cruzadas y revisiones hacia atrás e informa claves cambiadas y eliminadas con un resumen compacto. Las reglas llegan como `AcceptanceCheck`; la puerta bloquea el cierre mientras falle alguna requerida. Las opcionales nunca bloquean.

## Simulación

`SimulatedCapability` actúa como remoto con su propio archivo JSON, separado de la base del arnés, así que sobrevive a reinicios. Los guiones revelan un paso por consulta, `CancelBehavior` cubre cumplir, ignorar y cumplir tras N consultas, y `fail_next` y `lose_next_responses` reproducen caídas y tiempos agotados tras la entrega. Los contadores de recepciones y efectos prueban que cada inicio, respuesta y cancelación con clave se aplica una vez por más que se reciba.

## Verificación

```sh
cargo test -p excalibur-capabilities --test lifecycle --test properties
cargo fmt --check
cargo clippy -p excalibur-capabilities --all-targets -- -D warnings
cargo check --workspace --all-targets
cargo build -p excalibur-capabilities
```

`lifecycle` cubre las filas §14.12 del alcance: ciclo completo por todos los estados con reinicio a mitad, reenvío con clave aplicado una vez tras respuestas perdidas, cancelación ignorada reconsultada hasta completar, retroceso transitorio tras reinicio con marca de efecto incierto, cancelaciones previas y por plazo sin efectos extra, intenciones durante el vuelo con resultados obsoletos descartados, inicio único concurrente con límites por capacidad, estados no declarados con respuestas de secuencia errónea y aislamiento por ámbito, y entrada rechazada con chequeo de esquema. `properties` barre entradas deterministas de semilla fija sobre estabilidad del identificador, forma del retroceso, reglas de idempotencia del manifiesto, negación por defecto con exclusión de credenciales y autorización de compromisos, y deltas y puertas de aplicación. Las pruebas usan SQLite temporal y la simulación determinista; no llaman a modelos ni a red.

## Límites

- Solo biblioteca: sin daemon, RPC, herramientas ni Agenda; sin transporte MCP/ACP; sin capacidades concretas de navegador, búsqueda, visión, voz, dispositivo, documentos, mensajería, espacios, reuniones, contacto entre agentes o nodos.
- Rutinas de navegador, eventos de fuente `web_read`/`source_search`, marcado de instrucciones ocultas, credenciales por referencia, consentimiento de reuniones, sandbox de nodos, conversión de documentos, entrenamiento GPU, flujos de tienda y demás filas §14.12 no están aquí.
- Plazos y presupuestos son solo por tarea; costos y conmutación de proveedor viven en otro lado.
- Modos de archivo y SQLite verificados solo en Linux. Otras plataformas no se han compilado ni ejecutado.
