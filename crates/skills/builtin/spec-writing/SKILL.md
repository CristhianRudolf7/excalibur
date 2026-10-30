---
name: spec-writing
description: "Write the specification (goals, numbered requirements, structure, tests, limits and, for interfaces, screens and states) as task_db operations. Use before implementing nontrivial work."
metadata:
  excalibur.group: planning
  excalibur.version: "1.0.0"
  excalibur.platforms: "linux,macos,windows"
  excalibur.risk: low
  excalibur.display: "{\"en\": {\"name\": \"Specification writing\", \"description\": \"Write the specification as task_db operations\"}, \"es\": {\"name\": \"Redacción de especificaciones\", \"description\": \"Redactar la especificación como operaciones de task_db\"}}"
---
# Specification writing

The specification lives in the durable task database, not in free text.

## Content

- **Goals**: the outcome for the user, in their words.
- **Requirements**: numbered `R1`, `R2`... Each one is atomic, testable and traceable to a literal source message.
- **Structure**: components, data, interfaces and the boundaries between them.
- **Tests**: for every requirement at least one observable criterion and the method that checks it (command, test, metric or inspection).
- **Limits**: what is out of scope, constraints, assumptions and unanswered questions.
- **Interfaces** (when there is a UI): screens, components, interactions and states, including empty, loading, error and disabled states.

## Procedure

1. Keep the literal source messages attached to the task.
2. Write the requirements and criteria as one atomic `task_db` batch.
3. Prefer executable evidence (tests, commands, metrics); mark inspection-only criteria as such.
4. Never assert derived success and never write trusted evidence yourself; evidence comes from checks.
5. Record impact paths so later changes know which requirements they touch.
