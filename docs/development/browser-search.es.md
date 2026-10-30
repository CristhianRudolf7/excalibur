# Fundamento del buscador del navegador

[English](browser-search.md)

`excalibur-search` implementa los fundamentos independientes del hito de
búsqueda del documento externo `arquitectura-navegador.md` (secciones
15.4–15.5 y 15.7–15.9). El crate es autocontenido: solo depende de
`rusqlite`, `url`, `serde`, `serde_json`, `sha2` y `thiserror`, ya fijados
en el lock del workspace, y no toca el ciclo de vida del navegador, el
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
| `ranking` | Combinación (`bm25/max + 0,3·prior + 0,2·frescura`, con frescura solo bajo petición o intención de actualidad), priors por rango armónico y popularidad, máximo dos resultados por host, `source_id` derivado de `content_hash`, desempates estables por URL, fusión RRF con `k = 60`, `k ≤ 50` |
| `search` | Servicio `web_search` con resultados ordenados, cobertura (`profile`, `documents`, `last_crawl_at`, `by_source`, `budget_reached`) y bandera `insufficient` sobre BM25 bruto y recuento, filtros `site:`/dominio/idioma aplicados antes de truncar candidatos, `k = 10` por defecto, más gancho de latencia por consulta para la calibración posterior |
| `config` | Perfiles `minimo` (por defecto, índice 1,5 GiB, estado 0,5 GiB) y `ampliado`; el rastreo no arranca sin URL de contacto del operador |

El backend en disco es un `DiskIndex` explícito tras fronteras de métodos
simples. El almacén tantivy de producción se conectará al mismo modelo de
documento y flujo de consulta más adelante sin cambiar los puntos de
llamada. El refresco sigue la aproximación de la arquitectura con intervalos
guardados explícitamente; las ventanas de feeds, Hacker News y arXiv son
política de integración sobre los mismos estados de la frontera. La columna
`opstamp` queda reservada para el traspaso al escritor de producción; en
este backend cada inserción confirma en su propia transacción.

## Verificación

Ejecuta la suite backend aislada de este alcance:

```sh
cargo fmt --check
CARGO_BUILD_JOBS=2 cargo clippy -p excalibur-search --all-targets -- -D warnings
CARGO_BUILD_JOBS=2 cargo test -p excalibur-search
CARGO_BUILD_JOBS=2 cargo build -p excalibur-search
```

La suite cubre decisiones RFC 9309 (regla más larga, empates en ambos
órdenes, grupos combinados, solo grupos exactos con prefijo como
reserva, comodines con ancla final, alcance por token, mapeo de estados,
topes de `Crawl-delay`, líneas de sitemap globales y por grupo),
denegación sin robots vigentes, promoción de candidatos con migración que
mantiene los candidatos antiguos sin semilla, omisión de la cabecera
prohibida con la siguiente candidata bajo el agente configurado, cortesía con redondeo hacia arriba y tres hosts
con reloj controlado, backoff con `Retry-After` y aparcamiento de hosts,
equidad por host ante presión en cabeza, exigencia de URL de contacto,
manejo de `noindex`/`gone`, recuperación de caídas con cierre y
reapertura, refresco que se duplica a 14 días con re-despacho de
`indexed` vencidas, fusión de duplicados exactos y cercanos, ítem conocido
con 100 documentos y éxito@10 ≥ 0,9, filtros `site:`/idioma/`k` antes de
truncar con conjuntos profundos, orden repetible determinista, diversidad
por host, bandera de insuficiencia, orden por frescura, persistencia en
disco, admisión por presupuesto con prioridad de fuente, host y entidades en una pasada
de sitemap, análisis Cirrus tolerante a nulos y fusión RRF. El lock del
workspace solo registra las nuevas aristas de dependencia de este crate; no
se añadió ni se cambió ningún paquete y el manifiesto raíz está intacto.

## No implementado aquí

El cableado tantivy de producción, la integración con el intermediario y
los recursos, los rastreos amplios en vivo, los demonios de feeds, la
aceptación completa de B8 y la medición de cobertura en la VPS pertenecen
al carril principal. Como trabajo pendiente del carril principal quedan el
lector Cirrus en flujo con conversión a `Doc` y prior de
`popularity_score` sobre 10 000 líneas, la calibración del umbral de
`insufficient` sobre BM25 bruto con el conjunto manual, una prueba de
interrupción real con subproceso sobre frontera más índice y el ajuste del
tope de páginas por segundo en la VPS. Un resultado de búsqueda sigue
siendo un candidato: solo una lectura posterior crea un evento de fuente.
