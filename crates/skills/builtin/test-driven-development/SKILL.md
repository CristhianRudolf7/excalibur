---
name: test-driven-development
description: "Write the tests first, check that they fail for the pending behavior, then implement only what is needed to make them pass. Use when implementing new behavior or fixing a bug."
metadata:
  excalibur.group: testing
  excalibur.version: "1.0.0"
  excalibur.platforms: "linux,macos,windows"
  excalibur.risk: low
  excalibur.display: "{\"en\": {\"name\": \"Test-driven development\", \"description\": \"Write the tests first, then the code\"}, \"es\": {\"name\": \"Desarrollo guiado por pruebas\", \"description\": \"Escribir primero los tests y después el código\"}}"
---
# Test-driven development

## Cycle

1. **Red**: write a test for the next piece of behavior. Run it and confirm it fails for the expected reason (not a typo or setup error).
2. **Green**: write the minimum code that makes it pass. Run the test.
3. **Refactor**: improve the code with the tests green. Run the full suite.

## Rules

- Test observable behavior through public interfaces, not implementation details.
- Use deterministic fixtures and local mocks; never call paid or external services from tests.
- For bugs, the first test reproduces the bug.
