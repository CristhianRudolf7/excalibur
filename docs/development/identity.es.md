# Identidad y sesiones del propietario

La biblioteca compartida `excalibur-identity` implementa el contrato de identidad y sesiones del propietario de las secciones 20.12 (acceso del propietario y puerta de entrada) y 20.13 (varios usuarios y equipos) de la arquitectura, con los primitivos de representación de 20.14 (objetos de autoridad por principal) y las mitigaciones de vulnerabilidades de referencia de 20.15 que pertenecen a la capa de credenciales. Vive en el núcleo compartido (`crates/identity/src/`), no en la CLI ni en la API HTTP. Las pruebas usan principales sintéticos, claves de dispositivo generadas localmente y un reloj manual inyectado; nunca duermen, usan la red, llaman a una API de pago ni almacenan un secreto real.

## Registro de cuentas

- El primer principal (el propietario) se crea exactamente una vez por el canal local certificado por el anfitrión (`LocalChannel::host_attested`); cualquier intento posterior es un conflicto que no altera el registro.
- El propietario crea los demás principales con espacios de datos aislados (`AuthenticatedSession::data_namespace`, un espacio estable por principal). Solo el propietario crea o deshabilita principales, y deshabilitar revoca todas las sesiones de ese principal.
- Las contraseñas se verifican con argon2id (valores OWASP: 19 MiB, 2 iteraciones, paralelismo 1; configurable). Los usuarios desconocidos se comparan contra una sal ficticia por proceso con el mismo coste, así que contraseña errónea y usuario desconocido son indistinguibles (`Unauthenticated` en ambos casos). Un inicio válido con costes cambiados re-deriva el hash de forma transparente.
- Cambiar la contraseña exige la actual y revoca todas las demás sesiones del principal.

## Dispositivos y sesiones

- Cada dispositivo registra una clave pública Ed25519 de 32 bytes (canal local tras comprobar la contraseña, o emparejamiento remoto). Cada inicio prueba además la posesión de la clave: el cliente firma `device_proof_message` sobre un desafío fresco de un solo uso, y el desafío se consume sea cual sea el resultado, así que una prueba capturada nunca se repite.
- Los inicios válidos abren sesiones ligadas al dispositivo con vida absoluta y límite de inactividad deslizante que nunca la supera. Los tokens portadores (`exs1.<id>.<secreto>`) llevan un secreto de 256 bits del que solo se guarda el resumen SHA-256; la búsqueda es por id y la comparación es en tiempo constante. Tokens desconocidos, malformados, caducados y revocados se deniegan por igual.
- Cada operación por principal recibe un `AuthenticatedSession` que solo puede producirse verificando un token vivo, y la fila de sesión, su dispositivo y su principal se revalidan dentro de la misma transacción de almacenamiento que lee o cambia datos, así que una revocación que se confirme primero siempre gana.
- Los intentos de inicio están limitados por nombre de usuario con conteo pesimista antes del hash, para que intentos concurrentes no superen el límite; el éxito limpia el contador.

## CSRF y emparejamiento

- Cada sesión lleva tokens CSRF de un solo propósito (`exc1.<id>.<secreto>`, acotados por sesión). La verificación está ligada a la sesión emisora y a su caducidad; un token de otra sesión, o un token portador de sesión presentado como CSRF, se deniega.
- El registro remoto usa códigos de emparejamiento efímeros de un solo uso (base-32 de Crockford, tolerante a mayúsculas, espacios y guiones) canjeados con una clave de origen provista por el anfitrión. Los fallos se limitan por origen y globalmente, y los límites también bloquean códigos correctos. Los códigos consumidos y caducados se deniegan sin registrar nada.

## Equipos, cuotas y proyectos (§20.13)

