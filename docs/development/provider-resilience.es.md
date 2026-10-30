# Errores de proveedor, espera y disyuntor

La biblioteca compartida `excalibur-models` implementa el contrato de errores de proveedor de la sección 19.5 de la arquitectura (hallazgo AUD570-11): reintentos acotados con espera exponencial y jitter, tratamiento de `Retry-After`, un disyuntor y paso a la ruta de respaldo del rol. Vive en el núcleo compartido (`crates/models/src/resilience.rs`), no en la CLI ni en la API HTTP. Las pruebas usan servidores HTTP simulados locales y un reloj manual inyectado; nunca duermen, no usan la red ni llaman a API de pago.

## Estado compartido

`ProviderResilience` guarda la política de reintentos, todos los disyuntores y un registro de evidencia acotado tras cerrojos seguros entre hilos. `ProviderResilience::process()` devuelve la única instancia del proceso (política por defecto, reloj real); `install_process` permite configurarla una vez antes del primer uso. Todas las pasarelas que reciben el mismo `Arc` (por ejemplo, la pasarela con streaming del maestro y las de los trabajadores) comparten los mismos disyuntores. `ProviderResilience::new` crea una instancia aislada con un `ResilienceClock` inyectado (tiempo monotónico, hora de pared para fechas HTTP y un temporizador de espera).

`OpenAiGateway::with_resilience` activa la resiliencia en una pasarela. Es aditivo: sin él la pasarela mantiene su comportamiento anterior de un solo intento, y activarlo no cambia la solicitud enviada, la identidad de la pasarela ni el hash de configuración.

## Reintentos

- Se reintentan: fallos de transporte, HTTP 408, 429 (límite de tasa), 5xx salvo 501/505, y las categorías clasificadas `overloaded` y `timeout`.
- No se reintentan: los demás 4xx, incluidos 401/403 (autenticación/permiso), el agotamiento de cuota (429 con código de gasto o crédito), errores de ventana de contexto, validación de salida, truncamiento y fallos del consumidor del stream. Se devuelven sin cambios.
- Un reintento reenvía exactamente el mismo cuerpo de solicitud. La pasarela compatible no tiene cabecera de clave de idempotencia, así que no se envía ninguna; la simulación comprueba que los cuerpos son idénticos byte a byte.
- Política por defecto: 2 reintentos, espera exponencial desde 500 ms con tope de 8 s y jitter igualado (uniforme en [d/2, d]).
- `Retry-After` se respeta como segundos o como fecha HTTP (IMF-fixdate, RFC 850 y asctime, relativa al reloj de pared inyectado; una fecha pasada significa ahora). La cabecera específica `retry-after-ms` tiene prioridad cuando es válida. El retraso del servidor se usa exacto, sin jitter. Un retraso pedido mayor que `max_retry_after_ms` (60 s por defecto) no se espera: la llamada falla, para que el respaldo pueda actuar, en lugar de reintentar antes de lo que permitió el servidor.
- Los streams solo se reintentan mientras se abren. Cuando una delta ya llegó al receptor, el fallo se informa y nunca se reinicia en silencio.
- Cada intento reintentado se contabiliza como un fallo final: queda en `ModelCallMetrics.attempts` con su fallo clasificado y la misma estimación conservadora de entrada (bytes de la solicitud más el encuadre), con precio cuando hay tarifa, y cuenta en `provider_calls`. El `usage_known` de la llamada pasa a falso y el consumo de la respuesta durable incluye la estimación, de modo que los presupuestos la cobran. La cota previa `call_bound` crece con los reenvíos que permite la política.
- Cuando un fallo abre el disyuntor, la llamada no espera su espera exponencial ni `Retry-After`; devuelve ese fallo de inmediato para que actúe el respaldo.

## Disyuntor

Los valores por defecto siguen la sección 19.5: el disyuntor se abre tras 5 fallos de disponibilidad consecutivos, o cuando los fallos superan estrictamente el 50 % de los resultados en una ventana deslizante de 1 minuto. Permanece abierto 60 s y después admite exactamente una sonda semiabierta. Una sonda con éxito lo cierra; una sonda fallida lo reabre otros 60 s. Una sonda cancelada (descartada) libera su hueco sin decidir. Mientras está abierto no se envía ninguna solicitud y la llamada falla con la nueva `FailureClass::CircuitOpen`. Esa clase se reserva para llamadas que no enviaron nada (`provider_calls == 0`): si un intento anterior de la misma llamada se envió, se devuelve su fallo real.

La especificación no fija una muestra mínima para la regla de tasa; esta implementación exige 10 resultados en la ventana (`min_samples`), de modo que uno o dos fallos iniciales no pueden abrirlo. `min_samples` se limita a la capacidad de la ventana, 4096 resultados, para que la regla de tasa nunca quede desactivada en silencio.

