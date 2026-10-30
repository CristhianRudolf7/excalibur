---
name: task-skill-matching
description: "Choose the relevant skills and working rules for each subtask and suggest at most two or three in the delegation card. Use when preparing delegation cards."
metadata:
  excalibur.group: delegation
  excalibur.version: "1.0.0"
  excalibur.platforms: "linux,macos,windows"
  excalibur.risk: low
  excalibur.display: "{\"en\": {\"name\": \"Task-skill matching\", \"description\": \"Choose relevant skills for each subtask\"}, \"es\": {\"name\": \"Asignación de skills\", \"description\": \"Elegir las skills pertinentes para cada subtarea\"}}"
---
# Task-skill matching

## Steps

1. Describe the subtask in one sentence and search the skill catalog with it (`search_skills`).
2. Keep only skills that directly change how the subtask is done; drop generic ones.
3. Suggest at most two or three skills. More than three dilutes their effect.
4. Put mandatory working rules in the pinned instructions, not in the suggestions.
5. Note in one line why each suggested skill applies.
