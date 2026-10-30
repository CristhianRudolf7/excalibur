# Auditoría B8 — Cableado de búsqueda con backend tantivy y superficie broker (meta)

- **Commit auditado:** `a85bded92d345ffbaeb22c68f78bb949eb5be3bf` (merge de `a60a1a6` sobre `e5e4d90`)
- **Fecha de auditoría:** 2026-10-30 (UTC)
- **Alcance:** Solo commit `a85bded`; B8 search wiring (arch §19.2 B8; §§15.4–15.9; tantivy 15.5 production backend tras feature flag; frontier/ingest/refresh 15.6; ranking 15.7; `web_search` 15.9; cobertura 15.11 known-item success@10>=0.9; NB-20/21/22)
- **Rol:** auditor independiente. Sin modificaciones a producto, arquitectura ni Git; árbol limpio en `a85bded` tras la auditoría.
- **Veredicto:** `approved` para el alcance B8 wiring (9/9 requisitos verificados, sin hallazgos bloqueantes; BSF-12/13/14 main-lane pendientes según lo documentado; un hallazgo nuevo bajo no bloqueante B8-01)

## Base documental y referencias

- `AGENTS.md` (sin línea B8: el commit no lo toca; la integración la hace el coordinador en B9 según arch §19.2).
- `/home/cristhian/projects/Excalibur/browser/arquitectura-navegador.md` §§15.4–15.9, 15.10–15.11, 19.2 (fila B8), NB-20/NB-21/NB-22.
- Informes previos relevantes: `b7-meta.md` (patrón de la meta) y `recomendaciones-browser-search-foundation.md` (BSF-01 a BSF-16; correcciones 1–2 ya integradas en la base `ff84849` de este checkout: `seeded`, `matching_groups` exacto/prefijo, `refresh_interval_secs`, admisión al 95%, desempate por URL).
- Frontend: no procede (sin visor ni herramientas registradas; el propio código y la documentación lo declaran: solo traits `BudgetProvider`/`FetchGate`, sin imports de broker ni recursos).

## Alcance entregado por el commit (12 ficheros, +2641/−66)

- `crates/search/src/{backend,pipeline,tantivy_index,wire}.rs` + registro en `lib.rs` y feature opcional `tantivy` en `crates/search/Cargo.toml`.
- `crates/search/tests/{pipeline_tests,tantivy_tests,wire_tests}.rs` (7+4+3 tests nuevos).
- Docs pareadas `docs/development/browser-search.md` / `browser-search.es.md` (verificación con los 6 comandos, tabla de módulos, cobertura 15.11 y pendientes del carril principal).
- `Cargo.lock` (+505 líneas: solo aristas tantivy y el calificador `downcast-rs` que exige el resolver; sin re-pins ajenos; manifiesto raíz intacto).
- Fuera de alcance intacto: sin cambios en `browser`, `webd`, broker, harness, herramientas de agente, `AGENTS.md` o el doc de arquitectura.

## Requisitos y evidencia

