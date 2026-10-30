# Fundamento del buscador del navegador

[English](browser-search.md)

`excalibur-search` implementa los fundamentos independientes del hito de
búsqueda del documento externo `arquitectura-navegador.md` (secciones
15.4–15.5 y 15.7–15.9). El crate es autocontenido: solo depende de
`rusqlite`, `url`, `serde`, `serde_json`, `sha2` y `thiserror`, ya fijados
en el lock del workspace, más una arista opcional a `tantivy` 0.27 tras la
feature `tantivy`. No toca el ciclo de vida del navegador, el
intermediario de red ni la contabilidad de recursos del host. No hay rastreo
en vivo, despliegue ni acceso a modelos; todas las comprobaciones usan
fixtures locales y sintéticas.

## Mapa de módulos

| Módulo | Responsabilidad |
|---|---|
| `robots` | Analizador RFC 9309 y decisiones de rastreo: gana la regla más larga, los empates prefieren `Allow`, grupos exactos combinados cuando hay coincidencia con grupos por prefijo solo sin grupo exacto y `*` como alternativa, `/robots.txt` siempre permitido, líneas `Sitemap:` globales conservadas, `Crawl-delay` topado a 30 s, 4xx permite y 5xx/inaccesible deniega con reintento en una hora |
| `normalize` | Normalización de URL WHATWG: host en minúsculas, fragmento y puertos por defecto eliminados, parámetros de rastreo retirados, aproximación del dominio registrable con sufijos comunes de dos niveles, ruta para robots y tokens de búsqueda |
| `frontier` | Frontera SQLite (`hosts`, `urls`) con el esquema de la arquitectura y los estados `queued`, `fetching`, `pending_commit`, `indexed`, `skipped`, `failed`, `gone`; solo las semillas son rastreables mientras los enlaces entre hosts registran candidatos y los candidatos antiguos siguen sin semilla tras migrar, `robots.txt` vigente exigido antes de despachar (24 h accesible, 1 h de reintento ante 5xx/inaccesible, denegación en caso contrario), una petición en vuelo por host, tope global más tope de páginas por segundo, cortesía de `clamp(2 × duración, 1 s, 5 s)` más jitter determinista redondeado hacia arriba al segundo, intervalos de refresco explícitos por URL (7 d iniciales, mitad si cambia, doble si no, 1–60 d) con re-despacho de `indexed` vencidas, candidatas prohibidas por robots marcadas como `skipped` con la siguiente candidata en la misma llamada bajo el agente configurado, respeto de `Retry-After`, aparcamiento tras cinco errores seguidos, exigencia de `contact_url` y recuperación de caídas que reencola filas `fetching` y `pending_commit` al cerrar/reabrir |
| `dedup` | Duplicados exactos por `sha256` del Markdown normalizado (conserva mayúsculas); casi duplicados por SimHash de 64 bits sobre tejas de 3 palabras con distancia de Hamming ≤ 3 dentro del mismo host |
| `ingest` | Sitemap acotado (`<loc>`, 50 000 URL, 50 MiB) con comprobación de host y decodificación de entidades en una pasada más variante por directorio del sitemap, feeds (RSS `<link>`, Atom `href`, JSON Feed `url`) y análisis de pares de líneas CirrusSearch con filtro por umbral de `incoming_links` y `opening_text` tolerante a nulos |
| `index` | Almacén determinista de documentos en disco (`DiskIndex`) con el modelo de documento de la arquitectura, BM25 (`k1 = 1,2`, `b = 0,75`, título ×3, encabezados ×1,5, tokens de URL ×2, cuerpo ×1), fragmentos guardados de 1 KiB, actualización por borrado con la URL, durabilidad atómica por inserción con `commit` que refresca el estado al 95 % del presupuesto, admisión por prioridad de fuente (web general, noticias y HN se detienen al 95 %, wiki/docs/arXiv/read siguen admitidas) y desempates estables por URL en cada ordenación |
| `backend` | Rasgo `IndexBackend` compartido por ambos almacenes: mismo contrato de `insert`/`remove`/`commit`/`search_bm25`, misma fusión de duplicados y admisión al 95 % por prioridad de fuente, mismos filtros estrictos antes de truncar; el pipeline y el cableado de `web_search` programan contra este rasgo |
| `tantivy_index` | Almacén tantivy 0.27 de producción (feature `tantivy`) con el mismo modelo de documento: `url` STRING STORED como clave de actualización, `host`/`domain`/`source`/`lang`/`license` STRING STORED, `title` TEXT con posiciones STORED, `headings` TEXT solo con frecuencias, `body` TEXT solo con frecuencias sin posiciones, `url_tokens` TEXT, `snippet` STORED de 1 KiB, `published_at`/`fetched_at` I64 FAST STORED en segundos unix, `prior` F64 FAST STORED, `content_hash` STRING STORED, `simhash` U64 FAST STORED; analizador `excalibur` con `SimpleTokenizer + RemoveLongFilter(40) + LowerCaser + AsciiFoldingFilter`; BM25 con los valores de tantivy (`k1 = 1,2`, `b = 0,75`) y boosts por consulta; presupuesto de escritor de 64 MiB; inserciones por etapas con `commit` que refresca el estado al 95 % |
| `pipeline` | Cableado de extremo a extremo con fixtures sintéticas: `seed_with_sitemap`/`seed_with_feed` encolan conjuntos acotados, `run_once` despacha vía `select_due`, comprueba el `FetchGate`, registra el `FetchOutcome`, inserta en el backend, marca `indexed` con intervalos de 15.6 y confirma; `search` replica `SearchService` (conjunción primero, disyunción como reserva, filtros antes de truncar, mezcla/diversidad, cobertura más `insufficient`); `known_item_recall` y `bytes_per_doc` sostienen el informe de 15.11 |
| `wire` | Solo superficie de rasgos hacia recursos e intermediario: `BudgetProvider` (cuotas en bytes), `FetchGate` (`AllowAllGate`/`DenyAllGate` para pruebas, forma de allowlist `HostProxyWire` para B9), `is_low_priority_source`; sin importar `browser-resources`, el intermediario ni `web_read` |
| `ranking` | Combinación (`bm25/max + 0,3·prior + 0,2·frescura`, con frescura solo bajo petición o intención de actualidad), priors por rango armónico y popularidad, máximo dos resultados por host, `source_id` derivado de `content_hash`, desempates estables por URL, fusión RRF con `k = 60`, `k ≤ 50` |
| `search` | Servicio `web_search` con resultados ordenados, cobertura (`profile`, `documents`, `last_crawl_at`, `by_source`, `budget_reached`) y bandera `insufficient` sobre BM25 bruto y recuento, filtros `site:`/dominio/idioma aplicados antes de truncar candidatos, `k = 10` por defecto, más gancho de latencia por consulta para la calibración posterior |
| `config` | Perfiles `minimo` (por defecto, índice 1,5 GiB, estado 0,5 GiB) y `ampliado`; el rastreo no arranca sin URL de contacto del operador |

