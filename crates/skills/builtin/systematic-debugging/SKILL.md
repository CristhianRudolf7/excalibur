---
name: systematic-debugging
description: "Reproduce the problem systematically, formulate and discard hypotheses, and reach the root cause. Use for failures whose cause is not obvious."
metadata:
  excalibur.group: debugging
  excalibur.version: "1.0.0"
  excalibur.platforms: "linux,macos,windows"
  excalibur.risk: low
  excalibur.display: "{\"en\": {\"name\": \"Systematic debugging\", \"description\": \"Reproduce and narrow hypotheses to the root cause\"}, \"es\": {\"name\": \"Depuración sistemática\", \"description\": \"Reproducir y acotar hipótesis hasta la causa raíz\"}}"
---
# Systematic debugging

## Steps

1. **Reproduce** with deterministic inputs and record the exact command. A failure you cannot reproduce cannot be confirmed fixed.
2. **Observe**: collect the literal error, logs, inputs and environment differences between passing and failing cases.
3. **Hypothesize**: list candidate causes ranked by likelihood and cost to test.
4. **Test** one hypothesis at a time with the cheapest discriminating experiment; record the result.
5. **Narrow** until one cause explains all observations.
6. **Fix** the cause, rerun the reproduction, then the full test suite.

Keep a short log of hypotheses tested and their outcomes so work is not repeated.
