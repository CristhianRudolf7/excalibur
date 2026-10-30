# Auditoría B7 — Lectura ligera y citas (meta)

- **Commit auditado:** `e5e4d90b4664ed0cb6941f7ce18d07b5b719a251` (merge de `db74aae` sobre `b9c0c23`)
- **Fecha de auditoría:** 2026-10-30 (UTC)
- **Alcance:** B7 (arch §19.2 B7; §14.2 `web_read` incl. reglas 1–7, heurística HTTP-first, render de respaldo, extracción, fragmentos, caché; §14.3 `cite`; cierre §14.4; NB-19/NB-23)
- **Rol:** auditor independiente. Sin modificaciones a producto, arquitectura ni Git; árbol limpio en `e5e4d90` tras la auditoría.
- **Veredicto:** `changes_required` (bloqueante de calidad: F-B7-01; más F-B7-03 bloqueante de alcance/seguridad pendiente de integración)

## Base documental y referencias

- `AGENTS.md` (línea B7: comandos `read_cite`, `browser --lib`, `webd --lib read`; declara pendiente broker/extractores/RPC).
- `/home/cristhian/projects/Excalibur/browser/arquitectura-navegador.md` §§14.2 (algoritmo pasos 1–10), 14.3, 14.4 (tabla de 10 filas), 19.2 (fila B7), NB-19/NB-23, y 12.3 (broker).
- Informe previo relevante: `b4-reaudit2-meta.md` (patrón de aprobación por hito con pendientes fuera de alcance; aquí se aplica igual salvo los hallazgos bloqueantes).
- Frontend: no procede (sin visor ni herramientas registradas; el propio código y la documentación lo declaran).

## Alcance entregado por el commit (10 ficheros, +1769, solo adiciones)

- `crates/browser/src/{extract,read,cite}.rs` + registro en `lib.rs`: backend puro sin Chrome ni red propia salvo `fetch_http`.
- `crates/webd/src/read.rs` + registro en `lib.rs`: `Service` con caché TTL y almacén por hash para fragmentos y citas. Sin herramienta de agente.
- `crates/browser/tests/read_cite.rs`: 5 tests de integración con fixtures sintéticas locales (HTTP solo en loopback).
- Docs pareadas `docs/development/browser.md` / `browser.es.md` (sección «Light reading and citations») + línea B7 en `AGENTS.md` (inglés solo, según regla del repo).

## Mapeo de requisitos y evidencia

| # | Requisito | Estado | Evidencia |
|---|---|---|---|
| R1 | NB-19, §14.2-1: validación URL (solo http/https, sin userinfo, ≤2048, fragmento fuera de identidad de caché) | verificado | `read::normalize_url` + test `rejects_userinfo_and_scheme`; `read_cite::http_first_with_cache_hits` en loopback |
| R2 | §14.2-2: caché 24 h + `fresh` con fecha original | verificado | `ReadCache` TTL; `webd::read::caches_with_original_date`; test de expiración TTL en `read_cite` |
| R3 | §14.2-3: timeouts 10 s/30 s, ≤5 redirecciones, rechazo de degradación https→http, tope 10 MiB, allowlist de tipos | verificado (implementación; matriz adversaria pendiente, ver F-B7-03) | `fetch_http`; `http_first_with_cache_hits` (200 + `unsupported_type` por construcción); `supported_type` |
| R4 | §14.2-4: heurística HTTP-first + evitación por datos incrustados | verificado | `needs_browser_render` + `has_embedded_data`; tests `spa_shell_needs_browser`, `embedded_data_avoids_browser`, `decide_render` en `wait_before_capture_with_spa_fallback` |
| R5 | Regla 7.4-1: captura solo con `wait` vigente del epoch; degradado no estricto vs `timeout` estricto | verificado | `WaitGate::check`; tests `wait_before_capture_with_spa_fallback`, `long_poll_degraded_vs_strict`, unitarios `wait_gate_refuses_without_ready`, `strict_long_poll_times_out` |
| R6 | §14.2-6/8: extracción (título, Markdown, fragmentos `pN` con offsets UTF-8 y ruta de encabezados; sin nav/footer/ads) | verificado | `extract_document`; `static_article_fragments_and_citations` (offsets exactos, ausencia de nav/footer) |
| R7 | §14.2-8: `slice_fragments` por `pN` y `pA-pB`, errores `unknown_fragment`/`out_of_range` | verificado | `fragment_slicing_and_inline_limit` (simple, rango, `p999`) |
| R8 | §14.2-9: inline ≤50 KiB con `truncated`, `full_len`, blob completo direccionable | **no verificado (ASCII sí; no-ASCII falla)** | ASCII verificado en `fragment_slicing_and_inline_limit`; ver F-B7-01 |
| R9 | §14.2-9: `content_hash` (`sha256:`), `source_id` (`w_<16hex>`), `untrusted: true`, metadatos (`render`, extractor, estado HTTP) | verificado | `cite::content_hash/source_id`, `build_read`; aserciones en tests de artículo y SPA |
| R10 | §14.3: `locate` exacta/normalizada/ausente, `#:~:text=`, `format_citation` sin páginas, `Service::{read_fragments,cite}` por hash | verificado (con aproximación documentada en F-B7-04) | tests de citas en `read_cite` + unitarios `exact_quote_wins`, `normalized_quote_matches`, `missing_quote_is_none` |
| R11 | Docs EN/ES pareadas, comandos en AGENTS.md, mensaje en lenguaje natural, disciplina de alcance (sin herramientas ni visor) | verificado | diff del commit: solo ficheros B7; mensaje «Integrate light reading and citation backend with wait-gated capture»; sin `tool`/`serve`/`RPC` nuevo |
| R12 | Cierre §14.4 (matriz de 10 filas) | **no cerrado** | Cubiertas por tests: artículo, SPA→browser, `__NEXT_DATA__`→http, caché 24 h, 300 KB/50 KiB, citas. Pendientes honestos en docs: broker, `dom_smoothie`/`htmd`, PDF/feed/charset, `robots.txt`→índice, render efímero real, RPC; ver F-B7-03 |

