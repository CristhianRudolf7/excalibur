- Ideas y decisiones de arquitectura. Son requisitos del proyecto, no capacidades ya implementadas.

## Cosas críticas


- Excalibur debe tener soporte para Windows, Linux, Mac, Android e iOS.

- Contar con modelos de vectorización (embeddings) y un modelo para apoyar decisiones, como Jev de TypeSafe AI.

- Compactar el contexto al alcanzar un límite de 500 000 tokens.



- Desde la primera versión, medir el consumo de RAM, el costo y la cantidad de llamadas a herramientas por tarea.

- Evaluar **Muse Glimmer con el arnés de Excalibur y con el de Muse Code** mediante **Terminal Bench 2.1**; crear benchmarks pequeños para evaluar la memoria y otros escenarios, comparando RAM, cantidad de tokens, tiempo y costo por tarea y otras métricas relevantes.

### Funcionalidades

- La arquitectura debe contemplar la **instalación y actualización de Excalibur**, y permitir configurar su **inicio automático durante la instalación**.









- Contar con una interfaz de línea de comandos CLI.


- Excalibur debe poder **generar y desarrollar investigaciones por sí mismo**; el alcance y los criterios de validación quedan por definir.


- Autopoiesis.


- El usuario debe personalizar su zona horaria, y esta debe utilizarse para los recordatorios y las tareas programadas.

## Funcionalidades

- Por instrucción de Cristhian del 2026-10-22, se incorporan como requisitos de Excalibur los 12 grupos y 54 títulos del inventario de Fénix de `agente/APUNTES.md`, revisado sobre el commit `aace2375eb370e06b0710a1413ef38e0b0f04249`. No implica que estén implementados en Excalibur ni autoriza copiar código. Los números identifican los títulos de origen; las viñetas sin número conservan los requisitos propios de Excalibur.

### Tarea y planificación

- Para cada tarea, identificar requisitos y restricciones, aclarar dudas relevantes o registrar suposiciones y definir criterios de aceptación observables. Asignar identificadores estables y vincular en SQLite requisitos y criterios con sus comprobaciones y evidencias mediante un grafo de relaciones. Registrar estados (cumplido, fallido o no verificado), resultados, fechas y versión del código cuando corresponda; detectar criterios sin comprobar, fallidos o con evidencia desactualizada y contrastar los resultados con la petición original antes de finalizar. Consultar solo la información pertinente y limitar el tamaño de los resultados.

- En tareas grandes, crear automáticamente un subagente con la skill de verificación correspondiente para comprobar que se cumplieron las tareas y los requisitos antes de finalizarlas.


- Gestionar la planificación de tareas, hitos y dependencias en SQLite como fuente principal, sin utilizar archivos Markdown para planificar; consultar solo la información necesaria para cada tarea.



- Consulta, modificación, pausa, cancelación y replanificación de tareas.

- Mantener el estado de cada tarea para recuperarla y retomarla de forma conservadora tras interrupciones, caídas del servidor o reinicios.

- Para conversaciones generales, mantener en SQLite tareas, títulos, estados, detalles y un bloc de notas general; las tareas pequeñas pueden usar una planificación temporal breve sin crear un proyecto o una memoria específica.
- Registrar en SQLite cada proyecto con su nombre, directorio y estado.
- Mantener en SQLite un plan macro por proyecto, con pasos y dependencias; crear planes micro para sus tareas solo cuando se necesiten, con el detalle y estado correspondiente.
- Mantener por proyecto sus requisitos de software, un bloc de notas o bitácora y un registro de pruebas y resultados. Vincular mediante identificadores los requisitos, criterios, comprobaciones y evidencias en el grafo de relaciones de SQLite, conservando el contrato de verificación de tareas.

#### Dudas






### Agentes y subagentes

- Detectar bucles durante la ejecución de tareas.

- El agente maestro debe consultar los trabajos, estados, avances y herramientas de sus subagentes; redirigirlos o interrumpirlos individualmente, incluso sus herramientas, y deshacer sus efectos cuando sea posible. El usuario podrá consultar esta información en lenguaje sencillo y recibirla continuamente.

- Los subagentes notificarán al agente maestro cuando una tarea en paralelo o en segundo plano termine, quede incompleta o se bloquee, indicando su estado y un resumen textual de lo ocurrido y lo pendiente. El agente maestro informará al usuario del resultado.

- Crear un prototipo de portero para SQLite que encole las escrituras concurrentes y las ejecute en transacciones cortas, permita lecturas en paralelo mediante WAL y gestione las esperas y los errores de bloqueo, sin serializar las tareas completas de los agentes.

- Cada subagente debe tener un nombre, un ID y una descripción de su función.

- La jerarquía tendrá una profundidad máxima de tres niveles: un único agente maestro que conversa con el usuario y permanece disponible, subagentes padre y subagentes hijos. Los subagentes padre comparten el motor y las capacidades de coordinación del maestro para su tarea o proyecto, pueden crear y dirigir subagentes hijos, informan al maestro y utilizan las herramientas y permisos necesarios para su encargo.

- Al editar documentos en paralelo, coordinar las ediciones del mismo archivo: antes de escribir, comparar una huella con la versión leída; si cambió, releer el archivo y ajustar la edición para evitar sobrescribir cambios.

#### Dudas

- Verificar si, con las funcionalidades y requisitos previstos, el agente maestro permanecerá siempre disponible para el usuario o habrá momentos en que esté ocupado y no pueda atenderlo.


### Memoria y gestión del contexto

- Usar un selector por conversación para elegir el contexto general o un proyecto activo. Si el usuario menciona varios proyectos, consultar el contexto pertinente de cada uno por separado y distinguir sus resultados; asignar a cada subagente el ámbito de su tarea y solicitar aclaración cuando no esté claro.

- Mantener memoria general persistente para preferencias y hechos compartidos, memoria y notas independientes por proyecto, y contexto temporal para la tarea activa con una ventana breve de acciones recientes y sus resultados; recuperar solo la información pertinente a cada ámbito y evitar mezclar recuerdos de proyectos distintos.

- Registrar en SQLite las acciones, estados, fechas y referencias a los resultados, con una herramienta para recuperar detalles.

- Mantener en SQLite tres documentos lógicos: el perfil del agente con su identidad y personalidad, el perfil del usuario con sus preferencias y forma de comunicación, y un bloc de notas con apuntes generales y por proyecto. Contar con una herramienta para leerlos y editarlos por fragmentos sin cargarlos completos en el contexto; comprobar que el fragmento no haya cambiado antes de aplicar la edición. Los recuerdos y sus relaciones, tareas y recordatorios se mantienen como registros consultables; las reglas básicas viven en la configuración del arnés y las skills conservan sus archivos SKILL.md.

- Al compactar el contexto, conservar lo necesario para continuar y preservar literalmente las instrucciones importantes.

- Organizar el contexto en capas para aprovechar la caché de entrada: primero las instrucciones base y herramientas permanentes; después las preferencias pertinentes del usuario, el contexto estable del proyecto activo, los requisitos, planes y recuerdos de la tarea, y por último la conversación y actividad reciente. Mantener primero lo más estable e incluir solo información pertinente.



- Tras cinco días de uso, identificar las horas de menor actividad y realizar una revisión diaria en segundo plano para seleccionar, guardar y compactar hechos, preferencias y relaciones por ámbito; verificar su procedencia y vigencia, vincular acciones y decisiones mediante un grafo contextual y consultar el historial para corregir las incoherencias o contradicciones detectadas.

- Adaptar los tipos de relaciones de la memoria a las necesidades y al contexto de cada usuario, como su área de trabajo o investigación.

- Permitir una **fecha de caducidad opcional** para recuerdos temporales; al vencer, dejar de tratarlos como vigentes y conservar su historial y procedencia.

