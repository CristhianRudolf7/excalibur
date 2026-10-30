---
name: context-provisioning
description: "Select the relevant context for each agent and moment: which memories, documents, decisions and instructions to include in a card or query. Use when preparing a delegation card or a context-heavy step."
metadata:
  excalibur.group: memory
  excalibur.version: "1.0.0"
  excalibur.platforms: "linux,macos,windows"
  excalibur.risk: low
  excalibur.display: "{\"en\": {\"name\": \"Context provisioning\", \"description\": \"Select the relevant context for each agent\"}, \"es\": {\"name\": \"Provisión de contexto\", \"description\": \"Seleccionar el contexto pertinente para cada agente\"}}"
---
# Context provisioning

## Steps

1. State what the agent must decide or produce.
2. Include only what changes that decision: requirements, constraints, prior decisions with their reasons, relevant memories and the exact documents or fragments.
3. Prefer references (paths, identifiers) over pasted content when the agent can read them itself.
4. Respect scopes and permissions; never pass data the receiving agent is not allowed to see.
5. Pin mandatory instructions literally; summarize everything else.
6. Keep the package small; missing context can be requested, excess context cannot be ignored reliably.
