# Auditoría B7 re-audit — Lectura ligera y citas, corrección 1 (meta)

- **Commit auditado:** `bd8676c0c89bab7dbb2bef2e74d5fccdb6ea0c62` (commit de documentación EN/ES sobre `3a0c689` merge de `6527c1f` fix sobre `a85bded`)
- **Fecha de auditoría:** 2026-10-30 (UTC)
- **Alcance:** B7 (arch §19.2 B7; §14.2 `web_read` reglas 1–7, heurística HTTP-first, captura con puerta de espera, extracción, fragmentos, caché; §14.3 `cite`; cierre filas navegador de §14.4; NB-19/NB-23)
- **Rol:** auditor independiente. Sin modificaciones a producto, arquitectura ni Git; árbol limpio en `bd8676c` tras la auditoría. Sin subagentes, sin puentes.
- **Veredicto:** `approved` (los cuatro hallazgos previos quedan cerrados con evidencia; única nota abierta de severidad `info`, no bloqueante, sin pendientes necesarios)

## Base documental y referencias

- `AGENTS.md` (línea B7: comandos `read_cite`, `browser --lib`, `webd --lib read`; declara pendiente broker/extractores/RPC).
- `/home/cristhian/projects/Excalibur/browser/arquitectura-navegador.md` §§14.2 (algoritmo pasos 1–10), 14.3 (citas), 14.4 (matriz de 10 filas), 19.2 (fila B7), NB-19/NB-23, 12.3 (broker).
- Informe previo relevante: `/home/cristhian/.local/state/excalibur-browser-owner/reports/b7-meta.md` (auditoría de `e5e4d90`, origen de F-B7-01…F-B7-04, conservados aquí).
- Frontend: no procede (sin visor ni herramientas registradas; `webd::read::Service` es borde de biblioteca, sin `serve`/RPC nuevo en el diff).

## Alcance entregado por la corrección (6527c1f código + bd8676c docs)

- `crates/browser/src/read.rs`: `floor_char_boundary`, `FetchPolicy::{secure,test_loopback}`, `host_block_error`/`ip_block_error`/`check_fetch_url`/`host_is_blocked`, `is_downgrade`, redirecciones manuales con revalidación por salto (máx. 5, `too_many_redirects`), `read_capped_body` por trozos con contador, `robots_allows`/`should_enqueue_index`, `cookie_isolation_active`.
- `crates/browser/src/cite.rs`: `find_normalized_span` (ventana de tokens → tramo exacto de bytes, alineado a frontera de carácter).
- `crates/browser/src/extract.rs`: `robots_meta_directives`/`has_robots_noindex` (gancho meta-noindex).
- `crates/browser/tests/read_cite.rs`: 13 tests (5 previos + 8 nuevos de la corrección).
- `crates/webd/src/read.rs`: `should_enqueue_index(html, robots_allow, policy)` (gancho índice honesto sin tocar la ruta serve).
- Docs pareadas `docs/development/browser.md` / `browser.es.md` actualizadas en `bd8676c` (inglés+español, solo B7).

## Mapeo de requisitos y evidencia

