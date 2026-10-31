# Control de ingesta

[English](ingestion.md)

`excalibur-ingest` es la biblioteca compartida y determinista de control de archivos de la sección 20.7 de la arquitectura. Es solo una biblioteca: no arranca ningún antivirus, no abre conexiones de red y no escribe nada salvo el directorio de cuarentena que el host configure explícitamente. La biblioteca de documentos la usa detrás del trait existente `IngestGuard`, de modo que quienes llaman a `library_ingest` no cambian.

## Qué comprueba

`ContentGuard::examine(name, declared_type, bytes)` devuelve una `Inspection` o un `Rejection` con un `Code` estable. En orden:

1. **Tamaño.** Se rechazan archivos vacíos y los que superan `max_input_bytes` (64 MiB por defecto). Los tipos de texto declarados (`text/*`, JSON, XML, YAML, TOML y `+json`/`+xml`) deben ser UTF-8 sin bytes NUL. Son las reglas que ya aplicaba el guardián incluido anterior.
2. **Tipo por contenido.** El tipo se detecta por bytes mágicos: PDF, PNG, JPEG, GIF, WebP, WAV, MP3 con ID3, Ogg, FLAC, MP4/QuickTime `ftyp`, zip, gzip, zstd, ustar, ELF, PE, Mach-O, WebAssembly, texto o binario desconocido. ELF, PE, Mach-O y WebAssembly se rechazan siempre, sea cual sea el nombre o el tipo declarado. El tipo de medio declarado y, cuando quien llama lo tiene, la extensión del nombre se comparan con el tipo detectado. Una excepción conserva el comportamiento anterior: el texto UTF-8 válido que el tipo declarado o el nombre prometen como texto sigue siendo texto aunque empiece con los bytes mágicos de otro formato (por ejemplo, una nota Markdown que empieza con `%PDF` o `ID3`). Ninguna firma ejecutable es UTF-8 válido sin NUL, así que esto nunca admite código. PDF, PNG, JPEG, GIF, WebP, formatos ofimáticos basados en zip, gzip, zstd y tar exigen su firma. Los tipos de audio y vídeo sin firma fiable (por ejemplo, tramas MP3 sin ID3) solo se rechazan cuando el contenido lleva otra firma positiva distinta; los demás `image/*`, `audio/*` y `video/*` rechazan documentos, archivos comprimidos y ejecutables. Con `MismatchPolicy::Flag` las discrepancias se registran en `Inspection.flags` y se aceptan; por defecto es `Reject`.
3. **Expansión acotada.** zip (almacenado y deflate), gzip (incluido multimiembro), zstd y ustar POSIX se expanden en memoria. Toda la estructura del contenedor se valida antes de decodificar o entregar ninguna entrada, y las entradas zip almacenadas y tar toman prestada la entrada en lugar de copiarse. Los límites cubren:
   - el presupuesto de expansión (256 MiB), que cuenta la salida de todos los decodificadores más los bytes de las entradas zip almacenadas y tar entregadas para inspección, en todos los niveles. Un contenedor anidado en otro se cuenta en cada nivel;
   - la proporción entre salida y entrada comprimida, por flujo y para el archivo completo (100:1, exenta por debajo de 1 MiB de salida);
   - el total de entradas (10.000);
   - los miembros gzip o tramas zstd por flujo (4.096);
   - la profundidad de anidamiento (3; un `.tar.gz` tiene profundidad 2).

   Los decodificadores producen salida en pasos de 64 KiB y se detienen en cuanto el siguiente paso cruzaría un límite. El número de entradas de un zip se comprueba con el directorio central antes de inflar ninguna. La ventana de zstd se limita a 32 MiB. Las tramas zstd heredadas (anteriores a 1.0) se rechazan en cualquier punto del flujo antes de decodificar, de modo que el límite siempre se aplica.