| # | Requisito | Estado | Evidencia |
|---|---|---|---|
| R1 | NB-20 (15.4/15.6): `pipeline` siembra por sitemap/feed acotados, despacha por `select_due`, respeta robots/cortesía y re-despacha `indexed` vencidas con intervalos 7 d/mitad/doble/1–60 d | verificado | `pipeline_tests::{sitemap_to_search_end_to_end,feed_ingestion_enqueues_bounded_links,refresh_redispatches_indexed_after_interval,fetch_gate_deny_leaves_budget_untouched}` (7 ok) |
| R2 | NB-21 (15.5): `TantivyIndex` tras feature `tantivy` con el mismo modelo (url update-key, filtros STORED, title con posiciones, headings/body solo freqs, snippet 1 KiB, I64/F64 FAST, simhash U64), analizador `SimpleTokenizer+RemoveLongFilter(40)+LowerCaser+AsciiFoldingFilter`, BM25 k1=1.2/b=0.75 con boosts y writer 64 MiB | verificado | `tantivy_tests::tantivy_matches_disk_document_model`; clippy/build con y sin feature en verde |
| R3 | Paridad del contrato `IndexBackend`: dedup exacta/near, admisión 95% por prioridad, filtros duros antes de truncar, desempate estable por URL | verificado | `tantivy_dedup_and_budget_match_disk`, `tantivy_end_to_end_search_with_insufficient`; sondas Q1 (gate 95% real) y Q2 (30 consultas → 1 ordenación) en `/tmp/b8-probe-final.txt` |
| R4 | NB-22 (15.7): `pipeline.search` replica `SearchService` (conjuntivo primero, disyuntivo bajo 10, filtros antes de truncar, blending/diversidad vía `ranking`, `k` 0→10 y clamp 1–50) | verificado | inspección `pipeline.rs` vs `search.rs` (misma forma); `search_tests` + pipeline e2e en verde |
| R5 | 15.9 `web_search`: `{results, coverage{profile,documents,last_crawl_at,by_source,budget_reached}, insufficient}` sobre ambos backends | verificado | `tantivy_end_to_search_with_insufficient` (hit + miss con `insufficient`), `budget_and_insufficient_signalling`, `sitemap_to_search_end_to_end` |
| R6 | 15.10/15.11: known-item success@10 ≥ 0.9 en ambos backends + bytes/doc reales medidos en índices file-backed | verificado | `known_item_recall_over_synthetic_corpus` (disk, mrr>0.5) y `tantivy_known_item_and_bytes_per_doc` (success@10≥0.9; 50 B–20 KB/doc disk, 100 B–100 KB/doc tantivy) |
| R7 | Superficie broker/recursos solo como traits (`BudgetProvider`, `FetchGate`, `HostProxyWire` fail-closed, `is_low_priority_source` idéntico en `DiskIndex`/`TantivyIndex`/`wire`); B9 aporta la implementación sin cambiar call sites | verificado | `wire_tests` (3 ok: presupuestos de perfil, prioridades, gates fail-closed); diff sin cross-crate edits; docs lo declaran pendiente B9 |
| R8 | Docs EN/ES pareadas con comandos de verificación, mapa de módulos fiel y pendientes honestos; `Cargo.lock` solo con aristas tantivy | verificado | diff pareado ±equivalente EN/ES (verificación, cobertura, "Not implemented here"); `Cargo.lock` sin re-pins (grep `^[+-]name` solo paquetes tantivy-dependientes) |
| R9 | fmt/clippy/tests/build del alcance, con y sin feature `tantivy` | verificado | `FMT_EXIT=0`, `CLIPPY_EXIT=0`, `CLIPPY_TANTIVY_EXIT=0`, `TEST_EXIT=0` (75 ok), `TEST_TANTIVY_EXIT=0` (79 ok), `BUILD_EXIT=0`, `BUILD_TANTIVY_EXIT=0` en `/tmp/b8-audit-log2.txt` |

## Comprobaciones ejecutadas (blanco pesado con `flock …/heavy-build.lock`, `CARGO_BUILD_JOBS=2`, target aislado `/tmp/b8-audit-target`, sin tocar artefactos ajenos)

- `cargo fmt --check` → salida 0, limpio.
- `cargo clippy -p excalibur-search --all-targets -- -D warnings` → salida 0, sin avisos.
- `cargo clippy -p excalibur-search --all-targets --features tantivy -- -D warnings` → salida 0, sin avisos (cubre el código tras `cfg` que el clippy por defecto no compila).
- `cargo test -p excalibur-search` → 75 passed, 0 failed (dedup 5, frontier 18, index 12, ingest 7, pipeline 7, robots 16, search 7, wire 3).
- `cargo test -p excalibur-search --features tantivy` → 79 passed, 0 failed (75 + tantivy 4).
- `cargo build -p excalibur-search` y `--features tantivy` → salida 0.
- Sondas externas en crate temporal `/tmp/b8probe` (dependencia por ruta, fuera del repositorio, `cargo run --release --offline`, salida en `/tmp/b8-probe-final.txt`, EXIT=0):
  - Q1: índice tantivy file-backed con presupuesto 8192 B → 2 docs `Web` admitidos, el siguiente devuelve `BudgetReached` y un doc `Wiki` posterior se admite (`web_admitted=2 budget_hit=true wiki_after_gate=true`).
  - Q2: 40 docs empatados, 30 consultas idénticas con `k=5` → 1 ordenación (`distinct_orderings=1`).
  - Q3: 700 docs fuertes en otros hosts + 1 débil en `target.example`, `search_bm25` con filtro de dominio y `limit=10` → tantivy no lo devuelve (`found_target=false`), `DiskIndex` sí (`found_target=true`); ver B8-01.

