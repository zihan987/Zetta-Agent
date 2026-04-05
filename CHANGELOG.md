# Changelog

## v0.3.0

P3 terminal UX release.

Included in this release:

- live engine event streaming from the core runtime into the CLI
- `--ui-mode off|pretty|json` for stderr turn presentation
- compact pretty turn summaries with tool progress and elapsed time
- session overview helpers in both `session overview` and REPL `:overview`
- REPL UI mode controls via `:ui`, while keeping `:events` and `:json` as compatibility aliases

This release keeps the headless + REPL shape from `v0.2.0`, but makes the terminal experience feel more like a product instead of a raw event dump.

## v0.2.0

P2 model/provider depth release.

Included in this release:

- OpenAI-compatible models now advertise native tool schemas
- OpenAI-compatible responses now prefer native tool-calling over the text `/tool ...` fallback
- streaming responses can accumulate native tool-call deltas
- the text `/tool ...` path remains as a compatibility fallback for providers that do not support native tool-calling cleanly

This release keeps the existing CLI/REPL surface from `v0.1.0`, and deepens model/provider behavior instead of changing the user workflow.

## v0.1.0

First usable Zetta release.

Included in this release:

- `P0` core runtime is complete:
  - bounded multi-step agent loop
  - tool registry with file/search/bash primitives
  - permission policy and path protections
  - session persistence
  - hook policy and JSONL hook logging
  - OpenAI-compatible model driver with timeout/retry/backoff
- `P1` interactive CLI is complete:
  - `repl`
  - provider profile switching
  - session history/search/export helpers
  - retry/rerun/trim workflow
  - lightweight REPL status prompt
  - event and JSON tracing toggles

Known limits:

- no full-screen terminal UI
- no native provider tool-calling integration yet
- no MCP / remote / IDE bridge yet
- OpenAI-compatible providers still rely on the text `/tool ...` fallback protocol