4. **Seguridad de entradas.** Se rechazan rutas absolutas, rutas con unidad, componentes `..`, caracteres de control y nombres que se normalizan a nada en zip, tar y el campo `FNAME` de gzip. Se rechazan enlaces simbólicos, enlaces duros y entradas de dispositivo o FIFO. En zip, los 16 bits altos de los atributos externos se leen como modo Unix para cualquier sistema de origen, como hace Info-ZIP con Unix, VMS, BeOS, AtheOS y otros. Las entradas zip cifradas, zip64, archivos multidisco, otros métodos zip, cabeceras pax y GNU y tamaños tar en base 256 se rechazan como no soportados en lugar de omitirse. Se rechazan como malformadas estas condiciones de zip:
   - un directorio central con más datos que sus entradas declaradas;
   - una cabecera local cuyos flags, método, CRC, tamaños o nombre difieren del directorio central (los descriptores de datos también deben coincidir);
   - registros locales que se solapan, dejan huecos o no llegan exactamente al directorio central;
   - bytes tras el flujo deflate de una entrada;
   - discrepancias de CRC o tamaño y nombres duplicados.

   También se rechazan como malformados: los datos no nulos tras el bloque de fin de archivo tar, los datos tras el último miembro gzip y los datos tras la última trama zstd. El contenido ejecutable dentro de un archivo comprimido se rechaza, y una entrada cuya extensión no coincide con su contenido es una discrepancia.
5. **Gancho de escáner.** Un `Scanner` opcional recibe el archivo y cada entrada expandida, incluidos los contenedores intermedios descomprimidos. Los veredictos `Infected` y `Failed` rechazan el archivo; con `require_scanner`, un guardián sin escáner lo rechaza todo. `SignatureScanner` es un escáner determinista por patrones de bytes; `SignatureScanner::eicar()` reconoce la cadena de prueba estándar EICAR, que `eicar_test_string()` monta en tiempo de ejecución.
6. **Cuarentena.** Con una `Quarantine` configurada, cada archivo rechazado no vacío se guarda como `objects/<sha256>.bin` y se añade una línea JSON a `manifest.jsonl`. La línea registra hash, tamaño, código y motivo del rechazo, nombre original, tipo declarado, tipo detectado, ruta del objeto y marca de tiempo, con los campos de texto truncados a límites fijos.
   - **Permisos:** los directorios tienen modo 0700 y los archivos 0600, aplicados mediante manejadores abiertos.
   - **Escrituras:** cada objeto se escribe en un archivo temporal, se sincroniza, se renombra y se sincroniza su directorio. Un objeto existente solo se reutiliza si es un archivo regular cuyo contenido sigue coincidiendo con su hash; un enlace simbólico plantado o un objeto corrupto se reemplaza con el renombrado y nunca se toca su destino.
   - **Restos:** los archivos temporales con más de diez minutos se eliminan al abrir la cuarentena.
   - **Manifiesto:** las adiciones toman un bloqueo de archivo del sistema operativo, se sincronizan y reparan la última línea truncada que deje una adición interrumpida. Se rechaza un directorio de cuarentena, un directorio `objects` o un manifiesto que sea enlace simbólico.
   - **Fallos:** un fallo al poner en cuarentena se informa en el rechazo y nunca lo convierte en aceptación.

Nada de lo expandido se escribe en disco, se ejecuta ni recibe un modo: los modos con bit de ejecución dentro de un archivo comprimido solo se informan como marcas `executable_mode_ignored`. Los nombres, tipos declarados y textos del escáner que aparecen en los mensajes de rechazo se entrecomillan y se truncan a 96 caracteres.

## Integración

`BasicIngestGuard { max_bytes }` conserva su tipo y valor por defecto y ahora delega en `ContentGuard` sin escáner ni cuarentena. `ContentGuard` también implementa `IngestGuard`, por lo que el host instala un guardián configurado mediante `LibraryServices.guard`. `excalibur_memory::ingest` reexporta el crate. El trait no recibe nombre de archivo, así que la ingesta de la biblioteca compara el tipo de medio declarado con el contenido; quien tenga el nombre usa `examine` directamente. Los rechazos aparecen como `Error::Rejected("<código>: <motivo>")`.

## Verificación

`cargo test -p excalibur-ingest` ejecuta tres suites:

