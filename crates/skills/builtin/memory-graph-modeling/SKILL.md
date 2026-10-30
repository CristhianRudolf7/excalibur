---
name: memory-graph-modeling
description: "Represent, update and query people, interactions and their context in the memory graph respecting permissions and scopes, adapting relation types to the user's domain. Use when storing or querying relational memory."
metadata:
  excalibur.group: memory
  excalibur.version: "1.0.0"
  excalibur.platforms: "linux,macos,windows"
  excalibur.risk: low
  excalibur.display: "{\"en\": {\"name\": \"Memory graph modeling\", \"description\": \"Represent people and interactions in the memory graph\"}, \"es\": {\"name\": \"Modelado del grafo de memoria\", \"description\": \"Representar personas e interacciones en el grafo de memoria\"}}"
---
# Memory graph modeling

## Steps

1. Identify the entities (people, organizations, projects, places) and resolve them against existing nodes before creating new ones.
2. Record interactions as dated edges with their source and scope.
3. Choose relation types that fit the user's domain; reuse existing types before inventing new ones.
4. Attach permissions and scope to every node and edge; never expose data outside its scope.
5. When querying, filter by scope first, then by relevance and recency.
6. Update instead of duplicating; keep the history of changed facts.
