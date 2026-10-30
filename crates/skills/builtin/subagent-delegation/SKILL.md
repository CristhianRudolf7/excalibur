---
name: subagent-delegation
description: "Group related requirements and assign each set to a subagent with a complete delegation card, decide when to parallelize, and organize work in branches or worktrees. Use before spawning subagents."
metadata:
  excalibur.group: delegation
  excalibur.version: "1.0.0"
  excalibur.platforms: "linux,macos,windows"
  excalibur.risk: low
  excalibur.display: "{\"en\": {\"name\": \"Subagent delegation\", \"description\": \"Assign work to subagents with complete cards\"}, \"es\": {\"name\": \"Delegación en subagentes\", \"description\": \"Asignar trabajo a subagentes con fichas completas\"}}"
---
# Subagent delegation

## Steps

1. Group requirements that touch the same files or decisions; independent groups are candidates for parallel work.
2. Parallelize only when groups do not write the same resources and the coordination cost is lower than the time saved.
3. For each subagent write a complete delegation card: goal, deliverable, scope, inputs, pinned instructions, capability and budget subset, and how the result will be verified.
4. Mandatory instructions go in the pinned instructions of the card, never as a suggested skill.
5. Suggest at most two or three skills when you know they are relevant; the subagent may search and load others.
6. Give parallel writers isolated branches or worktrees and plan the integration step.
7. Check each result against its card before integrating.
