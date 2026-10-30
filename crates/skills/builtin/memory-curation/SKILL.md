---
name: memory-curation
description: "Decide what to store and how to state it as an atomic fact with source, validity and expiry, and how to handle duplicates and contradictions. Use when writing memories or reviewing them."
metadata:
  excalibur.group: memory
  excalibur.version: "1.0.0"
  excalibur.platforms: "linux,macos,windows"
  excalibur.risk: low
  excalibur.display: "{\"en\": {\"name\": \"Memory curation\", \"description\": \"Decide what to store as atomic facts with source and validity\"}, \"es\": {\"name\": \"Curación de memoria\", \"description\": \"Decidir qué guardar como hechos atómicos con fuente y validez\"}}"
---
# Memory curation

## What to store

- Stable facts, preferences, decisions and commitments that will matter later.
- Not: transient states, secrets, credentials, or content the user asked not to keep.

## How to state it

- One atomic fact per memory, in a full sentence that stands alone.
- Source (message, document, observation), date, validity window and expiry when applicable.
- Scope: personal, project or shared.

## Duplicates and contradictions

- Merge exact duplicates, keeping the strongest source.
- When a new fact contradicts an old one, keep both with dates and mark the old one superseded; ask the user when the contradiction matters and the source is unclear.