El backend en disco es un `DiskIndex` explícito tras el rasgo
`IndexBackend`. El almacén tantivy de producción (feature `tantivy`)
implementa el mismo modelo de documento y flujo de consulta; cambiar de
backend solo cambia constructores, nunca los puntos de llamada de consulta
o ingesta. El refresco sigue la aproximación de la arquitectura con
intervalos guardados explícitamente; las ventanas de feeds, Hacker News y
arXiv son política de integración sobre los mismos estados de la frontera.
La columna `opstamp` queda reservada para el traspaso al escritor de
producción; el backend determinista confirma cada inserción en su propia
transacción mientras el backend tantivy las agrupa por etapas y las publica
en `commit`.

## Cableado de producción

`tantivy_index::TantivyIndex` va tras feature para que la compilación por
defecto siga ligera: `cargo test -p excalibur-search` ejercita `DiskIndex`
mientras `cargo test -p excalibur-search --features tantivy` ejercita el
mismo contrato contra tantivy (mismo modelo título/encabezados/cuerpo sin
posiciones/fragmento de 1 KiB, misma fusión de duplicados, misma admisión al
95 %, mismo orden estable). `pipeline::Pipeline<B: IndexBackend>` cablea
frontera, ingesta, índice, ranking y `web_search` con sitemaps/feeds
sintéticos locales: la siembra queda acotada por host y directorio, el
despacho respeta frescura de robots y cortesía, el refresco re-despacha
filas `indexed` vencidas con los intervalos guardados de 7 d/mitad/doble/1–
60 d, el presupuesto detiene las fuentes de baja prioridad al 95 % mientras
las de alta prioridad siguen admitidas, y la respuesta lleva `coverage`
más la bandera `insufficient` sobre BM25 bruto y recuento. `wire` deja
constancia de la superficie de integración B9 (presupuestos desde
`browser-resources`, lecturas vía allowlist del `HostProxy`) como rasgos; el
carril principal aporta las implementaciones reales sin cambiar los puntos
de llamada del pipeline.