| # | Requisito | Estado | Evidencia |
|---|---|---|---|
| R1 | NB-19, §14.2-1: validación URL (solo http/https, sin userinfo, ≤2048, fragmento fuera de identidad) | verificado | `read::normalize_url` + `rejects_userinfo_and_scheme`; `http_first_with_cache_hits` en loopback |
| R2 | §14.2-2: caché 24 h + `fresh` con fecha original | verificado | `ReadCache` TTL; `caches_with_original_date`; expiración TTL en `read_cite` |
| R3 | §14.2-3: timeouts 10 s/30 s, ≤5 redirecciones, rechazo bajada https→http, tope 10 MiB, allowlist de tipos | verificado (implementación; dos rutas sin fixture dedicada, ver nota N-01) | `fetch_http_with_policy`; `large_body…` (50 MiB→`body_too_large`); `is_downgrade` unit; `secure_default_refuses_loopback_fixture`; `redirect_to_private…` |
| R4 | §14.2-4: heurística HTTP-first + evitación por datos incrustados | verificado | `needs_browser_render` + `has_embedded_data`; `spa_shell_needs_browser`, `embedded_data_avoids_browser` |
| R5 | Regla 7.4-1: captura con puerta de espera (ready/época; degradación vs timeout estricto) | verificado | `WaitGate::check`; `wait_gate_refuses_without_ready`, `strict_long_poll_times_out`, `long_poll_degraded_vs_strict`, `wait_before_capture_with_spa_fallback` |
| R6 | §14.2-6/7/8: extracción propia con fragmentos `pN`, offsets UTF-8, rutas de encabezado | verificado | `extract_document`; `extracts_title_and_fragments` (offset↔texto exactos); `static_article_fragments_and_citations` |
| R7 | §14.2-9: inline ≤50 KiB con `truncated`, blob completo, `slice_fragments` por hash | verificado | `floor_char_boundary` + `build_read`; `fragment_slicing_and_inline_limit`, `inline_limit_floors_char_boundary_for_non_ascii` |
| R8 | §14.3: `cite` exacta+normalizada con offsets exactos, `content_hash`, `#:~:text:`, sin páginas inventadas | verificado | `find_normalized_span`; `normalized_cite_returns_exact_covering_span` (normalizar(tramo)==cita), `exact_quote_wins`, `normalized_quote_matches`, `missing_quote_is_none` |
| R9 | Filas navegador de §14.4: loopback/privadas/metadatos, redirección a privada, bajada, robots permitir/denegar, meta-noindex, no reenvío de cookies | verificado | `loopback_private_and_metadata_hosts_are_refused` (incl. `2130706433` y `0x…`), `redirect_to_private_ip_is_refused_before_fetch`, `secure_default_refuses_loopback_fixture` (contador 0), `cookies_are_never_stored_or_replayed`, `robots_and_index_hooks`; reqwest sin feature `cookies` (sin jar por construcción) |
| R10 | NB-23, `webd::read::Service`: caché TTL, páginas por hash para fragmentos/citas, sin herramienta de agente | verificado | `caches_with_original_date`, `spa_needs_browser_without_render`; diff sin `serve`/RPC/tool nuevo |
| R11 | Docs EN/ES pareadas, mensaje en lenguaje natural, disciplina de alcance | verificado | `bd8676c` solo toca `docs/development/browser{,.es}.md`; sin ficheros fuera de B7 |
| R12 | Cierre §14.4 lado navegador honesto; resto declarado pendiente (B9/out-of-scope) | verificado | docs `bd8676c`: pendientes explícitos (egreso DNS/broker, `robots.txt` retrieval/caché, encolado real, `dom_smoothie`/`htmd`, PDF/feed/charset, render efímero real, RPC) |

## Comprobaciones ejecutadas

- `cargo fmt --check` → salida 0, limpio.
- `cargo clippy -p excalibur-browser -p excalibur-webd --all-targets -- -D warnings` → salida 0, sin avisos (compilación pesada inicial bajo `flock …/heavy-build.lock` con `CARGO_BUILD_JOBS=2`; recheck incremental sobre artefactos en caché confirma limpio).
- `cargo test -p excalibur-browser --test read_cite -- --nocapture` → 13 passed, 0 failed.
- `cargo test -p excalibur-browser --lib` → 31 passed, 0 failed (incl. los 9 unitarios B7: `read::tests::*`, `cite::tests::*`, `extract::tests::*`).
- `cargo test -p excalibur-webd --lib read` → 3 passed, 0 failed (2 de `read::tests` + 1 coincidente del filtro).
- `cargo build -p excalibur-browser -p excalibur-webd` → salida 0.
- Aislamiento: `CARGO_TARGET_DIR=/tmp/b7-reaudit-target` (sin tocar artefactos ajenos); compilación pesada inicial bajo el lock global; re-ejecuciones rápidas sobre caché durante la ventana idle de otro test con target propio. Fixtures locales y sintéticas; sin cuentas, sin APIs de pago, sin Chrome (el render de respaldo se inyecta como HTML ya esperado, según diseño declarado).

