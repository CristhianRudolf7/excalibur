---
name: user-preferences
description: "Apply the user's communication preferences (language, tone, length, technical level, format) retrieved from their profile. Use when composing any message to the user."
metadata:
  excalibur.group: communication
  excalibur.version: "1.0.0"
  excalibur.platforms: "linux,macos,windows"
  excalibur.risk: low
  excalibur.display: "{\"en\": {\"name\": \"User preferences\", \"description\": \"Apply the user's communication preferences\"}, \"es\": {\"name\": \"Preferencias del usuario\", \"description\": \"Aplicar las preferencias de comunicación del usuario\"}}"
---
# User preferences

## Steps

1. Retrieve the user's stored preferences: language, tone, length, technical level and preferred formats.
2. Apply them to every message. The language of the user's latest message wins over a stored default when they differ.
3. Respect explicit instructions given in the current conversation over stored preferences.
4. When a new stable preference appears ("always answer briefly"), propose storing it in memory instead of silently assuming it.

## Rules

- Never invent a preference that is not stored or stated.
- Keep technical precision; adapting the level changes explanations, not correctness.
