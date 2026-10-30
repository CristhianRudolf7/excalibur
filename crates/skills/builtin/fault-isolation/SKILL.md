---
name: fault-isolation
description: "Locate and isolate the fault (commit bisection, test case reduction), fix it and add a regression test that prevents it from returning. Use when a regression or a large failing case must be narrowed."
metadata:
  excalibur.group: debugging
  excalibur.version: "1.0.0"
  excalibur.platforms: "linux,macos,windows"
  excalibur.risk: low
  excalibur.display: "{\"en\": {\"name\": \"Fault isolation\", \"description\": \"Isolate the fault, fix it and add a regression test\"}, \"es\": {\"name\": \"Aislamiento de fallos\", \"description\": \"Aislar el fallo, corregirlo y añadir un test de regresión\"}}"
---
# Fault isolation

## Techniques

- **Commit bisection**: find a known good and a known bad revision and bisect (for example `git bisect run <test>`).
- **Case reduction**: shrink the failing input or test until removing anything makes it pass.
- **Component isolation**: replace collaborators with fakes to see which boundary introduces the fault.

## Finish

1. Fix the isolated cause.
2. Add a regression test that fails before the fix and passes after it.
3. Run the full test suite.