## Disposición de hallazgos previos (IDs conservados)

- **F-B7-01 · media · resuelto**: `build_read` truncaba con índice de bytes (`markdown[..INLINE_LIMIT]`). Corregido con `floor_char_boundary` (`read.rs`) y test no-ASCII>50 KiB (relleno + `é`/emoji a caballo del corte) que afirma `is_char_boundary`, `len<=INLINE_LIMIT` e igualdad con el piso calculado. Evidencia: `read_cite::inline_limit_floors_char_boundary_for_non_ascii` en verde (13/13).
- **F-B7-02 · media · resuelto**: `fetch_http` leía `response.bytes()` entero antes del tope. Corregido con `read_capped_body` (trozos + contador, aborta `body_too_large` pasada 10 MiB sin buferizar el cuerpo). Evidencia: `large_body_aborts_past_cap_without_buffering_all` sirve 50 MiB por streaming y recibe `body_too_large`, en verde.
- **F-B7-03 · alta · resuelto (lado navegador)**: ruta de descarga con guardas SSRF propias — `FetchPolicy::secure` deniega loopback/privadas/link-local/metadatos/sufijos sensibles por defecto; `test_loopback` solo admite literales loopback para fixtures; cada salto de redirección se revalida + rechazo de bajada; sin jar de cookies (reqwest `default-features=false`); ganchos `robots_allows`/`should_enqueue_index`/meta-noindex sin tocar red. Evidencia: 5 tests de guarda en verde + contador 0 en `secure_default_refuses_loopback_fixture`. El egreso con resolución DNS, recuperación/caché de `robots.txt` y encolado real al índice siguen siendo carril B9 del broker (declarado en código y docs) → solo resumen, no hallazgo.
- **F-B7-04 · baja · resuelto**: `cite::locate` normalizado devolvía offsets aproximados. Corregido con `find_normalized_span` (ventana de tokens sobre palabras del documento, tramo exacto con casing/espaciado original, frontera de carácter). Evidencia: `normalized_cite_returns_exact_covering_span` afirma `normalize(tramo)==cita normalizada` y `empieza/termina` en palabras originales + `cite` con `raw==normalize(cita)`, en verde.

## Nota nueva (no bloqueante)

- **N-01 · info · abierta · no bloquea**: las rutas `unsupported_type` (allowlist) y `too_many_redirects` (6 saltos) están implementadas (`read.rs`) pero sin fixture dedicada en `read_cite` (el 14.4 las lista junto a binario/6-redirecciones). Recomendación: dos fixtures sintéticas (octet-stream; cadena de 6 redirecciones loopback) como pulido de seguimiento. No impide el veredicto: los requisitos funcionales están verificados y la documentación describe con honestidad la cobertura.

## Resumen y entrega

B7 queda cerrado en su lado determinista: validación, caché 24 h con fecha original, heurística HTTP-first con evitación por datos incrustados, captura con puerta de espera (regla 7.4-1), extracción propia con fragmentos y offsets, corte inline 50 KiB en frontera UTF-8, guardas SSRF del lado navegador con redirecciones revalidadas y sin cookies, ganchos robots/meta-noindex, citas exactas/normalizadas con tramos exactos, hashes e IDs y docs EN/ES honestas sobre sus límites (47 aserciones de backend en verde: 13+31+3; fmt/clippy/build limpios). Trabajo futuro fuera de este alcance (no verificado aquí): egreso completo por broker con DNS, recuperación/caché de `robots.txt` y encolado real al índice, extractores de producción `dom_smoothie`/`htmd`, PDF/feeds/juegos de caracteres, render efímero real con contexto efímero, registro RPC — más el resto de la arquitectura completa (B0–B6, B8 con buscador propio, B9 con recursos integrados y todos los NB), que esta auditoría no declara ni evalúa.

- Commit auditado: `bd8676c0c89bab7dbb2bef2e74d5fccdb6ea0c62` (sin modificar).
- Informe: este archivo.
