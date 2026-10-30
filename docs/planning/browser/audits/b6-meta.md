# Auditoría B6 — Cableado de presupuestos de recursos en webd (meta)

- **Commit auditado:** `9a374a274631d626946bd5c773d18ef2016994a9` (merge de `a70b2cc` sobre `4ad646e`)
- **Fecha de auditoría:** 2026-10-30 (UTC)
- **Alcance:** Solo commit `9a374a2`; B6 resources wiring (arch §19.2 B6; §§13.1–13.6 presupuestos/admisión/desalojo/puertas de disco con protección de escucha, procedimiento 18.3; NB-17, NB-18)
- **Rol:** auditor independiente. Sin modificaciones a producto, arquitectura ni Git; árbol limpio en `9a374a2` tras la auditoría.
- **Veredicto:** `changes_required` (6/9 requisitos verificados; decisión y puertas a nivel de biblioteca en verde, pero 13.6 no cerrable con esta evidencia, sin aplicación en el servicio y con regresión documental del visor; F-B6-01 a F-B6-03 bloqueantes)

## Base documental y referencias

- `AGENTS.md` (línea B6 añadida por el commit: `resources_gates` + `resources_measure`).
- `/home/cristhian/projects/Excalibur/browser/arquitectura-navegador.md` §§13.1–13.6, 18.3, 19.2 (fila B6), 19.3, NB-17/NB-18, apéndice E.
- Informes previos relevantes: `b8-meta.md` (patrón de meta), `b7-meta.md`, `b5-meta.md` / `b5-retry-meta.md` (carril del visor servido, afectado por la regresión documental).
- Frontend: no procede (sin visor nuevo ni herramientas registradas; solo se reutiliza el visor servido existente de B5 como corroboración).

## Alcance entregado por el commit (16 ficheros, +958/−26)

- `crates/webd/src/resources.rs` (nuevo, 213 líneas): `ResourceGate` sobre `excalibur-browser-resources` (admisión con media móvil, plan 1–7, puertas de disco, limpieza con disco bajo, telemetría) más `telemetry_fields`, `capacity_exceeded_message`, `denial_code`, `growth_allowed_with_reserve`, `suspend_error_code`.
- `crates/webd/src/main.rs` (+14): crea `ResourceGate` en `serve` y rechaza arrancar si el disco libre rompe la reserva configurada.
- `crates/webd/src/lib.rs`, `crates/webd/Cargo.toml`: registro del módulo y dependencia `excalibur-browser-resources`.
- `crates/browser-resources`: `AdmissionRequest<'a>` con `AdmissionReadings` + `with_estimator` (la estimación por origen queda en el núcleo); `Cargo.toml` sin marcador `[workspace]`; ajuste de lifetime en `tests/budgets_admission.rs`.
- `crates/webd/tests/resources_gates.rs` (268 líneas, 6 tests) y `crates/webd/tests/resources_measure.rs` (231 líneas, 1 test con informe JSON en `target/test-evidence/browser/resources/measure.json`).
- Docs pareadas `browser-resources.md`/`.es.md` (cableado + mediciones + pendientes) y sección B6 en `browser.md`/`.es.md`; línea B6 en `AGENTS.md`; `Cargo.lock` (+5 líneas, arista `excalibur-browser-resources`).
- Fuera de alcance intacto en código: sin cambios en `browser`, broker, sesiones, herramientas, `search`, harness o arquitectura. **Excepción documental:** 3 hunks de `browser.md`/`.es.md` degradan el visor servido de B5 (ver F-B6-02).

## Requisitos y evidencia

