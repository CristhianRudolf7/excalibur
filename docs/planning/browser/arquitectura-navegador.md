# Excalibur — Arquitectura del navegador y del buscador propio

> Documento de arquitectura técnica del **navegador de Excalibur** (servicio en Rust que gobierna Chromium para el agente, con intervención humana ocasional y continuidad de sesiones) y del **buscador propio** (descubrimiento, rastreo, índice en disco, actualización, ranking, lectura ligera y citas). Fecha de la investigación y de las mediciones: 2026-10-29. Documentos de partida: `anotaciones.md` (sección «Navegador y búsqueda web» y requisitos 32–37 de Fénix), `arquitectura-arnes.md` (contratos 14.1, 14.2 y 14.3, y las secciones 10.9, 13.3, 17, 20 y 22 de las que dependen) y el estado real del código en el checkout `excalibur-browser`, rama `browser`, commit `768a497`.

---

## 0. Cómo leer este documento

**Qué cubre.** El interior de las dos capacidades que el arnés trata como cajas negras: `browser` (14.2 del arnés) y `search` (14.3). Para cada parte se describen responsabilidades, entradas y salidas, estados, algoritmos, errores, seguridad, recursos, pruebas y el hito que la construye. El documento es autosuficiente: cuando cita el arnés, resume el contrato que usa.

**Qué no cubre.** El arnés en sí (motor, memoria, contexto, maestro), la interfaz general de Excalibur, la tienda y las demás capacidades. Del frontend solo se define el **visor de intervención humana** mínimo y su revisión visual; no hay suites funcionales de frontend.

**Convenciones.**

- **[E]** hecho verificado en una fuente (documentación oficial, código con versión o commit, paper con sección o tabla). La clave entre corchetes, por ejemplo [CDP] o [WASP], remite a la sección 22.
- **[M]** medición propia hecha el 2026-10-29 en la VPS de desarrollo con los borradores descritos en el apéndice E. Es evidencia local: vale para esa máquina, esa versión de Chrome y esas páginas, no es un benchmark general.
- **[M\*]** medición local hecha en la misma VPS por un agente auxiliar de investigación de esta sesión; algunas se tomaron como root con `--no-sandbox` en perfiles temporales, por lo que sirven para comparar flags y costes de lectura, no como cifra de producción.
- **[I]** inferencia o decisión propia que influye en el diseño.
- **NO VERIFICADO** marca lo que no pudo confirmarse en una fuente primaria; el hito correspondiente incluye la comprobación.
- Los nombres de herramientas, crates, campos y estados están en inglés (regla de las anotaciones); el resto, en español.
- Los requisitos llevan identificador **NB-xx** (sección 2.1); los hitos, **B0–B9** (sección 19); las ampliaciones fuera del presupuesto, **A-xx** (sección 20); los riesgos, **R-xx** (sección 21).
- Los valores numéricos son **iniciales a calibrar**; están reunidos en el apéndice A.
- Cada sección funcional termina con **Pruebas de verificación**: qué probar, resultado esperado y evidencia a conservar. Se prueban contra el backend (protocolo del servicio, herramientas del arnés, registros), con Chrome real y sitios sintéticos locales. Los sitios reales (Google, WhatsApp Web, Teams) solo aparecen en recorridos manuales opcionales que no forman parte de la integración continua.

**Mapa de exigencias → secciones.**

| Exigencia | Secciones |
|---|---|
| Rust, motor maduro reutilizado, mínima RAM, seguridad | 1, 3, 4, 12, 13 |
| Operación mayormente sin interfaz, intervención humana ocasional, continuidad de sesiones | 5, 9 |
| Google, WhatsApp Web, Teams | 4.2, 5.2, 5.4, 9, 18.4, 21 (R-03 a R-06), apéndice E |
| Contratos 14.1/14.2: DOM y capturas, referencias, espera, acciones, perfiles, credenciales, bloqueos, descargas, cancelación, recuperación | 6, 7, 8, 9, 10, 11, 16 |
| Aislamiento, cookies, XSS, contenido no confiable, permisos | 5.5, 12 |
| Limitar sesiones y cachés, HTTP ligero cuando baste, medir la memoria de todos los procesos sin suspender sesiones que deban recibir eventos | 13, 14 |
| Buscador propio sin API externa de búsqueda, cobertura inicial viable | 15 |
| Pruebas, hitos, presupuesto de 36 h, flujo Muse Code → auditoría | 18, 19, 20 |

---

## 1. Resumen de decisiones