- Recuperar recuerdos y mensajes mediante **búsqueda léxica, semántica, temporal y relacional**, combinándolas según la consulta: palabras exactas, significado, fechas y relaciones. Conservar la fuente y el vínculo al mensaje y su conversación; señalar la incertidumbre ante ambigüedades o contradicciones y comprobar que exista evidencia suficiente.

- Almacenar la mayoría de los recuerdos como texto en **SQLite**, junto con sus metadatos, fuentes y relaciones estructuradas por identificador, formando un grafo consultable y corregible desde una interfaz. Registrar cuándo se guardó cada recuerdo y cuándo ocurrió o fue válido el hecho, cuando se conozca. Usar un índice léxico y **sqlite-vec** para la búsqueda semántica de los recuerdos y fragmentos relevantes, generando sus vectores mediante un proveedor de embeddings y conservando el texto original; los identificadores, fechas y relaciones se consultan como datos estructurados.



#### Propuestas

- Construir una **memoria en grafo del código, los archivos y los documentos**, que represente sus relaciones y dependencias para facilitar su consulta y comprensión.

#### Dudas

### Skills y aprendizaje

#### Base

- Los nombres y el contenido de las skills deben estar en inglés.
- Mantener en RAM un índice de skills con nombre, descripción y ubicación; seleccionar las pertinentes para enviarlas al contexto y actualizar el índice cuando cambien.

- Catálogo, búsqueda y carga de skills bajo demanda: incluir inicialmente en el contexto una lista con el nombre y una breve descripción de cada skill; el agente decide cuáles consultar y utilizar. Cuando necesite averiguar qué skills hay disponibles, utilizará la herramienta de búsqueda de skills.
- Aprendizaje selectivo de skills con revisión, mantenimiento y reversión, incluyendo la creación de skills para tareas repetitivas.
- Al delegar, el agente maestro podrá sugerir skills si sabe cuáles son pertinentes, sin estar obligado a asignarlas; cada subagente podrá buscar y cargar las skills que necesite. Las instrucciones obligatorias se transmitirán explícitamente.

#### Propuestas

- Mejorar automáticamente las skills a partir de la experiencia del agente cuando esté seguro de que el cambio mejorará su desempeño en un ámbito concreto.

- Buscar nuevas **skills especializadas para distintas áreas**, como biología, química y física.

- Evaluar en qué partes del arnés podrían ser de utilidad modelos de decisión como **Jev de TypeSafe AI** o **Perplexity Decider**.

- Organizar las **skills en grupos especializados** y proporcionar a cada subagente los grupos pertinentes para su tarea, presentando las skills agrupadas en lugar de un listado plano.

#### Dudas

#### Skills principales

| Grupos | Descripción | Skills |
| --- | --- | --- |
| Planificación | Definir los requisitos y el diseño antes de implementar. | • Entrevistar al usuario para aclarar requisitos, dudas, ambigüedades y datos faltantes. • Explorar y refinar ideas hasta convertirlas en propuestas concretas. • Elaborar una especificación con los objetivos, requisitos, estructura, pruebas y límites, incluyendo las pantallas, componentes, interacciones y estados necesarios. • Definir y documentar los criterios de calidad y restricciones del proyecto, y comprobar su cumplimiento. • Dividir la especificación en tareas pequeñas y verificables, con criterios de aceptación, dependencias y contenido mínimo del plan. |
| Ingeniería de software | Analizar código y trabajar con plataformas de desarrollo y despliegue, como GitHub y Vercel. | • Problemas e issues de GitHub. • Consultar los identificadores de commit (SHA) locales y de GitHub y comparar sus diferencias para verificar qué cambios se realizaron y cuáles están publicados. • Usar Git worktrees. • Identificar cómo se relacionan y llaman entre sí las funciones del código. • Buenas prácticas en los distintos frameworks de backend, frontend y móvil. • Automatización de DevOps. • Ingeniería de redes. • Optimización de bases de datos. • Implementar cambios de forma incremental en partes pequeñas, verificables y fáciles de revertir. • Consultar documentación oficial para decisiones sobre frameworks y bibliotecas, identificando lo no verificado. • Revisar críticamente las decisiones importantes durante el desarrollo y contrastar las dudas con evidencia. • Diseñar APIs y límites entre módulos con contratos claros, manejo de errores y validación de entradas. • Revisar la calidad del código y clasificar los hallazgos según su gravedad. • Simplificar código sin alterar su comportamiento. • Medir y optimizar el rendimiento antes y después de los cambios. • Gestionar versiones y commits coherentes con los cambios realizados. • Automatizar integración y despliegue continuos con verificaciones y manejo de fallos. • Planificar migraciones y retirada de funcionalidades obsoletas. • Registrar decisiones de arquitectura y sus motivos. • Instrumentar aplicaciones con registros, métricas, trazas y alertas. • Preparar lanzamientos con comprobaciones, despliegue gradual, seguimiento y reversión. • Crear herramientas. • Crear skills. |
| Memoria y relaciones | Organizar y recuperar el contexto de los usuarios y sus interacciones mediante relaciones en memoria. | • Definir cómo representar, actualizar y consultar en grafos las interacciones entre usuarios y su contexto, respetando los permisos de acceso. • Seleccionar y proporcionar a cada agente el contexto pertinente en el momento adecuado. |
| Finalización y verificación de requisitos | Comprobar el estado del trabajo y sus verificaciones antes de darlo por terminado. | • Antes de finalizar, verificar que el resultado cumpla todos los requisitos de la planificación inicial y que todas las pruebas de la carpeta de tests se hayan ejecutado y pasado correctamente. • Errores comunes. |
| Comunicación y retroalimentación | Dar respuestas útiles y fundamentadas al usuario. | • Al comunicar una revisión o responder a comentarios, evitar elogios y acuerdos automáticos como «tienes toda la razón» o «excelente observación»; aportar comentarios concretos, razonados y orientados a mejorar el resultado. • Preferencias del usuario. • Consultar las preferencias del usuario para informes y comunicaciones, incluidas las estructuras o formatos que ya utilice. • Generar respuestas claras y naturales para el modo de conversación por voz en vivo con el usuario. |
| Subagentes | Delegar y coordinar tareas entre agentes especializados. | • Identificar los requisitos relacionados y asignar cada conjunto a un subagente, con el contexto necesario, organizando el trabajo en ramas o flujos de GitHub según corresponda. (Investigar cómo permitir que trabajen en paralelo.) • Identificar las skills y reglas de trabajo pertinentes para cada tarea. |
| Debug | Investigar errores y determinar sus causas antes de corregirlos. | • Identificar y comprobar la causa del error antes de proponer soluciones. • Diagnosticar errores: reproducir sistemáticamente el problema, examinar sus posibles causas e identificar la causa raíz. • Localizar y aislar fallos, corregirlos y comprobar que no vuelvan a ocurrir. |
| Pruebas | Guiar la implementación mediante pruebas del comportamiento esperado. | • Aplicar desarrollo guiado por pruebas (TDD): escribir primero los tests y comprobar que fallan por el comportamiento pendiente; después implementar el código necesario para que pasen. |
| Diseño | Aplicar buenas prácticas de diseño de interfaces. | • Analizar el encargo, el producto y su identidad visual para definir una dirección de diseño. • Consultar un catálogo de estilos visuales y ejemplos. • Elegir paletas de colores y combinaciones tipográficas según el producto. • Aplicar pautas de experiencia de usuario, accesibilidad, distribución e interacciones. • Consultar recomendaciones de diseño según la tecnología utilizada. • Generar y documentar el contexto del producto y su sistema de diseño, incluidos estilos, colores, tipografía y componentes reutilizables. • Seleccionar y utilizar imágenes adecuadas, respetando sus licencias y créditos. • Diseñar interfaces web, aplicaciones y dashboards según sus necesidades. • Incorporar animaciones apropiadas, con soporte para movimiento reducido. • Revisar visualmente la interfaz en un navegador y en distintos tamaños de pantalla, identificar problemas y corregirlos. • Evaluar y mejorar jerarquía visual, accesibilidad, adaptación a dispositivos y rendimiento. • Ajustar colores, tipografía, distribución, textos y animaciones para mejorar la interfaz. • Diseñar la arquitectura de componentes y la gestión de estado en interfaces. |


