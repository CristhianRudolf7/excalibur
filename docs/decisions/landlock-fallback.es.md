# Política de alternativa con Landlock

[English](landlock-fallback.md) · [Contratos de seguridad](../development/security.es.md)

Estado: aceptada. Esta decisión corresponde a la adaptación de procesos Linux de la sección 20.6 de arquitectura; no cambia la portabilidad del núcleo Rust compartido.

La ejecución de proyecto exige su runtime bubblewrap configurado y validado en Linux. Si falta, no puede usarse o no establece la frontera solicitada, se deniega la ejecución antes de iniciar el comando del usuario. No se selecciona Landlock automáticamente. No hay una alternativa implícita con red del anfitrión ni sin sandbox.

## Evidencia y motivo

Landlock es un mecanismo de control de acceso aditivo del kernel que no exige privilegios. Las reglas conceden derechos tratados bajo objetos del sistema de archivos; las concesiones de una capa se combinan y todas las capas apiladas deben permitir una operación. No aporta una excepción negativa por nombre bajo una concesión amplia de escritura. Los descriptores ya abiertos también exigen tratamiento separado. Son propiedades pertinentes del [contrato Landlock del kernel](https://docs.kernel.org/userspace-api/landlock.html#layers-of-file-path-access-rights).

| Capacidad | ABI upstream | Consecuencia para esta adaptación |
| --- | --- | --- |
| Control de renombrados/enlaces entre directorios | 2 | La alternativa debe permitir mover directorios sin ampliar accesos. |
| Control de truncado de archivos | 3 | Proteger la escritura también exige proteger el truncado. |
| Puertos TCP de bind/connect | 4 | Los puertos no aplican por sí solos una lista de dominios. |
| Ámbito de sockets UNIX abstractos/señales | 6 | Las ABI anteriores necesitan otra frontera para estas interacciones. |
| Resolución de sockets UNIX por ruta | 9 | Las ABI anteriores no aportan este derecho del sistema de archivos. |
| Puertos UDP de bind/connect/send | 10 | Las ABI anteriores requieren restricciones UDP separadas. |

Los requisitos ABI proceden de las [definiciones UAPI del kernel](https://github.com/torvalds/linux/blob/master/include/uapi/linux/landlock.h). La [documentación de Linux 6.8](https://raw.githubusercontent.com/torvalds/linux/v6.8/Documentation/userspace-api/landlock.rst) describe además la consulta de ABI en ejecución y su política de red limitada a TCP. Estos contratos del kernel no establecen una alternativa automática para proyectos.

El contrato de proyecto escribible permite crear/renombrar directorios ordinarios y protege la configuración raíz preprovisionada por el anfitrión, metadatos anidados existentes y rutas ocultas de baúl/datos. Los nombres reservados nuevos dentro de directorios recién creados no adquieren autoridad sobre el arnés: su raíz de confianza está fijada. Bubblewrap no aplica un glob negativo de nombres a todo directorio futuro. Una concesión Landlock amplia al proyecto también incluiría los descendientes sensibles existentes; traducir las excepciones actuales de montajes requiere una organización o intermediario explícitos. Un diseño Landlock más acotado es posible, con higiene de descriptores, seccomp y otra frontera de red de cumplimiento. Esta es una conclusión de diseño de la aplicación, no una afirmación de que Landlock no sirva para construir sandboxes útiles.

Sigue siendo candidata una alternativa de solo lectura sobre entradas aisladas aportadas por el anfitrión. No se activa porque la consulta ABI funcione: primero deben especificarse y probarse perfil admitido, datos excluidos, descriptores heredados, frontera de procesos/IPC y red. El binding Rust upstream aporta [abstracciones seguras de las llamadas](https://github.com/landlock-lsm/rust-landlock/blob/main/README.md); añadir esa dependencia no cumple por sí solo estas condiciones.

## Verificación y reconsideración

En el entorno Linux x86-64 comprobado, kernel `6.8.0-147-generic`, la consulta de capacidades sin cambios `landlock_create_ruleset(NULL, 0, LANDLOCK_CREATE_RULESET_VERSION)` devolvió ABI `4`. La consulta de capacidad no instala una política ni establece confinamiento. Bubblewrap informó `0.13.1`. Estas observaciones acreditan mecanismos disponibles, no una alternativa Landlock.

`cargo test -p excalibur-tools --test sandbox_adversarial missing_runtime_never_selects_an_unimplemented_fallback` verifica la denegación por runtime ausente mediante configuración temporal sintética y un comando marcador. No necesita un runtime bubblewrap operativo ni debilita la prueba si Landlock está disponible. La suite existente de sandbox comprueba por separado confinamiento real del kernel configurado.

La decisión se reconsidera cuando un perfil alternativo con nombre acredite las mismas garantías aplicables, detecte su ABI necesaria en ejecución y falle de forma cerrada si faltan funciones. Antes de activarlo, debe añadir pruebas negativas reales de archivos, descriptores, procesos/IPC, TCP/UDP e intentos de evasión. Hasta entonces, la denegación explícita cumple la política de sandbox no disponible; no acredita una alternativa automática para proyectos ni cobertura de ejecución de plataformas no probadas.

El [perfil explícito de contenedor de evaluación](../development/noninteractive.es.md) utiliza su propio contrato Landlock/seccomp declarado por el anfitrión. No es una alternativa automática del modo proyecto. Sus pruebas del kernel y requisitos de preparación del contenedor se documentan por separado.
