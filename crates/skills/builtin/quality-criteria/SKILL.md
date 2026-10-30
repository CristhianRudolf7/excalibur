---
name: quality-criteria
description: "Define the project's quality criteria and constraints (performance, security, accessibility, style, compatibility) as verifiable criteria with a checking method. Use when starting a project or a significant feature."
metadata:
  excalibur.group: planning
  excalibur.version: "1.0.0"
  excalibur.platforms: "linux,macos,windows"
  excalibur.risk: low
  excalibur.display: "{\"en\": {\"name\": \"Quality criteria\", \"description\": \"Define verifiable quality criteria\"}, \"es\": {\"name\": \"Criterios de calidad\", \"description\": \"Definir criterios de calidad verificables\"}}"
---
# Quality criteria

## Steps

1. Collect constraints from the user, project settings, existing configuration (linters, CI, formatters) and documentation.
2. For each dimension that applies (performance, security, accessibility, style, compatibility, reliability, maintainability) write criteria that can be checked: a threshold, a command, a tool or a checklist item.
3. For each criterion record the checking method and when it runs (every change, before delivery, release).
4. Store the criteria in the task contract so the finalization gate can check them.
5. Reuse the project's existing tools before proposing new ones.

## Rules

- A criterion without a method is a wish: either give it a method or mark it as inspection-only.
- Do not lower an existing project standard without the user's explicit agreement.