#### Skills secundarios

| Grupos | Descripción | Skills |
| --- | --- | --- |
| Documentos | Trabajar con documentos, hojas de cálculo, presentaciones y PDF. | • Documentos de Google. • Excel. • Diapositivas. • PDF. • Actualizar la documentación según los cambios realizados. |
| Marketing | Planificar y ejecutar tareas de marketing según el producto, su público y posicionamiento. | • Redactar textos, correos y contenido para redes sociales. • Optimizar SEO y páginas para buscadores. • Planificar campañas publicitarias y analizar resultados. • Investigar clientes y competidores. • Diseñar precios, ofertas y estrategias de crecimiento. • Mejorar registros, formularios y conversión de visitantes. • Elaborar planes de marketing y crear contenido visual o video. |
| Investigación científica | Apoyar investigaciones en distintos campos científicos. | • Redacción científica y preparación de gráficas y presentaciones de resultados. • Formular y poner a prueba hipótesis. • Planificar y coordinar ciclos de investigación, experimentos y refinamiento de hipótesis. • Preparar, filtrar y deduplicar datos para entrenamiento y evaluación. • Diseñar y comparar arquitecturas y tokenizadores de modelos. • Entrenar y ajustar modelos, incluyendo entrenamiento distribuido y métodos eficientes. • Investigar aprendizaje por refuerzo y alineamiento con preferencias. • Optimizar modelos e inferencia mediante cuantización, poda, destilación y decodificación especulativa. • Evaluar modelos y agentes con benchmarks reproducibles y métricas de calidad, costo y rendimiento. • Analizar el funcionamiento interno de modelos mediante activaciones e intervenciones. • Investigar búsqueda semántica y generación aumentada por recuperación (RAG). • Optimizar instrucciones y generar respuestas estructuradas. • Registrar y comparar experimentos, versiones, métricas y trazas, conservando evidencia y procedencia. • Investigar modelos multimodales y políticas para robótica. • (Investigar en cuántas skills conviene dividir estas capacidades y cómo utilizarlas.) |
| Seguridad | Auditar código y sistemas para detectar y verificar vulnerabilidades. | • Analizar la arquitectura, las entradas y los límites de confianza. • Delegar la búsqueda de vulnerabilidades a subagentes especializados. • Verificar cada hallazgo con un agente independiente. • Generar informes que distingan hallazgos confirmados, pendientes de validación y descartados. • Revisar seguridad de aplicaciones web, autenticación, agentes de IA, dependencias, infraestructura y aislamiento de datos. • Auditar la seguridad de skills antes de instalarlas, analizando archivos, carpetas, repositorios o paquetes para detectar inyección de instrucciones, filtración de datos, escalada de privilegios, código peligroso, uso indebido de herramientas, riesgos en dependencias y servidores MCP; combinar análisis estático con revisión semántica opcional mediante un modelo, consultar vulnerabilidades conocidas cuando corresponda y generar un informe con hallazgos, gravedad y recomendaciones, permitiendo revisar falsos positivos. • Aplicar protección de autenticación, gestión de secretos, validación de entradas y revisión de dependencias. |
| Google Cloud | Trabajar con productos y tecnologías de Google. | • Crear y desplegar agentes de IA y utilizar Gemini y otros modelos. • Administrar infraestructura en Google Cloud y Kubernetes. • Gestionar bases de datos y servicios como BigQuery. • Revisar seguridad, permisos, registros y monitoreo. • Utilizar herramientas de desarrollo y Google Ads. |
| Cumplimiento normativo |  |  |
| Finanzas |  |  |
| Navegador | Buscar información en internet y consultar fuentes públicas. | • Realizar búsquedas en GitHub, Hacker News, arXiv, Polymarket, DEV Community, Techmeme, Digg y YouTube; investigar más fuentes. • Inspeccionar elementos, consola, solicitudes de red y rendimiento de una página mediante herramientas de desarrollo. |

#### Estructura de las skills

