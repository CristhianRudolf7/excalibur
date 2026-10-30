---
name: test-planning
description: "Derive from the acceptance criteria the tests and checks (unit, integration, end to end) and record them as checks of the task. Use when planning how a task will be verified."
metadata:
  excalibur.group: testing
  excalibur.version: "1.0.0"
  excalibur.platforms: "linux,macos,windows"
  excalibur.risk: low
  excalibur.display: "{\"en\": {\"name\": \"Test planning\", \"description\": \"Derive tests from acceptance criteria\"}, \"es\": {\"name\": \"Planificación de pruebas\", \"description\": \"Derivar tests de los criterios de aceptación\"}}"
---
# Test planning

## Steps

1. For each acceptance criterion, decide the cheapest level that can prove it: unit, integration or end to end.
2. Cover the main path, the boundaries and the failure paths named in the specification.
3. Write each test as a check with its exact command and expected result, and record it in the task's `checks`.
4. Mark criteria that can only be inspected, and say who inspects them.
5. Make sure the full suite command is recorded so the finalization gate can rerun it.
