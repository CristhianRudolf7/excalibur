# Runtime automático de Agenda

`excalibur_daemon::agenda::AgendaRuntime` es la tarea propia del daemon que hace vencer los recordatorios sin un tick explícito del anfitrión. `excalibur daemon start` la lanza bajo el supervisor de servicios existente en el runtime de trabajo, junto a los servicios de orquestación y mantenimiento. No añade RPC, herramienta del modelo ni una segunda vía de entrega: toda admisión pasa por `notify_host_guarded`, protegido por generación, descrito en [Núcleo de despacho explícito de recordatorios](agenda-dispatch.es.md), y el servicio de mantenimiento existente entrega los avisos encolados al Master local.

## Ciclo de vida

- **Arranque.** La primera pasada tras iniciar el proceso aplica la política de disparos perdidos de la sección 17.4 en lugar de un tick ordinario. Los temporizadores de recordatorios usan `RunOnceNow`: un recordatorio `every` que perdió varios intervalos con el daemon parado produce un único disparo en la última hora prevista, y el aviso conserva esa hora programada. El modo de recuperación se mantiene entre pasadas hasta que ningún temporizador de recordatorio esté vencido, de modo que las páginas acotadas no conviertan el resto en una repetición de cada intervalo. Un despertar más de 60 segundos después de su plazo (suspensión, anfitrión detenido) se trata igual.
- **Índice.** Cada pasada reconstruye su vista desde el estado durable: un índice paginado por clave de todos los espacios de recordatorios con su `next_at` habilitado más temprano, si tiene disparos sin admitir y la metadata de creador más reciente. El runtime no tiene vínculo propio. Abre un espacio solo si ese vínculo de creador persistido deriva exactamente al espacio sellado (`reminders::namespace_of` con `source=None`); si no, el espacio no se abre, no avanza ni admite nada, y sus filas quedan para auditoría. El despacho comprueba además el vínculo de la generación de cada disparo contra el espacio antes de usarlo, de modo que la metadata no puede redirigir un disparo a otro principal, ámbito o conversación, y un vínculo ausente se omite en vez de sustituirse por uno de respaldo.
- **Espera.** El runtime duerme hasta `min(next_fire_utc)` sobre todos los espacios de recordatorios, acotado por una revisión inactiva de 60 segundos. Esa revisión cubre autoridad que vuelve a ser válida, avisos locales perdidos y relojes monótonos que no avanzan durante una suspensión. El reloj es inyectable (`AgendaClock`); el daemon usa `SystemClock` y las pruebas `ManualClock`.
- **Despertar por mutación.** `excalibur_scheduler::mutation_signal` es un aviso local del proceso indexado por la ruta de la base. Las operaciones correctas de crear, posponer, completar y borrar recordatorios, `Scheduler::create` y `Scheduler::change_timezone` lo notifican. Un permiso almacenado cubre una mutación que llega durante una pasada. El aviso es solo una optimización; los temporizadores durables siguen siendo la autoridad.
- **Modo seguro y recuperación de arranque.** Mientras el registro de recuperación indica `recovering` o modo seguro, las pasadas siguen registrando disparos vencidos con la política de perdidos pero no admiten nada; el writer de avisos también rechaza la admisión protegida en ese estado. El runtime revisa cada 5 segundos, así que una salida manual del modo seguro o el fin de su ventana liberan los disparos registrados sin reiniciar.
- **Apagado.** La señal de parada del daemon cancela la espera de inmediato. El resultado de la tarea forma parte de la decisión de apagado ordenado que registra el recibo de salida limpia. Una pasada cancelada a medias puede repetirse sin riesgo.

## Idempotencia

Los disparos se identifican por ámbito, temporizador y hora prevista; el avance del temporizador es un compare-and-set sobre la definición y el cursor guardados; las claves de aviso son un hash del ID tipado del disparo; el ACK del scheduler ocurre solo cuando ya existe el recibo durable del aviso. Por ello, runtimes concurrentes, un `Agenda::tick_once` explícito y un reinicio tras SIGKILL registran y encolan cada disparo una sola vez. Las pasadas de un mismo runtime se serializan.

## Límites y trabajo pendiente

Esto cierra el despertar automático, el recálculo al arrancar y el bloqueo en modo seguro solo para avisos de recordatorios. No implementa RPC ni herramientas Agenda del modelo, creación de tareas desde recordatorios o programaciones, disparadores `after`/`window`, repetición cuando el usuario no responde, notificaciones programadas del sistema móvil ni adaptadores de canales externos. Este runtime no despacha temporizadores genéricos fuera de los espacios de recordatorios, y los temporizadores de recordatorios aún no tienen API de cambio de zona horaria. No completa H9 ni las secciones 21.1–21.2.

## Verificación

```sh
cargo test -p excalibur-daemon --test agenda_runtime
cargo test -p excalibur-cli --test agenda_runtime
cargo test -p excalibur-scheduler
cargo test -p excalibur-daemon --test agenda_dispatch --test host_notifications --test service_supervision
```

La prueba del daemon ejecuta el runtime contra un anfitrión, Orchestrator y Master reales con proveedor sin conexión y reloj manual: `RunOnceNow` al arrancar sobre diez intervalos perdidos, despertar en el temporizador más temprano, despertar por mutación sin mover el reloj, conservación de la fuente del creador al posponer, registro sin admisión en modo seguro y liberación tras una salida manual, parada limpia, reapertura de la base, dos runtimes en carrera y un espacio falsificado que permanece inerte. La prueba de la CLI siembra recordatorios, arranca el proceso real en primer plano `excalibur daemon start`, observa por HTTP autenticado el disparo perdido antes del recordatorio futuro y este a su hora, después mata el daemon con SIGKILL, lo reinicia y comprueba que nada se duplica antes de una parada limpia. Los fixtures son locales y sintéticos.
