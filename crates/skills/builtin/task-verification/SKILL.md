---
name: task-verification
description: "Check that every requirement has fresh evidence, the complete test suite ran and passed on the current version, and the result answers the original request. Use before declaring work done."
metadata:
  excalibur.group: verification
  excalibur.version: "1.0.0"
  excalibur.platforms: "linux,macos,windows"
  excalibur.risk: low
  excalibur.display: "{\"en\": {\"name\": \"Task verification\", \"description\": \"Check fresh evidence for every requirement before finishing\"}, \"es\": {\"name\": \"Verificación de la tarea\", \"description\": \"Comprobar evidencia fresca de cada requisito antes de terminar\"}}"
---
# Task verification

Used by the executor before finishing and by the independent verifier.

## Steps

1. Reread the literal original request and the recorded requirements and criteria.
2. For each requirement, find evidence produced on the current version of the work. Evidence older than the last relevant change is stale; rerun the check.
3. Run the complete test suite of the project, not a subset, and record the exact command and result.
4. Reproduce executable checks yourself with read-only tools when acting as verifier; do not rely on the executor's reasoning.
5. Judge inspection-only criteria separately and never count a judgment as executable proof.
6. Compare the deliverable with the original request: report omissions, scope creep and anything that only satisfies a proxy.

## Report

Per criterion: pass, fail or not verifiable, with its evidence. Stop after two disagreement rounds and report both positions.