## Hallazgos

- **BSF-12 · baja · no bloquea · abierta**: la parte del carril principal sigue pendiente según lo documentado (lector Cirrus en flujo con conversión a `Doc` y prior de `popularity_score` sobre fixture de 10 000 líneas). La parte de este alcance (parseo nul-tolerante de `opening_text`) está verificada por `ingest_tests`. **Recomendación:** mantener como pendiente del carril principal; no bloquea el wiring.
- **BSF-13 · baja · no bloquea · abierta**: la calibración del umbral `insufficient` (BM25 bruto 0.1) contra el conjunto manual de consultas sigue pendiente según lo documentado. Los valores por defecto (`k=10`) están verificados (`SearchOptions::default`, pipeline y servicio). **Recomendación:** calibrar en el carril principal con el query set manual; no bloquea el wiring.
- **BSF-14 · baja · no bloquea · abierta**: la prueba de interrupción real con subproceso sobre frontera + índice sigue pendiente según lo documentado. La recuperación close/reopen y la reserva de `opstamp` están verificadas en este alcance. **Recomendación:** añadirla en el carril principal; no bloquea el wiring.
- **B8-01 · baja · no bloquea · abierta**: el backend tantivy aplica los filtros duros tras una pre-captura acotada (`fetch_limit = (limit*10).clamp(500, 5000)` en `tantivy_index.rs`, además de `TopDocs(100_000)` en `last_crawl_at` y `TopDocs(5000)` en `host_simhashes`), mientras `DiskIndex::search_bm25` puntúa todo el conjunto y filtra antes de truncar sin tope. Evidencia Q3: con 700 docs más fuertes fuera del dominio, el doc válido de `target.example` cae fuera de la ventana y se pierde en tantivy pero no en disco. Vía `pipeline.search` (`limit=200`) la ventana efectiva es 2000 candidatos; a escala sintética el comportamiento coincide (todos los tests en verde). **Recomendación:** documentar la ventana de pre-captura en `browser-search{,.es}.md` y calibrarla en el carril principal con la medición VPS a 1 M de docs (p95 < 150 ms); no bloquea este alcance.

## Pendientes verificados como honestos (no re-auditados aquí)

La documentación declara pendientes del carril principal: lector Cirrus en flujo con priors, calibración del umbral `insufficient`, interrupción real con subproceso, ajuste del tope de páginas/s en la VPS, fuentes Hacker News/arXiv, rastreos amplios, demonios de feeds, medición de cobertura en VPS y cableado broker/recursos B9 tras los traits `wire`. Coherente con el diff (sin cross-crate edits) y con `AGENTS.md` (sin línea B8). Ninguno se presenta como implementado. Un resultado de búsqueda sigue siendo un candidato: solo una lectura posterior crea un evento de fuente.

## Resumen y entrega

B8 wiring entrega el backend tantivy 15.5 tras feature flag con el mismo modelo de documento y contrato que `DiskIndex`, el pipeline frontier→ingest→índice→ranking→`web_search` con refresco 15.6 y señalización de presupuesto/cobertura/`insufficient`, y la superficie de traits para el carril B9, con 79 aserciones de backend en verde en la variante tantivy (75 en la base), fmt/clippy/build limpios en ambas variantes, sondas propias de paridad (gate 95%, determinismo, filtros) y documentación bilingüe honesta sobre sus límites. **Se aprueba el alcance B8 wiring**: 9/9 requisitos verificados con evidencia real y sin hallazgos bloqueantes. Trabajo futuro fuera de este alcance (solo resumen, no verificado aquí): los pendientes del carril principal citados arriba — más el resto de la arquitectura completa (B0–B7, B9 con recursos integrados y todos los NB), que esta auditoría no declara ni evalúa.

- Commit auditado: `a85bded92d345ffbaeb22c68f78bb949eb5be3bf` (sin modificar).
- Informe: este archivo.
