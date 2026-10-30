---
name: request-interview
description: "Interview the user to clarify requirements, ambiguities and missing data, grouping questions with defaults. Use when a request is underspecified or has several plausible readings."
metadata:
  excalibur.group: planning
  excalibur.version: "1.0.0"
  excalibur.platforms: "linux,macos,windows"
  excalibur.risk: low
  excalibur.display: "{\"en\": {\"name\": \"Request interview\", \"description\": \"Clarify requirements, doubts and missing data with the user\"}, \"es\": {\"name\": \"Entrevista de requisitos\", \"description\": \"Aclarar requisitos, dudas y datos faltantes con el usuario\"}}"
---
# Request interview

Use this skill before planning when the request leaves decisions open.

## Steps

1. Restate the literal request in one sentence and list what is already explicit.
2. List every open point: missing inputs, ambiguous terms, conflicting constraints, unknown acceptance criteria, irreversible choices.
3. Classify each point:
   - **Blocking**: an irreversible, costly or security-relevant choice, or one where plausible readings lead to different deliverables. Ask.
   - **Reversible**: a cheap, easily changed detail. Do not ask; choose the most conservative default and record it as an assumption.
4. Group the blocking questions in a single message, at most five, ordered by impact. Give each question a proposed default the user can accept with one word.
5. Record every assumption in the task contract (`task_db`) with its reason, so it is visible and can be revisited.
6. When the user answers, update requirements literally; never paraphrase a user constraint into a weaker one.

## Rules

- Never ask what can be discovered by reading the workspace, the task database or memory.
- Never ask for confirmation of something already stated.
- If the user does not answer and the work is not blocked, continue with the recorded defaults and say so in the next progress update.
