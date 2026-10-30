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
| `robots` | Analizador RFC 9309 y decisiones de rastreo: gana la regla más larga, los empates prefieren `Allow`, token `ExcaliburBot` con alternativa `*`, `/robots.txt` siempre permitido, `Crawl-delay` topado a 30 s, 4xx permite y 5xx/inaccesible deniega con reintento en una hora |
| `normalize` | Normalización de URL WHATWG: host en minúsculas, fragmento y puertos por defecto eliminados, parámetros de rastreo retirados, aproximación del dominio registrable, ruta para robots y tokens de búsqueda |
| `frontier` | Frontera SQLite (`hosts`, `urls`) con el esquema de la arquitectura y los estados `queued`, `fetching`, `pending_commit`, `indexed`, `skipped`, `failed`, `gone`; una petición en vuelo por host, tope global, cortesía de `clamp(2 × duración, 1 s, 5 s)` más jitter determinista, respeto de `Retry-After`, aparcamiento tras cinco errores seguidos, exigencia de `contact_url` y recuperación de caídas que reencola filas `fetching` y `pending_commit` |
| `dedup` | Duplicados exactos por `sha256` del Markdown normalizado; casi duplicados por SimHash de 64 bits sobre tejas de 3 palabras con distancia de Hamming ≤ 3 dentro del mismo host |
| `ingest` | Sitemap acotado (`<loc>`, 50 000 URL, 50 MiB), feeds (RSS `<link>`, Atom `href`, JSON Feed `url`) y análisis de pares de líneas CirrusSearch con filtro por umbral de `incoming_links` |
| `index` | Almacén determinista de documentos en disco con el modelo de documento de la arquitectura, BM25 (`k1 = 1,2`, `b = 0,75`, título ×3, encabezados ×1,5, tokens de URL ×2, cuerpo ×1), fragmentos guardados de 1 KiB, actualización por borrado con la URL, commits atómicos y bandera de admisión al 95 % del presupuesto |
| `ranking` | Combinación (`bm25/max + 0,3·prior + 0,2·frescura`, con frescura solo bajo petición o intención de actualidad), priors por rango armónico y popularidad, máximo dos resultados por host, fusión RRF con `k = 60`, `k ≤ 50` |
| `search` | Servicio `web_search` con resultados ordenados, cobertura (`profile`, `documents`, `last_crawl_at`, `by_source`, `budget_reached`) y bandera `insufficient`, más gancho de latencia por consulta para la calibración posterior |
| `config` | Perfiles `minimo` (por defecto, índice 1,5 GiB, estado 0,5 GiB) y `ampliado`; el rastreo no arranca sin URL de contacto del operador |

El backend en disco es un `DiskIndex` explícito tras fronteras de métodos
simples. El almacén tantivy de producción se conectará al mismo modelo de
documento y flujo de consulta más adelante sin cambiar los puntos de
llamada. El refresco sigue la aproximación de la arquitectura: la próxima
visita parte de 7 días, se divide a la mitad si el contenido cambió y se
duplica si no, entre 1 y 60 días; las ventanas de feeds, Hacker News y arXiv
son política de integración sobre los mismos estados de la frontera.

## Verificación

Ejecuta la suite backend aislada de este alcance:

```sh
cargo fmt --check
CARGO_BUILD_JOBS=2 cargo clippy -p excalibur-search --all-targets -- -D warnings
CARGO_BUILD_JOBS=2 cargo test -p excalibur-search
CARGO_BUILD_JOBS=2 cargo build -p excalibur-search
```

La suite cubre decisiones RFC 9309 (regla más larga, empates, comodines con
ancla final, alcance por token, mapeo de estados, topes de `Crawl-delay`,
líneas de sitemap), cortesía con reloj controlado, backoff con
`Retry-After` y aparcamiento de hosts, exigencia de URL de contacto,
manejo de `noindex`/`gone`, recuperación de caídas con cierre y
reapertura reales, fusión de duplicados exactos y cercanos, ítem conocido
con 100 documentos y éxito@10 ≥ 0,9, filtros `site:`/idioma/`k`,
diversidad por host, bandera de insuficiencia, orden por frescura,
persistencia en disco, admisión por presupuesto y fusión RRF. El lock del
workspace solo registra las nuevas aristas de dependencia de este crate; no
se añadió ni se cambió ningún paquete y el manifiesto raíz está intacto.

## No implementado aquí

El cableado tantivy de producción, la integración con el intermediario y
los recursos, los rastreos amplios en vivo, los demonios de feeds, la
aceptación completa de B8 y la medición de cobertura en la VPS pertenecen
al carril principal. Un resultado de búsqueda sigue siendo un candidato:
solo una lectura posterior crea un evento de fuente.