## Informe de cobertura (15.11)

La recuperación de ítem conocido corre sobre un corpus sintético de 100
documentos con títulos distintivos de 3–5 términos en ambos backends: la
consulta son los términos distintivos del título, el éxito es la URL
esperada entre los 10 primeros, y la puerta es éxito@10 ≥ 0,9 con MRR
informado (`known_item_recall`,
`pipeline_tests::known_item_recall_over_synthetic_corpus`,
`tantivy_tests::tantivy_known_item_and_bytes_per_doc`). Los bytes reales por
documento se miden en índices de disco bajo `tempdir` mediante el tamaño del
directorio tras `commit` (`bytes_per_doc`): la prueba de `DiskIndex` acepta
50 B–20 KB/doc y la de tantivy acepta 100 B–100 KB/doc sobre el corpus
sintético de 100 documentos, ambas en torno a la estimación de 1–2,5 KB/doc
de la sección 15.3 (Lucene ≈620 B/doc solo con frecuencias, ≈5 KB/doc con
texto guardado). No hay rastreos en vivo, descargas de Tranco/Common-Crawl
ni acceso a red.

## Verificación

Ejecuta la suite backend aislada de este alcance:

```sh
cargo fmt --check
CARGO_BUILD_JOBS=2 cargo clippy -p excalibur-search --all-targets -- -D warnings
CARGO_BUILD_JOBS=2 cargo test -p excalibur-search
CARGO_BUILD_JOBS=2 cargo test -p excalibur-search --features tantivy
CARGO_BUILD_JOBS=2 cargo build -p excalibur-search
CARGO_BUILD_JOBS=2 cargo build -p excalibur-search --features tantivy
```

La suite cubre decisiones RFC 9309 (regla más larga, empates en ambos
órdenes, grupos combinados, solo grupos exactos con prefijo como reserva,
comodines con ancla final, alcance por token, mapeo de estados, topes de
`Crawl-delay`, líneas de sitemap globales y por grupo), denegación sin
robots vigentes, promoción de candidatos con migración que mantiene los
candidatos antiguos sin semilla, omisión de la cabecera prohibida con la
siguiente candidata bajo el agente configurado, cortesía con redondeo hacia
arriba y tres hosts con reloj controlado, backoff con `Retry-After` y
aparcamiento de hosts, equidad por host ante presión en cabeza, exigencia
de URL de contacto, manejo de `noindex`/`gone`, recuperación de caídas con
cierre y reapertura, refresco que se duplica a 14 días con re-despacho de
`indexed` vencidas, fusión de duplicados exactos y cercanos, ítem conocido
de 100 documentos con éxito@10 ≥ 0,9 en ambos backends,
filtros `site:`/idioma/`k` antes de truncar con conjuntos profundos, orden
repetible determinista, diversidad por host, bandera de insuficiencia,
orden por frescura, persistencia en disco, admisión por presupuesto con
prioridad de fuente, host y entidades en una pasada de sitemap, análisis
Cirrus tolerante a nulos, fusión RRF, pipeline de extremo a extremo con
sitemaps/feeds sintéticos con re-despacho de refresco y contabilidad de
denegaciones del gate, la forma de allowlist `HostProxyWire` y las cuotas
`BudgetProvider`, y medición de bytes reales por documento en índices de
disco. El lock del workspace solo registra las nuevas aristas de
dependencia tantivy de este crate (más el calificador de versión
`downcast-rs` que exige el resolvedor cuando coexisten dos versiones); no se
re-fijó ningún paquete ajeno y el manifiesto raíz está intacto.

## No implementado aquí

Los cambios entre crates hacia el intermediario y los recursos, los
rastros amplios en vivo, las fuentes Hacker News/arXiv (el recorte de 19.4
mantiene los sitemaps primero), los demonios de feeds, la aceptación
completa de B8 y la medición de cobertura en la VPS pertenecen al carril
principal. Como trabajo pendiente del carril principal quedan el lector
Cirrus en flujo con conversión a `Doc` y prior de `popularity_score` sobre
10 000 líneas, la calibración del umbral de `insufficient` sobre BM25 bruto
con el conjunto manual, una prueba de interrupción real con subproceso sobre
frontera más índice, el ajuste del tope de páginas por segundo en la VPS y
el cableado B9 tras los rasgos de `wire`. Un resultado de búsqueda sigue
siendo un candidato: solo una lectura posterior crea un evento de fuente.
