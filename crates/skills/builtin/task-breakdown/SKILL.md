---
name: task-breakdown
description: "Split a specification into small verifiable tasks with acceptance criteria, dependencies, milestones and the minimum content of a micro plan. Use after the specification is agreed."
metadata:
  excalibur.group: planning
  excalibur.version: "1.0.0"
  excalibur.platforms: "linux,macos,windows"
  excalibur.risk: low
  excalibur.display: "{\"en\": {\"name\": \"Task breakdown\", \"description\": \"Split the specification into small verifiable tasks\"}, \"es\": {\"name\": \"División en tareas\", \"description\": \"Dividir la especificación en tareas pequeñas y verificables\"}}"
---
# Task breakdown

## Steps

1. Group requirements into milestones that each deliver observable value.
2. Split each milestone into tasks small enough to verify independently (one coherent change, one clear check).
3. For every task record: the requirements it serves, acceptance criteria, dependencies, and the check that proves it.
4. Validate that dependencies reference existing tasks and form no cycle.
5. Write the plan as one atomic `task_db` batch.

## Micro plan

Before executing a task, keep a compact micro plan: next step, files or resources involved, the check that will confirm it, and the fallback if it fails.

## Replanning

Replan only the affected dependency subgraph. Preserve completed unaffected work and keep previous plan versions.