| # | Decisión | Razón principal | Alternativa descartada |
|---|---|---|---|
| D1 | **Chromium/Google Chrome estable en modo headless nuevo** (`--headless`), el mismo binario que con interfaz | Es el navegador real; WhatsApp Web carga su pantalla de vinculación en headless [M]; Teams exige una de las tres últimas versiones de Edge, Chrome o Firefox [E][Teams-req] | `chrome-headless-shell` (no reproduce Chrome completo [E][Headless]); Servo, Ladybird, WebKitGTK, Lightpanda (compatibilidad insuficiente para estas tres webs [E][Servo][Ladybird][Lightpanda]); Firefox (sin CDP desde la 141 y con herramientas Rust de BiDi inmaduras [E][Firefox-CDP]) |
| D2 | **CDP por `--remote-debugging-pipe`** (descriptores 3 y 4, JSON separado por NUL) con un **cliente CDP propio y mínimo** sobre Tokio | Sin puerto TCP ni archivo `DevToolsActivePort`: solo el proceso padre controla el navegador [E][Pipe]; Playwright usa siempre la tubería [E][Pipe]; `chromiumoxide` no admite tubería (issue #294 abierta, PR #325 en borrador) y genera ~60 000 líneas [E][chromiumoxide]; browser-use y agent-browser también abandonaron capas gruesas por clientes CDP propios [E][browser-use][agent-browser] | Puerto `--remote-debugging-port` (sin autenticación; cualquier proceso local lo controla; en esta VPS hay un Chrome ajeno a este trabajo con ese patrón [M]); WebDriver BiDi (salto extra por chromium-bidi) |
| D3 | **Servicio separado `excalibur-webd`** (navegador + buscador) que corre **como usuario del SO sin privilegios** y con el **sandbox de Chrome activo y verificado** | Chrome se niega a correr como root sin `--no-sandbox` [E][LinuxSandbox]; el arnés corre hoy como root en la VPS [M]; aislar fallos, memoria y privilegios; las sesiones en escucha sobreviven a reinicios del arnés | Biblioteca dentro del daemon (comparte privilegios y caídas); `--no-sandbox` (prohibido) |
| D4 | **Dos clases de instancia**: una **persistente** por perfil de cuentas (Google, WhatsApp, Teams) y una **efímera** para la web abierta, con un contexto aislado (`BrowserContext`) por ejecución | Romper la «tríada letal» (datos privados + contenido no confiable + comunicación externa) [E][Trifecta][RuleOfTwo]; un contexto efímero no comparte almacenamiento ni procesos de renderizado con otros [E][ProcModel] | Un perfil único para todo (mezcla cuentas con contenido hostil); una instancia por pestaña (≈250 MiB fijos cada una [M]) |
| D5 | **Instantánea = `DOMSnapshot.captureSnapshot` + árbol de accesibilidad por marco**, texto compacto para el modelo con referencias `e<N>`/`f<K>e<N>`, estructura completa como blob | El árbol de accesibilidad con identificadores es 10–60 veces más barato que el HTML y es la representación por defecto más sólida [E][ReadMore][AgentOccam][M]; la lista interactiva de una página de GitHub ocupa ≈8 KB frente a ≈547 KB del `DOMSnapshot` [M] | HTML completo (≈200 000 tokens en páginas grandes [E][ReadMore]); solo captura (la anclación visual es el cuello de botella [E][SeeAct]) |
| D6 | **Espera híbrida**: compuerta de navegación + red propia (excluye WebSocket, EventSource, beacons y peticiones de larga duración) + estabilidad del DOM + condición opcional + detectores de bloqueo | El `networkIdle` de Chromium se emite una sola vez por documento y nunca llega con *long-polling* [E][Idleness][M]; Playwright desaconseja `networkidle` [E][PlaywrightMCP] | Esperar solo `networkIdle` o un tiempo fijo |
| D7 | **Intervención humana por transmisión de pantalla CDP** (`Page.startScreencast`) dentro del mismo proceso headless, con el visor servido en loopback | No hay que relanzar el perfil (se conservan `sessionStorage` y conexiones vivas); 5,7–11,4 Mbit/s con desplazamiento real a 960–1280 px [M*] | Xvfb + VNC (más RAM y piezas); relanzar con interfaz (pierde estado de sesión) |
| D8 | **Credenciales por referencia** resueltas con el `SecretExecutor` del arnés y escritas con `Input.insertText` solo si el **origen comprometido** del marco y de la página coinciden exactamente con el del baúl | El valor nunca entra en el contexto, el registro ni la telemetría [E][arnés 20.10]; los gestores con coincidencia laxa filtraron TOTP en 9 de 11 productos [E][Toth-clickjacking]; la comparación de cadenas analizadas en lugar del origen real causó CVE-2025-47241 [E][browser-use] | Escribir el secreto en el JSON de la herramienta; autocompletado propio de Chrome |
| D9 | **Toda la salida de red del navegador pasa por el broker del arnés** (`excalibur-sandbox`, CONNECT/SOCKS5 con DNS fijado y direcciones privadas denegadas) | Reutiliza código ya probado contra *DNS rebinding*, redirecciones y salida directa [E][arnés: security.es.md]; Chrome omite por defecto el proxy para loopback y `169.254/16` (metadatos de la nube) salvo con `<-loopback>` [E][ProxyDocs] | Confiar en listas de URL de Chrome (no cubren *rebinding*) |
| D10 | **HTTP ligero primero** (`reqwest` + extracción en Rust) y Chrome solo cuando la página lo exige | Wikipedia devolvió «Too many requests» al Chrome headless y 200 por HTTP desde la misma VPS [M]; un Chrome en blanco cuesta ≈257 MiB PSS [M] | Navegador por cada lectura (descartado por las anotaciones) |
| D11 | **Buscador propio con tantivy 0.27** (BM25 con pesos por campo + prior de host + frescura), rastreador educado según RFC 9309 y **cobertura inicial acotada por presupuesto de disco** | BM25 sigue siendo una base fuerte en cero disparos y gana en consultas de agentes [E][BEIR][SAGE][Pi-Serini]; tantivy es la biblioteca Rust más mantenida y usada (MIT) [E][Tantivy] | Índice denso como primera etapa; motores AGPL archivados (Stract) [E][Stract]; metabuscadores (usan APIs ajenas) |
| D12 | **No hay evasión**: ni ocultación de huellas, ni resolución automática de CAPTCHA, ni falsificación del navegador | Los términos de Google, WhatsApp y Microsoft restringen el acceso automatizado [E][Google-signin][WA-ToS][MS-SA]; la arquitectura gana más con intervención humana honesta que con una carrera que no se puede garantizar | Plugins *stealth*, servicios de resolución de CAPTCHA |

---

## 2. Requisitos, alcance y entorno

### 2.1 Requisitos con identificador

| ID | Requisito | Origen | Sección |
|---|---|---|---|
| NB-01 | Motor maduro (Chromium) y transporte CDP seguro, con versión fijada y política de actualización | Usuario, Fénix 32 | 4 |
| NB-02 | Servicio en Rust, separado del arnés, nunca como root, sandbox de Chrome verificado | Usuario, arnés P10 | 3, 12.2 |
| NB-03 | Perfiles persistentes y efímeros, pestañas y continuidad de sesiones | Fénix 33, 14.2 | 5 |
| NB-04 | Varias interfaces abiertas a la vez, con su vínculo a ejecuciones y subtareas | Anotaciones (duda del 2026-10-25), 14.2 regla 5 | 5.6 |
| NB-05 | Modo sin interfaz por defecto; intervención humana con interfaz; modo con interfaz cuando hay pantalla; credenciales compartidas por el baúl | Anotaciones, 14.2 | 5.4, 9 |
| NB-06 | Captura visual y árbol DOM completo con jerarquía y lista identificable de controles | Anotaciones (2026-10-25), 14.2 | 6 |
| NB-07 | Referencias ligadas a la instantánea; `stale_ref` ante vistas cambiadas | 14.2 regla 2 | 6.4 |
| NB-08 | Esperar la carga antes de capturar o actuar, con criterio definido y verificado | Anotaciones (2026-10-25), 14.2 regla 1 | 7 |
| NB-09 | Acciones `click`, `type`, `select`, `hover`, `scroll`, `press`, `upload`, `drag` | 14.2 | 8 |
| NB-10 | Bloqueos `blocked_captcha`, `needs_login`, `needs_2fa`, `blocked_by_site` → `input_required` | 14.2, anotaciones (CAPTCHA) | 9 |
| NB-11 | Credenciales como referencias; el modelo nunca ve el valor | 14.1, 20.10 | 10 |
| NB-12 | Descargas y subidas seguras | Fénix 34, 20.7 | 11 |
| NB-13 | Cancelación por clase | 14.1, 13.1 | 16.5 |
| NB-14 | Recuperación tras caídas del arnés, del servicio, de Chrome y de la máquina | 14.1, 17 | 16.6 |
| NB-15 | Aislamiento, red, permisos y cookies | Anotaciones (dudas de cookies y XSS) | 5.5, 12 |
| NB-16 | Contenido no confiable, inyección de instrucciones y XSS sin prometer seguridad absoluta | Usuario, 20.8 | 12.4–12.6 |
| NB-17 | Medir la memoria de todos los procesos; limitar sesiones; no congelar sesiones que reciben eventos | Usuario, Fénix 36 | 13 |
| NB-18 | Limitar cachés y disco | Usuario | 13.6 |
| NB-19 | Lectura HTTP ligera cuando basta, con escalado al navegador | Fénix 37, 14.3 | 14 |
| NB-20 | Buscador: descubrimiento y rastreo propios | Fénix 37, anotaciones | 15.3–15.4 |
| NB-21 | Índice en disco y actualización | Usuario | 15.5–15.6 |
| NB-22 | Ranking | Usuario | 15.7 |
| NB-23 | Extracción, citas y fuentes desde una URL | Usuario, 10.9 | 14.4, 15.8 |
| NB-24 | Integración con el arnés: manifiesto, catálogo diferido, ciclo de tareas MCP, idempotencia, auditoría | 14.1, 13.3 | 16 |
| NB-25 | Emulación de pantalla y herramientas de desarrollo (consola, red, rendimiento básico) | Fénix 35, 14.2 | 6.7 |
| NB-26 | Eventos de las sesiones en escucha (mensajes nuevos) hacia el arnés | Usuario, anotaciones (disparadores) | 5.7 |
| NB-27 | Pruebas backend de negocio y seguridad; revisión visual del visor en pantalla pequeña y grande | Usuario | 18 |
| NB-28 | Rutinas de clics deterministas | 14.2 regla 4 | Ampliación A-01 |

### 2.2 Qué entra en las 36 horas y qué no

El presupuesto solicitado son **36 horas de programación del conjunto** (navegador, buscador, integración y pruebas). Con ese presupuesto, la sección 19 reparte **32,5 horas de implementación** y **3,5 horas de reserva** para las correcciones de auditoría. Las estimaciones son orientativas para un agente de programación (Muse Code); **no son plazos garantizados**.

**Entra (entrega integrada y probada):** todo NB-01 a NB-27 en su versión mínima descrita en cada sección, solo en **Linux x86-64**, con un único principal (usuario de Excalibur).

**No entra y queda como ampliación explícita (sección 20):**

| Exigencia o mejora | Por qué no cabe | Ampliación |
|---|---|---|
| Rutinas de clics con grabación y reproducción (`browser_routine`) | Grabar acciones humanas, parametrizar y validar aserciones es un subsistema propio (≈4–6 h) | A-01 |
| Relanzar un perfil en modo con interfaz conservando las cookies de sesión | Requiere exportar e importar cookies y gestionar el bloqueo del perfil; en servidor no hay pantalla | A-02 |
| Chrome dentro de un espacio de nombres de red propio (bubblewrap) | Exige espacios de nombres de usuario anidados y un perfil AppArmor que hay que probar; el seccomp actual del arnés los prohíbe | A-03 |
| Un cgroup por instancia mediante D-Bus de systemd | Se usa un cgroup para todo el servicio cuando está disponible; el reparto por instancia se hace con `/proc` | A-04 |
| macOS, Windows, Android, iOS | La plataforma verificada es Linux | A-05 |
| Generación de TOTP en el ejecutor de secretos | El baúl actual no tiene tipo TOTP | A-06 |
| Transmisión por WebRTC para intervención remota con poco ancho de banda | El screencast basta en red local o túnel | A-07 |
| Reuniones de Teams o Meet (audio, vídeo, cámara) | Es la capacidad `meetings` del arnés (14.9), no el navegador | Fuera |
| Recuperación densa o reordenamiento con codificador cruzado | Primero se mide BM25 | A-08 |
| Perfil `estandar` del buscador (Wikipedia en inglés y español completas, ≈3 millones de páginas), rastreo de PDF, anclas de enlaces, grafo web propio, relleno desde WARC de Common Crawl | Superan el presupuesto explícito de disco de v1 (13.5) o el tiempo; el perfil `ampliado` (15.3) sí cabe, con el mismo código | A-09 a A-12 |
| Disparadores del arnés a partir de eventos de sesiones | Los disparadores son del hito H12 del arnés; aquí solo se emiten los eventos | Integración posterior |
| Actualizador automático de Chrome | Aquí se comprueba la versión y se bloquea o avisa; actualizar depende del sistema | A-13 |
| Cifrado en reposo del perfil (fscrypt) | Requiere privilegios y soporte del sistema de archivos | A-14 |
| Evaluación con BU Bench | Son tareas en sitios reales con juez LLM; con coste y sin aprobación no se ejecutan | A-15 |

**Exigencias que no se pueden garantizar con ninguna arquitectura** y que este diseño trata con detección y paso a humano:

- Que Google permita iniciar sesión o buscar desde un navegador automatizado: Google exige navegadores sin automatización y no headless para el inicio de sesión [E][Google-signin], y desde esta VPS la primera búsqueda en Chrome headless devolvió el reCAPTCHA de «tráfico inusual» [M]. Por eso el buscador propio no depende de Google y el inicio de sesión lo hace una persona (9.4).
- Que WhatsApp no restrinja una cuenta: sus términos prohíben mensajes masivos o automáticos y el acceso por medios automatizados no autorizados [E][WA-ToS]. El diseño limita el uso a operaciones personales con confirmación humana de cada envío (12.4).
- Que Teams funcione en una versión de Chrome concreta: Microsoft admite solo las tres últimas versiones [E][Teams-req] y, con el ciclo de dos semanas de Chrome desde la versión 153 [E][Chrome-2w], eso equivale a unas seis semanas.

### 2.3 Entorno real verificado

Medido el 2026-10-29 en la VPS de desarrollo [M]:

| Aspecto | Valor | Consecuencia |
|---|---|---|
| Sistema | Ubuntu 24.04.5 LTS, kernel 6.8.0-147, x86-64, systemd 255 | Plataforma objetivo del presupuesto |
| CPU / RAM | 6 núcleos, 11,7 GiB de RAM, **sin swap**, ≈7 GiB disponibles | Presupuesto de memoria del navegador por defecto 1,5 GiB (13.2); sin swap, un `memory.high` estricto congelaría páginas |
| Disco | 193 GB. Al empezar la sesión, 97–99 % ocupado (3,5–6,2 GB libres). Tras la limpieza del usuario (≈22,5 GiB de ejecutables antiguos en `target/debug/deps`), `df` del 2026-10-29 da **165 GB usados (86 %) y 30 086 959 104 bytes libres (≈28 GiB)**; el `target` del checkout principal ocupa ahora 113 GB | Hay espacio para compilar este trabajo y para el perfil `minimo` del buscador. No es un presupuesto ilimitado: webd y el buscador tienen límites de disco explícitos y una reserva libre obligatoria (13.5); el riesgo R-01 pasa a medio |
| Usuario | El arnés y la sesión corren como `root` | El servicio del navegador debe cambiar a un UID sin privilegios al arrancar (12.2) |
| Espacios de nombres | `kernel.apparmor_restrict_unprivileged_userns=1`; existe el perfil AppArmor `chrome` para `/opt/google/chrome/chrome` con `userns` | El sandbox de espacios de nombres de Chrome funciona para un usuario sin privilegios; verificado: renderers con `Seccomp: 2`, espacio de nombres de usuario, PID y red propios (12.2) |
| cgroups | cgroup v2 con controladores `cpu`, `io`, `memory`, `pids`; la sesión vive en `user.slice/user-0.slice/session-*.scope` | Medición por cgroup cuando el servicio se lanza en su propio *scope* (13.1) |
| Chrome | `google-chrome-stable` 155.0.8059.84 (paquete del 2026-10-14); `chrome-sandbox` setuid presente | Estable vigente según Chrome for Testing: **156.0.8078.73** [E][CfT]; la VPS va una versión por detrás (4.5) |
| Rust | rustc 1.99.1, cargo 1.99.1 | Toolchain disponible |
| Dependencias ya en `Cargo.lock` | `tokio` 1.54.1, `reqwest` 0.12.28, `rustls` 0.23.46, `hyper` 1.12.0, `rusqlite` 0.37.0, `serde_json` 1.0.154 | Se reutilizan; no se añaden runtimes nuevos |
| Pantalla | Sin `DISPLAY` ni Wayland | En esta máquina el modo con interfaz es el visor (9.3) |
| Proceso ajeno | Un Chrome lanzado por otro flujo, como root, con `--no-sandbox` y `--remote-debugging-port=9227` | No se toca. Ilustra el patrón que este diseño prohíbe (D2, D3) |

### 2.4 Estado real del código del arnés que se reutiliza

El repositorio contiene las bibliotecas de los hitos H1–H7 del arnés (almacenamiento, motor durable, seguridad, modo no interactivo, trabajo verificable, contexto, herramientas diferidas). **No existe código de navegador ni de búsqueda**: `web_search`, `web_read` y `source_search` solo aparecen como nombres filtrados por el perfil `eval` (`crates/eval/src/tools.rs`, `profile.rs`) y en la guarda `benchmark_source_blocked`. El daemon con puerta de entrada y el maestro (H8) tampoco existen todavía, por eso el visor de intervención lo sirve el propio servicio (9.3).

| Pieza existente | Ubicación | Uso en este diseño |
|---|---|---|
| `ToolRuntime` (`prepare`, `execute`, `reconcile`, `interrupt_execution`, `execution_state`, `live_status`, `configure_network_authorizer`…) | `crates/types/src/runtime.rs` | El adaptador del navegador y del buscador lo implementa (16.2) |
| `Effect` (`Pure`, `Idempotent`, `Keyed`, `NonIdempotent`), `PreparedTool`, `Reconcile` (`Done`, `Retry`, `Uncertain`) | `crates/types/src/runtime.rs` | Clases de efecto por llamada (16.3) |
| `CatalogTool` (efecto, idempotencia, riesgo, `timeout_ms`, `output_budget_bytes`, `fast`, `cancel`), `DeferredTools`, `search_tools`/`call_tool` | `crates/tools/src/deferred.rs` | Registro de las herramientas como diferidas, fuera del prefijo (16.2) |
| `SecuredTools`, `Security::network_authorizer`, regla de dos fuentes, contaminación `tainted_secret`/`untrusted_seen` | `crates/security` | Autorización de cada llamada y de cada dominio nuevo (12.4) |
| `Vault`, `SecretRef`, `SecretKind::WebsitePassword` con URL obligatoria, `SecretExecutor::execute`, `Redactor` | `crates/vault`, `crates/security` | Rellenado de credenciales (10) |
| `ProxyConfig`, `NetworkPolicy`, `HostProxy::start` (broker en socket Unix con DNS fijado y direcciones privadas denegadas), `canonical_domain` | `crates/sandbox/src/network.rs`, `network/unix.rs` | Salida de red de Chrome y del lector (12.3) |
| `BlobStore` (direccionado por contenido) | `crates/store/src/blobs.rs` | Capturas, árboles DOM, descargas, textos leídos |
| `ResourceSample`/`ResourceSummary` (RSS, `memory.current`, `memory.peak`, eventos OOM) | `crates/telemetry/src/resources.rs` | Base de la medición de memoria (13.1) |
| `ContextInput::untrusted` y anotación de procedencia | `crates/context` | Marcado de todo contenido web (12.4) |

---

## 3. Mapa general del sistema

### 3.1 Componentes

```mermaid
flowchart LR
  subgraph Arnes["Arnés excalibur (proceso existente)"]
    MOT[Motor del agente]
    SEC[SecuredTools / Security]
    DEF[DeferredTools]
    ADP[Adaptador web<br/>ToolRuntime de browser y search]
    VAULT[Baúl + SecretExecutor]
    BLOB[BlobStore]
    SRCE[source_events / citas 10.9]
  end

  subgraph Webd["excalibur-webd (usuario sin privilegios)"]
    RPC[Protocolo webd<br/>JSON-RPC sobre socket Unix]
    SM[Gestor de sesiones<br/>perfiles, contextos, pestañas]
    CDP[Cliente CDP por tubería]
    OBS[Observación: instantáneas, espera, bloqueos]
    ACT[Acciones, diálogos, descargas, subidas]
    GATE[Compuerta de orígenes y permisos]
    RES[Medición y presupuesto de memoria]
    VIEW[Visor de intervención<br/>HTTP+WebSocket en loopback]
    PRX[Puente al broker de red]
    RD[Lector ligero: HTTP, extracción, Markdown]
    SE[Buscador: frontera, rastreador, índice tantivy, ranking]
    DB[(webd.db SQLite)]
  end

  subgraph Chrome["Chrome (procesos hijos de webd)"]
    LIVE[Instancia persistente<br/>perfil de cuentas]
    EPH[Instancia efímera<br/>un contexto por ejecución]
  end

  NET((Internet))

  MOT --> DEF --> SEC --> ADP
  ADP <--> RPC
  ADP --> VAULT
  ADP --> BLOB
  ADP --> SRCE
  RPC --> SM --> CDP --> LIVE & EPH
  SM --> OBS & ACT & GATE & RES
  RPC --> RD & SE
  VIEW --> CDP
  LIVE & EPH --> PRX --> NET
  RD & SE --> PRX
  SM & SE --> DB
```

| Componente | Responsabilidad | Estado propio |
|---|---|---|
| Adaptador web (en el arnés) | Publicar el manifiesto, traducir herramientas a operaciones de webd, clasificar efectos, resolver secretos, guardar blobs, registrar fuentes, reconciliar tras caídas | Ninguno aparte del registro durable del arnés |
| Protocolo webd | JSON-RPC 2.0 sobre socket Unix con credenciales del par comprobadas | Conexiones, suscripciones de eventos |
| Gestor de sesiones | Ciclo de vida de instancias, perfiles, contextos, sesiones, pestañas, bloqueos de pestaña y propietarios | Tablas `web_profiles`, `web_sessions`, `web_tabs` |
| Cliente CDP | Tubería, correlación de peticiones, eventos por sesión CDP plana, tiempos de espera | En RAM |
| Observación | Instantáneas, referencias, espera, detectores de bloqueo, capturas | Instantáneas en RAM con LRU + blobs |
| Acciones | Comprobación de accionabilidad, envío de eventos de entrada, diálogos, descargas, subidas, relleno de credenciales | Registro de operaciones `web_ops` |
| Compuerta | Conjuntos de orígenes por sesión, permisos denegados por defecto, bloqueo de esquemas | En RAM, derivado de la configuración |
| Recursos | Muestreo de memoria, admisión, desalojo, límites de caché y disco | Muestras en RAM; resumen en `webd.db` |
| Visor | Transmisión de pantalla y reenvío de entrada durante una intervención humana | Tokens de un solo uso |
| Puente de red | Proxy loopback para Chrome que reenvía al broker del arnés | — |
| Lector ligero | `web_read`: descarga, decisión HTTP/navegador, extracción, fragmentos con desplazamientos | Caché de lecturas |
| Buscador | Semillas, frontera, rastreo educado, índice, actualización, ranking, `web_search` | `search.db` + índice tantivy |

### 3.2 Procesos y usuarios del sistema operativo

- **`excalibur-webd`** es un binario propio. Si lo lanza un proceso con UID 0 (como hoy en la VPS), cambia a un UID y GID sin privilegios configurados (`web.os_user`, por defecto un usuario dedicado `excalibur-web` creado por el instalador, o `nobody` en desarrollo) **antes** de abrir ningún socket o perfil, con `setgroups([])`, `setgid`, `setuid` y comprobación de que no puede recuperar privilegios. Si lo lanza un usuario normal (escritorio), corre con ese usuario. **Nunca corre como root**: si tras el cambio el UID efectivo es 0, termina con error.
- **Chrome** es hijo de webd, lanzado con `pre_exec` que coloca la tubería en los descriptores 3 y 4, establece `PR_SET_PDEATHSIG(SIGKILL)` y un grupo de procesos propio. Si webd muere, Chrome muere con él.
- **El arnés** no lanza Chrome nunca. Se conecta a webd por el socket Unix `<run_dir>/webd.sock` (modo `0660`, grupo compartido entre el arnés y el usuario web). webd comprueba `SO_PEERCRED` y solo acepta el UID del arnés configurado o root.
- **Arranque**: el host lanza webd si no está vivo (desvinculado con `setsid`, para que sobreviva a un reinicio del arnés) o, en instalaciones con supervisor, lo gestiona systemd como servicio propio. En modo evaluación (`excalibur exec` con perfil `eval`) webd arranca y termina con el episodio y sus datos son efímeros.
- **Un solo runtime Tokio** en webd: el muestreo de `/proc`, el análisis de HTML grande y el indexado van a un pool de bloqueo con prioridad baja (`nice 10`), igual que el arnés separa su trabajo intensivo (arnés 3.2).

### 3.3 Organización en crates

Todo vive en crates nuevos del workspace, en la rama `browser`. Los archivos compartidos que se tocan son solo el `Cargo.toml` raíz (miembros), `Cargo.lock` y, en el último hito, el punto de composición del host que registra las herramientas (detrás de configuración). Así se minimizan los conflictos con el flujo del arnés en `main`.

| Crate | Contenido | Depende de |
|---|---|---|
| `excalibur-cdp` | Transporte por tubería, correlación, sesiones planas, tipos de las ~45 órdenes y ~25 eventos usados (escritos a mano con `serde`) | `tokio`, `serde_json` |
| `excalibur-browser` | Gestor de sesiones, observación, espera, acciones, compuerta, recursos, visor, protocolo webd (servidor) | `excalibur-cdp`, `excalibur-store` (blobs), `excalibur-sandbox` (broker), `rusqlite` |
| `excalibur-search` | Lector ligero, extracción, rastreador, frontera, índice, ranking | `tantivy`, `reqwest`, `dom_smoothie`, `htmd`, `texting_robots`, `feed-rs` |
| `excalibur-webd` | Binario: configuración, cambio de usuario, arranque de servicios, socket | Los tres anteriores |
| `excalibur-web-adapter` | Cliente del protocolo webd + `ToolRuntime` + manifiestos + `CatalogTool` | `excalibur-types`, `excalibur-tools`, `excalibur-security`, `excalibur-vault` |

El adaptador vive en un crate propio para que el arnés dependa de un cliente ligero sin arrastrar tantivy ni el cliente CDP.

### 3.4 Protocolo webd

JSON-RPC 2.0 con mensajes delimitados por salto de línea sobre el socket Unix. Cada petición lleva `op_key` (clave de idempotencia derivada de la clave de efecto del arnés), `run_id` y un plazo. Familias:

- `session.*` (`open`, `close`, `suspend`, `list`), `tab.*` (`list`, `new`, `switch`, `close`), `nav.*` (`go`, `back`, `forward`, `reload`), `observe.*` (`wait_ready`, `snapshot`, `subtree`), `act.*` (`perform`, `fill_secret`, `stage_upload`), `download.*`, `devtools.*`, `emulate.*`, `status.*`, `takeover.*` (`start`, `end`, `state`), `op.*` (`get`, `cancel`), `read.*` (`web_read`), `search.*` (`query`, `cite`, `coverage`, `crawl_status`).
- **Eventos** con cursor monotónico: `session_state`, `tab_state`, `blocked`, `takeover_state`, `download_progress`, `listener_activity` (5.7), `memory_pressure`, `instance_crashed`.
- **Datos sensibles**: los campos marcados `sensitive` (solo el valor de un secreto en `act.fill_secret`) no se registran, no se copian a eventos y se borran de memoria (`zeroize`) tras usarse.
- **Tamaños**: un mensaje no supera 4 MiB; capturas y árboles completos viajan como referencias a blobs que el adaptador descarga por streaming con `blob.read`.

### 3.5 Pruebas de verificación

| Prueba | Resultado esperado | Evidencia |
|---|---|---|
| Lanzar webd como root con `web.os_user` configurado | Tras el arranque, `/proc/<pid>/status` muestra UID/GID efectivos y reales del usuario web, sin grupos suplementarios ni capacidades | Lectura de `/proc` |
| Lanzar webd con un UID efectivo final 0 (configuración errónea) | Termina con error antes de abrir el socket | Código de salida, registro |
| Conectar al socket desde un UID no autorizado | Conexión cerrada sin respuesta | Registro de webd |
| Matar webd con SIGKILL con dos instancias de Chrome vivas | No queda ningún proceso de Chrome (PDEATHSIG) | Lista de procesos |
| Mensaje de más de 4 MiB y JSON inválido | Error tipado, la conexión sigue | Respuestas |
| Petición `act.fill_secret` con registro de depuración activado | El valor no aparece en registros, eventos ni `webd.db` | Búsqueda del valor en todos los archivos |

---

## 4. Motor, transporte y versiones

### 4.1 Criterios

1. Compatibilidad real con Google (búsqueda y cuentas), WhatsApp Web y Microsoft Teams web.
2. Seguridad: sandbox de procesos maduro, aislamiento por sitio, parches frecuentes.
3. RAM: coste fijo por instancia y marginal por pestaña.
4. Control desde Rust con un protocolo estable y sin superficie de red.
5. Esfuerzo dentro del presupuesto.

### 4.2 Comparación

| Motor | Estado (octubre de 2026) | Encaje con Google, WhatsApp y Teams | Veredicto |
|---|---|---|---|
| **Chrome/Chromium, headless nuevo** | Desde Chrome 112 headless y con interfaz comparten implementación; desde la 132 el headless antiguo solo existe como `chrome-headless-shell` [E][Headless]. Estable 156.0.8078.73 [E][CfT] | WhatsApp Web muestra su pantalla de vinculación por QR en headless [M]; Teams admite las tres últimas versiones de Chrome [E][Teams-req]; Google bloquea el inicio de sesión automatizado y headless [E][Google-signin] | **Elegido** |
| `chrome-headless-shell` | Envoltorio ligero del módulo `//content`; Puppeteer advierte que no reproduce Chrome completo [E][Headless] | Riesgo de diferencias en aplicaciones complejas | Ampliación posible solo para lectura anónima, tras medirlo |
| Firefox | CDP retirado; 141 es la primera versión sin CDP; queda WebDriver BiDi [E][Firefox-CDP] | Teams lo admite; herramientas Rust BiDi inmaduras (`thirtyfour` 0.38.1 con BiDi opcional) [E][chromiumoxide] | Descartado ahora; vía futura de portabilidad |
| WebKitGTK / WPE | WebDriver, BiDi en curso | WhatsApp lo trató como «Safari no admitido»; Teams admite Safari solo en macOS [E][WebKit] | Descartado |
| Servo | 0.7.0 (2026-10-05); service workers e IndexedDB tras preferencias experimentales [E][Servo] | Insuficiente para estas aplicaciones | Descartado |
| Ladybird | Sin alfa publicada a la fecha [E][Ladybird] | — | Descartado |
| Lightpanda | 1.0 (2026-10-02), Zig, **AGPL-3.0**, CDP; afirma 16× menos memoria en su propio benchmark; sin renderizado gráfico [E][Lightpanda] | Sin capturas reales; no apto para QR de WhatsApp ni para Teams | Descartado para sesiones; posible ampliación para lectura masiva |
| CEF | Google no permite iniciar sesión desde navegadores basados en CEF desde 2021 [E][CEF-Google] | Bloqueado en Google | Descartado |

### 4.3 Transporte: tubería y cliente propio

**Flags de transporte**: `--remote-debugging-pipe` con un `--user-data-dir` no predeterminado. Desde Chrome 136 los flags de depuración remota no se respetan sobre el directorio de datos predeterminado, para frenar el robo de cookies por *infostealers* [E][Chrome136]; usar siempre un directorio propio cumple esa regla.

**Encuadre**: Chrome lee órdenes en el descriptor 3 y escribe respuestas y eventos en el 4; cada mensaje es JSON terminado en `\0` (`kASCIIZ`); el formato CBOR está marcado como experimental [E][Pipe]. El búfer de recepción de Chrome es de 100 MB [E][Pipe]. Verificado con Chrome 155 como usuario sin privilegios en la VPS [M] (apéndice E).

**Cliente `excalibur-cdp`** (≈700–900 líneas):

1. Un lector dedicado parte el flujo por `\0`, deserializa en un `serde_json::Value` y despacha: si tiene `id`, completa el `oneshot` pendiente; si tiene `method`, lo publica en el canal de eventos de su `sessionId` (o del navegador).
2. `send(method, params, session)` asigna `id` creciente, registra el `oneshot` y escribe el mensaje completo con su `\0` en una única escritura bajo un `Mutex` del escritor.
3. Plazos por orden (por defecto 10 s; capturas 15 s; `DOMSnapshot` 20 s). Un plazo vencido devuelve `cdp_timeout` y no reutiliza el `id`.
4. Sesiones planas: `Target.setAutoAttach{autoAttach: true, waitForDebuggerOnStart: true, flatten: true}` en cada página, de modo que cada iframe fuera de proceso (OOPIF) recibe su propio `sessionId` [E][CDP].
5. Límite de mensaje entrante de 64 MiB; si se supera, se cierra la sesión CDP de esa pestaña y se marca `crashed`.
6. Fin de flujo en el descriptor 4 = navegador terminado → evento `instance_crashed` (5.8).

Los tipos se escriben a mano para el subconjunto usado (apéndice B.3), con campos opcionales tolerantes a cambios del protocolo. El protocolo se toma de `devtools-protocol` r1729950 [E][CDP]; varias órdenes usadas son experimentales (`DOMSnapshot`, `Accessibility.getFullAXTree`, `Browser.setDownloadBehavior`, `Browser.setPermission`, `Page.startScreencast`, `Input.insertText`, `Input.dispatchDragEvent`), por eso B1 incluye una prueba de contrato contra la versión instalada.

**Por qué no `chromiumoxide`**: sin soporte de tubería [E][chromiumoxide], compilación lenta por el código generado y una capa de abstracción (páginas, elementos) que este diseño reemplaza por la suya. Su fork `chromey` tampoco menciona tubería [E][chromiumoxide]. agent-browser (Apache-2.0, Rust nativo) usa su propio cliente CDP sobre WebSocket y puerto 0 [E][agent-browser]; se toma su modelo de referencias, no su transporte.

### 4.4 Flags de lanzamiento

Comunes a todas las instancias:

| Flag | Motivo | Nota de seguridad |
|---|---|---|
| `--headless` (o ninguno en modo con interfaz) | Modo headless nuevo | — |
| `--remote-debugging-pipe`, `--user-data-dir=<perfil>` | Transporte (4.3) | Nunca `--remote-debugging-port` |
| `--no-first-run`, `--no-default-browser-check`, `--disable-default-apps`, `--disable-sync`, `--mute-audio` | Quitar trabajo y diálogos | — |
| `--disable-extensions`, `--disable-component-extensions-with-background-pages` | Sin las extensiones de componentes: de 14 a 9 procesos y de 336 a 257 MiB PSS en blanco [M] | — |
| `--disable-background-networking` | Sin peticiones de fondo (servicios de Google) | Puede retrasar componentes de seguridad descargables como CRLSet (NO VERIFICADO); se compensa con la política de versión (4.5) |
| `--disable-features=Translate,OptimizationHints,MediaRouter,AutofillServerCommunication` | Menos procesos y tráfico | — |
| `--password-store=basic` solo si no hay Secret Service | Ver 5.5: en Linux sin llavero equivale a texto claro | Documentado como riesgo R-08 |
| `--proxy-server=http://127.0.0.1:<puerto>` y `--proxy-bypass-list=<-loopback>` | Toda la salida por el broker, incluida loopback y `169.254/16` [E][ProxyDocs] | 12.3 |
| `--host-resolver-rules=MAP * ~NOTFOUND , EXCLUDE 127.0.0.1` | Evita resoluciones DNS locales (prefetch) fuera del proxy [E][ProxyDocs] | Verificar en B5 |
| `--force-webrtc-ip-handling-policy=disable_non_proxied_udp` | WebRTC no usa UDP fuera del proxy [E][ProxyDocs] | Las llamadas quedan fuera de alcance |
| `--disable-quic` | Todo por TCP a través del proxy | Comportamiento exacto tras proxy NO VERIFICADO; se prueba |
| `--disk-cache-size=<bytes>` | 32 MiB efímera, 64 MiB persistente | Un 0 significa «por defecto», no «desactivado» [E][Caches] |
| `--window-size=1280,900` | Vista estable para instantáneas | — |

Solo en la instancia persistente con sesiones en escucha: `--disable-background-timer-throttling`, `--disable-renderer-backgrounding`, `--disable-backgrounding-occluded-windows` (13.5).

**Prohibidos** (webd se niega a arrancar si la configuración los contiene): `--no-sandbox`, `--disable-setuid-sandbox`, `--single-process`, `--no-zygote`, `--disable-web-security`, `--disable-site-isolation-trials`, `--disable-features=site-per-process`, `--remote-allow-origins=*`, `--ignore-certificate-errors`, `--enable-unsafe-swiftshader`, `--remote-debugging-port`, `--enable-automation` [E][ProcModel][LinuxSandbox]. `--no-zygote` además empeoró la memoria en la medición local (291 frente a 276 MiB PSS) [M*].

### 4.5 Binario, versión y actualización

- **Selección del binario** (configurable): Google Chrome estable del repositorio del fabricante (se actualiza con el sistema) → Chromium del sistema → Chrome for Testing fijado. CfT publica Linux arm64 desde la 153 [E][CfT], pero Google advierte que no se actualiza solo y que solo debe usarse con contenido confiable [E][CfT]; por eso CfT se reserva para las pruebas reproducibles.
- **Comprobación de versión** al arrancar y cada 24 h: webd lee la versión instalada (`Browser.getVersion`) y, si está configurado y la red lo permite, la última estable de `last-known-good-versions.json` de CfT [E][CfT] (dato público, no una API de búsqueda).

| Diferencia con la estable | Acción |
|---|---|
| 0 o 1 versiones mayores | Normal (la VPS está hoy en este caso: 155 frente a 156) |
| 2 versiones mayores | Aviso al usuario en `browser_status` y evento |
| 3 o más, o más de 6 semanas desde la versión instalada | **Bloqueo de los perfiles persistentes** (las cuentas), con mensaje claro; la instancia efímera sigue disponible. Fundamento: actualizaciones de seguridad semanales desde Chrome 116 [E][Chrome-update] y siete *zero-days* explotados en 2026 [E][Chrome-update]; Teams exige una de las tres últimas versiones [E][Teams-req] |

### 4.6 Pruebas de verificación

| Prueba | Resultado esperado | Evidencia |
|---|---|---|
| Lanzar Chrome con tubería, `Browser.getVersion`, crear pestaña, navegar a un sitio local, cerrar | Respuestas correlacionadas, eventos por sesión, cierre limpio | Traza CDP redactada |
| Comprobar que no hay sockets de escucha del árbol de Chrome ni archivo `DevToolsActivePort` | Ningún puerto TCP en escucha de esos PID; archivo ausente | `/proc/net/tcp*`, listado del perfil |
| Prueba de contrato de cada orden CDP del apéndice B.3 contra la versión instalada | Todas responden sin `method not found`; las experimentales quedan registradas con la versión | Informe por versión |
| Configuración con un flag prohibido | webd no arranca | Error, registro |
| Simular una versión instalada tres mayores por detrás | Perfiles persistentes bloqueados; efímera disponible; aviso emitido | Respuesta de `browser_open_session`, evento |
| Mensaje de Chrome de 70 MiB (sitio local que genera un árbol enorme) | La sesión de esa pestaña se marca `crashed`; el resto sigue | Estado, registro |

---

## 5. Perfiles, sesiones, pestañas y continuidad

### 5.1 Entidades

| Entidad | Qué es | Persistencia |
|---|---|---|
| **Perfil** (`web_profiles`) | Directorio de datos de Chrome (`--user-data-dir`) con su configuración: clase `persistent` o `ephemeral`, sitios con sus conjuntos de orígenes, sitios en escucha, estado observado de inicio de sesión por sitio (`unknown`, `logged_in`, `needs_login`), versión de Chrome que lo abrió por última vez, tamaño en disco | Disco del usuario web, modo `0700` |
| **Instancia** | Un proceso Chrome con su árbol. Hay una instancia por perfil persistente y una instancia efímera compartida | En RAM; su último estado en `webd.db` |
| **Contexto** | `BrowserContext` de CDP. En una instancia persistente se usa el contexto por defecto del perfil; en la efímera, uno nuevo por ejecución (`Target.createBrowserContext{disposeOnDetach: true}`), que no comparte almacenamiento ni procesos de renderizado con otros [E][ProcModel] | El de la efímera desaparece al cerrarse |
| **Sesión** (`session_id` del contrato) | Vínculo entre un propietario (ejecución del arnés), un perfil, un contexto, un modo, un conjunto de orígenes y la marca `listen` | `web_sessions` |
| **Pestaña** (`tab_id`) | Un *target* de tipo página, cada uno en su propia ventana (`Target.createTarget{newWindow: true}`), con propietario (ejecución y etiqueta de subtarea), época de documento, instantánea vigente y estado | `web_tabs` (URL y título para restaurar) |

### 5.2 Clases de perfil y configuración por defecto

**Perfil persistente `personal`** (uno por principal). Aloja las cuentas del usuario. Cada sitio declara qué orígenes de **documento de nivel superior** admite, cuáles son de inicio de sesión (solo humano por defecto) y si debe mantenerse en escucha:

```toml
[web.profiles.personal]
class = "persistent"
isolation = "shared_instance"   # o "per_site": una instancia por sitio (más RAM, más aislamiento)

[[web.profiles.personal.sites]]
name = "google"
rw = ["https://www.google.com", "https://mail.google.com", "https://calendar.google.com", "https://drive.google.com", "https://docs.google.com"]
login = ["https://accounts.google.com"]      # human_only
listen = false

[[web.profiles.personal.sites]]
name = "whatsapp"
rw = ["https://web.whatsapp.com"]
listen = true
listen_probe = { unread_selector = "" }     # opcional; vacío = solo cambios de título

[[web.profiles.personal.sites]]
name = "teams"
rw = ["https://teams.microsoft.com", "https://teams.cloud.microsoft"]
login = ["https://login.microsoftonline.com", "https://login.live.com"]
listen = true
```

Los subrecursos (CDN, API, WebSocket) no pasan por la compuerta de documentos, pero sí por el proxy (12.3). Los orígenes de `login` son **solo para humanos**: el agente no escribe en ellos salvo que la configuración del sitio permita expresamente el relleno de credenciales (10.3). Para Google es obligatorio que sea humano [E][Google-signin].

**Instancia efímera** para la web abierta: arranca bajo demanda, crea un contexto por ejecución, bloquea los permisos (12.6) y se detiene tras 10 minutos sin contextos. No guarda cookies ni almacenamiento entre ejecuciones.

**Coste medido** [M] (Chrome 155 headless, sandbox activo, flags de 4.4):

| Configuración | Procesos | PSS total | USS total |
|---|---|---|---|
| Instancia en blanco, flags por defecto | 14 | 336 MiB | 192 MiB |
| Instancia en blanco, flags de reducción de 4.4 | 9 | 257 MiB | 134 MiB |
| `www.google.com` | 9 | 308 MiB | 171 MiB |
| `web.whatsapp.com` (pantalla QR, sin sesión) | 10 | 466 MiB | 317 MiB |
| `teams.microsoft.com` (pantalla de carga, sin sesión) | 10 | 320 MiB | 173 MiB |
| **Los tres sitios en una sola instancia** | 14 | **528 MiB** | 361 MiB |
| Los tres sitios en tres instancias (suma) | 29 | 1094 MiB | 661 MiB |

Por eso el perfil `personal` usa por defecto **una instancia compartida** para sus sitios: el aislamiento por sitio de Chrome mantiene procesos de renderizado separados para Google, WhatsApp y Microsoft [E][ProcModel], y el ahorro medido es de ≈566 MiB. La variante `per_site` existe para quien prefiera aislar también el proceso del navegador. Son cifras **sin sesión iniciada**; con cuentas reales WhatsApp Web se ha descrito en 400–500 MB por sesión y una pestaña de Gmail en 120–180 MB PSS [E][Mem-public], valores que B6 debe medir en la propia máquina.

**Variante compacta** (`web.compact = true`, para máquinas con poca RAM): los contextos efímeros se crean dentro de la instancia persistente. Ahorra ≈250 MiB (el coste fijo de una instancia) a cambio de compartir el proceso del navegador con las cuentas. Desactivada por defecto.

### 5.3 Estados

```mermaid
stateDiagram-v2
  [*] --> opening
  opening --> ready: instancia y contexto listos
  opening --> blocked: needs_login / version_blocked
  ready --> busy: operación admitida
  busy --> ready: operación terminada
  busy --> blocked: captcha / login / 2fa / sitio
  ready --> blocked: detector en espera o evento
  blocked --> human_control: takeover.start
  human_control --> ready: takeover.end y detectores limpios
  human_control --> blocked: takeover.end sin resolver / timeout
  blocked --> ready: condición desaparece
  ready --> suspended: browser_suspend_session (no listen)
  suspended --> opening: reapertura
  ready --> closing: browser_close_session
  closing --> closed
  ready --> lost: caída de webd o de Chrome (efímera)
  ready --> recovering: caída de Chrome (persistente)
  recovering --> ready: relanzada y pestañas restauradas
  recovering --> lost: demasiados reintentos
  closed --> [*]
  lost --> [*]
```

| Estado | Significado | Qué devuelve una operación del agente |
|---|---|---|
| `opening` | Lanzando instancia o creando contexto | Espera hasta 30 s |
| `ready` | Disponible | Se ejecuta |
| `busy` | Otra operación sobre la misma pestaña | Cola de hasta 30 s; después `tab_busy` |
| `blocked(kind)` | Requiere a una persona (9) | `blocked` con detalle; el adaptador lo convierte en `input_required` |
| `human_control` | Una persona controla la pestaña | `blocked(human_control)` |
| `suspended` | Sin pestañas vivas; URL y títulos guardados | Se reabre en la primera operación |
| `recovering` | Relanzando la instancia persistente tras una caída | Espera hasta 30 s; después `session_recovering` |
| `lost` | No se puede recuperar (efímera tras caída) | `session_lost`; el agente abre otra |
| `closed` | Cerrada a petición | `session_closed` |

**Instancias**: `starting`, `running`, `degraded` (versión con aviso), `stopping`, `stopped`, `crashed`, `failed` (más de 5 relanzamientos en 10 minutos; requiere orden del usuario).

**Pestañas**: `loading`, `ready`, `blocked`, `crashed`, `closed`.

### 5.4 Modos: sin interfaz, con interfaz e intervención humana

- **Por defecto, `headless`.** Es lo que el agente usa casi siempre.
- **`headed` con pantalla disponible** (`DISPLAY` o `WAYLAND_DISPLAY` en el entorno del usuario web): si la instancia del perfil no está viva, se lanza sin `--headless`, con el mismo perfil. Si ya está viva en headless, se devuelve `mode_unavailable` con la sugerencia de intervención por visor; relanzar conservando cookies de sesión es la ampliación A-02.
- **`headed` sin pantalla** (la VPS): se trata como **intervención por visor** sobre la instancia headless (sección 9). No hay relanzamiento, así que se conservan `sessionStorage`, WebSockets y el estado de la aplicación.
- **Credenciales compartidas entre modos**: el perfil es el mismo directorio y las credenciales del baúl se resuelven igual en ambos modos (10). Chrome no permite dos procesos sobre el mismo perfil a la vez (bloqueo `SingletonLock`), y webd garantiza una sola instancia por perfil.

**Inicio de sesión inicial de cada sitio.** Lo hace una persona, una vez:

1. El agente abre la sesión del sitio; el detector de 9.1 devuelve `needs_login` (WhatsApp muestra su QR; Teams y Google redirigen a su página de inicio de sesión).
2. El arnés pide al usuario intervenir; el usuario abre el visor, escanea el QR con su teléfono o introduce sus credenciales y su segundo factor.
3. Al terminar, webd confirma que la página ya no es de inicio de sesión y marca el sitio `logged_in`.
4. En adelante el agente reutiliza la sesión persistente.

Si Google rechaza el inicio de sesión («Este navegador o aplicación puede no ser seguro»), webd lo detecta como `blocked_by_site(signin_rejected)`. No se intenta eludir: el usuario puede usar un nodo de escritorio (arnés 14.10) o los conectores oficiales del arnés (`workspace`, 14.9). **Que el inicio de sesión en Google funcione desde la VPS no está verificado** (R-04).

### 5.5 Cookies y almacenamiento

- **Dónde**: dentro del perfil (cookies en la base SQLite de Chrome; `localStorage`, IndexedDB y service workers en sus directorios). WhatsApp Web guarda sus claves de vinculación en el almacenamiento del sitio, por eso **nunca** se borran automáticamente `cookies`, `indexeddb`, `local_storage` ni `service_workers` de un sitio en escucha [E][Caches].
- **Cifrado en Linux**: Chrome cifra cookies y contraseñas con OSCrypt; con `gnome-libsecret` o `kwallet` la clave procede del llavero. Sin llavero (servidor sin sesión gráfica) usa `basic`, cuya clave v10 es la constante pública `PBKDF2-HMAC-SHA1("peanuts","saltysalt",1)`: en la práctica, **texto claro para quien pueda leer el perfil** [E][OSCrypt]. webd usa `gnome-libsecret` si el Secret Service del usuario web responde y, si no, `basic` con aviso explícito en `browser_status`. La protección real en el servidor es de permisos: perfil `0700` del usuario web, que el arnés no monta en ninguna sandbox de herramientas. Si el arnés corre como root puede leerlo; no se promete otra cosa. El cifrado en reposo es la ampliación A-14.
- **Las órdenes de cookies de CDP nunca se exponen al modelo**: `Storage.getCookies` devuelve todas las cookies, incluidas las `HttpOnly` [E][OSCrypt]. Ninguna herramienta acepta un método CDP arbitrario (12.4, control P0-3).
- **Cookies de terceros**: en contextos efímeros se bloquean (privacidad, rastreo entre sitios). En el perfil persistente se mantiene el comportamiento por defecto de Chrome, porque los flujos federados de Microsoft y Google pueden necesitarlas. El mecanismo concreto (política `BlockThirdPartyCookies` o control CDP equivalente en la versión fijada) está **NO VERIFICADO** y se resuelve en B5.
- **Cookies de sesión**: viven mientras la instancia vive. Para no perderlas en un relanzamiento, el perfil persistente se lanza con la restauración de sesión activada en sus preferencias. Que Chrome 155–156 conserve así las cookies de sesión está **NO VERIFICADO** y se prueba en B2.
- **Caducidad y borrado**: las cookies caducan según el sitio. Existe una operación del host (no del modelo), `web.profile.clear(profile, site, kinds)`, que requiere aprobación del usuario y usa `Storage.clearDataForOrigin`. Los contextos efímeros desaparecen al cerrarse.

### 5.6 Varias interfaces abiertas a la vez

Responde a la duda de las anotaciones del 2026-10-25:

- Cada pestaña tiene **propietario**: la ejecución que la creó y una etiqueta de subtarea opcional (`label`). El arnés guarda en la ejecución qué pestaña corresponde a cada subtarea (arnés 14.2, regla 5).
- **Límites**: 4 pestañas por ejecución, 12 en total y 3 contextos efímeros simultáneos (apéndice A). El manifiesto publica estos máximos para que el orquestador no exceda la capacidad.
- **Exclusión por pestaña**: una sola operación a la vez; las demás esperan en cola hasta 30 s y después reciben `tab_busy`. Una intervención humana toma la pestaña en exclusiva.
- **Sesiones persistentes compartidas**: la pestaña de WhatsApp es una sola, aunque la usen varias ejecuciones. Cada ejecución obtiene un **arrendamiento** de la pestaña mientras dura su operación; la escucha sigue activa entre medias.
- **`browser_tabs list`** muestra a cada ejecución sus pestañas y las de las sesiones persistentes que tiene abiertas. `switch` cambia la pestaña activa lógica de la ejecución y la trae al frente (`Page.bringToFront`) antes de capturar.

### 5.7 Sesiones en escucha y eventos

Una sesión de un sitio con `listen = true` (WhatsApp, Teams) es de clase **LIVE**:

1. Su pestaña vive en ventana propia, con `Emulation.setFocusEmulationEnabled(true)`. En Chrome 155 headless una pestaña antigua de la misma ventana pasa a `hidden`, y una cadena de `setTimeout` de 50 ms disparó solo 6 veces en 5 s; con `--disable-background-timer-throttling` disparó 98 [M*].
2. La instancia persistente se lanza con los tres flags antirretención de 4.4.
3. **Nunca se congela, descarta, recarga por presión ni se le envían notificaciones de presión de memoria** (13.4). Congelar una página detiene temporizadores y retrollamadas de red: con `Page.setWebLifecycleState(frozen)`, un `setInterval` de 100 ms dio 0 ticks en 3 s [M*][Lifecycle].
4. **Eventos**: webd observa `Target.targetInfoChanged` (cambio de título, por ejemplo «(3) WhatsApp») y, si el sitio configura `unread_selector`, un `MutationObserver` instalado en un *mundo aislado* que cuenta los no leídos. Emite `listener_activity{session, site, kind: title_changed|unread_count, count?, at}`, **sin contenido de mensajes**. Leer un mensaje exige una operación explícita del agente, cuyo resultado se marca como no confiable. Como máximo un evento cada 5 s por sitio.
5. **Fallos**: si la pestaña cae, se recarga; si la instancia cae, se relanza con espera creciente (1 s, 5 s, 30 s); si el sitio cierra la sesión, se emite `needs_login`.
6. La conexión con disparadores del arnés (21.3) es posterior; aquí se emiten los eventos y se prueba su llegada.

### 5.8 Pruebas de verificación

| Prueba | Resultado esperado | Evidencia |
|---|---|---|
| Abrir el perfil persistente, guardar una cookie persistente y un valor en IndexedDB en un sitio local, cerrar webd y reabrir | Cookie e IndexedDB presentes; sitio `logged_in` conservado | Lectura desde la página de prueba |
| Cookie de sesión antes y después de relanzar la instancia persistente | Se conserva, o se documenta que no (NO VERIFICADO) | Resultado registrado en el informe de B2 |
| Dos ejecuciones con contexto efímero visitando el mismo sitio local que fija una cookie | Cada una ve solo la suya; al cerrar no queda nada en disco | Cookies por contexto, listado del directorio |
| Abrir el mismo perfil persistente dos veces | Una sola instancia; la segunda sesión reutiliza la instancia | Lista de procesos |
| Quinta pestaña de una ejecución y decimotercera global | `tab_limit` con el límite aplicado | Respuestas |
| Dos operaciones simultáneas sobre la misma pestaña | La segunda espera; si supera 30 s, `tab_busy` | Tiempos |
| Página local de «chat» con *long-polling* que cambia el título cada 20 s, en escucha durante 10 min con otras pestañas activas | Un `listener_activity` por cambio (límite de frecuencia respetado); temporizadores sin congelar; nunca desalojada | Eventos, contador de la página |
| Matar el proceso de renderizado de la pestaña en escucha | Se recarga sola y vuelve a emitir eventos | Eventos `tab_state` |
| Sitio de `login` configurado como `human_only` y `browser_act type` sobre él | `denied(human_only_origin)` | Respuesta, auditoría |

---

## 6. Observación: instantáneas, DOM, referencias y capturas

### 6.1 Evidencia que fija el formato

| Fuente | Hallazgo |
|---|---|
| [Mind2Web] | Un clasificador pequeño preselecciona candidatos del DOM (recall@50 de 85–89 %) y el LLM elige entre 5; las páginas tienen ≈1 135 elementos de media (§3–4, tablas 1–2) |
| [WebArena] | El árbol de accesibilidad es más compacto que el DOM; los elementos se representan como `[1582] role 'text'` |
| [BrowserGym] | Árbol AX con un identificador `bid` inyectado; el árbol puede superar 100 000 tokens y el DOM un millón; añadir capturas SoM aportó ≈0 (§3.1, §5.5) |
| [SeeAct] | La anclación es el cuello de botella: con elecciones textuales la tasa por paso es 39,1 frente a 20,3 con *Set-of-Mark* (tabla 3) |
| [VWA] | Captura + árbol con SoM: 16,37 %; solo texto: 7,25 %; SoM empeora levemente a modelos visuales débiles (tabla 3) |
| [WebVoyager] | Multimodal 59,1 % frente a 40,1 % solo texto |
| [AgentOccam] | Limpiar la observación (fusionar textos estáticos, tablas en Markdown) sube WebArena de 31,7 a 37,1 (tabla 17) |
| [ReadMore] | Tokens por paso en WorkArena L1: árbol 6 720, HTML 56 653, árbol + captura 7 446; el HTML hunde a modelos débiles (Llama-3.1-70B de 18,2 a 3,6) (tabla 1) |
| [FocusAgent] | Podar el árbol en más del 50 % conserva la precisión; truncar a ciegas a 5 000 tokens la baja a 41,8; la poda reduce la inyección por *banners* de 32–90 % a ≈1 % |
| [D2Snap] | Reducir el DOM conservando la jerarquía alcanza 67–73 %, frente a 38 % del DOM truncado (muestra pequeña) |
| [M] GitHub (repositorio de tantivy) | `DOMSnapshot` 547 KB en 254 ms, 3 615 nodos; árbol AX 926 KB en 167 ms; `DOM.getDocument` 844 KB; 264 controles interactivos → lista compacta de ≈8 KB (≈2 000 tokens); captura JPEG q60 ≈90 KB |
| [M*] Wikipedia y Amazon | Árbol completo en texto ≈18 000 tokens; lista interactiva 3 700–5 700 tokens; `Accessibility.getFullAXTree` 0,6–1,5 s |

Conclusión [I]: el modelo recibe por defecto una **lista interactiva con jerarquía mínima, limitada a la vista ampliada**, con referencias cortas; la estructura completa queda en un blob para consultas por subárbol; la captura existe siempre como blob y viaja al modelo solo cuando aporta (arnés 14.2, regla 3).

### 6.2 Algoritmo de captura

Entrada: `tab_id`, `screenshot`, `dom: full|interactive`, `extent: viewport|fullpage`, `wait` opcional.

1. **Comprobación de espera**: si no hay un `ready` vigente para la época de documento actual (7.5), se ejecuta la espera indicada o la predeterminada. Si el resultado es `blocked`, la instantánea no se toma y se devuelve el bloqueo.
2. **Captura en paralelo**, con un presupuesto total de 3 s:
   - `DOMSnapshot.captureSnapshot{computedStyles: [display, visibility, opacity, pointer-events, cursor, overflow-x, overflow-y, position, content-visibility], includePaintOrder: true, includeDOMRects: true}` en la sesión principal (incluye iframes del mismo proceso y *shadow DOM* aplanado) [E][CDP]. La lista de estilos se mantiene corta porque las largas aumentan el coste y browser-use las limita para evitar caídas en sitios pesados [E][browser-use].
   - `Accessibility.getFullAXTree` en la sesión principal y en **cada sesión de OOPIF**: la llamada en la sesión principal no devuelve el contenido de iframes fuera de proceso [E][CDP][M*].
   - `Page.getLayoutMetrics` para la vista y el desplazamiento.
   - `Page.captureScreenshot{format: jpeg, quality: 60}` si `screenshot`, tras `Page.bringToFront` (las pestañas ocultas pueden colgar la captura [E][Lifecycle-headless]).
3. **Unión**: nodos AX → nodos DOM por `backendDOMNodeId`, **siempre dentro de la misma sesión**, porque los identificadores colisionan entre procesos de OOPIF (28 colisiones medidas entre una página y un iframe de otro sitio) [M*]. Las coordenadas de cada OOPIF se trasladan con la caja de su `<iframe>` propietario.
4. **Detección de interactivos**, por niveles, guardando el motivo (`why`):
   1. rol AX de widget (`button`, `link`, `textbox`, `searchbox`, `combobox`, `checkbox`, `radio`, `switch`, `slider`, `tab`, `menuitem`, `option`, `spinbutton`, `treeitem`) o propiedades `focusable`, `editable`, `settable`;
   2. etiquetas: `a[href]`, `button`, `input`, `select`, `textarea`, `summary`, `label` que envuelve un control, `[contenteditable]`, `[tabindex>=0]`, `[onclick]`;
   3. `isClickable` del `DOMSnapshot` (incluye escuchadores de clic añadidos por JavaScript) [E][CDP] combinado con `cursor: pointer` no heredado.
5. **Visibilidad y habilitación**: `visible` = caja no vacía, `visibility` distinta de `hidden`, opacidad > 0 y no tapado por completo en su centro según el orden de pintado; `in_viewport` aparte; `enabled` desde la propiedad AX `disabled` (cubre `fieldset` y `aria-disabled`).
6. **Asignación de referencias** (6.4).
7. **Redacción**: el valor de los campos `password`, `autocomplete=one-time-code`, `cc-*` y los rellenados desde el baúl se sustituye por `"[redacted]"` antes de serializar, tanto en el texto como en el blob [E][browser-use]. El pipeline de redacción del arnés (`Redactor`) se aplica además a todo lo que entra en contexto.
8. **Serialización**: texto compacto (6.3), lista JSON de elementos y blob de estructura completa (JSON comprimido con zstd): índice de padre, etiqueta, atributos filtrados (`id`, `class` truncado, `name`, `type`, `href`, `aria-*`, `role`, `alt`, `title`, `placeholder`), rol, nombre, propiedades, caja, orden de pintado, estilos y marco.
9. Si se agota el presupuesto de 3 s, se devuelve lo capturado con `partial: true` y la lista de lo que falta (por ejemplo, el árbol de un OOPIF).

### 6.3 Formato hacia el modelo

Alcance por defecto: la vista ± una pantalla, con elementos interactivos, regiones (`banner`, `main`, `navigation`, `form`, `dialog`) y encabezados, conservando la jerarquía. Tope de **16 KB (≈4 000 tokens)**; lo que excede se cuenta en `omitted` y queda en el blob.

```text
# snap s_42 · tab t1 · https://shop.example/s?k=laptop · "Resultados: laptop"
# load=complete ready=ready · viewport 1280x900 scroll y=1200/9800 · untrusted
- banner
  - searchbox "Buscar" [e3] value="laptop"
  - button "Ir" [e4]
- main
  - heading "Resultados" (h2)
  - link "Portátil 15,6 pulgadas, 16 GB…" [e17]
  - button "Añadir a la cesta" [e18] *new
  - checkbox "Envío rápido" [e21] checked
  - button "Siguiente" [e40] disabled
  - iframe "Patrocinado" [f1]
    - button "Cerrar anuncio" [f1e2]
# 412 elementos interactivos fuera de la vista (browser_snapshot extent=fullpage)
```

- `*new` marca elementos que no estaban en la instantánea anterior de la pestaña. Es un marcado barato; no se envían *diffs* estructurales completos, que Playwright añadió y retiró por complejidad sin beneficio [E][PlaywrightMCP].
- `dom: full` añade al blob el esquema jerárquico completo; el texto inline sigue limitado.
- Todo el texto procede de la página: el adaptador lo entrega como contenido no confiable con su URL de procedencia (12.4).

Salida JSON del contrato (arnés 14.2): `snapshot_id`, `url`, `title`, `load_state`, `screenshot_ref`, `dom_tree_ref`, `elements[]` con `ref`, `role`, `name`, `type`, `bbox{x,y,w,h}`, `visible`, `enabled`, `in_viewport`, `frame`, `why`, `value?`, más `text`, `omitted`, `partial`, `wait` (apéndice B).

### 6.4 Referencias y `stale_ref`

**Clave interna**: `ElementKey{target_id, session_id, frame_id, loader_id, backend_node_id}`.

- El `backendNodeId` es estable ante cambios de atributos, texto y desplazamientos del mismo nodo; un nodo reemplazado recibe otro; un nodo eliminado aún se resuelve, pero con `isConnected = false`; tras recargar, el identificador deja de pertenecer al documento [M*].
- Cada pestaña mantiene un mapa `ElementKey → ref`: el mismo nodo vivo conserva su referencia entre instantáneas, como hacen Chrome DevTools MCP y agent-browser [E][ChromeDevToolsMCP][agent-browser]. Los números nunca se reutilizan durante la vida de la pestaña.
- Referencias hacia el modelo: `e<N>` en el marco principal y `f<K>e<N>` en iframes.
- Con cada referencia se guarda una huella: rol, nombre, etiqueta y caja.

**Validez.** `browser_act(snapshot_id, ref)` devuelve `stale_ref{reason, ref, snapshot_id, current_snapshot_id, remapped_ref?}` si:

| Condición | `reason` |
|---|---|
| `snapshot_id` no es la instantánea vigente de la pestaña (hubo una posterior) | `superseded` |
| El `loaderId` del marco cambió (navegación) | `navigated` |
| El marco o su sesión desaparecieron | `frame_gone` |
| `DOM.resolveNode` falla | `node_gone` |
| `isConnected` es falso (los nodos separados se siguen resolviendo) | `node_detached` |
| La huella cambió (rol o nombre distintos) | `changed` |
| Hubo intervención humana o reinicio del navegador desde la instantánea | `takeover` / `browser_restarted` |

- Si la clave aún existe en la instantánea vigente, se incluye `remapped_ref` para que el agente se recupere en una llamada, **pero no se actúa por su cuenta**: nunca se redirige en silencio por rol y nombre (fallo abierto en agent-browser #1922) ni se devuelve una referencia vieja como éxito (browser-use #5438) [E][agent-browser][browser-use].
- **Qué hace vigente a una instantánea**: solo `browser_snapshot` crea una nueva. `browser_act` **no** la invalida: valida el nodo en cada acción. Así se rellena un formulario con varias acciones seguidas sobre la misma instantánea mientras el documento no cambie. Si una acción navega o modifica mucho la vista, su resultado lo indica (`navigated`, `suggest_snapshot`) y la siguiente acción con referencias del documento anterior recibe `navigated`.

### 6.5 Capturas y visión

- **Por defecto**: JPEG calidad 60 de la vista (≈90 KB en GitHub [M]); `extent: fullpage` usa `captureBeyondViewport` (experimental) con un alto máximo de 16 384 px.
- La captura se guarda siempre como blob. El modelo recibe la referencia y el adaptador **solo envía la imagen al rol `vision`** cuando el agente la pide o cuando la página es visual: árbol AX con menos de 20 nodos útiles y más del 40 % de la vista cubierto por `canvas`, `img` o `video` (heurística inicial [I]). Es la regla 3 de 14.2 del arnés.
- La elección de modelo, resolución y recorte para leer capturas es configuración del rol `vision` del arnés (duda de las anotaciones resuelta en el arnés 14.4).

### 6.6 Consultas de subárbol

`observe.subtree(snapshot_id, ref)` devuelve, desde el blob, el esquema completo bajo una referencia (por ejemplo, una tabla larga) sin volver a capturar. Es una operación de webd usada por `browser_snapshot` con `ref` opcional; no añade una herramienta nueva.

### 6.7 Herramientas de desarrollo y emulación

- **Consola**: `Runtime.consoleAPICalled`, `Runtime.exceptionThrown` y `Log.entryAdded` en un anillo de 500 entradas por pestaña.
- **Red**: anillo de 500 resúmenes (URL sin *query* si la ejecución está contaminada, método, estado, tipo, tamaño, tiempos), sin cuerpos y con `Authorization`, `Cookie` y `Set-Cookie` eliminados. `Network.enable{maxTotalBufferSize: 2 MiB, maxResourceBufferSize: 512 KiB}`, porque por defecto el renderizador guarda hasta 200 MB de cuerpos por sesión DevTools [E][NetBuf]. En pestañas en escucha, la red solo se habilita mientras dura una operación del agente.
- **Rendimiento**: `Performance.getMetrics` (nodos, documentos, oyentes, `JSHeapUsedSize`, duraciones de *layout* y *script*) [E][CDP].
- **Emulación** (`browser_emulate`): `Emulation.setDeviceMetricsOverride` con tamaños predefinidos `small` (360×740, factor 3, móvil), `tablet` (768×1024), `desktop` (1280×900) y `large` (1440×900), o un tamaño explícito. En sesiones persistentes se rechaza la emulación móvil con agente de usuario distinto, para no alterar cómo ve el sitio la cuenta del usuario; en efímeras se permite como comprobación de páginas (Fénix 35).

### 6.8 Pruebas de verificación

| Prueba | Resultado esperado | Evidencia |
|---|---|---|
| Página local con 300 controles, *shadow DOM* abierto y cerrado, iframe del mismo sitio y OOPIF de otro sitio (otro host local mapeado por el resolvedor de pruebas) | Todos los controles visibles aparecen con su marco; el OOPIF usa `f<K>e<N>`; coordenadas correctas | Instantánea, comparación con la lista esperada |
| Elementos ocultos (`display:none`, `visibility:hidden`, opacidad 0, tapados por un modal, fuera de vista) | `visible` e `in_viewport` correctos en cada caso | Instantánea |
| Página con 5 000 controles | Texto ≤ 16 KB con `omitted` > 0; blob completo; tiempo ≤ 3 s o `partial: true` | Tamaños, tiempos |
| Campos `password`, `one-time-code` y `cc-number` con valor | `"[redacted]"` en el texto, la lista y el blob | Búsqueda del valor en las tres salidas |
| Actuar con una referencia tras una navegación, tras eliminar el nodo, tras reemplazarlo, tras una instantánea posterior y tras una intervención | `stale_ref` con `navigated`, `node_detached`, `node_gone`/`changed`, `superseded` (+`remapped_ref`) y `takeover` | Respuestas |
| Dos instantáneas seguidas sin cambios | Mismas referencias para los mismos nodos; ninguna marca `*new` | Comparación |
| Captura de una pestaña en segundo plano | No se cuelga (frente al cuelgue conocido de pestañas ocultas) | Tiempo de captura |
| `browser_devtools` de red tras una petición con cabecera `Authorization` | Cabecera ausente; sin cuerpo | Registros |
| `browser_emulate small` y captura | Captura de 360 px de ancho lógico; la página responde a la consulta de medios | Captura, métricas |

---

## 7. Espera de carga (`browser_wait_ready`)

### 7.1 Por qué no basta con `networkIdle`

- El detector de inactividad de Blink considera «casi inactiva» la red con ≤2 conexiones y «inactiva» con 0, durante 500 ms; empieza tras `DOMContentLoaded`, cuenta solo las peticiones del documento y **se dispara una vez por documento y deja de observar** [E][Idleness]. No sirve tras una acción en una aplicación de una sola página.
- Medido con Chrome 155 [M*]: un bucle de *long-polling* hizo que `networkIdle` no llegara en 15 s; un WebSocket con 14 tramas no lo impidió (los WebSocket no cuentan). En Amazon, `networkIdle` llegó a los 10,1 s frente a `DOMContentLoaded` a los 4,0 s.
- Playwright marca `networkidle` como desaconsejado y recomienda aserciones sobre la página [E][PlaywrightMCP]; whatsapp-web.js espera `load` y luego condiciones propias de la aplicación, nunca inactividad de red [E][WA-js].

### 7.2 Algoritmo

Entrada: `tab_id`, `network_idle_ms` (500), `dom_stable_ms` (500), `selector` o `text` opcional, `timeout_ms` (10 000), `max_inflight` (0), `strict_network` (falso).

1. **Compuerta de navegación**: si hay una navegación pendiente (`Page.frameRequestedNavigation`/`frameStartedNavigating`) o cambió el `loaderId` del marco principal, esperar al *commit* y al `DOMContentLoaded` del nuevo documento. `Page.lifecycleEvent` se usa solo para esta primera carga.
2. **Contador de red** por sesión, incluidos los OOPIF: `requestWillBeSent` menos `loadingFinished`, `loadingFailed` y `requestServedFromCache`. Se excluyen WebSocket, EventSource, `ping`/beacons, medios y *prefetch*. Una petición abierta más de **3 s** se reclasifica como `long_lived` y deja de contar (Stagehand usa 2 s y browser-use 10 s [E][Stagehand][browser-use]). Red tranquila = no excluidas en vuelo ≤ `max_inflight` durante `network_idle_ms`.
3. **Estabilidad del DOM**: un `MutationObserver` (`childList`, `subtree`, `attributes`, `characterData`) instalado en un mundo aislado por marco con `Page.addScriptToEvaluateOnNewDocument` y comunicado por `Runtime.addBinding`. Se considera estable tras `dom_stable_ms` sin mutaciones relevantes; se ignoran cambios solo de estilo en nodos fuera de la vista. Un latido con `requestAnimationFrame` detecta un hilo principal ocupado.
4. **Condición**: si hay `selector` o `text`, debe existir y ser visible en algún marco. Es el criterio principal cuando se da.
5. **Bloqueos**: en cada ciclo de 100 ms se ejecutan los detectores de 9.1 y se comprueba si hay un diálogo JavaScript o un selector de archivos abierto. Si aparecen, se devuelve `blocked{kind, vendor, signals, confidence, url, frame_id}`.
6. **Resultado**:
   - `ready` si se cumple la condición (si la hay) y el DOM está estable y la red tranquila;
   - `ready` con `degraded: ["network_busy"]` y las URL en vuelo más antiguas si el DOM está estable y se cumple la condición, pero solo queda tráfico de larga duración durante `network_idle_ms` + 2 s (caso WhatsApp y Teams); con `strict_network: true` esto es `timeout`;
   - `timeout` con `criteria_met{navigation, network, dom, condition}` y diagnóstico;
   - `blocked` (paso 5).
7. **Después de cada acción** (interno): esperar hasta 250 ms a que empiece una navegación y luego ejecutar los pasos 1–6 con 5 s de plazo. La acción nunca falla por esta espera: informa `settled: false`.

### 7.3 Perfiles de espera por sitio

Los sitios en escucha declaran su condición de «listo» en la configuración (selector de la lista de conversaciones, por ejemplo) y usan `strict_network = false`. Los selectores concretos de WhatsApp y Teams cambian con sus versiones; se fijan en la configuración, no en el código, y su validez se comprueba en el recorrido manual (18.4). **No está verificado** que esas aplicaciones nunca lleguen a red inactiva [M*]; el diseño no depende de ello.

### 7.4 Contabilidad de esperas

webd registra por pestaña la última espera con resultado `ready` y su época de documento. Una instantánea o acción sin espera válida para la época vigente ejecuta primero la espera predeterminada (6.2, paso 1). Así se cumple la regla 1 de 14.2 («nunca actuar ni capturar sin esperar») sin obligar al agente a una llamada extra: `browser_navigate` y `browser_snapshot` aceptan el parámetro `wait`, y su resultado incluye el informe de la espera, que el arnés registra.

### 7.5 Pruebas de verificación

| Prueba (páginas locales) | Resultado esperado | Evidencia |
|---|---|---|
| Página estática | `ready` en < 1 s tras `load` | Tiempos |
| Aplicación de una página que carga datos 800 ms después de `load` | `ready` solo después de que aparezcan los datos | Momento del `ready` frente a la marca de la página |
| *Long-polling* permanente + selector presente | `ready` con `degraded: ["network_busy"]`; con `strict_network`, `timeout` | Resultados |
| WebSocket con tráfico continuo | `ready` normal | Resultado |
| DOM que muta cada 100 ms para siempre | `timeout` con `criteria_met.dom = false` | Diagnóstico |
| `alert()` durante la espera | `blocked{kind: dialog}` | Resultado |
| Página con iframe de reCAPTCHA simulado (host mapeado localmente) | `blocked{kind: captcha}` | Resultado |
| Acción que navega | La siguiente instantánea espera al nuevo documento | Secuencia de eventos |
| Instantánea sin espera previa | Ejecuta la espera predeterminada y la incluye en la salida | Salida |

---

## 8. Acciones (`browser_act`)

### 8.1 Canalización de una acción

1. **Admisión**: la pestaña existe y pertenece a la ejecución (o a una sesión persistente que la ejecución tiene abierta); no está en `human_control`; se obtiene su exclusión (5.6).
2. **Validación de la referencia** según 6.4. Si falla, `stale_ref` sin efecto alguno.
3. **Compuerta**: el origen comprometido del marco del elemento debe estar en el conjunto `rw` de la sesión (12.4). La clase de acción (8.3) ya fue calculada y autorizada por el adaptador en `prepare`; webd vuelve a comprobar origen y clase justo antes de enviar.
4. **Accionabilidad**, al estilo de Playwright [E][PlaywrightMCP]: conectado, visible, caja estable entre dos fotogramas consecutivos, habilitado, editable (para `type`) y **recibe los eventos**: `DOM.getNodeForLocation` en el punto de acción devuelve el nodo o un descendiente. Reintentos con espera `[0, 20, 100, 100, 500]` ms alternando la alineación del desplazamiento (`center`, `end`, `start`) para salir de cabeceras fijas. Si no se consigue: `not_actionable{reason, covered_by: "<rol> <nombre>"}`, sin forzar.
5. **Registro de intención en webd**: `web_ops(op_key)` pasa a `dispatched` (16.4).
6. **Envío** de los eventos de entrada (8.2).
7. **Asentamiento** interno (7.2, paso 7) y detección de navegación, pestañas nuevas, diálogos, descargas y selectores de archivos.
8. **Resultado**: `{result, navigated, url, new_tabs[], dialog?, download?, dom_changed, suggest_snapshot, settled, elapsed_ms}`; `web_ops` pasa a `completed` con el resultado.

### 8.2 Acciones

| Acción | Implementación CDP | Detalles |
|---|---|---|
| `click` | `DOM.scrollIntoViewIfNeeded` → `DOM.getContentQuads` (centro del primer cuadrilátero visible) → `Input.dispatchMouseEvent` `mouseMoved`, `mousePressed`, `mouseReleased` | Opciones: `button`, `click_count`, `modifiers`. Coordenadas en píxeles CSS del marco principal [E][CDP] |
| `type` | `DOM.focus` → seleccionar todo (`Input.dispatchKeyEvent` con `commands: ["selectAll"]`, experimental, o Ctrl+A) → `Input.insertText(value)` | Semántica de **rellenar**: sustituye el contenido, por eso es idempotente. No pulsa Enter. `options.key_events: true` envía tecla a tecla para *frameworks* que ignoran `insertText` |
| `select` | `Runtime.callFunctionOn` en mundo aislado sobre el `<select>`: elige la opción por valor o etiqueta y emite `input` y `change` | Las listas personalizadas se manejan con `click` |
| `hover` | `mouseMoved` al centro | — |
| `scroll` | Con referencia: `DOM.scrollIntoViewIfNeeded`; sin referencia: `mouseWheel` con `deltaY` en el centro de la vista | `direction`, `amount` en pantallas |
| `press` | `Input.dispatchKeyEvent` `keyDown`/`keyUp` con `key`, `code`, `text`, `modifiers` | Combinaciones como `Control+Enter`; tabla de ≈60 teclas |
| `upload` | `DOM.setFileInputFiles{files, backendNodeId}` con rutas del área de preparación (11.2); si el control abre un selector, `Page.setInterceptFileChooserDialog(true)` y `Page.fileChooserOpened` | Nunca con rutas del arnés |
| `drag` | `mousePressed` en el origen, 5 pasos de `mouseMoved`, `mouseReleased` en `target_ref`; con `options.html5: true`, `Input.setInterceptDrags` + `Input.dispatchDragEvent` (experimental) | — |
| `dialog` (extensión compatible) | `Page.handleJavaScriptDialog{accept, promptText?}` | Necesaria porque un `alert` detiene la página mientras el dominio `Page` está activo [E][CDP] |

### 8.3 Clase de acción, efecto y riesgo

El adaptador calcula la clase **de forma determinista** en `prepare` a partir de la instantánea (rol, etiqueta, formulario contenedor, nombre accesible, sitio). Es la base de la autorización del arnés y de la recuperación:

| Clase | Cuándo | `Effect` | Riesgo en efímera | Riesgo en persistente |
|---|---|---|---|---|
| `observe` | `hover`, `scroll` | `Pure` | Bajo | Bajo |
| `input` | `type` en un campo, `select` en un `<select>` | `Idempotent` | Bajo | Medio |
| `navigate_link` | `click` en `a[href]` fuera de un formulario, sin manejadores de envío | `NonIdempotent` | Bajo | Medio |
| `commit` | `click` en `button`/`input[type=submit]` dentro de un formulario; `press Enter` en un campo de formulario o en el compositor de un sitio en escucha; nombre accesible con el léxico de compromiso (enviar, send, submit, publicar, post, comprar, buy, pagar, pay, confirmar, confirm, eliminar, delete, borrar, transferir, transfer, firmar, sign, reservar, book, suscribir, subscribe, aceptar, accept) | `NonIdempotent` | Medio | **Alto** |
| `upload` | `upload` | `NonIdempotent` | Alto | Alto |
| `credential` | `type` con `secret_ref` (10) | `NonIdempotent` | Alto | Alto |
| `drag` | `drag` | `NonIdempotent` | Medio | Medio |

- En perfiles persistentes, **un `click` que no se puede clasificar con seguridad se trata como `commit`** [I]. Cada sitio puede afinar en su configuración (por ejemplo, en WhatsApp, Enter en el compositor es `commit` y un clic en la lista de chats es `navigate_link`).
- El arnés aplica su autonomía y sus reglas sobre esa clase: un `commit` en una sesión persistente pide aprobación salvo concesión explícita para ese sitio y esa acción; la solicitud muestra el origen, la acción, el nombre del elemento y un resumen redactado de lo escrito. La regla de dos fuentes (una acción motivada solo por contenido no confiable pide aprobación) es del arnés [E][arnés 20.8].
- Es la compuerta determinista de acciones que recomiendan Google para su agente de Chrome y Brave tras el caso Comet [E][ChromeAgentSec][Brave-Comet]. No sustituye a la persona: si el léxico o la clasificación fallan, la acción puede ejecutarse sin la aprobación que merecía. Por eso el conservadurismo en persistentes.

### 8.4 Diálogos, ventanas emergentes y pestañas nuevas

- `alert`: se acepta automáticamente y su texto (no confiable) va en el resultado.
- `beforeunload`: se acepta si la navegación la pidió el agente.
- `confirm` y `prompt`: se dejan abiertos y se devuelve `dialog_open{type, message}`; la siguiente acción debe ser `dialog`. En persistentes, aceptar un `confirm` tiene la clase `commit`.
- Una pestaña nueva con `openerId` de una pestaña de la ejecución se registra con el mismo propietario y aparece en `new_tabs`. En sesiones persistentes, una ventana emergente hacia un origen fuera de `rw` se cierra y se informa `popup_blocked`, salvo aprobación.

### 8.5 Pruebas de verificación

| Prueba (páginas locales) | Resultado esperado | Evidencia |
|---|---|---|
| Cada acción sobre su control: botón, campo, `<select>`, menú al pasar el ratón, lista desplazable, atajo de teclado, `input[type=file]`, arrastrar y soltar (ratón y HTML5) | El efecto aparece en la página (contador o valor reflejado por la propia página) | Estado de la página tras la acción |
| Botón tapado por un modal | `not_actionable{covered_by}`; ningún clic llega al botón | Contador de la página |
| Botón que se mueve durante 300 ms | El clic se hace cuando está estable y acierta | Contador |
| `type` dos veces con el mismo valor | Valor final igual (idempotente) | Valor |
| Formulario con envío y léxico «Enviar» en sesión persistente | Clase `commit`, riesgo alto; con autonomía por defecto, aprobación pedida antes del efecto | Solicitud de aprobación, auditoría |
| Clic desconocido en sesión persistente | Tratado como `commit` | Clasificación registrada |
| `confirm()` tras un clic | `dialog_open`; acción posterior que no sea `dialog` rechazada | Respuestas |
| `window.open` hacia otro origen desde sesión persistente | `popup_blocked` | Resultado, lista de pestañas |
| Acción en un marco cuyo origen no está en `rw` | `denied(origin_not_writable)` | Respuesta |

---

## 9. Bloqueos e intervención humana

### 9.1 Detectores

Solo detección; **nunca resolución** [I][D12]. Cada detector produce señales con confianza; una señal de confianza baja se informa como `degraded`, no como bloqueo.

| Tipo (`kind`) | Señales (de mayor a menor peso) | Fuente |
|---|---|---|
| `blocked_captcha` | Cabecera `cf-mitigated: challenge` en la respuesta del documento; iframe **visible** (> 256 px²) de `challenges.cloudflare.com`, `hcaptcha.com`, `arkoselabs.com`, `funcaptcha.com`, `captcha-delivery.com`, o de reCAPTCHA con ruta `/recaptcha/(api2\|enterprise)/anchor` en `google.com` o `recaptcha.net`; marcadores `.g-recaptcha`, `.h-captcha`, `.cf-turnstile`, `[data-sitekey]`, `textarea[name=g-recaptcha-response]`; interstitial de Google (`/sorry/` o texto «unusual traffic»/«tráfico inusual» con reCAPTCHA) | [E][CF-mitigated][Skyvern-challenge]; [M] Google desde la VPS |
| `blocked_by_site` | 403, 429 o 503 del documento con página de bloqueo (PerimeterX `meta[name=description][content=px-captcha]`, «Access to this page has been denied»); página de error de Wikimedia «Too many requests»; rechazo de inicio de sesión de Google | [E][Skyvern-challenge]; [M] Wikipedia desde la VPS |
| `needs_login` | URL de nivel superior en un origen `login` del sitio o de un proveedor de identidad conocido (`accounts.google.com`, `login.microsoftonline.com`, `login.live.com`); `input[type=password]` visible cuando el sitio estaba `logged_in`; detector del sitio (WhatsApp: pantalla «Scan to log in» con QR) | [I]; [M] captura de WhatsApp |
| `needs_2fa` | Campo visible con `autocomplete="one-time-code"`; grupo de casillas `maxlength=1` e `inputmode=numeric` | [E][Skyvern-challenge] |
| Corroboración | Título o texto «verify you are human», «just a moment», «verifica que eres humano» | Solo eleva la confianza |

Salida: `{kind, vendor, signals[], confidence: high|medium|low, url, frame_id}`. browser-use no tiene detector DOM y depende de un juez LLM [E][browser-use]; aquí es determinista y barato, y los selectores por sitio viven en configuración.

### 9.2 Flujo con el arnés

```mermaid
sequenceDiagram
  participant A as Agente (ejecución)
  participant H as Arnés (adaptador)
  participant W as webd
  participant U as Usuario (visor)
  A->>H: browser_wait_ready / browser_act
  H->>W: operación
  W-->>H: blocked{needs_login, whatsapp}
  H-->>A: tarea durable en input_required (sin consumir tokens)
  H->>U: aviso: «WhatsApp necesita que vincules el teléfono»
  U->>W: takeover.start(tab) con código de un solo uso
  W-->>U: transmisión de pantalla; entrada reenviada
  U->>W: takeover.end(done)
  W->>W: detectores de nuevo; época +1
  W-->>H: takeover_state completed, tab ready
  H->>W: reanuda la operación (espera de nuevo)
  W-->>H: resultado
  H-->>A: resultado (referencias anteriores: stale_ref takeover)
```

- El bloqueo se convierte en el estado `input_required` del ciclo de tareas de MCP que adopta el arnés (arnés 14.1) y en una pregunta al usuario. La ejecución queda en espera sin gastar tokens.
- Plazo de la intervención: 15 minutos por defecto. Si vence, o el usuario elige «No puedo», la operación termina en `failed{blocked_unresolved}` y el agente decide (otra fuente, otro método, informar).
- En modo no interactivo (perfil `eval` o `excalibur exec`), un bloqueo termina en fallo `blocked_requires_human` sin esperar, coherente con la política no interactiva del arnés.

### 9.3 Visor de intervención

Como el daemon con puerta de entrada (H8) aún no existe, **webd sirve el visor** mientras dura una intervención:

- **Dónde**: HTTP y WebSocket en `127.0.0.1:<puerto aleatorio>`. En la VPS se usa por túnel SSH; cuando exista la puerta del arnés, esta lo reenviará.
- **Autenticación sin token en la URL** (regla del arnés 20.12): el arnés entrega al usuario, por su canal, un código de un solo uso de 8 caracteres válido 10 minutos; la página lo envía por `POST` y recibe una cookie `HttpOnly`, `SameSite=Strict`, ligada a la pestaña y al plazo. El WebSocket exige esa cookie y `Origin` igual al del visor. Un visor por intervención.
- **Transmisión**: `Page.startScreencast{format: jpeg, quality: 60, maxWidth: 1280, maxHeight: 900, everyNthFrame: 1}` con acuse de cada trama (sin acuses la transmisión se detiene tras 3–4 tramas [M*]). Coste medido: página estática ≈1 trama en 4 s; desplazamiento real en Wikipedia 11,4 Mbit/s a 1280 px y 5,7 Mbit/s a 960 px; latencia de 25–37 ms [M*]. Adecuado para red local y túnel; para enlaces lentos, la ampliación A-07 (WebRTC, 0,7 Mbit/s en una comparación de terceros [E][Screencast]).
- **Entrada**: ratón, rueda, teclas y texto (para pegar y métodos de entrada se usa un cuadro de texto explícito que envía `Input.insertText`). Mapeo de coordenadas como el `InputModel` de DevTools: `x = offsetX / zoom`, `y = offsetY / zoom − offsetTop` [E][Screencast].
- **Seguridad del visor** (XSS y *clickjacking*): todo lo que procede de la página (título, URL, textos de diálogo) se pinta con `textContent`, nunca con `innerHTML`; la imagen va a un `canvas`; cabeceras `Content-Security-Policy: default-src 'none'; img-src data: blob:; script-src 'self'; style-src 'self'; connect-src 'self'; frame-ancestors 'none'; base-uri 'none'; form-action 'self'; require-trusted-types-for 'script'` [E][OWASP-XSS]. La cabecera del visor muestra en grande el **origen comprometido** de la pestaña (no el título de la página), para que la persona detecte suplantaciones. Sin sincronización de portapapeles ni transferencia de archivos. Inicio y fin auditados.
- **Interfaz mínima**: cabecera con origen y estado; botones «Terminado», «No puedo», «Atrás», «Recargar»; lienzo; cuadro de texto. En pantalla pequeña el lienzo se escala al ancho y los botones se apilan; en pantalla grande, escala 1:1 hasta 1280 px. Se revisa visualmente, sin suite funcional (18.5).

### 9.4 Lo que no se hace

No se resuelven CAPTCHA, no se ocultan huellas de automatización, no se cambia el agente de usuario para parecer otro navegador, no se automatiza el inicio de sesión de Google ni el escaneo del QR de WhatsApp. Chrome headless se identifica como `HeadlessChrome/155.0.0.0` y, sin flags de automatización, `navigator.webdriver` es falso [M]; no se toca ni lo uno ni lo otro.

### 9.5 Pruebas de verificación

| Prueba | Resultado esperado | Evidencia |
|---|---|---|
| Páginas locales con cada patrón de 9.1 (hosts mapeados por el resolvedor de pruebas) y páginas de control sin ellos (con insignia invisible de reCAPTCHA v3, con `/cdn-cgi/challenge-platform/` inyectado) | Tipo y proveedor correctos; ningún falso positivo en las de control | Matriz de resultados |
| Bloqueo → `input_required` → intervención simulada por el cliente de pruebas del visor (entra el código, hace clic en el formulario de inicio de sesión local, pulsa «Terminado») | La operación se reanuda y termina; referencias anteriores con `stale_ref(takeover)` | Eventos, registro del arnés |
| Intervención que vence a los 15 min (reloj simulado) | `failed{blocked_unresolved}` | Estado final |
| Acceso al visor sin cookie, con código caducado, con código reutilizado, con `Origin` ajeno y con el código en la URL | Todos rechazados | Respuestas HTTP, auditoría |
| Respuestas del visor | Cabeceras CSP y `frame-ancestors 'none'` presentes | Cabeceras |
| Título de página con `<img src=x onerror=…>` durante la intervención | Se muestra como texto en el visor (revisión visual 18.5); la carga JSON lo transporta escapado | Captura del visor, carga del WebSocket |
| Bloqueo en modo no interactivo | Fallo inmediato `blocked_requires_human` | Código de salida, registro |

---

## 10. Credenciales y baúl

### 10.1 Modelo

- El agente solo ve referencias `secret://…` y metadatos (nombre, tipo, uso, URL). Para el navegador se usan secretos de tipo `WebsitePassword`, que en el baúl **exigen URL** [E][arnés: vault].
- `browser_open_session{credentials: [secret_ref]}` declara qué referencias puede usar la sesión; el arnés autoriza su uso (`UseSecret`) con su política.
- Para escribir una contraseña: `browser_act{action: "type", ref, secret_ref}`. El nombre de usuario, si no es secreto, se escribe con `value` normal.

### 10.2 Algoritmo de relleno

1. **`prepare` en el adaptador**: la referencia está declarada en la sesión; de la URL del secreto se deriva el **origen esperado** (esquema, host y puerto exactos); la clase es `credential` (riesgo alto). El adaptador pregunta a webd `act.check_credential_target{tab, snapshot_id, ref, expected_origin}`, **sin el secreto**.
2. **Comprobaciones de webd**: el elemento es un `input` visible y habilitado (`type=password` para la contraseña); el origen de seguridad del marco del elemento y el de la página de nivel superior son **exactamente** el esperado (no se rellena en iframes de otro origen salvo un proveedor de identidad incrustado declarado en la configuración del sitio); el esquema es `https`; la URL no lleva `userinfo`; la pestaña no está bloqueada ni en intervención; el origen no es `login` `human_only`.
3. **Ejecución con `SecretExecutor::execute(run, operation_id, reference, now, closure)`** del arnés: registra la marca de uso del secreto y la auditoría **antes** de resolver el valor y reclama la aprobación de un solo uso si corresponde [E][arnés: security]. La *closure* envía `act.fill_secret{tab, snapshot_id, ref, expected_origin, value: <sensitive>}` por el socket y devuelve `ToolResult` con `{filled: true}`.
4. **En webd, de forma atómica**: vuelve a comprobar origen y nodo (evita la carrera entre comprobación y uso), `DOM.focus`, selección de todo, `Input.insertText(value)`, `zeroize` del valor, y marca el `ElementKey` como `filled_from_secret` (redactado en toda instantánea posterior).
5. Durante 5 s después del relleno y hasta la siguiente navegación, las capturas de esa pestaña no se envían al modelo con visión (por si el sitio muestra la contraseña al conmutar su visibilidad); el blob queda local.

### 10.3 Por qué origen exacto

- La comparación debe hacerse sobre el **origen comprometido del marco**, no sobre una URL analizada como cadena: CVE-2025-47241 de browser-use permitía saltar la lista de dominios con `https://example.com:pass@localhost:8080` [E][browser-use].
- El *clickjacking* de extensiones de autocompletado demostró fugas de TOTP en 9 de 11 gestores de contraseñas y rellenado en todos los subdominios por defecto; su autor recomienda coincidencia exacta de URL [E][Toth-clickjacking]. Bitwarden recomienda coincidencia por host y ofrece la exacta [E][Bitwarden].
- Por eso no se usa la lista de sufijos públicos para emparejar: un subdominio con XSS no recibe la contraseña del dominio principal.

### 10.4 Riesgos residuales

El JavaScript de la página puede leer el valor del campo (así funciona el DOM): un origen legítimo comprometido o con XSS obtiene la contraseña. Un secreto registrado con una URL equivocada se rellenará en ese origen. Las cookies de sesión posteriores al inicio de sesión valen tanto como la contraseña. Nada de esto lo resuelve el navegador; se documenta en la sección 12.8.

### 10.5 Pruebas de verificación

| Prueba | Resultado esperado | Evidencia |
|---|---|---|
| Relleno correcto en un formulario local con origen igual al del secreto | `filled: true`; la página recibe el valor | Valor leído por la página de prueba |
| Mismo formulario dentro de un iframe de otro origen; en `http`; con `userinfo` en la URL; en un subdominio; tras navegar entre la comprobación y el relleno | Rechazo en cada caso, sin resolver el secreto en los cuatro primeros y sin escribirlo en el último | Respuestas, contador de resoluciones del baúl simulado |
| Buscar el valor del secreto en `webd.db`, registros de webd y del arnés, eventos, telemetría, blobs de instantáneas y capturas de texto | Ausente | Búsqueda exhaustiva |
| Instantánea posterior al relleno | Campo con `"[redacted]"` | Instantánea |
| Referencia no declarada en la sesión | `denied(secret_not_bound)` | Respuesta |
| Reintento tras una caída entre la marca y el relleno | El `SecretExecutor` falla de forma cerrada y pide reconciliación; no se rellena dos veces | Estado, auditoría |

---

## 11. Descargas y subidas

### 11.1 Descargas

1. Cada contexto arranca con `Browser.setDownloadBehavior{behavior: "deny", browserContextId}` [E][CDP]. Una descarga no pedida se rechaza y se informa en el resultado de la acción como `download_blocked{suggested_filename, origin}`.
2. `browser_download{tab_id, trigger}` arma la pestaña con `allowAndName` en `quarantine/<session>/`, `eventsEnabled: true` y un plazo de 120 s. El disparador es `{snapshot_id, ref}` (clic) o `{url}` (navegación a un origen de la sesión o aprobado).
3. `Browser.downloadWillBegin` da el nombre sugerido (no confiable: nunca se usa como ruta) y `downloadProgress` el tamaño; por encima de 200 MiB se cancela y se borra el parcial.
4. Al completar, webd calcula SHA-256, tipo por bytes mágicos y tamaño, y transmite el archivo al adaptador, que lo entrega al **control de archivos del arnés** (arnés 20.7: detección por contenido, límites, escáner si existe, cuarentena) y lo guarda como blob. El archivo se borra de la cuarentena de webd.
5. La política `DownloadRestrictions` de Chrome depende de Safe Browsing [E][Policies] y su funcionamiento en headless está **NO VERIFICADO**; la defensa real es el paso 4.

### 11.2 Subidas

1. El agente indica la fuente: un blob del arnés (adjunto) o una ruta del espacio de trabajo.
2. El adaptador aplica la autoridad de archivos del arnés (permiso de lectura, rutas protegidas, ruta canónica, sin enlaces que salgan de la raíz) y transmite los bytes a webd con `act.stage_upload`.
3. webd escribe `staging/<session>/<uuid>/<nombre saneado>` (`0600`) y devuelve un identificador de preparación.
4. `browser_act{action: "upload", ref, files: [staged_id]}` usa `DOM.setFileInputFiles` con esas rutas.
5. Los archivos preparados se borran al cerrar la sesión o a la hora.

Chrome **nunca recibe rutas del arnés**: así se evita la clase de fallos de OpenClaw (CVE-2026-26329, recorrido de rutas en la subida) y de chrome-devtools-mcp (CVE-2026-53766, enlace simbólico que escapaba de la raíz y enviaba un archivo externo a la página) [E][OpenClaw-upload][ChromeDevToolsMCP]. Una subida es de riesgo alto: requiere aprobación salvo que el usuario pidiera expresamente subir ese archivo a ese sitio.

### 11.3 Pruebas de verificación

| Prueba | Resultado esperado | Evidencia |
|---|---|---|
| Enlace de descarga sin armar | `download_blocked`; nada en disco | Resultado, listado |
| Descarga armada de un PDF local | Blob con hash correcto; tipo detectado; cuarentena vacía después | Blob, registro del control de archivos |
| Descarga de 300 MiB | Cancelada al superar 200 MiB; parcial borrado | Eventos, listado |
| Nombre sugerido `../../etc/passwd` | El archivo queda con nombre GUID dentro de la cuarentena | Listado |
| Subida de una ruta protegida del arnés, de un enlace simbólico que sale de la raíz y de una ruta inexistente | Rechazo en el adaptador; nada llega a webd | Respuestas |
| Subida correcta | La página local recibe el contenido exacto; el preparado se borra al cerrar | Hash recibido, listado |

---

## 12. Seguridad

### 12.1 Modelo de amenazas

| Actor o vector | Ejemplo | Control principal | Lo que el control no cubre |
|---|---|---|---|
| Página web hostil (contenido) | Instrucciones ocultas, ventanas emergentes engañosas, formularios trampa | Marcado no confiable, compuerta de orígenes, compuerta de acciones, aprobación humana (12.4) | Manipulación solo textual (un resumen sesgado) [E][CaMeL] |
| Mensaje hostil en un servicio de confianza | Un WhatsApp o un correo con instrucciones | Igual que arriba: el contenido de servicios de confianza también es no confiable; todo envío es `commit` con aprobación | Fatiga de aprobaciones |
| Explotación del renderizador | Fallo de memoria en V8 o Blink | Sandbox de Chrome verificado, aislamiento por sitio, versión al día (12.2, 4.5) | Cadenas completas con escape del sandbox; *zero-days* |
| Proceso del navegador comprometido | Escape del sandbox | Usuario del SO sin privilegios, perfiles `0700`, sin acceso a datos del arnés | Lo que ese usuario puede leer y la red directa (A-03) |
| Proceso local sin privilegios | Conectarse a un puerto CDP o al proxy | Tubería sin puerto (4.3), socket Unix con `SO_PEERCRED`, proxy con comprobación de UID (12.3) | Root local |
| Modelo manipulado | El agente intenta navegar a un dominio del atacante con datos en la URL | Contaminación por ejecución, aprobación de orígenes nuevos con datos, ningún CDP crudo (12.4) | Canales encubiertos en orígenes ya permitidos [E][EchoLeak] |
| Interfaz del visor | XSS con el título de la página, *clickjacking* | `textContent`, CSP estricta, `frame-ancestors 'none'`, código de un solo uso (9.3) | *Phishing* dirigido a la persona que ve la página |
| Cadena de suministro | Crate o binario de Chrome alterado | Dependencias del workspace fijadas en `Cargo.lock`; Chrome del repositorio firmado del fabricante; CfT solo en pruebas | Compromiso del propio proveedor |

**Principio** [I]: la seguridad se apoya en fronteras del sistema operativo y en código determinista; los filtros y los modelos solo añaden fricción. Es el principio P10 del arnés aplicado al navegador.

### 12.2 Aislamiento de procesos y verificación del sandbox

- webd y Chrome corren como el usuario web, nunca como root (3.2). Chrome se niega a correr como root sin `--no-sandbox` [E][LinuxSandbox], y `--no-sandbox` está prohibido (4.4).
- **Verificación en cada arranque de instancia y cada vez que aparece un proceso de renderizado**: para cada PID con `--type=renderer`, webd lee `/proc/<pid>/status` y exige `Seccomp: 2` y `NoNewPrivs: 1`, y comprueba que `/proc/<pid>/ns/user` y `/proc/<pid>/ns/net` difieren de los del proceso del navegador. Si un renderizador no cumple, **se mata la instancia** y la sesión queda `blocked(sandbox_unavailable)`. Verificado en la VPS: todos los renderizadores de Chrome 155 cumplían esas cuatro condiciones; el navegador, el GPU y el servicio de red corren sin seccomp en el espacio de nombres del host, como es normal en Chrome [M].
- **AppArmor en Ubuntu 23.10+**: la restricción de espacios de nombres sin privilegios rompe el sandbox de binarios sin perfil. Chromium documenta cuatro salidas: desactivar la restricción globalmente (desaconsejado), un perfil AppArmor limitado a la ruta con `userns`, el sandbox setuid («puede ser algo más débil») o `--no-sandbox` («nunca» para la web abierta) [E][LinuxSandbox]. El paquete de Google Chrome trae su perfil (`/etc/apparmor.d/chrome`) [M]. Para Chromium en otra ruta o CfT, el instalador añade un perfil de ruta con el binario en un directorio de root de solo lectura (quien pueda crear un ejecutable en esa ruta saltaría la restricción). Excalibur no desactiva la restricción global.
- **Por qué no dentro de la sandbox de bubblewrap del arnés**: el seccomp del arnés prohíbe crear espacios de nombres, y `bwrap --disable-userns` deja a Chrome sin su sandbox y obliga a `--no-sandbox` [E][Flatpak-zypak]. Flatpak resuelve esto con zypak, que delega en su propio sandbox y pierde la capa de espacios de nombres por proceso de Chrome [E][Flatpak-zypak]. La combinación correcta (usuario dedicado dentro de un espacio de nombres de red propio, sin prohibir `CLONE_NEWUSER`) es la ampliación A-03.
- Perfiles, cuarentena y preparación: directorios `0700` del usuario web. El arnés no los monta en ninguna sandbox de herramientas.

### 12.3 Red

```mermaid
flowchart LR
  C[Chrome<br/>usuario web] -- "proxy loopback 127.0.0.1:P<br/>sin excepción para loopback" --> L[Oyente loopback de webd<br/>comprueba UID del par]
  R[Lector y rastreador<br/>en webd] --> L
  L --> B[HostProxy de excalibur-sandbox<br/>socket Unix privado]
  B -- "DNS fijado, sin privadas ni loopback,<br/>puertos 80/443" --> I((Internet))
```

- **Reutilización**: `HostProxy::start(ProxyConfig)` del arnés implementa HTTP con destino absoluto, `CONNECT` y SOCKS5, resuelve DNS tras admitir la política, rechaza respuestas DNS con cualquier dirección privada o especial, conecta a una dirección fijada sin volver a resolver y comprueba cada redirección [E][arnés: security]. webd lo arranca con `NetworkPolicy::Full`, puertos 80 y 443 y `permitted_private` vacío. Un perfil puede restringirse a una lista de dominios.
- **Oyente loopback**: Chrome solo habla TCP con su proxy, mientras el broker escucha en un socket Unix. webd abre `127.0.0.1:<puerto aleatorio>` y reenvía al socket del broker. Para que otro proceso local no lo use como proxy abierto, cada conexión aceptada se busca en `/proc/net/tcp` y `/proc/net/tcp6` (inodo → UID); si el UID no es el del usuario web, se cierra.
- **Flags de Chrome** (4.4): `<-loopback>` porque Chrome omite el proxy por defecto para `localhost`, `127/8`, `[::1]`, `169.254/16` y `fe80::/10`, lo que incluye los metadatos de la nube en `169.254.169.254` [E][ProxyDocs]; `--host-resolver-rules` contra resoluciones locales del *prefetch*; WebRTC sin UDP fuera del proxy; QUIC desactivado.
- **Local Network Access** (Chrome 142) es un permiso que CDP puede conceder; no es un control de salida para un agente que navega [E][LNA].
- **Límite**: sin espacio de nombres de red propio (A-03), un proceso del navegador comprometido puede abrir sockets directos. El proxy controla al navegador sano, no al comprometido.

### 12.4 Contenido no confiable, inyección de instrucciones y exfiltración

**Evidencia.**

| Trabajo | Resultado medido |
|---|---|
| InjecAgent [E][InjecAgent] | ReAct con GPT-4: 24 % de éxito de ataque; 47 % con *hacking prompt* |
| AgentDojo [E][AgentDojo] | GPT-4o 47,69 % de ataques dirigidos con éxito (tabla 3); delimitar datos solo lo baja de 57,7 % a 41,65 % (tabla 5) |
| WASP [E][WASP] | Agentes desviados de la tarea entre 16,7 % y 85,7 %; objetivo del atacante logrado entre 0 % y 16,7 % («seguridad por incompetencia») |
| EIA [E][EIA] | Hasta 70 % de robo de datos personales concretos por paso |
| Ventanas emergentes [E][Popups] | 86 % de éxito medio; pedir al agente que las ignore no ayudó |
| VPI-Bench [E][VPI] | Hasta 100 % en agentes de navegador |
| Inyecciones reales [E][InTheWild] | 15 300 inyecciones validadas en 11 700 páginas; ≈70 % en HTML no renderizado; obediencia baja (hasta 8 % en modelos pequeños) |
| Ataques adaptativos [E][Adaptive] | 12 defensas publicadas cayeron a más del 90 % de éxito |
| CaMeL [E][CaMeL] | 77 % de tareas con seguridad demostrable frente a 84 % sin defensa; ≈2,8× tokens; no cubre manipulación textual ni *phishing* |
| Anthropic (datos del proveedor) [E][Anthropic-PI] | Opus 4.5: de 8,0 % a 1,4 % con salvaguardas frente a un atacante adaptativo; «ningún agente de navegador es inmune» |

Conclusión [I]: ninguna defensa del modelo es una frontera. Lo que reduce el **impacto** es arquitectura: limitar qué orígenes puede leer y escribir cada tarea, compuertas deterministas en acciones con consecuencias, secretos fuera del modelo y romper la tríada letal [E][Trifecta][RuleOfTwo][ChromeAgentSec]. Excalibur reúne por defecto los tres ingredientes (cuentas con datos privados, web abierta y capacidad de enviar mensajes), así que los controles son estructurales:

| Control | Qué hace | Implementación |
|---|---|---|
| C1. Niveles de confianza | Cuentas en la instancia persistente; web abierta en contextos efímeros sin cuentas (D4) | 5.2 |
| C2. Conjuntos de orígenes por sesión | Orígenes `rw` y `ro` de documentos de nivel superior. En persistentes, fijos por configuración: el agente no los amplía. En efímeras, cada origen nuevo navegado se añade tras la autorización de dominio del arnés; si la ejecución está contaminada (`tainted_secret` o lectura de una sesión persistente), un origen nuevo pide aprobación | `Fetch.enable{patterns: [{resourceType: Document}]}` → `Fetch.continueRequest` o `Fetch.failRequest(BlockedByClient)`; es el modelo de *Agent Origin Sets* del agente de Chrome [E][ChromeAgentSec] |
| C3. Regla de dos | Si la ejecución leyó contenido no confiable, todo `commit`, `credential` o `upload` en una sesión persistente pide aprobación de la **acción exacta**, cualquiera que sea la autonomía. Leer WhatsApp ya es contenido no confiable, así que en la práctica todo envío se confirma salvo concesión exacta previa («envía este texto a este contacto») | Adaptador + aprobaciones exactas del arnés |
| C4. Exfiltración por navegación | Con la ejecución contaminada: navegación o envío de formulario hacia un origen nuevo con *query* o fragmento de más de 128 caracteres o alta entropía, o `POST` entre orígenes, pide aprobación. Las URL escritas por el modelo no se cargan solas en la interfaz | Compuerta de documentos (C2) + método y cuerpo de `Fetch.requestPaused` |
| C5. Sin CDP crudo | El modelo nunca envía órdenes CDP. No hay herramienta de `Runtime.evaluate`, cookies, permisos ni comportamiento de descargas; `browser_devtools` es de solo lectura | Catálogo cerrado (16.2) |
| C6. Marcado | Todo texto de la página llega al contexto como `ContextInput::untrusted` con su URL; la instantánea lo declara (`untrusted`) | Adaptador |
| C7. Observación prudente | La instantánea no incluye HTML no renderizado (comentarios, metadatos), donde vive ≈70 % de las inyecciones reales [E][InTheWild]; el alcance por vista reduce inyecciones de *banners* [E][FocusAgent] | 6.2 |
| C8. Mensajería personal | Envíos por WhatsApp o Teams: siempre `commit` con texto y destinatario visibles en la aprobación; como máximo 20 por hora por defecto | Configuración del sitio; coherente con los términos de WhatsApp [E][WA-ToS] |
| C9. Auditoría | Cada operación con origen, clase, decisión y resultado redactado | Auditoría encadenada del arnés |

### 12.5 XSS

- **Visor**: 9.3.
- **Texto de las páginas en la interfaz de Excalibur**: títulos, URL, textos de instantáneas y Markdown de `web_read` son datos. Contrato para cualquier frontend: se pintan como texto; el Markdown se muestra sin HTML crudo y sin cargar imágenes de URL no confiables (las URL de imagen son un canal de exfiltración conocido [E][EchoLeak]).
- **Scripts propios en las páginas**: se ejecutan en **mundos aislados** (`Page.createIsolatedWorld`, `addScriptToEvaluateOnNewDocument{worldName}`), así que el JavaScript de la página no ve sus funciones ni sus `bindings`; los mensajes de los `bindings` se validan por forma y tamaño. La página aún puede alterar el DOM para engañar: aceptado.
- **XSS en los sitios visitados**: no se puede corregir desde fuera; se limita su efecto con el aislamiento por sitio y el origen exacto de las credenciales.

### 12.6 Permisos y esquemas

- Al crear cada contexto: `Browser.setPermission{setting: denied}` para geolocalización, notificaciones, cámara, micrófono, MIDI, portapapeles, sensores, detección de inactividad, acceso a red local, captura de pantalla, gestión de ventanas y almacenamiento compartido (`Browser.setPermission` es experimental y `grantPermissions` está obsoleto [E][CDP]). La configuración de cada sitio puede conceder permisos concretos; ninguno es necesario para leer o escribir texto en WhatsApp o Teams.
- **Esquemas**: el adaptador y webd rechazan navegar a `file:`, `chrome:`, `chrome-extension:`, `devtools:`, `javascript:` y `data:` de nivel superior.
- **Gestor de contraseñas y autocompletado de Chrome desactivados** en las preferencias del perfil antes del lanzamiento, para que Chrome no guarde contraseñas en un almacén `basic`. Que esas preferencias basten en la versión fijada está **NO VERIFICADO** (B5).
- **Políticas de empresa** opcionales en la instalación (`/etc/opt/chrome/policies/managed/`, `/etc/chromium/policies/managed/` o `/etc/opt/chrome_for_testing/policies/managed/` [E][Policies]): `URLBlocklist` con `file://*` y `javascript://*`, `PasswordManagerEnabled=false`, autocompletado desactivado. Que se apliquen a navegaciones iniciadas por CDP está **NO VERIFICADO**; por eso los controles anteriores no dependen de ellas.

### 12.7 Mantenimiento de seguridad

- Política de versión de 4.5.
- Comprobación de avisos de seguridad de las dependencias (GHSA/OSV) como tarea del repositorio cuando exista integración continua.
- Las lecciones de las vulnerabilidades revisadas, que cumplen la tarea pendiente de las anotaciones, se aplican así:

| Vulnerabilidad | Lección aplicada |
|---|---|
| CVE-2025-47241 (browser-use): lista de dominios saltada con `userinfo` [E][browser-use] | Comparar el origen comprometido del marco, nunca cadenas (10.2, C2) |
| CVE-2025-9611 (Playwright MCP): sin validación de `Origin`, *DNS rebinding* desde cualquier página [E][PlaywrightMCP] | Sin servidor HTTP de control; el visor comprueba `Origin` y cookie (9.3) |
| CVE-2026-53766 (chrome-devtools-mcp): enlace simbólico en la subida [E][ChromeDevToolsMCP] | Preparación de subidas con copia (11.2) |
| CVE-2026-26329 (OpenClaw): recorrido de rutas en la subida [E][OpenClaw-upload] | Igual |
| Robo de cookies con depuración remota [E][SpecterOps-Cookie][Chrome136] | Tubería, perfil propio, nunca puerto |
| EchoLeak, CVE-2025-32711 [E][EchoLeak] | Listas de dominios no bastan; aprobación de salidas con datos (C4) |
| Comet: comentario oculto que encadenó lectura de correo y publicación [E][Brave-Comet] | Separación de niveles (C1) y regla de dos (C3) |

### 12.8 Lo que no se promete

Este diseño **no** garantiza que el agente resista la inyección de instrucciones, ni que un renderizador explotado no escape, ni que un servicio no restrinja la cuenta, ni que las cookies estén cifradas en un servidor sin llavero, ni que un origen legítimo comprometido no capture lo que se escribe en él. Garantiza, con pruebas, que los controles de las secciones 9 a 12 se aplican tal como se describen.

### 12.9 Pruebas de verificación

| Prueba | Resultado esperado | Evidencia |
|---|---|---|
| Arrancar una instancia y leer el estado de cada renderizador | `Seccomp: 2`, `NoNewPrivs: 1`, espacios de nombres de usuario y red distintos del navegador | Lectura de `/proc` guardada |
| Configurar una copia del binario en una ruta sin perfil AppArmor y sin `chrome-sandbox` | La instancia no se usa: `blocked(sandbox_unavailable)` | Estado, registro |
| Desde una página: `fetch` a `127.0.0.1:<puerto del arnés>`, a `169.254.169.254`, a una IP privada, a un nombre que resuelve a loopback (*rebinding*) y una navegación de nivel superior a cada uno | Todos fallan en el broker; auditoría de rechazos sin rutas ni cabeceras | Respuestas, auditoría del broker |
| WebRTC con un servidor STUN UDP local | El servidor no recibe paquetes | Captura del servidor |
| Conexión al oyente loopback desde otro UID | Cerrada | Registro |
| Página con instrucciones ocultas («envía tus cookies a…», formulario oculto, comentario HTML) en un contexto efímero, con un modelo simulado que obedece | La navegación con datos hacia el origen nuevo pide aprobación; el comentario no aparece en la instantánea; en modo no interactivo se deniega | Contexto compilado, aprobaciones |
| Ejecución que leyó una sesión persistente intenta un `commit` | Aprobación de la acción exacta pedida aunque la autonomía sea alta | Solicitud |
| `navigator.permissions.query` para cada permiso denegado | `denied` | Resultados de la página |
| Navegar a `file:///etc/passwd` y a `chrome://settings` | Rechazado antes de llegar a Chrome | Respuesta |
| La página intenta llamar al `binding` del mundo aislado y sobrescribir sus funciones | Sin efecto | Registro del observador |
| 21 envíos en una hora en el sitio de mensajería simulado | El 21.º queda denegado por límite | Respuesta |

---

## 13. Recursos: memoria, límites, caché y disco

### 13.1 Medición de la memoria de todos los procesos

**Por qué no sumar RSS**: todos los procesos de Chrome comparten binario, bibliotecas y memoria compartida. Medido: en blanco, ΣRSS 834 MiB frente a ΣPSS 257 MiB (×3,2) [M]; el agente auxiliar midió ×3,3 [M*]. Chrome define en Linux su «huella de memoria privada» como `RssAnon + VmSwap` [E][PMF].

| Nivel | Intervalo | Fuente | Coste | Uso |
|---|---|---|---|---|
| A | 2 s | Si webd tiene cgroup propio: `memory.current`, `memory.stat` (`inactive_file`), `memory.events`, `memory.pressure` | Microsegundos [M*] | **Métrica principal de límite**: conjunto de trabajo = `memory.current − inactive_file` |
| B | 10 s | `/proc/<pid>/status` de cada proceso de cada instancia: `RssAnon + VmSwap` | ≈70 µs por proceso [M*] | Métrica principal sin cgroup; reparto por instancia |
| C | 60 s, al acercarse al presupuesto y en `browser_status{detail: true}` | `/proc/<pid>/smaps_rollup` (PSS) | ≈8,2 ms por proceso de Chrome [M*] (`smaps_rollup` existe desde el kernel 4.14 [E][Proc]) | Cifra exacta con memoria compartida repartida, para informes |
| D | Eventos | Aumentos de `memory.events` (`high`, `oom_kill`) o PSI `some avg10` > 10 % | — | Muestreo inmediato y desalojo |

- **Enumeración de procesos**: `cgroup.procs` si hay cgroup; si no, descendientes del PID del navegador de cada instancia en `/proc`, en caché por PID y hora de inicio. El tipo sale de `--type=` en la línea de órdenes (Chrome reescribe su `cmdline` como una sola cadena separada por espacios [M]). Se incluyen `zygote`, `gpu-process`, `utility` (red y almacenamiento), `renderer`, `crashpad_handler` y el propio webd: **todos los procesos**.
- **cgroup propio**: si webd se lanza en su propio *scope* o servicio de systemd (`systemd-run --scope -p MemoryMax=…` funcionó en la VPS [M*]), lo detecta en `/proc/self/cgroup` y lo usa solo si `cgroup.procs` contiene únicamente su árbol. Si no, usa el nivel B. Un cgroup por instancia es la ampliación A-04.
- **Atribución**: por instancia es exacta. Por contexto efímero también, porque los contextos nunca comparten renderizador [E][ProcModel]. Por pestaña: señales baratas (`Performance.getMetrics`: `JSHeapUsedSize`, nodos) y, bajo demanda, el mapeo renderizador → pestaña con una traza breve (`Tracing.start` con `disabled-by-default-devtools.timeline` en la sesión de la página: `TracingStartedInBrowser` lleva el `processId` de cada marco; 110–150 ms [M*]). `Target.TargetInfo` no tiene PID y `SystemInfo.getProcessInfo` no tiene memoria [E][CDP][M*].
- **Telemetría del arnés**: el adaptador añade a las muestras de recursos de la ejecución (`ResourceSample`) los campos `browser_working_set_bytes`, `browser_pss_bytes` (cuando se midió), `browser_instances`, `browser_tabs` y sus picos. Así el apéndice F del arnés puede informar la RAM del navegador por separado de la del arnés y el contenedor.

### 13.2 Presupuestos por defecto (VPS de 12 GiB sin swap)

| Concepto | Valor inicial | Motivo |
|---|---|---|
| Presupuesto blando global del navegador (todos los procesos de webd y Chrome) | 1,5 GiB de conjunto de trabajo | ≈13 % de la RAM; deja sitio al arnés, compilaciones y otras tareas |
| Límite duro global | 2,0 GiB (con cgroup: `memory.max` = 2,5 GiB) | Respaldo del kernel |
| Instancia persistente | Aviso por pestaña en escucha a 700 MiB; recarga programada si mantiene > 1 GiB durante 10 min | Medido sin sesión: 466 MiB con WhatsApp [M]; con sesión real, a medir en B6 |
| Instancia efímera | ≤ 3 contextos y ≤ 3 pestañas activas; aviso por pestaña a 500 MiB, cierre a 800 MiB; parada tras 10 min sin contextos | Coste fijo ≈250 MiB [M] |
| Pestañas | 4 por ejecución, 12 globales | 5.6 |
| Admisión | Estimación por origen (media móvil de su huella; 200 MiB por defecto, 400 MiB para aplicaciones pesadas conocidas) + 200 MiB de margen ≤ presupuesto blando, y PSI `some avg10` < 20 % si se mide | Inspirado en el despachador adaptativo de crawl4ai (umbral del 90 %) [E][Crawl4ai] |
| webd | ≤ 60 MiB RSS sin índice abierto | — |

### 13.3 Admisión y desalojo (nunca congela las sesiones en escucha)

Clases: **LIVE** (pestañas en escucha), **ACTIVE** (pestaña con una operación del agente en curso o en los últimos 60 s), **IDLE** (pestañas o contextos sin uso) y lectura **HTTP**.

Orden de desalojo cuando se supera el presupuesto blando; se para en cuanto se vuelve por debajo:

1. Cachés de webd: instantáneas en RAM (LRU), caché de lecturas.
2. Contextos y pestañas IDLE, del menos reciente al más reciente (TTL 5 min), incluidas las pestañas de la instancia persistente que **no** están en escucha (por ejemplo, una pestaña de Gmail que usó el agente).
3. `HeapProfiler.collectGarbage` en las pestañas ACTIVE efímeras y `Memory.simulatePressureNotification(moderate)` **solo en la instancia efímera**. Esa notificación llega a todos los procesos de la instancia [E][CDP], así que nunca se envía a la persistente, donde viven las pestañas LIVE.
4. `Network.clearBrowserCache` en la instancia efímera.
5. Cierre de una pestaña ACTIVE efímera que supere su límite duro, con `tab_evicted` hacia el agente.
6. Reinicio de la instancia efímera.
7. **LIVE**: nunca congelar, descartar ni enviar presión. Si una pestaña LIVE supera 1 GiB durante 10 min, se programa una **recarga** en un momento tranquilo (sin actividad en 2 min) y se avisa; la recarga conserva la sesión porque IndexedDB y cookies siguen en el perfil.

Si tras los pasos 1–6 no hay sitio, la nueva operación espera hasta 120 s y después recibe `capacity_exceeded` con el desglose de memoria.

**Lo que no se hace**:

- `Memory.prepareForLeakDetection`, que termina los *workers* [E][CDP].
- Congelar como medida de memoria: congelar no libera memoria y detiene la recepción de eventos [M*][Lifecycle].
- Un `memory.high` estricto sin swap, que estrangula la página y equivale a congelarla [E][cgroupv2].

**Respaldo del kernel**: con cgroup, `memory.oom.group = 0`, para que un OOM mate un renderizador y no el navegador entero; Chrome ya da `oom_score_adj` 300 a los renderizadores, 200 a GPU, red y almacenamiento y 0 al navegador [M*]. webd sube a 1000 el de los renderizadores efímeros para que un OOM global los elija antes que a los LIVE; que un proceso sin privilegios pueda **subir** el valor de otro proceso de su mismo usuario se verifica en B6.

### 13.4 Visibilidad y ciclo de vida

- Pestañas LIVE: 5.7.
- Pestañas efímeras: ventana propia (evita el cuelgue de capturas en pestañas ocultas) con la limitación de temporizadores en segundo plano normal de Chrome, que ahorra CPU.
- `browser_suspend_session` cierra las pestañas de una sesión **que no está en escucha** y guarda sus URL para reabrirlas. En sesiones en escucha devuelve `listener_cannot_suspend`, salvo aprobación expresa del usuario.

### 13.5 Caché y disco

- Caché HTTP de Chrome: 32 MiB en la efímera y 64 MiB en la persistente (`--disk-cache-size`, en bytes; 0 significa «por defecto» [E][Caches]).
- **Tamaño del perfil**: cada 10 minutos, recorrido acotado del directorio y `Storage.getUsageAndQuota` por origen en escucha [E][CDP]; aviso a 1 GiB por perfil. El crecimiento real de WhatsApp y Teams en días no tiene datos fiables publicados y se mide (R-07).
- **Con poco disco** (libre por debajo de 7 GiB, es decir, a 2 GiB de la reserva, o presupuesto de webd excedido): se vacían las cachés efímeras y, en la persistente, solo `cache_storage` y `shader_cache` de orígenes que no están en escucha. Su efecto está sin probar. Nunca se toca el almacenamiento de orígenes en escucha.
- **Datos de webd**: instantáneas en blobs durante 24 h o hasta 500 MiB (LRU); caché de lecturas 200 MiB; cuarentena y preparación vacías al cerrar sesiones.
- **Comprobación previa** (B0) y vigilancia continua: ninguna operación que haga crecer los datos (crear un perfil, indexar, descargar, preparar una subida) se admite si el disco libre quedaría por debajo de la **reserva de 5 GiB**; el rastreador y el indexador tampoco arrancan si el libre no cubre su presupuesto más la reserva (15.5).

**Presupuesto explícito de disco.** Medido el 2026-10-29 tras la limpieza del usuario: ≈28 GiB libres. Aun así, los límites son explícitos y no dependen del espacio disponible:

| Concepto | Límite por defecto | Al alcanzarlo |
|---|---|---|
| Reserva libre del sistema | 5 GiB siempre libres | webd deja de crecer (sin perfiles nuevos, sin indexar, sin descargas) y avisa |
| Datos de webd sin el índice | 2,5 GiB en total | Desalojo de cachés y blobs por antigüedad; aviso |
| — perfil persistente | Aviso a 1 GiB; a 1,5 GiB vaciado de las cachés permitidas (punto anterior) | Nunca se borran datos de sesión; si sigue creciendo, aviso con el desglose por origen |
| — instantáneas en blobs / caché de lecturas | 500 MiB / 200 MiB | LRU |
| — cuarentena y preparación | 300 MiB | Rechazo de nuevas descargas o subidas hasta liberar |
| Buscador, perfil `minimo` (por defecto) | Índice 1,5 GiB; `search.db` y estado 0,5 GiB | 15.5 |
| Buscador, perfil `ampliado` (opcional) | Índice 6 GiB; estado 1 GiB | Solo se activa si el libre, tras compilar, cubre el presupuesto más la reserva |
| `target` de `excalibur-browser` | Fuera de webd; se mide en B0 y al cerrar cada hito | Si el libre baja de 10 GiB, el implementador lo informa y limpia artefactos obsoletos de **su propio** `target` antes de seguir; nunca toca el checkout principal |

Con los valores por defecto, webd y el buscador ocupan como máximo ≈4,5 GiB; con `ampliado`, ≈9,5 GiB. Los límites de RAM no cambian con el perfil de disco (13.2, 15.5).

### 13.6 Pruebas de verificación

| Prueba | Resultado esperado | Evidencia |
|---|---|---|
| Comparar el total del nivel B y el PSS del nivel C con una lectura independiente de `/proc` del mismo instante | Ninguna diferencia de procesos; los totales coinciden con la lectura independiente | Ambas lecturas |
| Instancia con todos los tipos de proceso | Todos los PID enumerados, incluidos `zygote`, `gpu-process`, `utility` y `crashpad_handler` | Lista clasificada |
| Con cgroup propio (*scope* de systemd en la VPS) | `memory.current` leído; `inactive_file` restado; eventos detectados al forzar `memory.high` en una prueba aislada | Muestras |
| Llenar hasta el presupuesto con pestañas efímeras pesadas (página local que reserva memoria) con una pestaña LIVE activa | Desalojo en el orden de 13.3; la pestaña LIVE sigue recibiendo eventos y su contador de temporizador no se detiene | Eventos, contador, orden registrado |
| Nueva operación sin sitio tras el desalojo | Espera y `capacity_exceeded` con desglose | Respuesta |
| `browser_suspend_session` sobre una sesión en escucha | `listener_cannot_suspend` | Respuesta |
| Pestaña LIVE simulada que retiene más de 1 GiB durante 10 min (reloj simulado) | Recarga programada en un momento sin actividad; la sesión sigue `logged_in` | Eventos |
| Disco libre por debajo del umbral (sistema de archivos de prueba pequeño) | Creación de perfil y arranque del indexador rechazados con mensaje claro | Respuestas |
| Coste del muestreo durante 10 min con 3 instancias | CPU de webd por muestreo < 1 % de un núcleo | Medición |

---

## 14. Lectura HTTP ligera (`web_read`)

### 14.1 Contrato

El arnés define `web_read(url)` → contenido en Markdown, título, URL final, fecha de lectura y hash del contenido (arnés 14.3). Este diseño lo amplía de forma compatible:

```json
{
  "markdown": "…", "title": "…", "final_url": "https://…", "canonical_url": "https://…",
  "fetched_at": "2026-10-29T21:40:00Z", "http_status": 200, "content_hash": "sha256:…",
  "source_id": "w_3f2a9c0d1e4b5a67", "lang": "es", "render": "http|browser",
  "extractor": "dom_smoothie@0.19.0", "fragments": [{"id": "p12", "start": 5120, "end": 5733, "heading_path": ["Diseño", "Frontera"]}],
  "truncated": false, "untrusted": true
}
```

### 14.2 Algoritmo

1. **Validación**: solo `http` y `https`, sin `userinfo`, ≤ 2 048 caracteres; en el perfil `eval` se aplica `benchmark_source_blocked` (ya existente en `crates/eval`). La autorización del dominio la hace antes el arnés (`SecuredTools`).
2. **Caché**: si la URL normalizada se leyó hace menos de 24 h y no se pide `fresh`, se devuelve la lectura guardada con su fecha original.
3. **Descarga** con `reqwest` (en el workspace: 0.12.28; la 0.13 ya usa rustls por defecto [E][reqwest013]) a través del broker (12.3):
   - conexión 10 s, total 30 s; hasta 5 redirecciones, cada una validada (esquema, sin bajar de `https` a `http`); `reqwest` no tiene por defecto ni plazos ni tope de cuerpo [E][reqwest013], así que se fijan explícitamente;
   - cuerpo leído por trozos con tope de 10 MiB (PDF hasta 20 MiB);
   - tipos aceptados: `text/html`, `application/xhtml+xml`, `text/plain`, `text/markdown`, feeds, `application/pdf` (extracción con `pdf-extract`) [E][Extract-crates]; el resto, `unsupported_type`;
   - descompresión gzip y brotli; juego de caracteres por cabecera, `meta`, BOM y, en último caso, detección (`encoding_rs` + `chardetng`);
   - agente de usuario honesto `ExcaliburReader/<versión> (+<URL del proyecto o de contacto configurada>)`.
4. **¿Basta HTTP?** Se escala al navegador si se cumple alguna de estas condiciones (heurística propuesta por la investigación de recursos de esta sesión; no publicada como tal) [I]:
   - texto visible < 500 caracteres y ≥ 3 scripts;
   - relación texto/marcado < 0,02;
   - nodo de montaje vacío (`#root`, `#app`, `#__next`, `[ng-app]`) con poco texto;
   - `<noscript>` que pide JavaScript;
   - página de desafío (403/503 con marcadores de 9.1).

   Antes de escalar se prueban los datos incrustados (`__NEXT_DATA__`, `__NUXT__`, JSON-LD). La decisión se guarda por host y prefijo de ruta durante 7 días y se vuelve a comprobar en un 10 % de los casos cuando hay presupuesto, como hace el rastreador adaptativo de Crawlee [E][Crawlee-adaptive]; Jina Reader usa el mismo principio de «ligero primero» [E][JinaReader].
5. **Render en navegador** (si se escala): contexto efímero de la ejecución, espera de 7.2 con 15 s de plazo y `DOM.getOuterHTML` del documento principal; mismo extractor. Si aparece un bloqueo, `web_read` devuelve `blocked{kind}` y el agente decide si usa `browser_*` con intervención.
6. **Extracción**: `dom_smoothie` 0.19.0 (adaptación de Readability.js a Rust, MIT, salida en Markdown, título, autor y fecha) [E][dom_smoothie]. Si no encuentra un artículo (listados, documentación), se convierte el cuerpo completo con `htmd` 0.5.6 (Apache-2.0) [E][htmd] tras quitar `nav`, `footer`, `aside`, `script` y `style`, y se elige el resultado con más texto útil. Evidencia: los extractores heurísticos centrados en la precisión (Readability, Trafilatura, DOM Distiller) son los más robustos y los neuronales rinden mal [E][Bevendorff]; las páginas estructuradas siguen siendo difíciles (F1 de 0,41–0,84) [E][WCXB].
7. **Normalización**: espacios colapsados; encabezados, listas, tablas y bloques de código conservados; enlaces absolutos `[texto](url)`; imágenes solo como texto alternativo (nunca se cargan).
8. **Fragmentos**: corte por encabezados y párrafos con identificadores `p1…pn`, desplazamientos en bytes UTF-8 dentro del Markdown y ruta de encabezados. `content_hash = sha256(Markdown normalizado)`.
9. **Tamaño**: al modelo van como máximo 50 KB (presupuesto de salida por defecto del arnés) con `truncated: true`; el texto completo queda en `full_output_blob`. `web_read{url, fragments: ["p12-p20"]}` lee del blob por `content_hash` sin volver a descargar.
10. **Registro**: el adaptador crea el evento de fuente del arnés (búsqueda, URL, título, fecha, hash) y guarda el Markdown como blob para validar citas (arnés 10.9). webd encola la página en el índice (`source = read`) **solo** si el `robots.txt` del sitio permite el token del rastreador y la lectura no procede de una sesión persistente.

**Política de `robots.txt` en lecturas pedidas** [I]: una lectura de una página concreta pedida por el usuario o su agente no es rastreo (no sigue enlaces) y actúa como un navegador, por eso no consulta `robots.txt`. Sí se identifica, respeta 429/503 y `Retry-After`, y nunca alimenta el índice si `robots.txt` no lo permite. El usuario puede cambiar esta política (`reader.respect_robots = true`).

**Nunca con cuentas**: `web_read` no usa los perfiles persistentes ni sus cookies. El contenido autenticado solo se lee con `browser_*`, de forma explícita.

### 14.3 Citas

- El modelo cita por identificador, por ejemplo `[[web:w_3f2a9c0d1e4b5a67#p12]]`. El arnés lo convierte en «Título — URL, párrafo 12 (leído el 2026-10-29)», como hace con sus documentos, y **nunca inventa números de página** en formatos sin paginación (anotaciones; arnés 10.9).
- Enlace verificable: un fragmento de texto `#:~:text=` con las primeras palabras del párrafo y, si hace falta, prefijo y sufijo para que sea único. Es un borrador del WICG, compatible con Chrome 81+, Safari 16.1+ y Firefox 131+ (≈95,6 % de uso) [E][TextFragments]. La referencia autoritativa es `content_hash` + desplazamientos; el enlace es una comodidad.
- `search.cite(content_hash, quote)`: busca la cita exacta o normalizada (espacios y mayúsculas) en el Markdown guardado y devuelve `{start, end, fragment_id, text_fragment_url}`. El arnés la usa para validar que una frase citada existe en lo que realmente se leyó.

### 14.4 Pruebas de verificación

| Prueba (servidor local) | Resultado esperado | Evidencia |
|---|---|---|
| Artículo HTML con navegación, anuncios y pie | Markdown con el artículo, sin menús; título y fecha correctos; fragmentos con desplazamientos que apuntan al texto | Salida, comprobación de desplazamientos |
| Página de una sola aplicación con `#root` vacío | `render: browser`; el contenido renderizado aparece | Salida |
| Página con `__NEXT_DATA__` suficiente | `render: http` (sin navegador) | Salida, contador de renders |
| Cuerpo de 50 MiB, `Content-Type` binario, 6 redirecciones, bajada de `https` a `http`, redirección a `127.0.0.1` | Corte a 10 MiB, `unsupported_type`, `too_many_redirects`, rechazo, rechazo del broker | Respuestas |
| Juegos de caracteres ISO-8859-1 y Shift_JIS sin cabecera | Texto correcto | Salida |
| Segunda lectura en menos de 24 h | Desde la caché, con la fecha original | Marcas de tiempo |
| Documento de 300 KB de texto | 50 KB inline con `truncated`; `fragments` del final legibles desde el blob sin nueva descarga | Contador de peticiones |
| `cite` con una frase presente, con otra alterada y con espacios distintos | Exacta, ausente y normalizada encontrada | Resultados |
| Sitio que prohíbe el token del rastreador en `robots.txt` | La lectura funciona, pero la página no entra en el índice | Índice |
| Lectura con un perfil persistente abierto con cookies del mismo dominio | La petición no lleva cookies | Cabeceras recibidas por el servidor |

---

## 15. Buscador propio

### 15.1 Objetivo y lo que no se promete

Un motor de búsqueda **propio y local**, sin llamar a ninguna API de búsqueda externa (ni Google, ni Bing, ni Brave, ni metabuscadores como SearXNG, que dependen de ellas). Descargar **conjuntos de datos abiertos** (volcados de Wikipedia, listas y grafos de dominios de Common Crawl, la lista Tranco, metadatos de arXiv por OAI-PMH, la API de ítems de Hacker News) no es usar una API de búsqueda: son fuentes de contenido y de descubrimiento. La búsqueda en APIs propias de fuentes concretas sigue siendo `source_search` del arnés (14.3), separada de este motor.

**No indexa toda Internet.** Publica en cada respuesta su cobertura (documentos, fuentes, fecha del último rastreo) y señala cuándo los resultados son insuficientes, para que el agente reformule, lea sitios conocidos directamente o use `source_search`.

**Qué importa para un agente.** Con 100 195 documentos, GPT-5 acierta 55,9 % en BrowseComp-Plus con BM25 y 70,1 % con un *embedding* grande; dar al agente un lector de documento completo subió gpt-4.1 de 35,4 % a 43,6 % [E][BrowseComp-Plus]. Un BM25 bien ajustado con recuperación profunda alcanzó 83,1 % en Pi-Serini [E][Pi-Serini], y BM25 supera en ≈30 % a recuperadores basados en LLM porque los agentes emiten subconsultas de palabras clave [E][SAGE]. De ahí el diseño: **BM25 con listas profundas + un buen lector** (14), y recuperación densa como siguiente incremento medido (A-08).

### 15.2 Arquitectura

```mermaid
flowchart LR
  subgraph Fuentes
    T[Tranco 1M + rangos de dominio de Common Crawl]
    CUR[Lista curada: documentación técnica, sitios de referencia, feeds]
    WIKI[Volcado CirrusSearch de Wikipedia, en flujo]
    AX[arXiv OAI-PMH]
    HN[Hacker News API]
    READ[Páginas leídas por el agente]
  end
  T & CUR --> SEED[Sembrador]
  SEED --> FR[(Frontera SQLite<br/>hosts, urls)]
  HN & AX --> FR
  READ --> FR
  FR --> SCH[Planificador educado por host]
  SCH --> FE[Descargador compartido con web_read]
  FE --> ROB[robots.txt, meta robots, X-Robots-Tag]
  ROB --> EX[Extractor]
  EX --> DD[Duplicados: sha256 + SimHash]
  DD --> IW[Escritor tantivy]
  WIKI --> IW
  IW --> IDX[(Índice tantivy en disco)]
  IDX --> SR[Buscador: BM25 + prior + frescura + diversidad]
  SR --> API[web_search]
```

### 15.3 Cobertura inicial viable

La cobertura se define por un **presupuesto de disco explícito** (13.5), no por el espacio libre. Con ≈28 GiB libres verificados el 2026-10-29, el perfil `minimo` es el predeterminado y el `ampliado` es opcional; el `estandar` sigue fuera de v1 porque, sumado a la reserva de 5 GiB y al crecimiento de las compilaciones, no cabe con margen.

**Perfil `minimo`** (por defecto; índice ≤ 1,5 GiB, datos del buscador ≤ 2 GiB):

| Fuente | Qué se indexa | Volumen estimado | Actualización |
|---|---|---|---|
| Wikipedia en español: volcado CirrusSearch `eswiki_content` (≈8,6 GB bz2, semanal; última fecha comprobada 20261025) [E][Cirrus] | `title`, `opening_text`, `heading`, prior desde `popularity_score` e `incoming_links`. El volcado **se procesa en flujo y no se guarda**; se filtra por `incoming_links` ≥ umbral (inicial 10) y se detiene al llegar a su cuota (0,6 GiB) | Cientos de miles de artículos (a medir) | Mensual |
| Documentación técnica curada (≈40 sitios: MDN, documentación de Rust, Python, SQLite, PostgreSQL, RFC Editor, man7.org, kernel.org…) | Páginas por *sitemap*, con tope de 2 000–10 000 por sitio | ≈40 000 páginas | Semanal |
| Dominios generales: los 2 000 primeros por centralidad armónica de Common Crawl que estén en Tranco y tengan portada HTML | Portada, entradas del *sitemap* por `lastmod`, profundidad ≤ 1, ≤ 30 páginas por dominio | ≤ 60 000 páginas | Semanal (diaria si tiene feed) |
| Noticias y blogs: ≈200 feeds RSS/Atom en español e inglés | Entradas y página enlazada | Ventana móvil de 60 días | Cada 2 h |
| arXiv (categorías `cs.*`, últimos 24 meses) por OAI-PMH con `from=` [E][arXiv-OAI] | Título, resumen, autores, fecha | Decenas de miles de registros | Diaria |
| Hacker News: `topstories` y `newstories` [E][HN-API] | Título, URL y texto propio; la página enlazada se encola con prioridad baja | Rotativo | Cada hora |
| Páginas leídas por el agente (14.2, paso 10) | Igual que el rastreo | Crece con el uso | Como su host |

Estimación [I]: 0,5–1 millón de documentos a 1–2,5 KB de índice por documento, porque no se guarda el texto completo sino título, encabezados, cuerpo sin posiciones y 1 KB de fragmento. Referencia: Lucene ocupa ≈620 B/doc con solo frecuencias de términos y ≈5 KB/doc guardando el texto [E][Anserini-sizes]. B8 mide los bytes por documento reales y ajusta los umbrales.

**Perfil `ampliado`** (opcional, mismo código y mismas horas; índice ≤ 6 GiB, estado ≤ 1 GiB): Wikipedia en español con umbral `incoming_links` ≥ 2 (cuota 2,5 GiB); Wikipedia en inglés procesada en flujo desde `enwiki_content` (≈41,9 GB bz2 de descarga, no se guarda) con umbral `incoming_links` ≥ 100 (cuota 2 GiB); 10 000 dominios × 30 páginas (≤ 300 000 páginas, cuota 1 GiB); documentación curada con topes mayores (0,5 GiB). Solo cambian los umbrales de configuración. El tiempo de ingesta (descarga y descompresión de ≈50 GB, más ≈17 h de rastreo a 5 páginas/s) está **NO VERIFICADO**. No aumenta los límites de RAM: el escritor sigue en 64 MiB y la búsqueda usa `mmap` y la caché de páginas del sistema, que el kernel libera bajo presión; solo crecen los campos rápidos (≈8 B por documento y campo).

**Perfil `estandar`** (ampliaciones A-09 a A-12; ≈25–30 GB): Wikipedia en inglés y español completas en su parte inicial, 10 000–20 000 dominios con 50–200 páginas cada uno (≈3 millones de páginas), una semana de rastreo educado.

**Por qué estas fuentes**:

- La centralidad armónica es la única medida que cumple todos los axiomas de centralidad de Boldi y Vigna [E][Harmonic], y Common Crawl publica rangos de host y dominio ya calculados (grafo de mayo–julio de 2026: 118 millones de dominios; `domain-ranks.txt.gz` de 2,35 GB, ordenado, del que basta leer el principio en flujo) [E][CC-webgraph].
- Tranco: 1 millón de dominios, actualizado a diario, 9,7 MB [E][Tranco].
- El volcado CirrusSearch da texto plano y una señal de popularidad sin analizar wikitexto [E][Cirrus].
- El índice de URL de Common Crawl (CDX) tiene límites de uso estrictos (un 504 en la prueba) [E][CC-index]; no se usa en v1. El Open Web Index exige autenticación y una licencia no comercial revocable [E][OWI]; queda fuera.

**Tiempos estimados** [I]: con 5 páginas/s globales (prudente; el techo realista de un rastreador educado de una máquina es 15–30 páginas/s con 64–128 hosts en paralelo y 1–2,5 s por host [E][Marginalia]), las ≈100 000 páginas web tardan ≈6 h. La ingesta en flujo de Wikipedia en español está limitada por la descarga y la descompresión bz2; su duración está **NO VERIFICADA** (estimación: 30–90 min).

### 15.4 Descubrimiento y rastreo

**Frontera** (`search.db`):

- `hosts(host PK, domain, prior, next_fetch_at, delay_ms, robots_blob, robots_fetched_at, robots_status, err_count, parked_until, page_budget, pages_fetched)`;
- `urls(url_hash PK, url, host, depth, priority, state, source, last_fetch, next_fetch, etag, last_modified, n_fetch, n_change, simhash, content_hash, opstamp)`.

`state` toma los valores `queued`, `fetching`, `pending_commit`, `indexed`, `skipped`, `failed` y `gone`.

**Planificador**: elige hosts vencidos por `next_fetch_at`; una petición en vuelo por host; semáforo global de 16 (perfil `minimo`) y tope global de páginas por segundo. Prioridad = prior del host + recencia de `lastmod` − penalización por profundidad.

**Cortesía**:

- retraso por host = `clamp(2 × duración de la última descarga, 1 s, 5 s)` + 0–150 ms aleatorios, como Marginalia [E][Marginalia]; Stract era más conservador (≥ 10 s) [E][Stract];
- `Crawl-delay` respetado hasta 30 s (no está en la RFC, que solo dice que se pueden interpretar otros registros [E][RFC9309]);
- 429/503: `Retry-After` (máximo 1 h) o, si no viene, duplicar el retraso hasta 120 s; tras 5 errores seguidos, el host queda aparcado 24 h.

**`robots.txt` según RFC 9309** [E][RFC9309]: se descarga antes de cualquier URL del host; caché de 24 h; se analizan hasta 500 KiB; hasta 5 redirecciones; 4xx = todo permitido; 5xx o inalcanzable = todo prohibido y nuevo intento en 1 h; gana la regla más larga y, en empate, `Allow`; token `ExcaliburBot` con alternativa `*`; `/robots.txt` siempre permitido. Biblioteca candidata: `texting_robots` 0.2.2 (MIT/Apache-2.0, de 2023, con tope de 500 KiB) [E][texting_robots]; si su mantenimiento no convence en B8, un analizador propio de ≈150 líneas con las pruebas de la RFC. También se respetan `<meta name=robots>` y `X-Robots-Tag` (`noindex`, `nofollow`).

**Identificación**: `ExcaliburBot/0.1 (+<contact_url>)`. La **URL de contacto la aporta el usuario**; sin ella el rastreador no arranca (las reglas del repositorio prohíben inventar datos de contacto). CCBot sigue la misma práctica de agente identificable [E][CC-terms].

**Normalización de URL**: el crate `url` implementa el estándar WHATWG, no RFC 3986 [E][RFC3986]; encima se pasa el host a minúsculas, se quitan el fragmento, el puerto por defecto y los parámetros de rastreo (`utm_*`, `gclid`, `fbclid`, `mc_cid`, `mc_eid`). No se reordena la *query*: RFC 3986 solo admite normalizaciones agresivas cuando la equivalencia está indicada [E][RFC3986]. Se usa `<link rel=canonical>` si está en el mismo dominio registrable.

**Enlaces**: los del mismo host se encolan dentro de la profundidad y el presupuesto del host; los de otros hosts solo aportan hosts candidatos, que no se rastrean salvo que estén en las semillas. Así la frontera no crece sin control.

**Sitemaps**: líneas `Sitemap:` de `robots.txt` y `/sitemap.xml`; hasta 50 000 URL y 50 MB sin comprimir por archivo; índices de sitemaps; restricción a su host y ruta [E][Sitemaps]. **Feeds**: `feed-rs` 3.0.0 (MIT) para Atom, RSS y JSON Feed [E][feed-rs].

**Duplicados**: `sha256` del Markdown normalizado para duplicados exactos; SimHash de 64 bits sobre tejas de 3 palabras con distancia de Hamming ≤ 3 dentro del mismo host (el valor que funcionó para 8 000 millones de páginas [E][SimHash]).

**Legalidad y ética**: se respeta `robots.txt`; el agente se identifica; se honra 429; el índice guarda fragmentos y no copias completas; los datos de Common Crawl se usan según sus términos [E][CC-terms]; Wikipedia se cita con su licencia (campo `license`); la lista Tranco combina fuentes con términos propios, algunas CC BY-NC [E][Tranco], válidas para uso personal no comercial (R-10).

### 15.5 Índice en disco

- **Biblioteca**: tantivy 0.27.0 (MIT, 2026-10-17; 21,2 millones de descargas) [E][Tantivy]. Segmentos inmutables, `commit` atómico mediante `meta.json`, borrados por término, fusiones en segundo plano, `MmapDirectory`. Descartadas: SeekStorm (BM25F real, pero adopción pequeña y benchmarks del propio autor) [E][SeekStorm], Meilisearch (BUSL en partes) y Quickwit (orientado a registros) [E][Search-engines].
- **Escritor único** dentro de webd con presupuesto de 64 MiB (el mínimo por hilo es 15 MB [E][Tantivy]); `commit` cada 5 000 documentos o 60 s; `LogMergePolicy` por defecto; almacén de documentos comprimido con zstd.
- **Esquema**:

| Campo | Tipo | Notas |
|---|---|---|
| `url` | STRING, STORED | Clave de actualización |
| `host`, `domain`, `source`, `lang`, `license` | STRING, FAST (`license` STORED) | Filtros |
| `title` | TEXT con posiciones, STORED | Frases sobre el título |
| `headings` | TEXT (frecuencias) | — |
| `body` | TEXT (solo frecuencias) | Sin posiciones, para ahorrar disco |
| `url_tokens` | TEXT | Partes de la URL |
| `snippet` | STORED | Primer 1 KB del texto principal |
| `published_at`, `fetched_at` | DATE, FAST, STORED | Frescura |
| `prior` | F64, FAST | 15.7 |
| `content_hash` | BYTES, STORED | Enlace con la lectura |

- **Analizador**: `SimpleTokenizer` + `RemoveLongFilter(40)` + `LowerCaser` + `AsciiFoldingFilter` (acentos del español). Sin lematización en v1; se mide su falta con el conjunto de consultas.
- **Actualización**: `delete_term(url)` + documento nuevo; borrado ante 404/410 y `noindex`.
- **Seguridad ante caídas**: una URL pasa a `pending_commit` con el `opstamp` del escritor y solo a `indexed` tras el `commit`; al arrancar, las `pending_commit` sin `commit` se vuelven a descargar.
- **Presupuesto**: tras cada `commit` se mide el tamaño del directorio del índice; al 95 % se detienen primero las fuentes de menor prioridad (dominios generales, noticias); se informa `coverage.budget_reached`.
- **RAM**: escritor de 64 MiB; búsqueda por `mmap` (caché de páginas del sistema); campos rápidos ≈8 B por documento y campo numérico (≈32 MB con 1 millón de documentos y 4 campos).

### 15.6 Actualización y frescura

- Feeds cada 2 h; Hacker News cada hora; arXiv cada noche (`from=` con la última fecha); portadas y *sitemaps* a diario para hosts con feed y semanalmente para el resto; `robots.txt` cada 24 h.
- **Páginas**: siguiente visita = última + `clamp(intervalo, 1 d, 60 d)`; empieza en 7 días, se divide por dos si cambió `content_hash` y se duplica si no. Es una aproximación barata de los estimadores de Cho y García-Molina; su resultado de que el refresco uniforme supera al proporcional en frescura media desaconseja sobremuestrear las páginas que cambian mucho [E][Cho-fresh].
- **Wikipedia**: nueva ingesta mensual del volcado semanal (borrado por `source = wiki` y reingesta).
- **Noticias**: se borran los documentos de más de 60 días.

### 15.7 Ranking

1. **Consulta**: `QueryParser` de tantivy sobre `title`×3, `headings`×1,5, `url_tokens`×2 y `body`×1; conjunción por defecto y, si hay menos de 10 resultados, disyunción. BM25 de tantivy con `k1 = 1,2` y `b = 0,75` fijos (no configurables sin bifurcar la biblioteca) [E][Tantivy].
2. **Filtros**: dominios (`domains`, `site:`), idioma, frescura (`published_at` o `fetched_at` ≥ límite), fuente.
3. **Candidatos**: los 200 primeros. Nueva puntuación: `final = bm25/max_bm25 + 0,3·prior + 0,2·frescura`, con `frescura = exp(−edad/30 d)` solo si se pide `freshness` o la consulta tiene intención de actualidad (año en curso, «hoy», «últimas», «noticias», «latest», «news»).
4. **Prior**: dominios con rango de Common Crawl: `1 − ln(posición_armónica)/ln(N)`; desconocidos 0,2; Wikipedia: `popularity_score` normalizado.
5. **Diversidad**: como máximo 2 resultados por host entre los primeros `k`; duplicados por `content_hash` o SimHash fusionados.
6. **Profundidad**: `k` hasta 50, para que el agente recupere en profundidad [E][Pi-Serini].
7. **Fusión de listas** (por ejemplo, índice y páginas leídas en la ejecución): RRF con `k = 60`, el valor fijado por sus autores, que demostraron que no es crítico [E][RRF].
8. **Reordenamiento** (A-08): `fastembed` 7.5.0 con `bge-reranker-v2-m3` (multilingüe) sobre los 20–30 primeros, una vez medida su latencia en CPU [E][Rerankers]. En BEIR, BM25 + reordenador cruzado mejora un 11 % de media frente a BM25 (gana en 16 de 18 conjuntos) [E][BEIR].

### 15.8 Fragmentos de resultado y fuentes

`SnippetGenerator` de tantivy sobre el campo `snippet` (1 KB guardado). Si los términos no aparecen ahí, se usan los encabezados. **Un resultado de búsqueda es un candidato, no una fuente**: solo una lectura con `web_read` crea un evento de fuente, de modo que la lista «Fuentes» de una respuesta contiene solo lo que el agente leyó y citó (anotaciones, requisito del 2026-10-26; arnés 10.9).

### 15.9 API

`web_search(query, {freshness, domains, lang, k})` →

```json
{
  "results": [{"url": "…", "title": "…", "snippet": "…", "published_at": "…", "fetched_at": "…",
               "source": "web|wiki|docs|news|arxiv|hn|read", "source_id": "w_…", "score": 0.83}],
  "coverage": {"profile": "minimo", "documents": 612034, "last_crawl_at": "…", "by_source": {"wiki": 301122, "web": 98011},
               "budget_reached": false},
  "insufficient": false
}
```

`insufficient` es verdadero con menos de 3 resultados o con una puntuación máxima por debajo del umbral calibrado: indica al agente que reformule, lea sitios conocidos o use `source_search`. Objetivo de latencia: p95 < 150 ms con 1 millón de documentos en la VPS (a medir).

### 15.10 Evaluación

- **Ítem conocido** (automática, en integración continua con un índice de prueba): 100 documentos indexados al azar; consulta con 3–5 términos distintivos de su título; éxito@10 ≥ 0,9 y MRR registrado. Comprueba que índice y ranking funcionan, no la calidad general.
- **Conjunto manual** (aceptación, fuera de integración continua): 40 consultas (20 en español y 20 en inglés) tomadas de tareas típicas del usuario, juzgadas por él; se informa P@5 y cobertura.
- Tamaño del índice por documento, tiempo de ingesta y latencia p50/p95.

### 15.11 Pruebas de verificación

| Prueba | Resultado esperado | Evidencia |
|---|---|---|
| Hosts locales con `robots.txt` de los casos de la RFC 9309 (reglas más largas, empates, 4xx, 5xx, 600 KiB, redirecciones, `Crawl-delay`) | Permitido o prohibido exactamente según la RFC; retraso respetado | Registro de decisiones |
| Rastreo de 3 hosts locales con reloj controlado | Nunca dos peticiones simultáneas al mismo host; intervalos ≥ retraso calculado; 429 con `Retry-After` respetado | Marcas de tiempo |
| Sin `contact_url` configurada | El rastreador no arranca | Error |
| Página con `noindex`, con `nofollow` y con `X-Robots-Tag: noindex` | No indexada; enlaces no seguidos | Índice, frontera |
| Dos páginas casi idénticas en el mismo host | Una sola en el índice | Índice |
| Matar webd durante una ingesta y reiniciar | Sin documentos perdidos ni duplicados; las URL `pending_commit` se vuelven a procesar | Recuento antes y después |
| Volcado CirrusSearch sintético de 10 000 líneas (formato verificado: pares de líneas `index` y documento [E][Cirrus]) | Documentos con `incoming_links` ≥ umbral indexados con su prior | Recuento |
| Presupuesto de índice pequeño (20 MiB) | La ingesta de baja prioridad se detiene al 95 %; `budget_reached` | Tamaño, respuesta |
| Ítem conocido con 100 documentos | éxito@10 ≥ 0,9 | Informe |
| `site:`, `lang`, `freshness` y `k = 50` | Filtros aplicados; ≤ 2 resultados por host | Resultados |
| Consulta sin resultados útiles | `insufficient: true` y cobertura informada | Respuesta |
| Resultados de búsqueda sin lectura posterior | No aparecen en `source_events` | Tabla del arnés |

---

## 16. Integración con el arnés (contratos 14.1, 14.2 y 14.3)

### 16.1 Manifiestos

Cada capacidad publica su manifiesto (arnés 14.1). Forma resumida del de `browser`:

```json
{
  "capability": "browser", "version": "0.1.0", "implementation": "excalibur-webd",
  "engine": {"name": "chrome", "version": "155.0.8059.84", "channel": "stable", "headless": "new"},
  "platforms": ["linux-x86_64"], "profiles": ["full"],
  "operations": ["browser_open_session", "browser_close_session", "browser_suspend_session", "browser_tabs",
                 "browser_navigate", "browser_wait_ready", "browser_snapshot", "browser_act", "browser_download",
                 "browser_devtools", "browser_emulate", "browser_status"],
  "unsupported": [{"operation": "browser_routine", "reason": "planned extension A-01"}],
  "resources": {"ram_fixed_mib_per_instance": 260, "ram_typical_mib_per_tab": 200, "max_sessions": 6,
                "max_tabs_per_run": 4, "max_tabs_total": 12, "max_ephemeral_contexts": 3},
  "concurrency": {"ops_per_tab": 1, "queue_timeout_ms": 30000},
  "states": ["working", "input_required", "completed", "failed", "cancelled"],
  "blocked_kinds": ["blocked_captcha", "needs_login", "needs_2fa", "blocked_by_site", "human_control", "sandbox_unavailable", "version_blocked"]
}
```

El de `search` declara `web_search` y `web_read`, el perfil de cobertura, el tamaño del índice, la fecha del último rastreo y los límites (`k ≤ 50`, 10 MiB por lectura).

### 16.2 Catálogo

Las herramientas `browser_*` son **diferidas** (fuera del prefijo, mediante `search_tools` y `call_tool`, arnés 13.3). `web_search` y `web_read` son **núcleo** (arnés 14.3). Metadatos de `CatalogTool` (estáticos; `prepare` puede rebajar el efecto de una llamada concreta):

| Herramienta | `effect` | `idempotency` | `risk` | `cancel` | `timeout_ms` | `fast` |
|---|---|---|---|---|---|---|
| `browser_open_session` | `write_local` | `keyed` | `medium` | `graceful` | 45 000 | no |
| `browser_close_session` | `write_local` | `idempotent` | `low` | `after_completion` | 15 000 | no |
| `browser_suspend_session` | `write_local` | `idempotent` | `low` | `after_completion` | 15 000 | no |
| `browser_tabs` | `write_local` | `keyed` | `low` | `after_completion` | 15 000 | no |
| `browser_navigate` | `write_external` | `idempotent` | `medium` | `graceful` | 30 000 | no |
| `browser_wait_ready` | `read_only` | `pure` | `low` | `immediate` | 60 000 | no |
| `browser_snapshot` | `read_only` | `pure` | `low` | `immediate` | 30 000 | no |
| `browser_act` | `write_external` | `non_idempotent` | `high` | `after_completion` | 20 000 | no |
| `browser_download` | `write_local` | `keyed` | `medium` | `graceful` | 150 000 | no |
| `browser_devtools` | `read_only` | `pure` | `low` | `immediate` | 10 000 | no |
| `browser_emulate` | `write_local` | `idempotent` | `low` | `immediate` | 10 000 | no |
| `browser_status` | `read_only` | `pure` | `low` | `immediate` | 5 000 | **sí** (el maestro puede consultarlo) |
| `web_search` | `read_only` | `pure` | `low` | `immediate` | 10 000 | sí |
| `web_read` | `read_only` | `pure` | `low` | `immediate` | 45 000 | sí |

- Los nombres cumplen las reglas del catálogo del arnés (ASCII `snake_case`, ≤ 64 bytes). Los esquemas JSON Schema 2020-12 de entrada y salida están en el apéndice B.
- **No existe** ninguna herramienta que acepte órdenes CDP arbitrarias, evalúe JavaScript, lea cookies o cambie permisos (control C5).
- Composición del host: `DeferredTools(ExecutionTools(SecuredTools(WebAdapter)))`, como el resto de runtimes del arnés.

### 16.3 Efecto por llamada y reconciliación

| Herramienta | Efecto en `prepare` | Si hay `tool_intent` sin `tool_result` tras una caída |
|---|---|---|
| `wait_ready`, `snapshot`, `devtools`, `status`, `web_search`, `web_read` | `Pure` | Se repite (`Reconcile::Retry`) |
| `navigate` | `Idempotent` | Sonda: si la pestaña ya está en la URL de destino con un documento nuevo, `Done`; si no, `Retry` |
| `act` de clase `observe` / `input` | `Pure` / `Idempotent` | `Retry` (la de `input` comprueba antes el valor) |
| `act` de clase `commit`, `navigate_link`, `upload`, `drag`, `credential` | `NonIdempotent` | Se consulta `web_ops(op_key)` (16.4): `completed` → `Done` con el resultado guardado; `received` sin `dispatched` → `Retry`; `dispatched` sin terminar o webd reiniciado → `Uncertain("…")`, nunca se repite y la ejecución pasa a `recuperando` con una instantánea nueva como información (arnés 17.2) |
| `open_session`, `tabs new`, `download` | `Keyed` | Se reenvía con la misma clave; webd devuelve la sesión, pestaña o descarga ya creada |
| `close_session`, `suspend_session`, `emulate` | `Idempotent` | Sonda del estado y `Done` o `Retry` |

### 16.4 Idempotencia y registro de operaciones de webd

- `op_key` = clave de efecto del arnés (la que exige el arnés 14.1 para toda operación con efectos).
- Tabla `web_ops(op_key PK, run_id, tool, state, request_hash, result_json, created_at, updated_at)` con estados `received` → `dispatched` → `completed` o `failed`. `dispatched` se escribe **justo antes** del primer evento de entrada o de la orden CDP con efecto. Retención: 24 h.
- Una petición repetida con la misma clave y el mismo `request_hash` devuelve el resultado guardado si está `completed`, se engancha a la operación en curso si sigue viva, o informa `uncertain` si estaba `dispatched` cuando webd se reinició. Si el `request_hash` difiere: `idempotency_conflict`.
- Esto da al adaptador información exacta para reconciliar. Una clave solo evita duplicados dentro de webd: los sitios no reciben claves, así que un `commit` incierto **nunca** se repite automáticamente (arnés 17.2).

### 16.5 Cancelación

| Clase | Operaciones | Qué hace webd |
|---|---|---|
| `immediate` | `wait_ready`, `snapshot`, `devtools`, `status`, `emulate`, `web_search`, `web_read` | Cancela al instante; descarta resultados parciales |
| `graceful` | `navigate`, `open_session`, `download` | `Page.stopLoading`; aborta el lanzamiento si aún no terminó; `Browser.cancelDownload` y borrado del parcial |
| `after_completion` | `act`, `close_session`, `suspend_session`, `tabs` | Termina el envío de eventos en curso (no se puede deshacer medio clic) y devuelve el resultado |
| `cooperative` | Espera de una intervención humana | El visor muestra «cancelado» y se cierra al terminar el gesto actual; el arnés vuelve a consultar el estado final (arnés 14.1) |

`ToolRuntime::interrupt_execution(run, cancelled)` llama a `op.cancel_run(run)`: cancela las operaciones de la ejecución y, si `cancelled` es verdadero, cierra sus contextos efímeros. Las sesiones persistentes siguen vivas. `execution_state(run, "paused")` libera las exclusiones de pestaña de la ejecución.

### 16.6 Recuperación

| Escenario | Detección | Comportamiento |
|---|---|---|
| Se reinicia el arnés con webd vivo | El adaptador reconecta al socket | Lista sesiones y operaciones; reconcilia con 16.3–16.4; las pestañas y las instantáneas en RAM siguen válidas; las sesiones en escucha no se enteraron |
| Se reinicia webd | `PDEATHSIG` mató a Chrome; webd lee `web_sessions` al arrancar | Sesiones efímeras → `lost`; sesiones persistentes → relanzadas, y las en escucha de inmediato, con sus URL restauradas; toda instantánea anterior → `stale_ref(browser_restarted)`; `web_ops` `dispatched` → inciertas |
| Cae el proceso del navegador | Fin de flujo en la tubería o salida del hijo | Persistente: `recovering` y relanzamiento con espera 1 s / 5 s / 30 s; más de 5 en 10 min → `failed` y aviso. Efímera: `lost` |
| Cae un renderizador | `Inspector.targetCrashed` | Pestaña `crashed`; si está en escucha, recarga; si no, se informa a la siguiente operación |
| Reinicio de la máquina | Como el reinicio de webd | Además, se retira el `SingletonLock` del perfil si el PID que contiene ya no existe (Chrome guarda `host-pid` en ese enlace) |
| Pérdida de red | Errores del broker | Operaciones de lectura con reintentos del arnés (17.7); sesiones en escucha intactas; el rastreador aparca hosts |
| Operación larga en curso durante un reinicio del arnés | Identificador de tarea durable guardado en el registro | El adaptador vuelve a consultar `op.get(op_key)` (ciclo de tareas MCP, arnés 14.1) |

### 16.7 Operaciones largas y ciclo de tareas

`browser_open_session` con arranque en frío, `browser_wait_ready` con bloqueo humano y `browser_download` pueden durar. Si una operación supera 2 s, webd responde `working{op_key}` y emite eventos de progreso. El adaptador guarda el identificador en el registro durable y la ejecución queda en `esperando` sin consumir tokens. `input_required` lleva el bloqueo y las instrucciones para el usuario. `completed`, `failed` y `cancelled` cierran la operación, con el mismo modelo de estados que la extensión de tareas de MCP que adopta el arnés.

### 16.8 Perfil `eval` y modo no interactivo

- Las herramientas web están sujetas a la marca `web` del perfil (`crates/eval/src/tools.rs`).
- `benchmark_source_blocked` se aplica a `web_read`, a `browser_navigate` y a los resultados de `web_search` (que se filtran).
- Perfiles persistentes **denegados** (`personal_external_capabilities: deny` en el perfil congelado); solo instancia efímera.
- Sin intervención humana: un bloqueo es un fallo inmediato.
- Directorios de webd efímeros por episodio.

### 16.9 Fuentes, contenido no confiable y auditoría

- Cada `web_read` y cada lectura de texto con `browser_snapshot` en una sesión efímera producen un evento de fuente con URL, título, fecha y hash.
- Todo resultado se marca no confiable con su procedencia.
- Las llamadas se auditan con argumentos redactados; los secretos, solo por referencia.

### 16.10 Pruebas de verificación

| Prueba | Resultado esperado | Evidencia |
|---|---|---|
| Ejecución del arnés con proveedor simulado que abre una sesión efímera, navega a un sitio local, espera, captura, rellena un formulario, lo envía (con aprobación simulada) y lee el resultado con `web_read` | Termina `completada` con evidencia; trayectoria ATIF con todas las llamadas; fuentes = solo las páginas leídas | ATIF, `source_events`, auditoría |
| Mismo recorrido con `kill -9` del arnés tras el `tool_intent` del envío y antes del `tool_result` | Al reanudar: `web_ops` dice `completed` → `Done` sin repetir el envío (contador del servidor = 1) | Contador, registro |
| `kill -9` de webd tras `dispatched` del envío | `Uncertain`; la ejecución pasa a recuperación con instantánea nueva; el envío no se repite | Estado, contador |
| `kill -9` de webd con una sesión en escucha | Relanzada; vuelve a emitir eventos en < 30 s | Eventos |
| Cancelar una ejecución durante `wait_ready`, `navigate`, `act` y una descarga | Comportamiento de 16.5 en cada clase; contextos efímeros cerrados; persistentes vivos | Tiempos, estados |
| `search_tools` con «tomar captura de la página» y «hacer clic en el botón» | Devuelve `browser_snapshot` y `browser_act` entre los 5 primeros; el prefijo no cambia | Resultados, hashes de prefijo |
| Perfil `eval`: abrir perfil persistente, URL del benchmark en `web_read` y en `navigate`, y un bloqueo | Denegaciones y fallo inmediato | Respuestas |
| Misma `op_key` con otra petición | `idempotency_conflict` | Respuesta |

---

## 17. Superficie hacia la interfaz de Excalibur

Este documento no diseña la interfaz general. Define lo que cualquier cliente necesita del navegador:

- **Eventos** que la puerta del arnés reenviará: `blocked` (con tipo, sitio y pestaña), `takeover_state`, `listener_activity`, `instance_crashed`, `memory_pressure` y avisos de versión.
- **Entrada a la intervención**: el aviso de un bloqueo incluye el código de un solo uso, entregado por el canal del usuario, y la dirección local del visor. Hasta que exista la puerta (H8), la CLI ofrece `excalibur browser takeover <session>`, que imprime el código y la dirección para usar por túnel SSH. Es la única pieza de CLI nueva y no duplica lógica: llama a `takeover.start` de webd.
- **Estado** en lenguaje sencillo a partir de `browser_status`: sesiones, sitios con sesión iniciada, memoria por instancia, presupuesto y bloqueos pendientes.
- **Reglas para el frontend** (12.5): texto de las páginas siempre como texto; Markdown sin HTML crudo ni imágenes remotas automáticas; el origen comprometido se muestra junto a cualquier contenido de una página.

---

## 18. Estrategia de pruebas

### 18.1 Principios

- **Backend, negocio y seguridad**: todas las pruebas automatizadas se ejecutan contra webd y el adaptador, con **Chrome real** y **sitios sintéticos locales**. No hay suites funcionales de frontend.
- **Sin Internet en integración continua**: un servidor de pruebas (Rust, `hyper`, ya en el workspace) sirve las páginas. Un resolvedor de pruebas inyectado en el broker (`ProxyConfig.resolver`) asigna nombres como `captcha.test`, `idp.test` u `other-site.test` a direcciones locales con excepciones privadas exactas, de modo que las pruebas de orígenes cruzados, OOPIF y detectores de terceros no tocan sitios reales.
- **Si falta Chrome, las pruebas fallan**: no se saltan en silencio. Solo `EXCALIBUR_BROWSER_TESTS=skip`, explícito, las omite, y el informe lo dice. Es la misma regla de «falla cerrado» que usa el sandbox del arnés.
- **Determinismo**: reloj simulado para plazos largos (intervenciones, refrescos, desalojos); semillas fijas; sin modelos ni API de pago (regla del repositorio).
- **Evidencia**: cada suite guarda sus resultados y los registros redactados en `target/test-evidence/browser/<suite>/`.

### 18.2 Suites

| Suite | Cubre | Secciones |
|---|---|---|
| `webd_boundary` | Usuario del SO, socket, tamaños, prohibición de flags, PDEATHSIG | 3.5, 4.6 |
| `cdp_contract` | Transporte por tubería y cada orden CDP usada contra la versión instalada | 4.6 |
| `sessions` | Perfiles, contextos, pestañas, límites, exclusión, escucha, persistencia | 5.8 |
| `observe` | Instantáneas, referencias, `stale_ref`, redacción, capturas, devtools, emulación | 6.8 |
| `wait` | Espera en todos los patrones | 7.5 |
| `actions` | Acciones, clases, diálogos, ventanas emergentes | 8.5 |
| `blocking` | Detectores, intervención, visor (protocolo y cabeceras) | 9.5 |
| `credentials` | Relleno por referencia y sus rechazos, búsqueda del secreto | 10.5 |
| `files` | Descargas y subidas | 11.3 |
| `browser_security` | Sandbox, red, inyección, permisos, esquemas, mundos aislados | 12.9 |
| `resources` | Medición, admisión, desalojo, escucha protegida, disco | 13.6 |
| `reader` | `web_read` y citas | 14.4 |
| `search` | Rastreo, índice, ranking, ítem conocido, caídas | 15.11 |
| `web_integration` | Recorridos con el arnés, caídas, cancelación, perfil `eval` | 16.10 |

Órdenes previstas, que el hito B9 añadirá a `AGENTS.md`: `cargo test -p excalibur-cdp -p excalibur-browser -p excalibur-search -p excalibur-web-adapter` y `cargo test -p excalibur-webd --test web_integration`, más `cargo fmt --check` y `cargo clippy --workspace --all-targets -- -D warnings`.

### 18.3 Medición de memoria reproducible

Procedimiento de B6, en la VPS y con el binario fijado:

1. Instancia en blanco; un sitio estático local; diez pestañas efímeras con la página pesada sintética.
2. Para cada caso: 30 s de reposo; lectura de los niveles A, B y C (13.1); PSS por tipo de proceso.
3. Se repite 3 veces y se informa mediana, mínimo y máximo, con la versión de Chrome, el kernel y los flags.
4. El recorrido manual de 18.4 añade, con sesión iniciada, WhatsApp y Teams tras 1 h y 24 h. Sin esas cifras no se afirma ningún consumo con sesión.

### 18.4 Recorridos manuales con sitios reales (fuera de integración continua)

Los hace el usuario o se hacen con su permiso, porque usan sus cuentas. Resultado esperado **honesto**, no «automatización completa»:

| Recorrido | Resultado esperado |
|---|---|
| Google Search desde la VPS | `blocked_captcha` detectado (ya observado [M]); intervención ofrecida; el agente usa `web_search` propio como alternativa |
| Inicio de sesión de Google por el visor | O bien `logged_in` y Gmail legible con `browser_snapshot`, o bien `blocked_by_site(signin_rejected)`. Ambos resultados se registran (R-04) |
| WhatsApp Web | `needs_login` con QR; vinculación por el visor; `logged_in`; tras ello, `listener_activity` al recibir un mensaje de prueba; enviar un mensaje pide aprobación con texto y destinatario; la sesión sigue tras reiniciar webd |
| Teams | `needs_login` en `login.microsoftonline.com`; inicio de sesión humano; lectura de un chat; envío con aprobación; eventos de no leídos |
| Lectura de 20 páginas reales variadas con `web_read` | Proporción `render: http` frente a `browser`, bloqueos y calidad del Markdown, revisados a mano |

### 18.5 Revisión visual del visor

Solo revisión visual, sin suite funcional, en dos tamaños, con capturas guardadas:

| Tamaño | Qué comprobar |
|---|---|
| Pequeño (360×740, móvil) | El lienzo cabe en el ancho sin desplazamiento horizontal; botones apilados y pulsables (≥ 44 px); origen legible en la cabecera; cuadro de texto visible con el teclado |
| Grande (1440×900) | Lienzo a escala 1:1 hasta 1280 px y centrado; cabecera con origen y estado; botones alineados |
| Ambos | Un título malicioso (`<img src=x onerror=alert(1)>`) aparece como texto literal; el estado «cancelado» o «caducado» se entiende; ningún contenido de la página fuera del lienzo |

El auditor revisa las capturas, que se toman con el propio Chrome headless de webd (`browser_emulate small|large` sobre el visor).

---

## 19. Hitos, estimaciones y flujo de auditoría

### 19.1 Reglas de trabajo

- **Dónde**: checkout `excalibur-browser`, rama `browser`. No se modifica `main` ni el checkout `excalibur`. Los archivos compartidos (`Cargo.toml` raíz, `Cargo.lock`, composición del host) se tocan lo mínimo y, la composición, solo en B9.
- **Disco antes de B0**: el usuario liberó ≈22,5 GiB el 2026-10-29 (≈28 GiB libres según `df`). B0 vuelve a medirlo; compilar los crates nuevos crea un `target` propio de varios GB cuyo tamaño se registra en cada informe de entrega, y se respetan la reserva y los límites de 13.5.
- **Orden**: secuencial, B0 → B9. Un hito empieza cuando el anterior está cerrado o aceptado según la política del usuario.
- **Discrepancias**: si algo de este documento resulta inviable, el implementador lo registra con su motivo en el informe de entrega y lo señala; no lo cambia en silencio (misma regla que el apéndice E del arnés).
- **Documentación del repositorio**: cada hito actualiza `docs/development/browser.md` y `browser.es.md` en paralelo (regla de `AGENTS.md`) y no describe como implementado lo que no lo está.
- **Sin servicios de pago** y sin cuentas reales en pruebas automatizadas.

### 19.2 Tabla de hitos

Total: **32,5 h de implementación + 3,5 h de reserva para correcciones = 36 h**. Estimaciones orientativas, no plazos garantizados.

| Hito | Alcance | NB | Estimación | Cierre (pruebas) |
|---|---|---|---|---|
| **B0. Preparación y comprobaciones del entorno** | Crates vacíos (3.3) y miembros del workspace; configuración `web.*` en TOML; binario `excalibur-webd` con cambio de usuario y rechazo de root; detección del binario de Chrome y su versión; comprobación de disco y de cgroup; informe JSON de comprobación previa; esqueleto de documentación bilingüe | NB-02 | 1,5 h | 3.5 (filas de usuario y root); informe de comprobación previa en la VPS |
| **B1. Cliente CDP y gestor de instancias** | `excalibur-cdp` por tubería (4.3); lanzamiento con flags y lista de prohibidos (4.4); PDEATHSIG y grupo de procesos; árbol de procesos; detección de caídas; **verificación del sandbox** (12.2); política de versión (4.5) | NB-01, NB-02 | 3,5 h | 4.6; 12.9 (filas de sandbox); suite `cdp_contract` |
| **B2. Perfiles, sesiones, pestañas y protocolo** | `webd.db` (apéndice C); máquina de estados (5.3); perfil persistente y contextos efímeros; pestañas en ventana propia; propietarios, límites y exclusión; modo en escucha y `listener_activity` (5.7); socket JSON-RPC con `SO_PEERCRED`; `web_ops` (16.4); recuperación de sesiones al reiniciar webd | NB-03, NB-04, NB-14, NB-26 | 3,5 h | 3.5, 5.8 |
| **B3. Espera y observación** | Espera (7.2–7.4); instantáneas (6.2–6.3); referencias y `stale_ref` (6.4); capturas (6.5); subárbol (6.6); devtools y emulación (6.7); blobs | NB-06, NB-07, NB-08, NB-25 | 4,5 h | 6.8, 7.5 |
| **B4. Acciones, diálogos, archivos y credenciales** | Canalización y acciones (8.1–8.2); clases (8.3); diálogos y ventanas (8.4); descargas y subidas (11); relleno de credenciales con `SecretExecutor` (10) | NB-09, NB-11, NB-12 | 4,0 h | 8.5, 10.5, 11.3 |
| **B5. Red, contenido, bloqueos e intervención** | Oyente loopback con comprobación de UID + `HostProxy` (12.3); compuerta de documentos y conjuntos de orígenes (C2–C4); permisos y esquemas (12.6); mundos aislados (12.5); detectores (9.1); intervención y visor con autenticación y CSP (9.2–9.3); revisión visual | NB-05, NB-10, NB-15, NB-16 | 4,0 h | 9.5, 12.9, 18.5 |
| **B6. Recursos** | Muestreo por niveles y atribución (13.1); presupuestos, admisión y desalojo con protección de las sesiones en escucha (13.2–13.4); caché y disco (13.5); campos de telemetría; medición reproducible (18.3) | NB-17, NB-18 | 2,0 h | 13.6; informe de mediciones |
| **B7. Lectura ligera y citas** | `web_read` completo (14.2) con broker, heurística HTTP/navegador, render de respaldo, extracción, fragmentos y caché; `cite` (14.3) | NB-19, NB-23 | 2,5 h | 14.4 |
| **B8. Buscador** | Frontera y planificador (15.4); `robots.txt`; duplicados; índice tantivy (15.5); ingesta en flujo de Wikipedia; feeds, Hacker News y arXiv; semillas Tranco + rangos de Common Crawl; ranking (15.7); `web_search` (15.9); refresco (15.6); presupuesto; ítem conocido (15.10) | NB-20, NB-21, NB-22 | 4,5 h | 15.11; informe de cobertura en la VPS si el disco lo permite |
| **B9. Integración con el arnés y entrega** | `excalibur-web-adapter` (`ToolRuntime`, catálogo, manifiestos, reconciliación, cancelación, perfil `eval`) (16); registro en la composición del host detrás de configuración; `excalibur browser takeover`; suite `web_integration`; órdenes en `AGENTS.md`; documentación; informe final; recorridos manuales de 18.4 documentados con sus resultados reales | NB-13, NB-24, NB-27 | 2,5 h | 16.10; `cargo fmt --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace` |
| **Reserva** | Correcciones de auditoría de todos los hitos | — | 3,5 h | — |

### 19.3 Flujo de implementación y auditoría

1. **Muse Code implementa** el hito y entrega un informe con esta forma:

   | Campo | Contenido |
   |---|---|
   | Hito y commit | Identificador y SHA (si el usuario autoriza commits) |
   | Alcance entregado | NB cubiertos y secciones de este documento |
   | Pruebas ejecutadas | Órdenes exactas y resumen de salida (aprobadas, fallidas, ignoradas con motivo) |
   | Evidencia | Rutas en `target/test-evidence/browser/` |
   | Desviaciones | Qué cambió respecto al documento y por qué |
   | Pendientes | Lo no terminado, con su NB |
   | Mediciones | Memoria, tamaños y tiempos cuando el hito los produce |
   | Horas | Tiempo empleado frente a la estimación |

2. **Auditoría** (esta sesión): revisión del diff y de las invariantes de seguridad, ejecución de las pruebas de cierre y comprobación de los NB del hito. Hallazgos clasificados en **bloqueante**, **importante** y **menor**, cada uno con su evidencia.
3. **Muse corrige**: como máximo **dos rondas de corrección** por hito.
4. **Tras la segunda ronda** se avanza según la política del usuario: aceptar con pendientes registrados, que pasan como entrada al hito siguiente, o detenerse.
5. **Presupuesto**: las correcciones consumen la reserva. Si un hito supera su estimación en más de un 30 % o se agota la reserva, se aplica el recorte ordenado de 19.4 en lugar de ampliar el presupuesto, y se informa.

### 19.4 Recorte ordenado si falta tiempo

Primero se recorta, en este orden:

1. Fuentes arXiv y Hacker News (B8).
2. Ingesta de feeds (se conservan los *sitemaps*).
3. Atribución de memoria por pestaña mediante traza (se conserva la de instancia).
4. `drag` en modo HTML5 (se conserva el de ratón).
5. Métricas de rendimiento de `browser_devtools`.
6. Tamaños de emulación distintos de `small` y `large`.

**Nunca se recortan**: verificación del sandbox, transporte por tubería, comprobación de origen de las credenciales, salida por el broker, protección de las sesiones en escucha, `stale_ref`, espera, registro de operaciones y recuperación, marcado no confiable, y las pruebas de seguridad.

---

## 20. Ampliaciones separadas (fuera de las 36 h)

| ID | Ampliación | Diseño resumido | Estimación orientativa |
|---|---|---|---|
| A-01 | Rutinas (`browser_routine`) | Rutinas definidas por el agente o grabadas de una intervención: pasos con rol y nombre (no coordenadas), valores parametrizados y una aserción por paso; reproducción determinista sin modelo; si una aserción falla, el control vuelve al agente en ese paso (arnés 14.2, regla 4) | 4–6 h |
| A-02 | Relanzar con interfaz conservando la sesión | `Storage.getCookies` antes de cerrar y `Storage.setCookies` tras relanzar (las de sesión incluidas); el `sessionStorage` se pierde y se documenta | 2 h |
| A-03 | Chrome en espacio de nombres de red propio | Perfil bubblewrap `browser` que permite `CLONE_NEWUSER` (sin `--disable-userns`), `--unshare-net`, `--die-with-parent`, `--new-session`, y el puente de red del arnés como única salida; perfil AppArmor si hace falta | 4–6 h + verificación |
| A-04 | Un cgroup por instancia | `StartTransientUnit` de systemd por D-Bus con `MemoryHigh`/`MemoryMax` por instancia | 2 h |
| A-05 | macOS, Windows y móviles | macOS: `phys_footprint` y límites propios; Windows: Job Objects y `--remote-debugging-io-pipes`; móviles: sin Chrome controlable, solo nodos remotos (arnés 14.10) | Grande |
| A-06 | TOTP | Tipo de secreto `otpauth`, código RFC 6238 generado en el ejecutor; separado de la contraseña, como recomienda la investigación de *clickjacking* [E][Toth-clickjacking] | 1,5 h |
| A-07 | Intervención por WebRTC | Captura de pestaña con códec de vídeo para enlaces lentos | 4 h o más |
| A-08 | Recuperación densa y reordenamiento | `fastembed` con `bge-reranker-v2-m3` sobre los 20–30 primeros; *embeddings* multilingües; medir latencia y mejora con el conjunto manual | 3–5 h |
| A-09 | Perfil `estandar` del buscador | Wikipedia en inglés y español, 10 000–20 000 dominios; ≈25–30 GB | 2 h + disco + 1 semana de rastreo |
| A-10 | PDF en el rastreador | `pdf-extract` con límites | 1 h |
| A-11 | Texto de anclas y grafo propio | Indexar el texto de los enlaces entrantes; centralidad propia | 4 h |
| A-12 | Relleno desde Common Crawl | `cluster.idx` (104,5 MB) + peticiones HTTP *Range* a los bloques CDX, sin la API limitada [E][CC-index] | 3 h |
| A-13 | Actualizador de Chrome | Canal CfT con verificación de integridad y relanzamiento ordenado | 3 h |
| A-14 | Perfil cifrado en reposo | fscrypt con clave del baúl, desbloqueada solo mientras corre el navegador (no protege frente a root ni frente al usuario en línea [E][OSCrypt]) | 3 h + privilegios |
| A-15 | Evaluación del navegador | BU Bench v2.1 (200 tareas en sitios reales con rúbrica y juez LLM [E][BUBench]), Online-Mind2Web (300 tareas vivas [E][OnlineM2W]) y BrowseComp para investigación profunda [E][BrowseComp]; con coste y aprobación previa. Responde a la referencia de las anotaciones a «Browser Use Bench V2.1», que existe: v2.1 y v2.1.1 publicadas el 2026-09-25 | 2 h de arnés + coste de ejecución |
| A-16 | Nivel de lectura anónima más barato | Medir `chrome-headless-shell` y, para lectura masiva sin capturas, Lightpanda (AGPL, sin cambios) frente a Chrome | 2 h |
| A-17 | Analizadores por idioma | Lematización de español e inglés en campos separados; medir recuperación frente a tamaño | 1,5 h |

---

## 21. Riesgos e incertidumbres

| ID | Riesgo | Probabilidad / impacto | Mitigación |
|---|---|---|---|
| R-01 | Disco de la VPS: tras la limpieza del 2026-10-29 hay ≈28 GiB libres (86 % de uso), pero el `target` del checkout principal ocupa 113 GB y las compilaciones vuelven a crecer | Media / detener compilaciones o ingestas | Límites explícitos y reserva de 5 GiB (13.5); medición del `target` propio en cada hito; comprobación previa que falla cerrado |
| R-02 | Chrome atrasado (155 frente a 156 estable) y ciclo de dos semanas | Media / seguridad y soporte de Teams | Política de versión (4.5); A-13 |
| R-03 | Google muestra CAPTCHA a la búsqueda automatizada desde IP de centro de datos (observado [M]) | Alta / sin búsqueda de Google | Buscador propio (15); intervención si el usuario la quiere; Google no ofrece alternativa oficial: su Custom Search JSON API está cerrada a nuevos clientes y se retira el 2027-01-01 [E][Google-unusual] |
| R-04 | El inicio de sesión de Google desde la VPS puede rechazarse | Media / sin Gmail en el navegador | Detección `signin_rejected`; nodo de escritorio; conectores oficiales del arnés |
| R-05 | Restricción de la cuenta de WhatsApp por uso automatizado | Media / pérdida de la cuenta vinculada | Solo uso personal, envíos con aprobación, límite por hora (C8); el usuario asume el riesgo, que queda documentado |
| R-06 | Cambios de interfaz de WhatsApp y Teams rompen selectores de escucha o de espera; página de bloqueo de Teams para navegadores sin ES2022 [E][Teams-req] | Alta / eventos o esperas degradados | Selectores en configuración; espera que no depende de ellos para funcionar; versión al día |
| R-07 | Memoria y disco reales con sesiones iniciadas desconocidos | Media / presupuestos mal dimensionados | Medición de B6 y de 18.4 antes de fijar valores |
| R-08 | Cookies en texto claro en servidores sin llavero | Alta en la VPS / robo si se lee el perfil | Permisos del usuario web; aviso; A-14 |
| R-09 | Órdenes CDP experimentales cambian entre versiones | Media / roturas tras actualizar | Suite `cdp_contract` por versión; campos opcionales tolerantes |
| R-10 | Términos y licencias de datos (Tranco con componentes CC BY-NC, Wikipedia CC BY-SA, Common Crawl) frente a la licencia con restricciones comerciales de Excalibur | Media / uso comercial | Uso personal; revisión antes de cualquier uso comercial; atribución en resultados |
| R-11 | La inyección de instrucciones no tiene frontera en el modelo; fatiga de aprobaciones | Alta / acciones no deseadas | Controles C1–C9; aprobaciones de acción exacta; límites de frecuencia |
| R-12 | Presupuesto de 36 h ajustado | Media / alcance incompleto | Reserva y recorte ordenado (19.4) |
| R-13 | La clasificación `commit` falla en algún sitio | Media / acción sin aprobación | En persistentes, lo desconocido es `commit`; configuración por sitio |
| R-14 | El proxy no frena a un navegador comprometido | Baja / salida directa | A-03 |
| R-15 | Conflictos de fusión con `main` | Media / trabajo de integración | Crates nuevos; archivos compartidos mínimos y al final |
| R-16 | Versiones de Chrome sin las mismas garantías (CfT sin actualización automática) | Media | CfT solo en pruebas |

---

## 22. Referencias

Consultadas el 2026-10-29 salvo indicación. Las marcadas **(secundaria)** se verificaron solo por fuentes de terceros.

### Motor, transporte y herramientas de automatización

| Clave | Fuente | Versión, fecha o ubicación |
|---|---|---|
| [CDP] | Chrome DevTools Protocol: `browser_protocol.json` y PDL en `github.com/ChromeDevTools/devtools-protocol`; `chromedevtools.github.io/devtools-protocol` | r1729950, commit `a45ae55` (2026-10-29); dominios `DOMSnapshot`, `Accessibility`, `Browser.setDownloadBehavior`, `Browser.setPermission`, `Page.startScreencast`, `Input.insertText`, `Memory` marcados experimentales |
| [Headless] | `developer.chrome.com/docs/chromium/headless`; `developer.chrome.com/blog/chrome-headless-shell`; `pptr.dev/guides/headless-modes` | Actualizado 2024-10-21; headless unificado desde Chrome 112; `chrome-headless-shell` separado desde 132.0.6793.0 |
| [CfT] | `github.com/GoogleChromeLabs/chrome-for-testing`; `googlechromelabs.github.io/chrome-for-testing/last-known-good-versions-with-downloads.json`; `developer.chrome.com/blog/chrome-for-testing` | Marca 2026-10-29T12:22:14Z: estable 156.0.8078.73; linux-arm64 desde 153.0.8001.0 |
| [Chrome-2w] | `developer.chrome.com/blog/chrome-two-week-release` | 2026-03-03; ciclo de dos semanas desde Chrome 153 (8 de septiembre) |
| [Chrome-update] | Google Security Blog, actualizaciones de seguridad semanales desde Chrome 116 (2023-08-08) **(secundaria)**; CVE-2026-2441, -3909, -3910, -5281, -11645, -85046 y -87491 explotadas en 2026 | — |
| [Chrome136] | `developer.chrome.com/blog/remote-debugging-port` | 2025-03-17 |
| [Pipe] | Chromium `content/public/common/content_switches.cc` y `devtools_pipe_handler.h` (`main`); Playwright `packages/playwright-core/src/server/chromium/chromium.ts`; Puppeteer `PipeTransport.ts`; `pptr.dev/api/puppeteer.launchoptions` | Playwright v1.64.1 (2026-10-14) |
| [chromiumoxide] | `crates.io/crates/chromiumoxide`; `github.com/mattsse/chromiumoxide` (issue #294, PR #325); `github.com/spider-rs/chromey`; `thirtyfour` en crates.io | 0.9.1 (2026-02-25); chromey 2.58.2 (2026-09-12); thirtyfour 0.38.1 (2026-10-21) |
| [agent-browser] | `github.com/vercel-labs/agent-browser` (`cli/src/native/snapshot.rs`, `cdp/chrome.rs`) | v0.41.0, Apache-2.0 |
| [browser-use] | `github.com/browser-use/browser-use`; `browser-use.com/posts/playwright-to-cdp`; CVE-2025-47241 / GHSA-x39x-9qw5-ghrf; issue #5438 | 0.13.14 (`2cc71890`, 2026-10-26); entrada del 2025-08-20; corregido en 0.1.45 |
| [PlaywrightMCP] | `github.com/microsoft/playwright-mcp`; `playwright.dev/docs/actionability`; documentación de `waitForLoadState` (networkidle desaconsejado); PR #37795, #39777, playwright-mcp#1654; CVE-2025-9611 | 0.0.83; corregido en 0.0.40 |
| [ChromeDevToolsMCP] | `github.com/ChromeDevTools/chrome-devtools-mcp`; CVE-2026-53766 / GHSA-8qf9-62x2-82pp | v1.12.0; corregido en 1.1.0 |
| [Stagehand] | `github.com/browserbase/stagehand` (`waitForDomNetworkQuiet`) | sdk-ts 4.3.0 (`edb76b0e`) |
| [Servo] | `servo.org/blog/`; `crates.io/crates/servo` | 0.7.0 (2026-10-05); LTS 0.1.4 |
| [Ladybird] | `ladybird.org` | Sin alfa a 2026-10-29 |
| [Lightpanda] | `lightpanda.io/blog/posts/lightpanda-1-0`; `github.com/lightpanda-io/browser` | 1.0 (2026-10-02), AGPL-3.0 |
| [WebKit] | `bugs.webkit.org/show_bug.cgi?id=274007`; foro de Purism **(secundaria)** | — |
| [Firefox-CDP] | `fxdx.dev/cdp-retirement-in-firefox/` | 2025-06-02 |
| [CEF-Google] | `developers.googleblog.com/2020/08/guidance-for-our-effort-to-block-less-secure-browser-and-apps.html` | Vigente desde 2021-01-04 |
| [ProcModel] | Chromium `docs/process_model_and_site_isolation.md`; Reis et al., «Site Isolation», USENIX Security 2019 (sobrecoste de memoria 9–13 %) | `main` |
| [LinuxSandbox] | Chromium `sandbox/linux/README.md`; `docs/security/apparmor-userns-restrictions.md`; crbug 638180 (root exige `--no-sandbox`) | `main` |
| [ProxyDocs] | Chromium `net/docs/proxy.md` y documento de diseño de SOCKS | `main` |
| [Policies] | Chromium `docs/enterprise/policies.md`; `support.google.com/chrome/a/answer/7579271` (`DownloadRestrictions`) | — |
| [LNA] | Local Network Access, Chrome 142, sustituye a Private Network Access **(secundaria; enlace exacto no registrado)** | — |
| [Flatpak-zypak] | `github.com/refi64/zypak`; `bwrap(1)` (`--disable-userns`); hilo de chromium-dev de 2019 | — |

### Sitios objetivo y términos

| Clave | Fuente | Fecha |
|---|---|---|
| [Teams-req] | `learn.microsoft.com/en-us/microsoftteams/teams-client-system-requirements`; centro de mensajes MC1230888 (`mc.merill.net/message/MC1230888`) | ms.date 2026-07-16; bloqueo de navegadores sin ES2022 desde 2026-05-15 |
| [Google-signin] | `security.googleblog.com/2019/04/better-protection-against-man-in-middle.html`; [CEF-Google] | 2019-04-18; 2020-08 |
| [Google-unusual] | `support.google.com/websearch/answer/86640`; `developers.google.com/custom-search/v1/overview` | Custom Search JSON API cerrada a nuevos clientes, fin el 2027-01-01 |
| [WA-ToS] | `www.whatsapp.com/legal/terms-of-service` | En vigor desde 2021-01-04 |
| [WA-js] | whatsapp-web.js `src/Client.js` | 2026-09-26 |
| [MS-SA] | `www.microsoft.com/en-us/servicesagreement` | Publicado 2026-07-30, en vigor 2026-09-30 |

### Observación, espera e intervención

| Clave | Fuente |
|---|---|
| [Mind2Web] | Deng et al., arXiv 2306.06070 |
| [WebArena] | Zhou et al., arXiv 2307.13854 (§2.3–2.4, tabla 2) |
| [VWA] | Koh et al., VisualWebArena, arXiv 2401.13649 (tabla 3) |
| [WebVoyager] | He et al., arXiv 2401.13919 (tabla 1) |
| [SeeAct] | Zheng et al., arXiv 2401.01614 (tabla 3) |
| [AgentOccam] | Yang et al., arXiv 2410.13825 (tabla 17) |
| [BrowserGym] | Le Sellier De Chezelles et al., arXiv 2412.05467 |
| [OnlineM2W] | Xue et al., «An Illusion of Progress?», arXiv 2504.01382 |
| [ReadMore] | «Read More, Think More», arXiv 2604.01535 (tabla 1) |
| [D2Snap] | arXiv 2508.04412 (tabla 1) |
| [FocusAgent] | arXiv 2510.03204 (tablas 1–3) |
| [BrowseComp] | Wei et al., arXiv 2504.12516 |
| [BUBench] | `github.com/browser-use/benchmark`, versiones v2.1 y v2.1.1 (2026-09-25) |
| [Idleness] | Chromium `third_party/blink/renderer/core/loader/idleness_detector.{h,cc}` |
| [Screencast] | CDP `Page.startScreencast`; `InputModel.ts` del frontend de DevTools; comparación de terceros en `github.com/pingdotgg/t3code/issues/26523` |
| [CF-mitigated] | Documentación de Cloudflare sobre detección de desafíos en la respuesta (`cf-mitigated`) |
| [Skyvern-challenge] | Skyvern `challenge_signature.py`, `challenge_evidence.py`, `multi_field_totp.py` |
| [Lifecycle] | `developer.chrome.com/docs/web-platform/page-lifecycle-api`; `developer.chrome.com/blog/freezing-on-energy-saver` (2025-01-20); `developer.chrome.com/blog/timer-throttling-in-chrome-88` |
| [Lifecycle-headless] | hermes-agent PR #135507; caracara PR #33 (capturas colgadas en pestañas ocultas) |
| [NetBuf] | Chromium `third_party/blink/renderer/core/inspector/inspector_network_agent.cc` (200 MB totales / 20 MB por recurso en escritorio) |

### Seguridad

| Clave | Fuente |
|---|---|
| [Trifecta] | Simon Willison, «The lethal trifecta for AI agents» (2025-06-16) |
| [RuleOfTwo] | Meta, «Agents Rule of Two» (2025) **(enlace exacto no registrado)** |
| [InjecAgent] | Zhan et al., arXiv 2403.02691 |
| [AgentDojo] | Debenedetti et al., arXiv 2406.13352 (tablas 3 y 5) |
| [WASP] | Evtimov et al., arXiv 2504.18575 (tabla 3, §4.2) |
| [EIA] | Liao et al., arXiv 2409.11295 |
| [Popups] | Zhang et al., arXiv 2411.02391 |
| [VPI] | VPI-Bench, arXiv 2506.02456 (resumen) |
| [InTheWild] | arXiv 2604.27202 (abril de 2026) |
| [Adaptive] | Nasr, Carlini et al., arXiv 2510.09023 |
| [CaMeL] | Debenedetti et al., arXiv 2503.18813 (§3.1, §5, §6.4, §6.5, §9.2) |
| [ChromeAgentSec] | Google, «Architecting security for agentic capabilities in Chrome», `blog.google/security/architecting-security-for-agentic/` (2025-12-08) |
| [Brave-Comet] | `brave.com/blog/comet-prompt-injection/` (2025-08-20) y continuación del 2025-10-21 |
| [Anthropic-PI] | `anthropic.com/research/prompt-injection-defenses` (2025-11-24); `claude.com/blog/claude-for-chrome` (2025-08-25) |
| [EchoLeak] | CVE-2025-32711; arXiv 2509.10540 |
| [OpenClaw-upload] | CVE-2026-26329 / GHSA-cv7m-c9jx-vg7q |
| [SpecterOps-Cookie] | SpecterOps, «Return of the Cookie Monster» (2026-08-13) |
| [Toth-clickjacking] | Marek Tóth, *DOM-based extension clickjacking*, DEF CON 33 (2025-08-09, actualizado 2026-01-14) |
| [Bitwarden] | `bitwarden.com/help/uri-match-detection/` |
| [OSCrypt] | Chromium `docs/linux/password_storage.md`; commit `c4eea4e` (2024-12-10); kernel, documentación de fscrypt |
| [OWASP-XSS] | OWASP Cross Site Scripting Prevention Cheat Sheet |

### Recursos

| Clave | Fuente |
|---|---|
| [Proc] | `docs.kernel.org/filesystems/proc.html`; commit `493b0e9d945f` (`smaps_rollup`, v4.14) |
| [cgroupv2] | `docs.kernel.org/admin-guide/cgroup-v2.html` (`memory.peak` desde 5.19) |
| [PMF] | Chromium `chrome/browser/resource_coordinator/utils.cc` (commit `c2b048cc`) |
| [Mem-public] | `superchargebrowser.com/library/chrome-ram-usage-per-tab-2026.md` (Chromium 148, junio de 2026); blog de comparación de whatsapp-web.js **(secundaria, aproximado)**; `lightpanda.io/docs/core-concepts/benchmarks` |
| [Caches] | MDN, *Storage quotas and eviction criteria*; semántica de `--disk-cache-size` **(secundaria)**; `Storage.pdl` |
| [Crawl4ai] | `docs.crawl4ai.com/advanced/multi-url-crawling/` |
| [Crawlee-adaptive] | `crawlee.dev/js/api/playwright-crawler/class/AdaptivePlaywrightCrawler` |
| [JinaReader] | `github.com/jina-ai/reader` (README) |
| [reqwest013] | `seanmonstar.com/blog/reqwest-v013-rustls-default/` (2025-12-30); `docs.rs/reqwest/latest/reqwest/struct.ClientBuilder.html` |

### Buscador y lectura

| Clave | Fuente | Versión o fecha |
|---|---|---|
| [Tantivy] | `github.com/quickwit-oss/tantivy` (`ARCHITECTURE.md`, `src/query/bm25.rs`, `index_writer.rs`, `log_merge_policy.rs`); `crates.io/crates/tantivy` | 0.27.0 (2026-10-17), MIT |
| [SeekStorm] | `github.com/SeekStorm/SeekStorm` | 3.4.5 (2026-10-27), Apache-2.0 |
| [Stract] | `github.com/StractOrg/stract` | Archivado el 2026-04-02, AGPL-3.0 |
| [Marginalia] | `github.com/MarginaliaSearch/MarginaliaSearch` (`CrawlDelayTimer.java`); `about.marginalia-search.com/article/crawler` | Activo (2026-10-27) |
| [Search-engines] | Mwmbl, YaCy, Quickwit (Apache-2.0 tras unirse a Datadog), Meilisearch (MIT y BUSL-1.1) en GitHub | — |
| [Anserini-sizes] | `huggingface.co/datasets/castorini/prebuilt-indexes-msmarco-v1` | MS MARCO v1 doc |
| [CC-index] | `index.commoncrawl.org/collinfo.json`; `data.commoncrawl.org/crawl-data/CC-MAIN-2026-43/cc-index/collections/CC-MAIN-2026-43/indexes/cluster.idx` | CC-MAIN-2026-43 |
| [CC-webgraph] | `commoncrawl.org/blog/host--and-domain-level-web-graphs-may-june-and-july-2026` | 2026-07-28 |
| [CC-terms] | `commoncrawl.org/terms-of-use`; `commoncrawl.org/ccbot` | — |
| [OWI] | `openwebsearch.eu/owil/current` | OWIL v1.0 (2024-05-08) |
| [Tranco] | `tranco-list.eu`; Le Pochat et al., NDSS 2019 (doi:10.14722/ndss.2019.23386) | Lista del 2026-10-28 |
| [Cirrus] | `dumps.wikimedia.org/other/cirrus_search_index/20261025/` | Semanal |
| [arXiv-OAI] | `info.arxiv.org/help/oa/index.html` | — |
| [HN-API] | `github.com/HackerNews/API` | — |
| [Sitemaps] | `www.sitemaps.org/protocol.html` | — |
| [feed-rs] | `crates.io/crates/feed-rs` | 3.0.0 (2026-09-27), MIT |
| [texting_robots] | `docs.rs/texting_robots` | 0.2.2 (2023) |
| [RFC9309] | `www.rfc-editor.org/rfc/rfc9309.html` | — |
| [RFC3986] | `www.rfc-editor.org/rfc/rfc3986.html` (§6.2) | — |
| [SimHash] | Manku, Jain y Das Sarma, WWW 2007 (doi:10.1145/1242572.1242592) | — |
| [Cho-fresh] | Cho y García-Molina, TODS 2003 (doi:10.1145/958942.958945) y TOIT 2003 (doi:10.1145/857166.857170) | — |
| [Harmonic] | Boldi y Vigna, «Axioms for Centrality», arXiv 1308.2140 | — |
| [RRF] | Cormack, Clarke y Büttcher, SIGIR 2009 | — |
| [BEIR] | Thakur et al., arXiv 2104.08663 (tablas 2 y 3) | — |
| [Rerankers] | `huggingface.co/BAAI/bge-reranker-v2-m3`; `github.com/Anush008/fastembed-rs` | fastembed 7.5.0 (2026-10-23) |
| [BrowseComp-Plus] | arXiv 2508.06600 | — |
| [Pi-Serini] | arXiv 2605.10848 (mayo de 2026) | — |
| [SAGE] | arXiv 2602.05975 | — |
| [Bevendorff] | Bevendorff et al., SIGIR 2023 (doi:10.1145/3539618.3591920); Trafilatura, Barbaresi, ACL 2021 demo (doi:10.18653/v1/2021.acl-demo.15) | — |
| [WCXB] | arXiv 2605.21097 (mayo de 2026; autor con conflicto de interés declarado por la investigación) | — |
| [dom_smoothie] | `github.com/niklak/dom_smoothie` | 0.19.0 (2026-10-27), MIT |
| [htmd] | `github.com/letmutex/htmd` | 0.5.6 (2026-10-15), Apache-2.0 |
| [Extract-crates] | crates.io: `pdf-extract` 0.12.1 (MIT), `lopdf` 0.46.0, `html5ever` 0.40.1, `lol_html` 3.0.3 (BSD-3), `scraper` 0.29.0 (ISC), `simhash` 0.3.0 (MIT); `html2md` es GPL-3.0+ y se evita | 2026-10-29 |
| [TextFragments] | `github.com/WICG/scroll-to-text-fragment`; MDN *Text fragments*; caniuse | Borrador WICG |

### Documentos y código de Excalibur

| Clave | Ubicación |
|---|---|
| [arnés …] | `arquitectura-arnes.md`, secciones citadas (10.9, 13.3, 14.1–14.3, 17.1–17.3, 20.6–20.15, apéndices E y F) |
| [arnés: security], [arnés: vault] | `excalibur-browser/docs/development/security.es.md`; `crates/security`, `crates/vault`, `crates/sandbox/src/network.rs`, `network/unix.rs` |
| Código | `crates/types/src/runtime.rs`, `crates/tools/src/deferred.rs`, `crates/telemetry/src/resources.rs`, `crates/eval/src/{tools,profile}.rs`, `crates/store/src/blobs.rs` (commit `768a497`) |

---

## Apéndice A. Parámetros iniciales

| Parámetro | Valor | Sección |
|---|---|---|
| Pestañas por ejecución / globales | 4 / 12 | 5.6 |
| Contextos efímeros simultáneos | 3 | 5.2 |
| Espera en cola por pestaña | 30 s | 5.6 |
| Parada de la instancia efímera sin contextos | 10 min | 5.2 |
| TTL de contexto o pestaña IDLE | 5 min | 13.3 |
| Relanzamientos de instancia | 1 s, 5 s, 30 s; máximo 5 en 10 min | 16.6 |
| Frecuencia máxima de `listener_activity` | 1 cada 5 s por sitio | 5.7 |
| Presupuesto de captura de instantánea | 3 s | 6.2 |
| Tope del texto de instantánea | 16 KB (≈4 000 tokens) | 6.3 |
| Captura | JPEG q60, vista; página completa ≤ 16 384 px | 6.5 |
| Anillos de consola y red | 500 entradas por pestaña | 6.7 |
| Búferes de `Network.enable` | 2 MiB total, 512 KiB por recurso | 6.7 |
| Espera: red / DOM / plazo / larga duración | 500 ms / 500 ms / 10 s / 3 s | 7.2 |
| Asentamiento tras acción | 250 ms para detectar navegación; 5 s de plazo | 7.2 |
| Reintentos de accionabilidad | 0, 20, 100, 100, 500 ms | 8.1 |
| Envíos de mensajería por hora | 20 | 12.4 |
| Plazo de intervención humana | 15 min | 9.2 |
| Código del visor | 8 caracteres, 10 min, un solo uso | 9.3 |
| Screencast | JPEG q60, 1280×900 máximo, acuse por trama | 9.3 |
| Retención de capturas tras relleno de credenciales | 5 s sin enviar al modelo | 10.2 |
| Descarga: tope / plazo | 200 MiB / 120 s | 11.1 |
| Preparación de subidas | Borrado al cerrar o a la hora | 11.2 |
| Presupuesto blando / duro del navegador | 1,5 GiB / 2,0 GiB (`memory.max` 2,5 GiB) | 13.2 |
| Instancia efímera: aviso / cierre por pestaña | 500 MiB / 800 MiB | 13.2 |
| Pestaña en escucha: aviso / recarga programada | 700 MiB / > 1 GiB durante 10 min | 13.2 |
| Estimación de admisión por origen | 200 MiB (400 MiB aplicaciones pesadas) + 200 MiB de margen | 13.2 |
| PSI para admitir | `some avg10` < 20 % | 13.2 |
| Muestreo A / B / C | 2 s / 10 s / 60 s | 13.1 |
| Caché HTTP de Chrome | 32 MiB efímera / 64 MiB persistente | 13.5 |
| Perfil persistente: aviso / vaciado de cachés permitidas | 1 GiB / 1,5 GiB | 13.5 |
| Reserva de disco libre | 5 GiB | 13.5 |
| Datos de webd sin índice | 2,5 GiB | 13.5 |
| Cuarentena y preparación | 300 MiB | 13.5 |
| Instantáneas en blobs | 24 h o 500 MiB | 13.5 |
| Caché de lecturas | 24 h; 200 MiB | 14.2 |
| `web_read`: conexión / total / redirecciones / cuerpo / PDF | 10 s / 30 s / 5 / 10 MiB / 20 MiB | 14.2 |
| Salida inline de `web_read` | 50 KB | 14.2 |
| Decisión HTTP/navegador por host | 7 días; 10 % de revalidación | 14.2 |
| Rastreo: retraso por host | `clamp(2 × descarga, 1 s, 5 s)` + 0–150 ms | 15.4 |
| `Crawl-delay` máximo / `Retry-After` máximo / tope de retroceso | 30 s / 1 h / 120 s | 15.4 |
| Aparcado de host | 5 errores → 24 h | 15.4 |
| Concurrencia y ritmo del rastreador (`minimo`) | 16 hosts; 5 páginas/s | 15.4 |
| Índice `minimo`: presupuesto / cuota de Wikipedia | 1,5 GiB / 0,6 GiB (estado 0,5 GiB) | 15.3 |
| Índice `ampliado` (opcional) | 6 GiB (estado 1 GiB) | 15.3 |
| Escritor tantivy / `commit` | 64 MiB / cada 5 000 documentos o 60 s | 15.5 |
| Ranking: candidatos / pesos | 200 / título 3, encabezados 1,5, URL 2, cuerpo 1; prior 0,3; frescura 0,2 (30 d) | 15.7 |
| Resultados por host / `k` máximo / RRF | 2 / 50 / 60 | 15.7 |
| Refresco de páginas | 7 d inicial; entre 1 y 60 d | 15.6 |
| Ventana de noticias | 60 días | 15.6 |

---

## Apéndice B. Esquemas

### B.1 Herramientas (resumen de JSON Schema 2020-12)

```text
browser_open_session
  in : { profile: string ("ephemeral" | nombre de perfil), mode: "headless"|"headed" = "headless",
         site?: string, credentials?: [secret_ref], label?: string }
  out: { session_id, profile, instance_id, mode, state, sites: [{name, login_state}], tabs: [Tab], resumed: bool }

browser_close_session | browser_suspend_session
  in : { session_id }            out: { session_id, state }

browser_tabs
  in : { session_id, action: "list"|"new"|"switch"|"close", url?: string, tab_id?: string, label?: string }
  out: { tabs: [ { tab_id, url, title, state, active, owner_label?, listen } ] }

browser_navigate
  in : { tab_id, action: "go"|"back"|"forward"|"reload", url?: string, wait?: WaitCriteria }
  out: { tab_id, url, http_status?, load_state, navigation_id, wait?: WaitResult }

browser_wait_ready
  in : WaitCriteria = { tab_id, network_idle_ms = 500, dom_stable_ms = 500, selector?: string, text?: string,
                        timeout_ms = 10000, max_inflight = 0, strict_network = false }
  out: WaitResult = { status: "ready"|"timeout"|"blocked", degraded: [string], criteria_met: {navigation, network, dom, condition},
                      blocked?: { kind, vendor?, signals: [string], confidence, url, frame_id? }, elapsed_ms, document_epoch,
                      inflight_sample?: [url] }

browser_snapshot
  in : { tab_id, screenshot: bool = false, dom: "interactive"|"full" = "interactive",
         extent: "viewport"|"fullpage" = "viewport", ref?: string, wait?: WaitCriteria }
  out: { snapshot_id, url, title, load_state, screenshot_ref?: blob, dom_tree_ref: blob, text: string,
         elements: [ { ref, role, name, type, bbox: {x,y,w,h}, visible, enabled, in_viewport, frame, why, value? } ],
         omitted: int, partial: bool, untrusted: true, wait: WaitResult }

browser_act
  in : { tab_id, snapshot_id, action: "click"|"type"|"select"|"hover"|"scroll"|"press"|"upload"|"drag"|"dialog",
         ref?: string, value?: string, secret_ref?: string, keys?: string, files?: [staged_id],
         target_ref?: string, options?: { button?, click_count?, modifiers?, key_events?, html5?, direction?, amount?,
         accept?, prompt_text? } }
  out: { result: "ok"|"stale_ref"|"not_actionable"|"blocked"|"denied"|"dialog_open",
         reason?, remapped_ref?, current_snapshot_id?, covered_by?, navigated: bool, url, new_tabs: [Tab],
         dialog?: {type, message}, download?: {state, suggested_filename}, dom_changed: bool,
         suggest_snapshot: bool, settled: bool, action_class, elapsed_ms }

browser_download
  in : { tab_id, trigger: { snapshot_id, ref } | { url }, timeout_ms = 120000 }
  out: { blob, file_name, mime, size, sha256, quarantine_status }

browser_devtools
  in : { tab_id, kind: "console"|"network"|"performance", filters?: { level?, url_contains?, since_seq?, limit = 100 } }
  out: { records: [object], next_seq, dropped }

browser_emulate
  in : { tab_id, preset?: "small"|"tablet"|"desktop"|"large", viewport?: { width, height, device_scale_factor, mobile } }
  out: { tab_id, viewport }

browser_status
  in : { detail: bool = false }
  out: { instances: [ { instance_id, profile, class, state, chrome_version, pids, working_set_bytes, pss_bytes? } ],
         sessions: [...], tabs: [...], memory: { working_set_bytes, budget_bytes, pressure, cgroup: bool },
         warnings: [string] }

web_search
  in : { query, freshness?: "day"|"week"|"month"|"year"|date, domains?: [string], lang?: string, k: int = 10 (≤ 50) }
  out: { results: [ { url, title, snippet, published_at?, fetched_at, source, source_id, score } ],
         coverage: { profile, documents, last_crawl_at, by_source, budget_reached }, insufficient: bool }

web_read
  in : { url, fresh?: bool, fragments?: [string] }
  out: ver 14.1
```

### B.2 Protocolo webd

```text
→ {"jsonrpc":"2.0","id":17,"method":"act.perform",
   "params":{"op_key":"run_01J…:step12:call3","run_id":"run_01J…","deadline_ms":20000,"tab_id":"t1",
             "snapshot_id":"s_42","action":"click","ref":"e18","action_class":"commit"}}
← {"jsonrpc":"2.0","id":17,"result":{"state":"completed","result":{"result":"ok","navigated":true,…}}}
← {"jsonrpc":"2.0","method":"event","params":{"cursor":9912,"type":"listener_activity",
   "session_id":"…","site":"whatsapp","kind":"title_changed","count":3,"at":"…"}}
```

Errores tipados: `tab_busy`, `tab_limit`, `session_lost`, `session_recovering`, `capacity_exceeded`, `denied{reason}`, `blocked{…}`, `idempotency_conflict`, `cdp_timeout`, `unsupported_type`, `version_blocked`, `sandbox_unavailable`, `listener_cannot_suspend`, `mode_unavailable`.

### B.3 Subconjunto CDP usado

| Dominio | Órdenes | Eventos |
|---|---|---|
| Browser | `getVersion`, `close`, `setDownloadBehavior`, `cancelDownload`, `setPermission` | `downloadWillBegin`, `downloadProgress` |
| Target | `createBrowserContext`, `disposeBrowserContext`, `createTarget`, `closeTarget`, `attachToTarget`, `setAutoAttach`, `getTargets` | `targetCreated`, `targetInfoChanged`, `targetDestroyed`, `attachedToTarget`, `detachedFromTarget` |
| Page | `enable`, `navigate`, `reload`, `stopLoading`, `getNavigationHistory`, `navigateToHistoryEntry`, `bringToFront`, `captureScreenshot`, `getLayoutMetrics`, `getFrameTree`, `createIsolatedWorld`, `addScriptToEvaluateOnNewDocument`, `setLifecycleEventsEnabled`, `handleJavaScriptDialog`, `setInterceptFileChooserDialog`, `startScreencast`, `stopScreencast`, `screencastFrameAck` | `lifecycleEvent`, `frameNavigated`, `frameRequestedNavigation`, `frameStartedNavigating`, `javascriptDialogOpening`, `fileChooserOpened`, `screencastFrame` |
| DOM | `enable`, `resolveNode`, `describeNode`, `getBoxModel`, `getContentQuads`, `scrollIntoViewIfNeeded`, `getNodeForLocation`, `focus`, `setFileInputFiles`, `getOuterHTML` | — |
| DOMSnapshot | `captureSnapshot` | — |
| Accessibility | `enable`, `getFullAXTree` | — |
| Input | `dispatchMouseEvent`, `dispatchKeyEvent`, `insertText`, `setInterceptDrags`, `dispatchDragEvent` | `dragIntercepted` |
| Runtime | `enable`, `callFunctionOn`, `addBinding` (solo en mundos aislados propios) | `consoleAPICalled`, `exceptionThrown`, `bindingCalled` |
| Network | `enable` (búferes reducidos), `disable`, `clearBrowserCache` | `requestWillBeSent`, `loadingFinished`, `loadingFailed`, `requestServedFromCache`, `responseReceived` |
| Fetch | `enable` (solo `Document`), `continueRequest`, `failRequest` | `requestPaused` |
| Emulation | `setDeviceMetricsOverride`, `clearDeviceMetricsOverride`, `setFocusEmulationEnabled` | — |
| Performance | `enable`, `getMetrics` | — |
| Memory | `simulatePressureNotification` (solo instancia efímera) | — |
| HeapProfiler | `collectGarbage` | — |
| Storage | `getUsageAndQuota`, `clearDataForOrigin` (solo operación de host aprobada) | — |
| Tracing | `start`, `end` (atribución bajo demanda) | `dataCollected`, `tracingComplete` |
| Inspector | — | `targetCrashed`, `detached` |
| Log | `enable` | `entryAdded` |

---

## Apéndice C. Esquema de datos

`webd.db` (SQLite en WAL, un escritor):

```sql
CREATE TABLE web_profiles(name TEXT PRIMARY KEY, class TEXT NOT NULL CHECK(class IN('persistent','ephemeral')),
  dir TEXT NOT NULL, config TEXT NOT NULL CHECK(json_valid(config)), chrome_version TEXT, size_bytes INTEGER,
  updated_at INTEGER NOT NULL);
CREATE TABLE web_site_state(profile TEXT NOT NULL REFERENCES web_profiles(name), site TEXT NOT NULL,
  login_state TEXT NOT NULL CHECK(login_state IN('unknown','logged_in','needs_login')), changed_at INTEGER NOT NULL,
  PRIMARY KEY(profile, site));
CREATE TABLE web_sessions(id TEXT PRIMARY KEY, owner_run TEXT, profile TEXT NOT NULL, context_id TEXT,
  mode TEXT NOT NULL, site TEXT, listen INTEGER NOT NULL, state TEXT NOT NULL, origins TEXT NOT NULL CHECK(json_valid(origins)),
  credentials TEXT NOT NULL CHECK(json_valid(credentials)), created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL);
CREATE TABLE web_tabs(id TEXT PRIMARY KEY, session_id TEXT NOT NULL REFERENCES web_sessions(id), target_id TEXT,
  owner_run TEXT, label TEXT, url TEXT, title TEXT, state TEXT NOT NULL, document_epoch INTEGER NOT NULL,
  updated_at INTEGER NOT NULL);
CREATE TABLE web_ops(op_key TEXT PRIMARY KEY, run_id TEXT NOT NULL, tool TEXT NOT NULL, request_hash TEXT NOT NULL,
  state TEXT NOT NULL CHECK(state IN('received','dispatched','completed','failed')), result TEXT CHECK(result IS NULL OR json_valid(result)),
  created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL);
CREATE TABLE web_memory(at INTEGER NOT NULL, instance_id TEXT NOT NULL, working_set INTEGER, pss INTEGER,
  processes INTEGER, PRIMARY KEY(at, instance_id));
CREATE TABLE web_read_cache(url_norm TEXT PRIMARY KEY, final_url TEXT NOT NULL, content_hash TEXT NOT NULL,
  blob TEXT NOT NULL, fetched_at INTEGER NOT NULL, render TEXT NOT NULL);
CREATE TABLE web_render_decisions(host TEXT NOT NULL, path_prefix TEXT NOT NULL, render TEXT NOT NULL,
  decided_at INTEGER NOT NULL, PRIMARY KEY(host, path_prefix));
```

`search.db`: tablas `hosts` y `urls` de 15.4, más `seeds(source, ref, added_at)`, `feeds(url PK, last_polled_at, etag, last_modified, err_count)` e `ingest_runs(id, source, started_at, finished_at, docs, bytes, status)`. El índice tantivy vive en `search/index/`.

Directorios del usuario web: `profiles/<nombre>/` (`0700`), `quarantine/`, `staging/`, `blobs/` (compartido con el arnés por streaming, no por montaje), `search/`, `run/webd.sock`.

---

## Apéndice D. Trazabilidad

| NB | Secciones | Hito | Pruebas |
|---|---|---|---|
| NB-01 | 4 | B1 | 4.6 |
| NB-02 | 3.2, 12.2 | B0, B1 | 3.5, 12.9 |
| NB-03 | 5.1–5.5 | B2 | 5.8 |
| NB-04 | 5.6 | B2 | 5.8 |
| NB-05 | 5.4, 9 | B5 | 9.5, 18.5 |
| NB-06 | 6.1–6.3, 6.5 | B3 | 6.8 |
| NB-07 | 6.4 | B3 | 6.8 |
| NB-08 | 7 | B3 | 7.5 |
| NB-09 | 8 | B4 | 8.5 |
| NB-10 | 9.1–9.2 | B5 | 9.5 |
| NB-11 | 10 | B4 | 10.5 |
| NB-12 | 11 | B4 | 11.3 |
| NB-13 | 16.5 | B9 | 16.10 |
| NB-14 | 16.6, 5.3 | B2, B9 | 5.8, 16.10 |
| NB-15 | 5.5, 12.2, 12.3, 12.6 | B5 | 12.9 |
| NB-16 | 12.4, 12.5 | B5 | 12.9, 9.5 |
| NB-17 | 13.1–13.4 | B6 | 13.6 |
| NB-18 | 13.5 | B6 | 13.6 |
| NB-19 | 14.2 | B7 | 14.4 |
| NB-20 | 15.3–15.4 | B8 | 15.11 |
| NB-21 | 15.5–15.6 | B8 | 15.11 |
| NB-22 | 15.7 | B8 | 15.11 |
| NB-23 | 14.2–14.3, 15.8 | B7, B8 | 14.4, 15.11 |
| NB-24 | 16 | B9 | 16.10 |
| NB-25 | 6.7 | B3 | 6.8 |
| NB-26 | 5.7 | B2 | 5.8 |
| NB-27 | 18 | Todos | 18 |
| NB-28 | A-01 | Ampliación | — |

Correspondencia con las dudas de las anotaciones:

| Duda de `anotaciones.md` | Respuesta |
|---|---|
| ¿CLI o MCP? | Ni lo uno ni lo otro como frontera principal: servicio propio con protocolo JSON-RPC sobre socket Unix y herramientas diferidas del arnés (3.4, 16). Exponerlo como servidor MCP es posible después, con las mismas herramientas |
| ¿Componente separado o integrado? | Separado (`excalibur-webd`), por privilegios, fallos, memoria y continuidad (D3) |
| ¿Chromium u otra alternativa según la RAM? | Chromium; las alternativas no sirven para las tres webs; la RAM se controla con instancias compartidas, contextos efímeros, desalojo y HTTP ligero (4, 13, 14) |
| Cookies y sesiones entre modos | 5.4–5.5 |
| XSS y amenazas | 12 |
| Varias interfaces abiertas | 5.6 |
| Buscador sin índice externo y cobertura | 15.1–15.3 |
| Fuentes abiertas sin clave de API | Volcados de Wikipedia, Common Crawl (grafos y rangos), Tranco, arXiv OAI-PMH, Hacker News API, sitemaps y feeds (15.3) |
| Modelo con visión para capturas | Configuración del rol `vision` del arnés; el navegador solo decide cuándo enviar la captura (6.5) |
| Evaluación con «Browser Use Bench V2.1» | Existe (v2.1 y v2.1.1, 2026-09-25); ampliación A-15 con aprobación por coste |
| Comparar velocidad, RAM y precisión de alternativas | Mediciones de este documento (apéndice E) y A-15/A-16 |

---

## Apéndice E. Mediciones locales y procedimiento

**Entorno**: VPS Ubuntu 24.04.5, kernel 6.8.0-147, 6 núcleos, 11,7 GiB sin swap, Google Chrome 155.0.8059.84, headless nuevo, usuario `nobody` (UID 65534) con el sandbox de Chrome activo, 2026-10-29. Borradores **fuera del producto** en `/tmp/excalibur-browser-draft/scripts/` (`measure.sh`, `measure_multi.sh`, `cdp_pipe_probe.py`); los perfiles temporales se borraron al terminar.

**Método de memoria**: lanzar Chrome con `setpriv --reuid=65534 --regid=65534 --clear-groups` y un HOME y perfil temporales; esperar 8–40 s; recorrer el árbol de procesos desde el PID del navegador; sumar `Rss`, `Pss` y `Private_Clean + Private_Dirty` de `/proc/<pid>/smaps_rollup`; clasificar por `--type=`. Una medición por caso: son indicativas, no estadísticas. B6 repite el procedimiento con la mediana de 3 repeticiones.

| Caso | Procesos | ΣRSS | ΣPSS | ΣUSS |
|---|---|---|---|---|
| `about:blank`, flags por defecto | 14 | 1 345 MiB | 336 MiB | 192 MiB |
| `about:blank`, sin extensiones ni componentes | 9 | 834 MiB | 257 MiB | 134 MiB |
| `www.google.com` | 9 | 930 MiB | 308 MiB | 171 MiB |
| `web.whatsapp.com` (35 s) | 10 | 1 208 MiB | 466 MiB | 317 MiB |
| `teams.microsoft.com` (35 s) | 10 | 1 052 MiB | 320 MiB | 173 MiB |
| Tres sitios en una instancia (40 s) | 14 | — | 528 MiB | 361 MiB |
| Ídem con `--renderer-process-limit=1` | 13 | — | 531 MiB | 364 MiB |

`--renderer-process-limit=1` no cambió nada: es un límite blando que el aislamiento por sitio supera [E][ProcModel].

**Compatibilidad observada** (capturas headless a 1280×900):

- WhatsApp Web muestra «Scan to log in» con el QR cargando: acepta el navegador headless.
- Teams se queda en la pantalla de carga (no se probó el inicio de sesión).
- Google Search (`/search?q=…`) muestra al primer intento «Our systems have detected unusual traffic from your computer network» con reCAPTCHA.
- Wikipedia devolvió al Chrome headless «Wikimedia Error — Too many requests», y en el mismo momento `curl` obtuvo 200 con el agente de usuario de HeadlessChrome y con uno descriptivo.

**Agente de usuario**: `Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) HeadlessChrome/155.0.0.0 Safari/537.36`; `navigator.webdriver = false` sin flags de automatización.

**Sandbox**: los 7 renderizadores y el *utility* sandboxeado tenían `Seccomp: 2`, `NoNewPrivs: 1`, espacio de nombres de usuario `4026532288` (el del host es `4026531837`), un espacio de nombres de PID por renderizador y un espacio de nombres de red propio. El navegador, el GPU y el servicio de red estaban en los espacios de nombres del host con `Seccomp: 0`.

**CDP por tubería** (`cdp_pipe_probe.py`):

| Página | `DOMSnapshot.captureSnapshot` | `Accessibility.getFullAXTree` | `DOM.getDocument(-1, pierce)` | Captura JPEG q60 / PNG (base64) | Lista interactiva compacta |
|---|---|---|---|---|---|
| Repositorio de tantivy en GitHub | 547 KB, 254 ms, 3 615 nodos | 926 KB, 167 ms, 2 237 nodos (264 interactivos) | 844 KB, 98 ms | 121 KB / 232 KB | 8,2 KB (≈2 000 tokens) |
| WhatsApp Web (QR) | 1,29 MB, 180 ms, 322 nodos | 8 KB, 22 nodos | 205 KB | 12 KB / 15 KB | 0 elementos a los 12 s |

**Mediciones del agente auxiliar** ([M\*], misma VPS): coste de lectura de `smaps_rollup` ≈8,2 ms por proceso de Chrome frente a 70 µs de `status`; `--no-zygote` sin ahorro (291 frente a 276 MiB PSS); visibilidad y limitación de temporizadores en pestañas ocultas headless; congelación con 0 ticks; `oom_score_adj` por tipo de proceso; atribución por traza en 110–150 ms; espera con *long-polling* y WebSocket; colisiones de `backendNodeId` entre OOPIF; screencast (tramas, ancho de banda, acuses). Algunas se tomaron como root con `--no-sandbox`, por eso no se usan como cifras de producción.

**Proceso ajeno observado**: un Chrome de otro flujo, lanzado como root con `--no-sandbox --remote-debugging-port=9227`. No se tocó.
