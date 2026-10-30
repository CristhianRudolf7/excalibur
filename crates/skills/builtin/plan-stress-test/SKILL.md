---
name: plan-stress-test
description: "Challenge a plan before executing it: fragile assumptions, hidden dependencies, edge cases and risks, with concrete proposed changes. Use before committing to a costly or irreversible plan."
metadata:
  excalibur.group: planning
  excalibur.version: "1.0.0"
  excalibur.platforms: "linux,macos,windows"
  excalibur.risk: low
  excalibur.display: "{\"en\": {\"name\": \"Plan stress test\", \"description\": \"Challenge a plan before executing it\"}, \"es\": {\"name\": \"Prueba de estrés del plan\", \"description\": \"Cuestionar un plan antes de ejecutarlo\"}}"
---
# Plan stress test

## Questions to ask of every plan

1. **Assumptions**: which facts does the plan rely on without evidence? How cheaply can each be checked now?
2. **Dependencies**: which external services, permissions, data, people or ordering constraints are implicit?
3. **Edge cases**: empty input, huge input, concurrency, interruption and restart, partial failure, permissions denied, platform differences.
4. **Irreversibility**: which steps cannot be undone, and is there a dry run, backup or approval before them?
5. **Verification**: does every step have a check, and does the final check prove the original request rather than a proxy?
6. **Cost**: which step dominates time or money, and is there a cheaper path?

## Output

A list of findings ordered by severity, each with a concrete change to the plan. Do not rewrite the plan wholesale; propose targeted edits.