Los fallos de disponibilidad son los fallos reintentables más los 5xx no reintentables y el agotamiento de cuota. Los errores del cliente, de autenticación/permiso y de validación de salida no cuentan para el disyuntor.

La especificación define el disyuntor por proveedor, y ese es el valor por defecto (`BreakerScope::Provider`). `BreakerScope::ProviderModel` está disponible si un anfitrión prefiere un disyuntor por proveedor y modelo. El encargo pedía proveedor/modelo; la especificación prevalece como valor por defecto y la diferencia queda explícita.

`snapshot` devuelve el estado, los contadores, el tiempo abierto restante y cuántas veces se abrió; `events` devuelve reintentos, topes de `Retry-After`, aperturas, pasos a semiabierto, cierres, rechazos y cambios de ruta.

## Respaldo

`FailoverGateway` contiene las rutas ordenadas de un rol (principal más hasta 8 respaldos, el mismo límite que `RoleConfig`). Una solicitud pasa a la siguiente ruta cuando el disyuntor abierto de la actual la rechaza o cuando falla por disponibilidad tras sus propios reintentos acotados. Los errores del cliente, de autenticación y de validación nunca cambian de ruta. Un stream que ya entregó una delta nunca se reinicia en otra ruta. Cada ruta genera su propio formato de solicitud, así que un respaldo de otra familia recibe su propia representación; la caché de la conversación se pierde, como acepta la especificación. Las métricas se agregan sobre todas las rutas probadas, con éxito o con fallo: llamadas al proveedor, consumo y duración se suman, el costo se suma (desconocido si el de alguna ruta lo es) y los indicadores de conocimiento se intersecan, de modo que `provider_calls > 0` siempre que alguna ruta envió una solicitud. Con éxito, el consumo de la respuesta durable incluye además el de las rutas anteriores, más su costo, o la cota de su ruta si su costo es desconocido. Los metadatos identifican la última ruta probada. Los valores `index` de los intentos conservan su significado por ruta (intento de validación de esa ruta); los intentos se listan en orden cronológico. Si todas las rutas fallan, se devuelve el último fallo real; `CircuitOpen` solo si ninguna ruta envió nada. `call_bound` es la suma de las cotas de las rutas, porque una llamada puede gastar en cada una. `unload_idle` llega a todas las rutas; la sustitución del catálogo diferido es la de la principal cuando todas las rutas coinciden y, si no, ninguna. `rate_limits`, `cache_settings` y los metadatos de la pasarela informan de la ruta principal.

## Higiene de errores

Solo categorías de fallo cerradas y códigos de estado HTTP llegan a quien llama, a las métricas y a la evidencia. `excalibur-providers` captura ahora los valores de las cabeceras `Retry-After` y `retry-after-ms` (como máximo 64 bytes ASCII visibles) mediante los métodos aditivos `native_chat_detailed` y `stream_native_chat_detailed`; el tipo `Error` y los métodos existentes no cambian. Una prueba envía texto con aspecto de secreto en el cuerpo de error y en una cabecera `Retry-After` malformada y comprueba que no aparece en errores, métricas, eventos ni instantáneas.

## Límites

- Aún no está conectado a los anfitriones: el daemon, el maestro, el motor y la CLI siguen creando `OpenAiGateway` sin `with_resilience`, y ningún anfitrión crea un `FailoverGateway` a partir de `RoleConfig`. Crear pasarelas de respaldo requiere credenciales y configuración del anfitrión que este cambio no añade.
- Los errores de contenido (rechazo, salida truncada) mantienen su tratamiento actual: un reintento de validación con el error y, si no, se informa. No se implementa un límite de salida mayor ante truncamiento.
- El estado de los disyuntores vive en memoria de un proceso y no persiste entre reinicios.
- Los disyuntores se identifican por tipo de proveedor (y opcionalmente modelo), no por URL base ni credencial. Distintos servidores propios de un mismo tipo de proveedor (por ejemplo, varios hosts de Ollama o Foundry) y distintas cuentas o proyectos con cuotas separadas comparten un disyuntor, y el agotamiento de cuota cuenta como fallo de disponibilidad.
- Un fallo a mitad de un stream (después de abrirse) se informa a quien llama pero el disyuntor no lo cuenta: el intento ya se registró como éxito al abrirse el stream.
- Los intentos fallidos se cobran con una estimación local conservadora; no se concilian proveedores que facturen de otra forma las solicitudes fallidas.
- La prueba de cobro de presupuesto replica literalmente la rama de fallo de `crates/engine/src/context_runtime.rs`; el motor en sí no se ejercita con resiliencia porque ningún anfitrión la conecta todavía.
- Verificado en Linux solo con simulaciones locales.

## Verificación

```sh
cargo test -p excalibur-models --test resilience
cargo test -p excalibur-models
cargo test -p excalibur-providers
cargo clippy -p excalibur-models -p excalibur-providers --all-targets -- -D warnings
```
