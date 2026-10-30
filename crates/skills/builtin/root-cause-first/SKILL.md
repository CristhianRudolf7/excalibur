---
name: root-cause-first
description: "Do not propose fixes until the cause of the error is identified and confirmed with evidence. Use whenever an error, failure or unexpected behavior appears."
metadata:
  excalibur.group: debugging
  excalibur.version: "1.0.0"
  excalibur.platforms: "linux,macos,windows"
  excalibur.risk: low
  excalibur.display: "{\"en\": {\"name\": \"Root cause first\", \"description\": \"Identify and confirm the cause before fixing\"}, \"es\": {\"name\": \"Causa raíz primero\", \"description\": \"Identificar y comprobar la causa antes de corregir\"}}"
---
# Root cause first

## Rules

1. Read the complete literal error, including the stack trace and the first failure, not only the last line.
2. State a specific hypothesis about the cause.
3. Confirm it with evidence: a reproduction, a log line, a value inspected, a minimal experiment.
4. Only then propose a fix, and explain how it addresses the confirmed cause.
5. If the fix does not work, the hypothesis was wrong: go back to step 2 instead of stacking more fixes.

Never silence an error, add retries or widen a timeout without knowing why it fails.
