---
name: flow-simulation
description: "Formalize a solution as a canonical flow and review its scenarios with the path simulator before implementing. Use for workflows with branches, approvals or failure paths."
metadata:
  excalibur.group: planning
  excalibur.version: "1.0.0"
  excalibur.platforms: "linux,macos,windows"
  excalibur.risk: low
  excalibur.display: "{\"en\": {\"name\": \"Flow simulation\", \"description\": \"Formalize a solution as a flow and review its scenarios\"}, \"es\": {\"name\": \"Simulación de flujos\", \"description\": \"Formalizar una solución como flujo y revisar sus escenarios\"}}"
---
# Flow simulation

## Steps

1. Express the solution as a canonical flow: named states, transitions with their triggers and guards, effects, and terminal states.
2. Enumerate scenarios: the happy path, each failure branch, retries, timeouts, cancellations, approval denied, and restart after interruption.
3. Walk every scenario through the flow (with the flow simulator when it is available) and record where it ends.
4. Flag dead ends, unreachable states, effects repeated on retry and missing approvals before external effects.
5. Fix the flow and repeat until every scenario reaches an intended terminal state.

Simulation never executes real effects; external actions are represented, not performed.