| # | Requisito | Estado | Evidencia |
|---|---|---|---|
| R1 | NB-17/13.2 constantes de presupuesto (blando 1.5 GiB, duro 2.0, cgroup 2.5, 700 MiB/1 GiB/10 min/2 min LIVE, 500/800 efímero, 4/12 pestañas, 200/400+200 admisión, PSI 20 %, webd 60 MiB) | verificado | `budget_constants_match_architecture` (6 ok); `budgets.rs` coincide tabla 13.2/13.5 |
| R2 | NB-17/13.2 admisión con media móvil, tab-limits/PSI/reserva inmediatos, espera 120 s con desglose | verificado | `admission_moving_average_gates` + `budgets_admission` (8 ok: admit/wait/deny, 120 s, breakdown) |
| R3 | NB-17/13.3–13.4 desalojo 1–7 con protección LIVE + `listener_cannot_suspend` | verificado (capa de decisión) | `eviction_order_protects_live`, `suspend_refuses_listening_without_approval` (6-suite ok); `plan_eviction` reproduce pasos 1–7 y `names_live_tab` |
| R4 | NB-18/13.5 puertas de disco (reserva 5 GiB, arranque de búsqueda, perfiles 1/1.5 GiB, limpieza <7 GiB sin tocar escucha, LRU 2.5 GiB/500/200/300) | verificado (capa de decisión) | `disk_gates_and_low_disk_cleanup` (ok); `disk.rs` reproduce 13.5 |
| R5 | 13.1 telemetría separada del arnés con picos | verificado | `telemetry_fields_report_peaks_separately` (ok) |
| R6 | 18.3 forma del procedimiento + informe (blank, estática, 10 pesadas; A/B/C con PSS por tipo; 3 reps mediana/min/max; metadatos) | verificado (forma sintética) | `measurement_procedure_three_reps_with_report` (ok, 0.28 s); `measure.json` con `procedure 18.3, repetitions 3, settle_secs 30`, ancla viva `/proc/self` y notas de pending-user |
| R7 | Cierre 13.6 (9 filas: B vs C independiente, todos los tipos, cgroup+`memory.high`, llenado real con LIVE continua, espera→`capacity_exceeded`, suspend en vía de sesión, LIVE>1 GiB simulada, FS pequeño, coste <1 %) | **no verificado** | Solo subconjunto sintético; sin cgroup/Chrome real/FS pequeño/microbench; docs admiten «aceptación completa de B6» en carril principal; ver F-B6-01 |
| R8 | Aplicación en el servicio (puerta retenida y exigida por operación) | **no verificado** | `main.rs:85` crea la puerta y la descarta (`_resource_gate`); cero call sites de servicio; ver F-B6-03 |
| R9 | Paridad y exactitud documental EN/ES | **no verificado** | Regresión del visor contradice código+tests+AGENTS.md; ver F-B6-02 |

## Comprobaciones ejecutadas (pesado con `flock …/heavy-build.lock`, `CARGO_BUILD_JOBS=2`, target aislado `/tmp/b6-meta-target`, sin tocar artefactos ajenos)

- `cargo fmt --check` → salida 0, limpio.
- `cargo clippy --workspace --all-targets -- -D warnings` → salida 0, sin avisos (`/tmp/b6meta-clippy.log`, `CLIPPY_EXIT:0`).
- `cargo test -p excalibur-webd --test resources_gates -- --nocapture` → 6 passed, 0 failed (`GATES_EXIT:0`).
- `cargo test -p excalibur-webd --test resources_measure -- --nocapture` → 1 passed (`MEASURE_EXIT:0`); informe en `target/test-evidence/browser/resources/measure.json`.
- `cargo test --manifest-path crates/browser-resources/Cargo.toml` → 8+7+7+8+3 passed, 0 failed (`FOUNDATION_EXIT:0`).
- `cargo test -p excalibur-webd --test viewer_http` → 2 passed (`VIEWER_HTTP_EXIT:0`); corroboración fuera de alcance para F-B6-02 (endpoint servido real por TCP, sin Chrome).
- `cargo build -p excalibur-webd -p excalibur-browser-resources` → salida 0 (`BUILD_EXIT:0`).
- Fixtures locales y sintéticas más una lectura viva de `/proc/self/status`; sin cuentas, sin APIs de pago, sin Chrome lanzado (solo sonda `--version` best-effort para la etiqueta).

## Hallazgos

