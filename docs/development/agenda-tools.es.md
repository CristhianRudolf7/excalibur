# Superficie de usuario de recordatorios de la Agenda

La herramienta rápida `reminders` del Master y el RPC autenticado `agenda.reminders` del gateway comparten un único núcleo (`excalibur_daemon::agenda_tools::AgendaTools`). Ambas vías pasan por la API `Agenda` del anfitrión hacia el dominio de recordatorios del planificador (replay por clave, CAS de generación, vínculos del anfitrión), y cada mutación correcta notifica el aviso de despertar del runtime (`excalibur_scheduler::mutation_signal`), para que el runtime automático siga los cambios sin reinicio.

## Acciones

`create`, `list`, `snooze`, `complete`, `delete` y `change_timezone`. `create` y `snooze` aceptan exactamente una programación: `at` (segundos Unix UTC), `every` (inicio más segundos), `local_at` (hora civil con zona IANA que por defecto es la registrada con el mensaje fuente aceptado) o `cron` (expresión numérica de cinco o seis campos en la zona del usuario). `change_timezone` reexpresa cada recordatorio civil activo (`LocalAt`/`Cron`) del espacio en una nueva zona IANA como una generación nueva cada uno: se recalcula la misma hora local o expresión cron, las programaciones UTC quedan intactas y un recordatorio con un disparo vencido o registrado sin entregar se omite en vez de perder ese disparo.

## Autoridad

La autoridad nunca viene del JSON del modelo o del cliente. Cada llamada queda vinculada al principal autenticado por el anfitrión más la conversación, el ámbito original y el mensaje fuente aceptado de la petición. El orquestador revalida esa fuente y el techo de ámbito del Master dentro de cada escritor (`bind_host_notification`). El espacio de recordatorios queda sellado por principal, ámbito y conversación, de modo que otro ámbito u otra conversación no puede ver ni mutar estos recordatorios. Las llamadas sin fuente aceptada se rechazan; no hay espacio de respaldo. Los campos `scope`/`principal` forjados en los argumentos del modelo se rechazan por el esquema aprobado de la herramienta, los ids de fuente fabricados o ajenos se rechazan como no autorizados y los llamantes RPC sin autenticar reciben `401` antes de llegar al núcleo.

## Idempotencia y validación

Cada llamada lleva una clave de operación durable. El Master la deriva de la llamada a la herramienta (`master:{conversation}:{source}:{tool_call_id}`); el RPC la deriva de la `operation` suministrada por el cliente. Un reintento exacto con la misma clave y argumentos devuelve la instantánea original; la misma clave con argumentos distintos se rechaza (`reminder operation key reused with different arguments`). Las mutaciones de un recordatorio existente necesitan su generación actual; una generación obsoleta se rechaza. Los ids de recordatorio se derivan por defecto de forma determinista desde la clave de operación cuando el llamante los omite, de modo que una creación reintentada es un replay exacto.

La validación de texto en el escritor se ejecuta después de la comprobación de replay por clave (AR-10): un reintento exacto devuelve su instantánea aunque el redactor haya aprendido un secreto coincidente desde entonces, mientras que una creación nueva cuyo texto el escritor de avisos rechazaría se deniega en vez de redactarse en silencio, para que el recordatorio guardado conserve la redacción del usuario. Los literales secretos se rechazan con un mensaje fijo que nunca repite el texto.

## Despertar por mutación

Las operaciones correctas de crear, posponer, completar, borrar, cambiar de zona y cambio de zona en lote notifican el aviso local de mutación indexado por la ruta de la base. El runtime automático espera ese aviso junto a su revisión monótona, de modo que una mutación despierta un runtime inactivo sin mover el reloj. El aviso es solo una optimización; los temporizadores durables siguen siendo la autoridad.

## Límites y trabajo pendiente

Esta superficie no despacha tareas desde recordatorios y no implementa disparadores `after`/`window`, repetición cuando el usuario no responde, notificaciones móviles del sistema ni adaptadores de canales externos. La poda de retención de disparos no está implementada. No completa H9 ni las secciones 21.1-21.2.

## Verificación

```sh
cargo test -p excalibur-daemon --test agenda_tools
cargo test -p excalibur-daemon --test agenda_runtime --test agenda_runtime_isolation --test agenda_dispatch --test host_notifications
cargo test -p excalibur-master
cargo test -p excalibur-scheduler --test reminders
```

`agenda_tools` ejercita el anfitrión, Orchestrator y Master reales con modelos locales guionizados o sin conexión en tres pruebas: una creación desde la herramienta del Master que despierta el runtime inactivo sin mover el reloj, una matriz RPC que cubre replay exacto por clave, reutilización de clave con otros argumentos, CAS de generación, denegación de ámbito/fuente/conversación, rechazo sin autenticar, rechazo de literales secretos y el caso AR-10 de replay tras aprender el redactor, además de completar y borrar; y un cambio civil de zona donde el runtime no entrega nada en el instante antiguo y sí los recordatorios civil y cron recalculados en el nuevo. Los fixtures son locales y sintéticos; no se usan red ni APIs de pago.