- Un equipo tiene exactamente un anfitrión. El anfitrión (y, para roles de miembro, los administradores) emite invitaciones de un solo uso; los administradores nunca pueden invitar administradores, y una invitación cuyo emisor perdió el derecho ya no admite a nadie. Quien no es miembro no distingue un equipo ajeno de uno inexistente (`NotFound` en ambos casos).
- El anfitrión comparte proveedores (siempre con una referencia a su baúl, nunca con el valor del secreto) o herramientas, y configura cuotas por miembro o por defecto del equipo por recurso y ventana de tiempo. Los miembros admiten uso contra una cuota con `request_key` idempotente: repetir con parámetros idénticos devuelve la admisión original sin cobrar de nuevo, mientras que parámetros distintos entran en conflicto. Liquidar por debajo de la reserva devuelve la diferencia y es idempotente por valor.
- Los miembros nunca reciben credenciales del anfitrión ni sus referencias: las vistas de miembro solo exponen tipo y nombre, y el pase de representación del anfitrión (`HostProxyTicket`) resuelve la credencial dentro del baúl del anfitrión con `with_host_credential`, cuyo cierre no debe devolver ni publicar los bytes prestados. El pase no es `Clone` ni `Serialize` y su `Debug` está redactado.
- Los proyectos compartidos exigen una concesión explícita por miembro (`read` o `write`, donde `write` implica `read`); el anfitrión posee sus proyectos sin fila de concesión. Concesiones y revocaciones son solo del anfitrión.

## Representación (§20.14)

La política de representación entre agentes en sí (categorías compartibles, propósitos autorizados, auditoría de cada intercambio) pertenece al motor de permisos de 14.11 y no se reimplementa aquí. Esta biblioteca aporta los primitivos de autoridad que esa política consume: sesiones intransferibles por principal, roles de equipo, concesiones de proyecto y pases de representación solo del anfitrión.

## Vulnerabilidades de referencia (§20.15)

| Clase de incidente de referencia | Mitigación en esta capa |
|---|---|
| Puerta local sin autenticación, CORS permisivo (OpenCode CVE-2026-22812) | Tokens obligatorios, valores portadores nunca en URL, tokens CSRF por sesión; las comprobaciones `Origin` y TLS de la pasarela siguen siendo responsabilidad del anfitrión |
| Escalado forzado entre sitios (OpenCode GHSA-632h-h47v-g4x4) | Lo que cambia estado exige sesión viva más su token CSRF ligado |
| Token de pasarela filtrado por una URL de enlace (OpenClaw CVE-2026-25253) | Los valores portadores no contienen metacaracteres URL y nunca se recuperan desde una consulta; los enlaces no llevan autoridad en esta biblioteca |
| Terminal alcanzada desde la interfaz web (OpenCode CVE-2026-22813) | Las sesiones son capacidades sin privilegio ambiental; la interfaz necesitaría su propia política de aprobación encima |
| Sandbox ampliado por argumentos, ejecución indirecta, enlaces simbólicos (CVEs de Codex, Overpatch) | Fuera de alcance aquí; lo cubren las capas de sandbox y autoridad de rutas |

## Límites

Esto es una biblioteca, no una puerta en ejecución: el cableado HTTP, las comprobaciones `Origin`, la terminación TLS, la interfaz de entrada segura y la entrega del emparejamiento remoto son responsabilidad del anfitrión y no se implementan aquí. Las rutas de autenticación del daemon, el maestro y la pasarela pertenecen a otro carril y no se tocan deliberadamente. La verificación de la cadena de auditoría de las filas de identidad (20.11) y la búsqueda de redacción del baúl (20.10) las cubren sus propios componentes.

## Verificación

`cargo test -p excalibur-identity` cubre autenticación que permite/deniega, aislamiento de alcance entre principales, un solo uso de la prueba de dispositivo, caducidad por inactividad y absoluta, revocación, ligadura CSRF, emparejamiento de un solo uso con caducidad y límite, idempotencia de admisión con clave con aplicación de cuota, resolución de credenciales solo del anfitrión, concesiones explícitas de proyecto, higiene URL de tokens y persistencia de sesiones, equipos y códigos al reabrir. Todo es sintético y local con reloj manual.