Las skills de Excalibur deben seguir la [especificación Agent Skills](https://agentskills.io/specification):

```text
nombre-de-la-skill/
├── SKILL.md       # Obligatorio: metadatos e instrucciones
├── scripts/       # Opcional: código ejecutable
├── references/    # Opcional: documentación de referencia
├── assets/        # Opcional: plantillas y recursos estáticos
└── ...            # Otros archivos o directorios, si se necesitan
```

`SKILL.md` debe contener metadatos YAML entre delimitadores `---`, seguidos de las instrucciones en Markdown:

```markdown
---
name: nombre-de-la-skill
description: Qué hace la skill y cuándo debe utilizarse.
license: Licencia o archivo de licencia (opcional)
compatibility: Requisitos del entorno (opcional)
metadata:
  author: Autor (opcional)
  version: "1.0"
allowed-tools: Herramientas autorizadas (opcional y experimental)
---

# Instrucciones

Pasos, ejemplos y casos especiales necesarios para ejecutar la tarea.
```

`name` y `description` son obligatorios. El nombre debe coincidir con la carpeta, tener entre 1 y 64 caracteres y usar letras minúsculas, números y guiones, sin guiones iniciales, finales ni consecutivos. La descripción debe tener entre 1 y 1024 caracteres e indicar qué hace la skill y cuándo utilizarla; `compatibility`, si se incluye, admite hasta 500 caracteres. Los demás campos son opcionales y el soporte de `allowed-tools` puede variar entre implementaciones.

### Herramientas principales y software

- Los nombres y el contenido de las herramientas deben estar en inglés.
- Mantener en RAM un catálogo de las herramientas y sus definiciones; seleccionar cuáles enviar al contexto y actualizar el catálogo cuando cambien.

- Crear siempre una **carpeta de tests o scripts con un README**, incluyendo un checklist que registre si cada prueba se ejecutó y pasó correctamente.

- Mantener un historial local de las ediciones del agente para restaurar cambios puntuales, incluso sin Git, y usar Git local para versionar proyectos.

- Incorporar control de concurrencia y bloqueo de escritura para impedir que varios agentes editen simultáneamente el mismo archivo.

- Edición de código con parches y navegación de símbolos mediante LSP configurable.

- Terminal interactiva para ejecutar comandos y programas con interfaz de texto, visualizar su salida en tiempo real, enviarles texto y teclas durante la ejecución, mantener procesos activos entre interacciones y cerrarlos o interrumpirlos cuando corresponda.

- Detección del entorno y selección del intérprete de comandos.

#### Herramientas principales

| Herramienta | Descripción |
|---|---|
| Enviar mensaje | Enviar mensajes al usuario. |
| Dejar nota de continuación | Guardar una nota breve con el siguiente paso o contexto relevante para incorporarla a la siguiente llamada del agente cuando sea necesario. |
| Pedir permiso al usuario | Solicitar autorización al usuario para una acción. |
| Preguntar al usuario | Solicitar aclaraciones o decisiones necesarias para continuar una tarea. |
| Búsqueda en internet | Buscar información en la web. |
| Leer página web | Consultar el contenido de una página mediante su URL. |
| Leer archivo | Leer el contenido de un archivo. |
| Editar archivo | Modificar el contenido de un archivo. |
| Escribir archivo | Crear o escribir un archivo. |
| Aplicar parches | Aplicar parches con múltiples operaciones sobre archivos. |
| Buscar texto | Buscar texto dentro de archivos, como grep. |
| Buscar archivos | Buscar archivos mediante patrones, como glob. |
| Buscar skills | Buscar skills disponibles según la tarea y devolver sus datos para consultarlas. |
| Ejecutar comandos | Ejecutar comandos y gestionar procesos interactivos: consultar su salida, enviar texto, interrumpirlos o cerrarlos. |
| Ejecutar subagentes | Delegar tareas a subagentes, consultar su estado, comunicarse con ellos e interrumpirlos; investigar más a fondo su funcionamiento. |
| Gestionar tareas, requisitos y verificaciones | Consultar y actualizar tareas y requisitos, y registrar resultados y evidencias mediante operaciones agrupadas sobre SQLite. |

#### Propuestas

- Comparar el uso exclusivo de la terminal con la combinación de terminal y herramientas específicas de lectura, búsqueda y edición, empleando las mismas tareas y modelo y midiendo éxito, tokens, tiempo y errores.

#### Herramientas secundarias

| Herramienta | Descripción |
|---|---|
| Gestionar recordatorios | Crear y gestionar recordatorios. |

#### Dudas


### Interfaz

- La interfaz web, incluidas las skills que se muestran en ella, debe tener soporte para español e inglés.

- Incorporar una **interfaz de hojas de cálculo con soporte para archivos tipo Excel y un panel de chat a la derecha**, donde tanto el usuario manualmente como el agente mediante instrucciones puedan modificar todo el contenido de la hoja de cálculo.

- Contar con una **interfaz para gestionar todos los proyectos**, mostrando el nombre y la descripción de cada proyecto, los agentes vinculados con sus nombres y las skills asociadas. Al acceder a un proyecto, incluir un botón de carpetas para navegar y editar sus archivos en una interfaz tipo Visual Studio Code, reutilizando la misma interfaz de gestión y edición de archivos disponible desde el chat.

- Al hacer clic en una referencia a un archivo en el chat, abrir un **editor de código en la mitad de la pantalla**, manteniendo el chat en la otra mitad. El editor tendrá un botón de carpetas que desplegará un explorador en una cuarta parte de la pantalla, con las carpetas y archivos del proyecto, como al abrir un proyecto en Visual Studio Code. Cada proyecto se registrará en la base de datos con su carpeta raíz y comprenderá todos los archivos dentro de ella; para identificar el proyecto de un archivo, se consultará su ubicación y esa relación. Si el archivo pertenece a varios proyectos según la base de datos, se utilizará el proyecto en el que está trabajando el agente o al que hizo referencia en su último mensaje para abrir automáticamente el árbol correspondiente. (Cuidar el rendimiento de la carga y evitar sobrecargar la interfaz al listar muchos archivos, especialmente carpetas como `node_modules`.)

- La interfaz permitirá **elegir y cambiar el agente personalizado con el que se quiere trabajar**, cada uno con su propio nombre e icono, como complemento de la interfaz 2D del funcionamiento de los agentes; también podrá mostrar un **organigrama** con los agentes, sus roles y tareas.

- Incluir una **interfaz de seguimiento de tokens, consumo y métricas de ejecución**.



- En la interfaz, el **agente interlocutor debe permanecer disponible y recibir nuevos mensajes sin bloquear la conversación**, mientras coordina trabajos en segundo plano; comunicará avances y notificará cuando sea necesario.

- Incorporar una interfaz de **conversación por voz en tiempo real con el agente**.

#### Dudas






#### Visualización de tareas y agentes

- Interfaces visualmente atractivas para consultar las actividades **pendientes, en progreso, terminadas y bloqueadas**, mostrando claramente su estado. Como opción de visualización, evaluar un **tablero tipo Kanban**, con columnas por estado.

- Interfaz visual para observar **en tiempo real los cambios que está realizando el agente**, con diseño adaptable tanto a **móviles como a pantallas grandes**.

- Interfaz elegante para visualizar los subagentes, sus trabajos y estados, y el rol asignado a cada uno cuando corresponda; la existencia de roles o personalidades predefinidos queda por decidir.


#### Fénix — Concepto de interfaz 2D interactiva

##### Visión general

Convertir la interfaz de Fénix en un **videojuego de gestión de agentes**, donde las conversaciones forman parte del mundo en lugar de aparecer únicamente en un chat tradicional.

La idea central es que **el usuario no estaría conversando con una inteligencia artificial dentro de una aplicación; estaría habitando un mundo donde su inteligencia artificial trabaja y vive**.

##### 1. El edificio inteligente

Empiezas con una oficina pequeña, donde Fénix tiene su escritorio. Cuando creas proyectos, aparecen nuevas habitaciones. Cuando lanzas subagentes, aparecen personajes trabajando en sus escritorios.

A medida que aumentan los proyectos, archivos y agentes, el edificio puede crecer: más oficinas e incluso más pisos.

Si escribes una instrucción, Fénix se levanta y empieza a realizar acciones. Si le pides investigar algo, camina hasta una biblioteca. Si está programando, se acerca a una computadora.

El mundo se representa en **2D con pixel art**, para mantener una identidad visual de videojuego y evitar un consumo excesivo de recursos.

##### 2. Conversaciones integradas en el escenario

Esta es la solución propuesta para conservar el historial sin convertir toda la aplicación en una interfaz de chat tradicional.

- **Respuestas breves:** aparece un globo de diálogo encima del personaje de Fénix.
- **Respuestas extensas:** aparece una ventana flotante, como los diálogos de un videojuego RPG.
- **Investigaciones, documentos y código:** el resultado se convierte en un objeto del mundo, como un libro, una pantalla o un expediente que puedes abrir.
- **Mensajes anteriores:** un pequeño reloj en una esquina abre una línea de tiempo con las conversaciones anteriores. Puedes buscar por palabras o fechas y recuperar cualquier mensaje sin abandonar el mundo 2D.

##### 3. Un mundo de misiones

En vez de tener una lista aburrida de tareas, Fénix podría presentar un **tablero de misiones**.

Cada proyecto tiene objetivos, progreso y personajes responsables. Cuando un agente termina su trabajo, aparece una animación y entrega el resultado.

No se trata de dar puntos artificiales, sino de hacer visible el progreso real.

##### 4. Barra de instrucciones inmediata

Al pulsar **Enter** en cualquier momento, se abre una pequeña barra para escribirle a Fénix.

Así puedes darle instrucciones inmediatamente, sin tener que mover tu avatar ni recorrer habitaciones.

El modo voz y la escritura deben convivir; no siempre será conveniente hablar.

##### 5. Ejemplo de interacción

El usuario escribe: «Fénix, investiga los mejores modelos de voz».

Entonces ve cómo un agente entra a la biblioteca, comienza a trabajar, muestra su progreso y finalmente deja un informe sobre el escritorio.

El usuario puede abrirlo, hacer preguntas adicionales o guardarlo.

##### 6. Recomendación de diseño

Combinar **el edificio 2D**, **las conversaciones integradas** y **el sistema de misiones**.

La interfaz debe resultar animada, agradable y cercana a un videojuego, sin perder la utilidad de una herramienta de productividad.

**Principio fundamental:** las animaciones deben ser opcionales. Nunca deberían retrasar una tarea ni obligar al usuario a recorrer oficinas para encontrar información.

El objetivo no es reemplazar la productividad por un juego, sino hacer que el trabajo de los agentes sea visible, comprensible y atractivo.


### Dispositivos y herramientas secundarias

- ¿Es viable mostrar al usuario **dos respuestas alternativas y preguntarle cuál prefiere**, usando esa preferencia como retroalimentación para actualizar y mejorar la skill correspondiente, de modo que futuras ejecuciones de tareas similares respondan mejor? Evaluar cómo validar y conservar las mejoras sin generalizar indebidamente una preferencia puntual.

- Investigar repositorios y tecnologías para crear experiencias web en 3D con JavaScript y explorar qué funcionalidades tridimensionales podrían incorporarse a Excalibur; tener en cuenta también las posibilidades de crear y utilizar avatares 3D con estas tecnologías. Queda como investigación pendiente, sin elegir una implementación.


- Crear y adaptar una **base de datos SQL con tablas y relaciones definidas según las necesidades del usuario**, junto con una interfaz ajustada a esa estructura para gestionar sus datos.

- Permitir la **comunicación entre agentes en representación de sus usuarios**: al pedir «comunícate con Juan», el agente conversará con el agente de Juan. Compartir únicamente información autorizada para ese contacto y propósito, sin exponer datos privados, memoria, documentos sensibles ni credenciales; respetar los límites de representación y solicitar autorización cuando corresponda.

- Incorporar **transcripción de reuniones** de Google Meet, Microsoft Teams, Zoom y otras plataformas.

- El agente podrá **realizar llamadas y videollamadas mediante Google Meet y Microsoft Teams**. (Duda: ¿sería buena idea utilizar un deepfake del usuario como avatar o gemelo digital?)

- Gestionar **Google Workspace, Microsoft 365 y el correo electrónico mediante tareas automatizadas en el navegador**.

- Incorporar **integración con Telegram**.

- El agente podrá abrir en cualquier momento una **pizarra interactiva en la interfaz** para mostrar y explicar contenido al usuario.

- El agente debe **crear herramientas especializadas para los sitios web, aplicaciones de escritorio y otras aplicaciones más utilizadas por el usuario**, cuando resulte conveniente, para reutilizar operaciones concretas sin reconstruir el procedimiento cada vez. Por ejemplo, una herramienta para crear un cubo en una aplicación de modelado 3D. Definir y comprobar las capacidades de cada integración.

- Incorporar **control del dispositivo y de su interfaz**, incluyendo manipular el escritorio y operar aplicaciones en Windows, Linux y macOS, y las capacidades equivalentes disponibles en Android e iOS. Este objetivo se aplica a todos los sistemas compatibles con Excalibur; el alcance efectivo dependerá de los permisos, mecanismos y restricciones de cada plataforma, sin dar por garantizado control universal.






### Navegador y búsqueda web

- **Fuentes consultables en la interfaz general:** después de una búsqueda en Internet, mostrar debajo del resultado un acceso **«Fuentes»**, tomando como referencia la interfaz de ChatGPT. Al hacer clic, abrir un **modal con todas las fuentes utilizadas en esa búsqueda**, detalladas e identificables mediante su título, enlace y la información de procedencia disponible. Requisito de Cristhian del 2026-10-26; conservar el vínculo con la búsqueda correspondiente y no añadir fuentes que el agente no haya consultado.

- **Evaluación del navegador:** medir la calidad y el rendimiento de sus capacidades mediante un benchmark de tareas de navegación. Tomar como referencia «Browser Use Bench V2.1», nombre y versión mencionados por Cristhian pendientes de confirmar, o un benchmark equivalente. Referencia propuesta el 2026-10-24; no implica resultados ya obtenidos ni elección definitiva de la suite.

- **32.** Navegación y automatización web con Chromium.
- **33.** Perfiles de navegador persistentes, pestañas y conservación de sesiones.
- **34.** Capturas, inspección DOM, formularios, subida y descarga de archivos en el navegador.
- **Información visual y estructural para el agente:** independientemente de la tecnología que se elija para el navegador de Excalibur, este debe permitir al agente tomar capturas de pantalla y obtener el árbol DOM completo de la página, con su jerarquía y elementos, incluyendo una lista identificable de botones, enlaces, campos de formulario y demás controles. El agente debe disponer tanto de la representación visual como de la estructura DOM para comprender la página y actuar sobre sus elementos. Requisito indicado por Cristhian el 2026-10-25.
- **35.** Emulación de tamaños de pantalla y comprobaciones de páginas web.
- **36.** Gestión de inactividad y capacidad de navegadores.
- **37.** Búsqueda web con índice propio, rastreador y lectura HTTP ligera.
- **Deep Research (investigación profunda):** realizar búsquedas iterativas, consultar y contrastar múltiples fuentes, profundizar en los hallazgos y entregar un informe con citas, conclusiones y limitaciones de la evidencia.
- Automatizar tareas repetitivas mediante secuencias predefinidas de clics y operaciones sobre botones y formularios cuya ubicación e interacción ya se conocen.
- Incorporar un **baúl de claves** para las credenciales de acceso utilizadas por el navegador.
- Permitir operar el navegador **con interfaz gráfica y sin interfaz gráfica**, compartiendo las credenciales de login ya utilizadas mediante el baúl de claves.
- Gestionar los **CAPTCHA** dentro del flujo de navegación, detectándolos y permitiendo su resolución por el usuario o mediante mecanismos admitidos por el sitio, para poder continuar la tarea.
- Crear **desde cero una API propia de búsqueda de información en Internet**, sin depender de Brave Search ni de otras APIs de búsqueda externas; estas se mencionaron únicamente como referencia del tipo de funcionalidad. La búsqueda no debe requerir automatizar un navegador para cada consulta.
- Comparar las alternativas de navegación y búsqueda midiendo **velocidad, consumo de RAM y precisión de los resultados**.

- **Esperar la carga antes de actuar:** el agente debe comprobar que el contenido necesario de la página ha terminado de cargar, incluidos los elementos dinámicos relevantes, antes de inspeccionarlo, capturarlo o interactuar con él, para evitar decisiones y acciones basadas en una vista incompleta. El criterio de carga y disponibilidad se deberá definir y verificar durante el desarrollo. Requisito indicado por Cristhian el 2026-10-25.

#### Dudas

- ¿Cómo construir un buscador que encuentre las webs relacionadas con cada consulta del usuario, mediante un índice propio o algún mecanismo alternativo que no requiera necesariamente ese índice? Investigar cómo descubrir las fuentes relevantes y lograr la mayor cobertura posible, sin dar por garantizado que se encontrarán todas las webs existentes. Duda planteada por Cristhian el 2026-10-25.

- ¿Cómo permitir mantener abiertas varias interfaces web simultáneamente y gestionar su contexto, sus pestañas o ventanas y el cambio entre ellas? Definir el comportamiento y evaluar consumo de recursos y continuidad de las tareas. Duda de Cristhian del 2026-10-25.

- Investigar qué **fuentes abiertas permiten realizar búsquedas de información en Internet sin requerir una clave de API**, para utilizarlas inicialmente en la búsqueda del navegador de Excalibur. Duda planteada por Cristhian el 2026-10-24.

- ¿La integración del navegador se ofrecerá mediante CLI o MCP?
- ¿El navegador será un componente separado con su propio compilado o estará integrado en el agente?
- ¿Se utilizará Chromium o se elegirá otra alternativa según el consumo de RAM?
- ¿Cómo se almacenarán, compartirán y gestionarán las cookies y las sesiones, incluyendo su persistencia, caducidad y uso entre los modos con y sin interfaz?
- ¿Cómo se prevendrán y tratarán vulnerabilidades como **XSS** y otras amenazas al navegador, las sesiones y el baúl de claves?
- ¿Qué modelo con visión, resolución y recorte convienen para interpretar capturas del navegador y extraer su texto, midiendo costo, latencia y precisión? Se descarta la duda anterior sobre leer capturas sin enviarlas al modelo, por decisión de Cristhian del 2026-10-23.

### Documentos, lectura visual y creación visual

- Contar con **skills para crear diagramas** de arquitectura, flujos de trabajo, secuencia, flujo de datos, ciclo de vida y estados, entidad-relación, clases, árboles o jerarquías, líneas de tiempo y waterfall (duración y superposición de procesos).

- Disponer de las **skills necesarias para trabajar con las aplicaciones y formatos de Office y para generar y editar diagramas**, en conexión con las funcionalidades de documentos y creación visual.

- **38.** Biblioteca privada de documentos, originales y versiones.
- **39.** Extracción de texto de PDF e imágenes mediante un modelo con visión.
- **40.** Fragmentación documental, citas y búsqueda léxica o híbrida con embeddings opcionales.
- Al responder con información de nuestros documentos, **citar obligatoriamente el documento de origen y la página exacta** donde se encontró; incluir también el párrafo cuando sea posible. Conservar la referencia a la fuente durante la extracción y fragmentación. Si el formato no tiene paginación, indicarlo y proporcionar una ubicación verificable equivalente, sin inventar números de página.
- **41.** Creación, lectura y edición de documentos Word, hojas de cálculo y presentaciones.
- **42.** Creación y edición de diagramas, gráficas, diseños y artefactos visuales.
- La generación de diagramas debe abarcar **la mayor variedad posible de tipos**, incluidos diagramas de flujo y de procesos; ampliar la cobertura a otros tipos según las necesidades del usuario y comprobar su soporte efectivo. Es una funcionalidad requerida, no solo una referencia. Precisión de Cristhian del 2026-10-24.
- **43.** Explorador de archivos y visores de documentos, imágenes y multimedia.
- Contar con una **interfaz general de archivos** donde el usuario pueda **listar y consultar los archivos creados por el agente y otros archivos de su espacio de trabajo**. Amplía la funcionalidad del explorador existente; organización y alcance por definir. Requisito de Cristhian del 2026-10-26.

- Excalibur debe poder comprender todos los tipos de archivos como objetivo de cobertura, incluyendo texto, documentos, imágenes y **audio**. Los formatos y su soporte efectivo deberán definirse y comprobarse durante el desarrollo; no se da por implementada compatibilidad universal.
- Para leer imágenes, capturas y el contenido visual de documentos, **enviar la imagen o las páginas del documento al modelo con visión correspondiente para que extraiga el texto y el contenido necesario**. Por decisión de Cristhian del 2026-10-23, esta vía sustituye la propuesta de integrar un motor OCR dedicado.
- Incorporar **conversión entre tipos y formatos de archivos**; los formatos compatibles deberán definirse y verificarse.
- Excalibur debe poder **editar correctamente archivos PDF, documentos Word, hojas de cálculo Excel y presentaciones de diapositivas**.
- Contemplar **soporte para las aplicaciones y formatos de toda la suite Office**, con las skills correspondientes. Word, Excel y presentaciones ya están incluidos; concretar y verificar progresivamente el soporte de las demás aplicaciones y formatos. Esta ampliación solicitada por Cristhian el 2026-10-24 no implica distribuir la suite dentro de Excalibur ni afirmar compatibilidad universal ya implementada.

#### Dudas

- ¿Sería buena idea incorporar un historial de versiones de documentos, similar al de Documentos de Google?


### Automatización y programación de tareas

- Cada bloque o parte de un flujo puede ejecutar **scripts en cualquier lenguaje de programación** (**también pueden funcionar modelos de visión artificial**).

- Incorporar **gestión de reintentos en tareas autónomas y programadas**, definiendo cómo tratar los fallos y volver a intentar la ejecución sin duplicar sus efectos. Los límites y las políticas concretas quedan por definir. Requisito de Cristhian del 2026-10-24.
- Investigar y comparar **metodologías para tareas autónomas y programadas de larga duración, incluso de varios días**, contemplando persistencia del progreso, recuperación tras interrupciones y reanudación fiable. La metodología concreta queda por elegir. Requisito de Cristhian del 2026-10-24.

- **44.** Recordatorios, cron y tareas programadas.
- Activación o reanudación del agente mediante **hooks o disparadores de eventos configurables**, por ejemplo, al recibir un correo electrónico o un mensaje de WhatsApp, para iniciar o continuar las tareas correspondientes dentro de los permisos establecidos.
- **45.** Workflows durables con pasos, condiciones, versiones y pruebas sin efectos.
- Mostrar visualmente las automatizaciones mediante flujos de nodos y conexiones, tomando como referencia la representación de n8n, para ver los pasos, condiciones y relaciones de cada automatización.

#### Dudas
- **Prioridad crítica, pendiente de definir:** ¿Cómo crear una metodología de simulación textual de soluciones y flujos en múltiples escenarios, descritos en lenguaje natural como recorridos de un diagrama de flujo, sin código ni pseudocódigo, para explorar su comportamiento antes de implementar?
- ¿Cuánto se puede compactar la representación textual de esos flujos sin perder condiciones, decisiones, dependencias, resultados ni cobertura de escenarios, y cómo reutilizar la metodología en distintos tipos de soluciones?
- ¿La simulación mediante diagramas de flujo textuales podría adoptar una metodología de **fuerza bruta** que explore todos los escenarios posibles dentro de un alcance definido? Evaluar la cobertura, los límites y el crecimiento de combinaciones antes de afirmar exhaustividad.


### Permisos, identidad, seguridad y sandboxes

- El **baúl de secretos** contará con una herramienta para que el agente gestione secretos clasificados por tipo, incluyendo **claves API, direcciones IP y contraseñas de sitios web**.

- Mantener un **registro de auditoría** para consultar qué hizo el agente y qué herramientas utilizó.

- Excalibur debe utilizar **mecanismos de aislamiento y restricciones de procesos del propio sistema operativo** para gestionar y limitar las operaciones de los procesos que ejecuten el agente y sus herramientas, y controlar explícitamente a qué carpetas y archivos pueden acceder, distinguiendo permisos de lectura y escritura. Las restricciones deben aplicarse a la ejecución real y a los procesos secundarios correspondientes, mediante las capacidades disponibles en cada plataforma; los mecanismos concretos y su cobertura quedan por definir y verificar.

- **11.** Autonomía configurable y aprobaciones textuales de acciones de riesgo.
- **48.** Acceso personal del propietario, perfil, contraseña y gestión de sesiones.
- **49.** Protección de credenciales, ocultación de secretos y tratamiento de contenido no confiable.

- Incorporar controles de seguridad en la carga y el procesamiento de archivos para **detectar y bloquear archivos maliciosos**, evitando que se acepten o ejecuten sin validar.
- Excalibur debe contar con un sistema para **gestionar los permisos del agente**.
- Debe existir una funcionalidad para **pedir permiso al usuario**, permitiéndole autorizar o rechazar la acción antes de ejecutarla cuando requiera aprobación.
- Los niveles de permisos y las acciones que requieren autorización quedan por definir.
- Crear **sandboxes para distintos tipos de escenarios**, con aislamiento y permisos adecuados a cada caso de uso. Los escenarios concretos y los mecanismos de aislamiento quedan por definir.

#### Equipos y recursos compartidos

Los usuarios podrán crear o unirse a equipos. El líder actuará como anfitrión, aportará la máquina y asumirá el consumo de tokens; podrá definir qué herramientas, proveedores y cuotas comparte con el equipo, sin exponer sus claves de acceso.

#### Dudas


### Tienda de herramientas, flujos y automatizaciones

- Debe haber **dos interfaces separadas**: una para la **tienda de herramientas** y otra para la **tienda de flujos de automatización**. Decisión explícita de Cristhian del 2026-10-24.

- Desde la **misma interfaz de Excalibur**, el agente podrá **instalar cualquier elemento disponible en la tienda**, **crear flujos de automatización y herramientas propios** y **publicarlos en la tienda correspondiente**. Debe **solicitar y obtener permiso del usuario antes de instalar, crear o publicar**. Requisito indicado por Cristhian el 2026-10-26; describe capacidades previstas, todavía no implementadas.

#### Crítico

- Incorporar una **herramienta de simulación o pizarra digital** para las interfaces que la necesiten (**Aula de cursos personalizados**). Requisito indicado por Cristhian el 2026-10-24.

Cada herramienta debe tener en el encabezado de su interfaz un **icono de chat** que abra un **panel lateral desde la derecha** con el chat del agente de esa herramienta. Desde allí el usuario podrá preguntar y dar instrucciones con el contexto propio de la herramienta, su estado actual y las acciones y resultados anteriores, conservando la continuidad de la conversación. El panel debe ser adaptable a móvil y pantallas grandes. Requisito indicado por Cristhian el 2026-10-24.

#### Flujos

##### Investigación y decisión multiagente

Flujo generalista que define la decisión, los criterios y las restricciones; reparte la investigación entre especialistas adaptados al encargo; reúne evidencia y pruebas, compara alternativas en condiciones equivalentes y contrasta conclusiones entre agentes que defienden propuestas y buscan fallos o evidencia contraria. Puede volver a investigar vacíos detectados, con un **número máximo de rondas de discusión entre agentes**, y un coordinador entrega una recomendación justificada con fuentes, incertidumbres y pruebas pendientes, o indica que la evidencia es insuficiente; las pruebas deben pesar más que el consenso. Incorporado por petición de Cristhian el 2026-10-24.

##### Búsqueda y postulación laboral

Flujo automatizado que conoce y confirma el perfil del candidato (CV, experiencia, habilidades, puestos, ubicación, salario y condiciones no aceptadas), busca ofertas en las fuentes aprobadas, descarta duplicadas y vencidas, y compara requisitos con la experiencia real explicando fortalezas, carencias y restricciones. Adapta CV, carta y respuestas sin inventar experiencia, utiliza la herramienta para generar PDFs y realiza una revisión de coherencia y fidelidad mediante otro agente, con un máximo de rondas de corrección. Presenta la candidatura para aprobación, envía solo con autorización del usuario y registra la confirmación del envío; después sigue los resultados, prepara entrevistas y recordatorios, aprende de las preferencias y resultados y propone formación ante carencias recurrentes. Sus tareas individuales también podrán ofrecerse como herramientas reutilizables. Solicitado por Cristhian el 2026-10-24.

#### Skills

#### Herramientas

##### Inteligencia de negocios

Herramienta para analizar y visualizar información y gestionar la gobernanza de datos, con una interfaz para administrar bases de datos y consultar sus tablas y contenido. (Falta investigar qué otras funcionalidades podría incluir esta herramienta de inteligencia de negocios.)

##### Entrenamiento de modelos con GPU

Herramienta para **entrenar modelos y algoritmos utilizando GPU**, con acceso a **distintos proveedores de GPU**: el usuario configura su **clave API del proveedor y la GPU que utilizará**, y el agente le pregunta **qué modelo quiere entrenar**, gestiona el entrenamiento y muestra **métricas de su evolución y resultados**. Los proveedores, tipos de modelos y métricas compatibles quedan por definir y verificar. **(Pendiente: ver cómo será la interfaz de esta herramienta.)** Requisito de Cristhian del 2026-10-26.

##### Simulación

Herramienta específica con interfaz propia para crear, ejecutar y visualizar simulaciones mediante el agente, contemplando simulaciones físicas, químicas y de otras áreas que resulte viable cubrir. Debe permitir definir el escenario, ajustar sus parámetros y observar los resultados, con el chat contextual previsto para las herramientas. Queda **pendiente investigar en qué áreas se podrá realizar simulación con esta herramienta**, qué motores, modelos y skills serán necesarios y cuáles serán sus capacidades y límites; los ámbitos mencionados son objetivos por evaluar, no compatibilidad ya implementada. Requisito indicado por Cristhian el 2026-10-25.

##### Generador de imágenes y diagramas

Herramienta con interfaz propia para generar y editar imágenes y la mayor variedad posible de diagramas, incluidos diagramas de flujo, procesos, arquitectura, secuencia, estados y relaciones de bases de datos, mediante instrucciones al agente y visualización del resultado en el área de trabajo. Debe incorporar el chat contextual lateral previsto para las herramientas y permitir guardar y exportar las creaciones; los tipos y formatos compatibles se concretarán durante el desarrollo. Como posibilidad aún no confirmada, explorar la generación de representaciones gráficas del código, la estructura y las relaciones de repositorios grandes; su alcance y metodología quedan pendientes de definir. Requisito y posibilidad indicados por Cristhian el 2026-10-25.

##### Modelado 3D

Herramienta con interfaz propia de modelado y edición 3D, con una experiencia de trabajo tipo AutoCAD 3D y herramientas básicas para crear figuras y sólidos, seleccionar, mover, rotar, escalar, duplicar y eliminar objetos, editar dimensiones y trabajar con unidades, coordenadas, rejilla y vistas del modelo. También debe servir para elaborar planos arquitectónicos y diseñar interiores, incluyendo la distribución de espacios y la ubicación de mobiliario en el modelo. Debe permitir edición manual y mediante instrucciones al agente sobre el mismo proyecto, mostrando los cambios en el visor y permitiendo deshacer, rehacer, guardar y volver a abrir el modelo. Es **crítico que el agente pueda manipular directamente las funciones del editor**, creando y modificando geometría y sus propiedades mediante herramientas especializadas y las skills correspondientes, con conocimiento del estado del proyecto; el chat contextual estará accesible desde el encabezado en el panel lateral derecho previsto para las herramientas. Debe estudiarse Blender y otras herramientas que compitan con él como referencias para esta herramienta, quedando pendiente evaluar sus funcionalidades y posibilidades de integración. Requisito indicado por Cristhian el 2026-10-25.

##### Editor de vídeo con línea de tiempo

- **Crítico:** investigar **cómo funciona Higgsfield**, sus herramientas y flujos de edición y generación de vídeo, para tomarlo como referencia al definir las capacidades y la experiencia de este editor. Investigación solicitada por Cristhian el 2026-10-26, todavía no realizada.

Herramienta con interfaz propia para editar manualmente y mediante el agente sobre el mismo proyecto: biblioteca de archivos y carpetas a la izquierda, vista previa central, propiedades del elemento seleccionado a la derecha, línea de tiempo multipista por segundos, minutos y fotogramas abajo, y chat contextual del agente accesible desde el encabezado. Debe importar vídeos, imágenes, audio y subtítulos, configurar resolución, orientación y fotogramas por segundo, guardar automáticamente y conservar versiones recuperables; permitir cortar, dividir, mover, duplicar, agrupar y ordenar clips, ajustar duraciones, bloquear, ocultar o silenciar pistas y deshacer o rehacer. Incluir recorte y reencuadre, posición, escala, rotación, estabilización, exposición, balance de blancos, corrección de color, máscaras, desenfoques y seguimiento de objetos; transiciones, capas superpuestas, pantalla dividida, animaciones por fotogramas clave, cámara lenta, aceleración y reproducción inversa. Para audio: formas de onda, sincronización, volumen por pista, fundidos, reducción de ruido, ecualización, música que baje durante la voz y grabación de narración; para texto: títulos, rótulos, estilos reutilizables, transcripción con marcas de tiempo, subtítulos editables y traducibles y edición del montaje desde la transcripción. El agente debe entender el objetivo, analizar el material, proponer y ejecutar un montaje, localizar momentos, quitar silencios, generar resúmenes, subtitular y adaptar formatos, mostrando su progreso y dejando todos sus cambios visibles y editables en la línea de tiempo. Cuando existan modelos disponibles, permitir generación de recursos, narración o escenas, eliminación de fondos, sustitución de elementos y extensión de tomas, indicando requisitos, limitaciones y costes y respetando el presupuesto autorizado. Debe conservar originales, comparar versiones, detectar archivos ausentes, pantallas negras, textos fuera de encuadre y problemas de audio, y exportar vídeo y subtítulos con formato, resolución y calidad configurables manteniendo el proyecto editable. También se podría considerar generar vídeos a partir de composiciones HTML, CSS y animaciones renderizadas fotograma a fotograma, tomando como referencia [Hyperframes](https://github.com/heygen-com/hyperframes); su integración queda por evaluar. Prioridad inicial: línea de tiempo, audio, textos, subtítulos, chat y exportación; los retoques generativos complejos se incorporarán después. Se mantiene como herramienta separada del alcance inicial del Copiloto de diseño. Descripción ampliada por petición de Cristhian el 2026-10-25.

##### Copiloto de diseño

Herramienta con interfaz propia para diseñar el frontend de páginas y aplicaciones web, presentaciones de diapositivas e imágenes, con una galería inicial de plantillas y diseños predefinidos, similar a Canva, que permita filtrar por tipo de creación y elegir tamaños y orientación, incluidas presentaciones horizontales y composiciones verticales tipo póster. Al elegir una plantilla debe cargarse directamente en el espacio de trabajo; también se podrá pulsar «Crear» para empezar desde cero. El chat contextual del agente estará disponible en un panel lateral derecho, accesible desde el encabezado; si no se eligió una plantilla, preguntará «¿Qué deseas hacer hoy?», aclarará lo necesario y propondrá opciones de diseño según la petición. El área central mostrará en vivo la creación y las correcciones, combinando instrucciones por chat con edición manual mediante el ratón: seleccionar, mover, redimensionar y modificar textos, imágenes, colores, tipografías y distribución, con una experiencia tipo Canva para composiciones y presentaciones y tipo Figma para interfaces web, manteniendo sincronizados los cambios manuales y los del agente. Para diseñar el frontend y las diapositivas, considerar la creación mediante tecnologías web y evaluar la transformación de esos diseños en presentaciones PowerPoint editables como una posible vía para facilitar el diseño; explorar y comparar también otras metodologías de exportación, incluida la conversión de SVG a PPTX, comprobando la fidelidad visual y qué elementos permanecen editables, sin dar por decidida una única implementación. El alcance inicial se limita a web, presentaciones e imágenes; el vídeo queda fuera por ahora. Requisito indicado por Cristhian el 2026-10-25.

##### Aula de cursos personalizados

Herramienta con interfaz propia tipo plataforma de cursos, similar a Coursera, que inicialmente muestra un chat clásico donde el agente pregunta «Hola, ¿qué quieres estudiar?» y, cuando sea necesario, hace preguntas adicionales para precisar el objetivo, la amplitud y el nivel del aprendizaje. Una vez aclarado, el agente crea el primer curso con su **temario completo desde el momento de creación** y muestra la interfaz de cursos; al abrir cada curso se debe ver todo su temario y su porcentaje de avance. Al abrir un tema, el agente decide el formato de la clase según lo que convenga: pizarra interactiva, simulación o conversación por chat; para las clases que lo necesiten, podrá utilizar la herramienta **Simulación**, según las áreas y capacidades que se definan para ella. Es **crítico que cada tema termine con un cuestionario obligatorio**, compuesto únicamente por preguntas para marcar opciones o relacionar elementos, sin respuestas abiertas; el tema o sesión solo se considera culminado cuando el usuario haya respondido correctamente **todas las preguntas**, y el porcentaje de avance debe reflejar los temas efectivamente completados. Para finalizar el curso también debe aprobarse un cuestionario final con más preguntas que los de cada tema, igualmente de marcar o relacionar y con todas las respuestas correctas; el agente decide la cantidad de preguntas y su contenido. Debe conservar el curso, temario, progreso y resultados para retomarlos, y contar con el chat contextual del encabezado definido en «Crítico». No requiere consulta ni aprobación de expertos externos ni una etapa de especialistas. Requisitos indicados por Cristhian el 2026-10-24.

##### Gestor de postulaciones laborales

Herramienta con su propia interfaz moderna y adaptable a móvil y pantallas grandes: un panel de postulaciones con vistas de tabla y tablero por estados (encontrada, en preparación, pendiente de aprobación, enviada, entrevista, oferta, rechazada y retirada), búsqueda y filtros por empresa, puesto, estado y fechas, y próximas acciones y plazos visibles. El agente debe preguntar «¿Tienes un CV o lo preparo yo?» sin exigir uno existente; si el usuario necesita crearlo, recopilará su información mediante conversación, confirmará los datos sin inventar experiencia y utilizará la herramienta para generar PDFs para crear el CV. Cada candidatura debe tener una ficha con la oferta original y su fuente, evaluación de encaje, versiones de CV y cartas, documentos y respuestas efectivamente enviados, canal y evidencia de envío, historial de mensajes y cambios de estado, entrevistas, notas y recordatorios; distinguir siempre lo preparado de lo enviado y permitir revisar, aprobar, pausar o descartar antes del envío. La interfaz mostrará el progreso del agente y las acciones que requieren intervención del usuario. Propuesta de interfaz registrada a petición de Cristhian el 2026-10-24.


##### Monitor de información y alertas

Herramienta con una interfaz moderna, atractiva y adaptable a móvil y pantallas grandes, organizada como un panel de seguimiento con mapa interactivo, capas de información, noticias, indicadores, alertas y actualizaciones en tiempo real según la disponibilidad de cada fuente, mostrando su procedencia y última actualización. Inicialmente utilizará la funcionalidad de búsqueda en Internet del navegador de Excalibur para obtener la información. El contenido, las fuentes, las regiones y la prioridad de los avisos deben responder a las preferencias del usuario y a las necesidades que su agente deba atender; mediante instrucciones y correcciones como «busca mejor esta fuente» o «estas noticias no me importan», debe guardar y aplicar lo aprendido para mejorar continuamente la selección y presentación de información. Cuando sea posible, también debe adaptar colores, distribución y otros aspectos de la interfaz a los gustos del usuario, manteniendo legibilidad y permitiéndole corregir esa personalización. Incorporado por petición de Cristhian el 2026-10-24.

#### Dudas

- ¿Cómo **aterrizar una herramienta de simulaciones con múltiples agentes que representen personas**, para realizar **simulaciones sociales, de mercado y de otros escenarios**? El objetivo es modelar sus comportamientos e interacciones como participantes de la simulación. Propuesta de Cristhian del 2026-10-26, pendiente de concretar metodología, alcance e interfaz; el realismo de esas representaciones queda por validar.

### Video

Notas para el video de demostración; no forman parte de las funcionalidades del agente.

- Mostrar que la aplicación Android podrá funcionar conectada a un servidor o en el dispositivo, sin servidor propio; en la modalidad local, el usuario utilizaría su propia API key y solo pagaría por el consumo de la API.

- Mostrar que toda la configuración del agente se realiza desde la **interfaz de chat**, con ayuda del propio agente. La experiencia está pensada para usuarios no técnicos, sin necesidad de saber qué es una skill, un plugin o una herramienta.

- Mostrar explícitamente una **comparación de los resultados obtenidos en Terminal Bench 2.1 entre Excalibur y Muse Code, utilizando el mismo modelo de Muse**, con el porcentaje de aciertos y la cantidad de tokens utilizados por cada uno.


## Dudas


- Excalibur debe tener soporte para múltiples usuarios.
- ¿Podría tener dos modalidades de funcionamiento en celulares: local o conectada a un servidor?







## Tareas pendientes

- Revisar las vulnerabilidades reportadas en los repositorios de referencia, incluidos sus avisos de seguridad, issues y correcciones, para tenerlas en cuenta al diseñar y desarrollar nuestro navegador y nuestro agente.






- Revisar los commits de OpenClaw, Hermes, OpenCode y Codex para estudiar su línea de mejora e identificar qué mecanismos podrían contribuir a la autopoiesis.