- `guard` cubre extensiones y tipos declarados falsos, ejecutables con cualquier nombre, política de marcado, archivos anidados normales, bombas gzip/zstd/zip de alta proporción, expansión total, bombas por profundidad y acumulativas, límites de entradas entre niveles, archivos dañados, recorrido de rutas, enlaces y entradas especiales, modos ejecutables ignorados, acierto, fallo y error del escáner, escáner obligatorio, muestras EICAR anidadas, manifiesto de cuarentena, permisos, reapertura, reparación de la cola truncada y adiciones concurrentes desde dos manejadores.
- `audit_regressions` cubre los hallazgos de la auditoría independiente:
  - entradas almacenadas cargadas al presupuesto;
  - entradas ocultas del directorio central y registros locales no contabilizados;
  - discrepancias entre cabecera local y central;
  - modos de enlace simbólico y ejecución para cualquier byte de origen;
  - texto con prefijos mágicos;
  - datos sobrantes en tar y deflate;
  - avalanchas de miembros gzip y tramas zstd;
  - ecos truncados;
  - casos de cuarentena con enlace simbólico plantado, objeto corrupto, manifiesto enlazado y temporales antiguos;
  - tramas zstd heredadas.
- `zip_memory` comprueba con un asignador contador que las entradas almacenadas solapadas se rechazan con crecimiento acotado del heap.

`cargo test -p excalibur-memory --test ingest_guard` ejecuta `library_ingest` con el guardián por defecto y con uno configurado. Todos los datos de prueba son sintéticos y se generan durante la prueba.

## Límites

- No se incluye ningún antivirus real, adaptador ClamAV ni escáner de red; la capacidad `file-scanner` sigue siendo una integración del host. La coincidencia por firmas es un doble de prueba, no detección de malware.
- El paso 5 de la sección 20.7 (análisis y conversión en el perfil de sandbox `file-analysis`) y el flujo de "cuarentena hasta superar los controles" para adjuntos, descargas e importaciones no están conectados aquí. Solo la biblioteca de documentos llama al guardián, y los archivos aceptados no se conservan en cuarentena.
- La inspección se ejecuta de forma síncrona en el hilo que llama, también dentro de la función asíncrona `library_ingest`; su coste solo lo acotan los límites anteriores.
- La detección cubre las firmas enumeradas al inicio del archivo. Formatos que no se expanden:
  - zip64 se rechaza;
  - tar v7 sin la marca ustar, bzip2, xz, 7z y RAR se tratan como binario desconocido;
  - los datos añadidos tras una firma que no es de archivo comprimido (por ejemplo, un PDF o GIF con un zip añadido) no se inspeccionan;
  - los documentos ofimáticos se inspeccionan como contenedores zip, sin analizar macros;
  - los archivos de clase Java comparten el número mágico universal de Mach-O y se rechazan como ejecutables, por lo que los JAR con clases se rechazan.
- Los scripts con shebang se aceptan como texto. El guardián nunca ejecuta archivos recibidos; no juzga el contenido de los scripts.
- Los nombres de entrada solo se comprueban contra el recorrido de rutas, porque nada se extrae a disco. No se rechazan homoglifos Unicode, marcas de dirección bidireccional, nombres reservados de Windows ni duplicados que solo difieren en mayúsculas; un futuro extractor debe aplicar sus propias reglas.
- Los mensajes de rechazo y los motivos de cuarentena contienen texto acotado pero no confiable; marcarlo como contenido no confiable cuando se muestre a un modelo (sección 20.8) corresponde a quien llama.
- La expansión ocurre en memoria dentro del total configurado; los hosts con poca memoria deben reducir `max_expanded_bytes`.
- El manifiesto y los objetos de la cuarentena crecen sin límite, y `records()` lee el manifiesto completo. Los directorios padre de la cuarentena se crean tal como se indican y se consideran de confianza. Las comprobaciones de enlaces simbólicos reducen, pero no eliminan, las carreras con otro proceso del mismo usuario.
- Los modos de archivo y sus comprobaciones son comportamiento Unix verificado en Linux. En otras plataformas la compilación condicional omite los cambios de modo; esa variante no se ha compilado ni ejecutado.