- **F-B6-01 · alta · bloqueante · abierta**: 13.6 no cerrable con esta evidencia. `resources_gates` cubre el subconjunto de lógica de decisión (constantes, admisión, orden 1–7, disco, `listener_cannot_suspend` a nivel de función, telemetría) pero ninguna de las filas que exigen entorno real: cgroup propio con `memory.high` forzado, llenado con pestañas pesadas reales y continuidad de eventos/temporizador LIVE, espera real hasta `capacity_exceeded`, `suspend` por la vía de sesión (`sessions.rs:1493` ya lo exige, pero la puerta webd no está conectada a ella), LIVE>1 GiB/10 min con `logged_in`, FS pequeño con rechazo de perfil/indexador, coste de muestreo <1 %, y la verificación B6 explícita de 13.3 de que un proceso sin privilegios puede subir `oom_score_adj` ajeno del mismo usuario (solo predicado `oom_raise_allowed`, sin escritura viva en `/proc`). AGENTS.md («covers … 13.6») y la pretensión de cierre exceden lo probado, aunque los docs matizan «synthetic fixtures only» y mandan la aceptación completa al carril principal. **Recomendación:** añadir las filas (fixtures Chrome/cgroup reales, subida viva de `oom_score_adj` de un hijo propio, puerta con tmpfs pequeño, microbench de coste) o acotar las pretensiones a «filas de lógica de decisión 13.6 sobre fixtures sintéticas» y dejar 13.6 abierto al carril principal.
- **F-B6-02 · media · bloqueante · abierta**: regresión documental fuera de alcance. Los 3 hunks de `browser.md`/`.es.md` afirman que el visor «existe solo como ayudas del backend, sin endpoint servido», pero `main.rs` sigue sirviéndolo (`serve_viewer`, campo `viewer` en el ready) y `viewer_http` (2 tests, TCP real) pasa en este commit; contradice además las líneas de `AGENTS.md` del carril B5. **Recomendación:** revertir esos hunks a la redacción de `4ad646e`.
- **F-B6-03 · media · bloqueante · abierta**: cableado sin aplicación. `serve` crea `ResourceGate` y la abandona en `let _resource_gate`; `admit`, `eviction_plan`, `disk_growth_allowed`, `telemetry_json`, `denial_code`, `capacity_exceeded_message` y `suspend_error_code` no tienen ningún call site de servicio (única referencia no-test es el binding muerto). Solo el chequeo de reserva al arranque se ejecuta. Coherente con que `browser.md` declara la vigilancia continua no implementada, pero entonces el «wiring» es capa de decisión + chequeo de arranque. **Recomendación:** retener la puerta en el estado del servicio, exigirla en operaciones que hacen crecer datos y registrar telemetría, o declarar la aplicación como pendiente del carril principal.
- **F-B6-04 · baja · no bloquea · abierta**: rozaduras menores de API: `AdmissionReadings.soft_budget_bytes` se ignora en silencio (`admit` lo sobrescribe con el presupuesto de la puerta); `InstanceClass` (sampling) duplica a `InstanceKind` (eviction); `disk_growth_allowed` exige dos veces la misma reserva (configurada + constante del núcleo); `denial_code` mapea la espera a `capacity_exceeded` antes de su expiración; la guarda `listener_cannot_suspend` vive en tres capas (`sessions`, `eviction`, `resources`) con solo la primera exigida. **Recomendación:** eliminar/renombrar el campo ignorado, unificar el enum de instancia, documentar el código de espera y designar la capa que exige cada guarda.

## Pendientes verificados como honestos (no re-auditados aquí)

cgroup por scope, supervisión real de Chrome, señales de presión del broker, llamadas CDP GC/presión/caché, recorridos de perfil con `Storage.getUsageAndQuota`, mediciones VPS y figuras con sesión 18.4 (1 h/24 h, pending-user) constan como pendientes en docs y notas del informe; coherente con `AGENTS.md`. Ninguno se presenta como implementado. El `target` propio del checkout no se midió en este informe; el presupuesto de disco del `target` (10 GiB) sigue vigente para el carril principal.

## Mediciones

Sintéticas: niveles B/C sobre árboles `/proc` simulados (blank 1 renderer, estática 2, diez pesadas 10) con crecimiento por repetición; nivel A aritmético sintético; mediana/min/max de 3; diez pesadas ≫ blank en B y C; ancla viva `live_self_level_b_bytes` de `/proc/self/status`. Sin Chrono real, sin espera de 30 s (constante registrada), sin cifras WhatsApp/Teams (18.4 pending-user, nunca afirmadas).