## Comprobaciones ejecutadas (blanco pesado con `flock …/heavy-build.lock`, `CARGO_BUILD_JOBS=2`, target aislado `/tmp/b7-meta-target`, sin tocar artefactos ajenos)

- `cargo fmt --check` → salida 0, limpio.
- `cargo clippy -p excalibur-browser -p excalibur-webd --all-targets -- -D warnings` → salida 0, sin avisos.
- `cargo test -p excalibur-browser --test read_cite -- --nocapture` → 5 passed, 0 failed.
- `cargo test -p excalibur-browser --lib` → 31 passed, 0 failed.
- `cargo test -p excalibur-webd --lib read` → 3 passed, 0 failed (2 de `read::tests` + 1 coincidente del filtro).
- `cargo build --workspace` → salida 0 (≈4 min).
- Fixtures locales y sintéticas; sin cuentas, sin APIs de pago, sin Chrome (el render de respaldo se inyecta como HTML ya esperado, según diseño declarado).

## Hallazgos

- **F-B7-01 · media · bloqueante · abierta**: `build_read` trunca con índice de bytes (`markdown[..INLINE_LIMIT]`, `crates/browser/src/read.rs:197`); panics si el byte 51200 cae dentro de un carácter multibyte (documentos no-ASCII >50 KiB, p. ej. español con tildes). Semántica de `str` de Rust: índice fuera de frontera = pánico. **Recomendación:** truncar en `floor_char_boundary(INLINE_LIMIT)` y test con fixture no-ASCII >50 KiB.
- **F-B7-02 · media · no bloquea · abierta**: `fetch_http` (`read.rs:411-413`) lee el cuerpo entero con `response.bytes()` y comprueba el tope de 10 MiB después; el paso 3 de §14.2 exige lectura por trozos con tope. El límite se cumple como resultado (`body_too_large`), pero un servidor hostil puede forzar picos de memoria antes del rechazo. **Recomendación:** consumir por chunks con contador y abortar al superar 10 MiB; test con cuerpo de 50 MiB (fila §14.4).
- **F-B7-03 · alta · bloqueante · abierta**: la ruta de descarga no pasa por el broker (§12.3) y no rechaza bucles/redirect a loopback ni aplica la política de `robots.txt`/cookies de §14.2-10/§14.4; el commit lo declara pendiente («Broker fetch wiring … stay pending»). Sin esto no puede cerrarse la fila §14.4 (6 redirecciones, bajada https→http ya cubierta parcial, redirección a 127.0.0.1, `robots.txt`, perfil persistente). **Recomendación:** cablear `fetch_http` vía broker con guarda de loopback + matriz §14.4 con Chrome/fixture local antes de declarar B7 cerrado.
- **F-B7-04 · baja · no bloquea · abierta**: en coincidencias normalizadas, `cite::locate` (`cite.rs:75-77`) devuelve offsets aproximados (heurística del primer token largo) presentados como offsets exactos, y el fragmento puede no ser el que cubre la cita. Los tests solo afirman existencia en ese caso. **Recomendación:** mapear la coincidencia normalizada a offsets reales o marcarla aproximada; aserción exacta en tests.

## Pendientes verificados como honestos (no re-auditados aquí)

La documentación declara pendientes: cableado del broker, extractores de producción `dom_smoothie`/`htmd`, PDF/feed/charset, `robots.txt`→índice, render efímero real con Chrome, registro de método RPC. Coherente con AGENTS.md. Ninguno se presenta como implementado.

## Resumen y entrega

B7 entrega el núcleo determinista de `web_read`/`cite` (validación, caché 24 h, heurística HTTP-first con evitación por datos incrustados, captura con puerta de espera regla 7.4-1, extracción propia con fragmentos y offsets, corte inline 50 KiB, hashes e IDs, citas verificables) con 39 aserciones de backend en verde, fmt/clippy/build limpios y documentación bilingüe honesta sobre sus límites. **No se aprueba**: F-B7-01 (pánico con no-ASCII >50 KiB) debe corregirse en el propio hito, y F-B7-03 (descarga sin broker: sin rechazo de loopback/SSRF ni matriz §14.4 completa) impide declarar cerrado B7/§14.4. Trabajo futuro fuera de este alcance (solo resumen, no verificado aquí): integración del broker y guardas de red, extractores de producción, PDF/feed/charset, `robots.txt` e ingesta a índice, render efímero real y RPC — más el resto de la arquitectura completa (B0–B6, B8 con buscador propio, B9 con recursos integrados y todos los NB), que esta auditoría no declara ni evalúa.

- Commit auditado: `e5e4d90b4664ed0cb6941f7ce18d07b5b719a251` (sin modificar).
- Informe: este archivo.
