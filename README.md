# Zetta

Rust agent runtime workspace for Zetta.

Docs:

- English minimal usage: `MINIMAL_USAGE.md`
- 中文使用指南：`MINIMAL_USAGE.zh-CN.md`
- Changelog: `CHANGELOG.md`
- Roadmap: `ROADMAP.md`

## Release Status

Current release target: `v0.4.0`

- `P0` complete: runtime core, permissions, hooks, sessions, OpenAI-compatible model driver
- `P1` complete: interactive REPL and CLI ergonomics
- `P2` complete: native tool-calling for OpenAI-compatible providers with text fallback preserved
- `P3` complete: full terminal UX, live turn presentation, session overview helpers, and a full-screen TUI
- Next focus: `P4` integrations and external surfaces

## Current Scope

The current workspace ships a usable CLI agent runtime:

- protocol types for messages, tool calls, sessions, and events
- a bounded multi-step engine for repeated model planning and tool execution
- pluggable model and tool traits
- JSON session persistence
- an interactive CLI and REPL
- a permission boundary and core tools
- rule-based tool visibility and allow/deny policy
- persistent global and session-level permission config
- a safe internal hook/event bus with optional JSONL logging
- controlled hook vetoes and session annotations without arbitrary scripts
- persistent global and session-level hook policy config
- workspace baseline config auto-loaded from `.zetta/project-permissions.json` and `.zetta/project-hooks.json`
- optional `openai-compatible` model driver for real remote completions
- native tool-calling for OpenAI-compatible providers, with the existing `/tool ...` path kept as a fallback
- invalid `/tool ...` attempts are fed back into the transcript so the model can correct and retry within the same turn
- a live stderr turn presenter with `--ui-mode off|pretty|json`
- session overview helpers in both `session overview` and REPL
- a full-screen terminal UI via `zetta tui`

This intentionally does **not** include:

- Ink/React terminal UI
- MCP/OAuth flows
- remote bridge / websocket session control
- plugin or skill loading
- parity with the original TypeScript feature set

## Workspace Layout

- `crates/zetta-protocol`: shared serializable types
- `crates/zetta-core`: engine, tool registry, model abstraction, session store
- `crates/zetta-cli`: user-facing CLI and REPL

## REPL

Zetta includes a lightweight interactive CLI loop:

```bash
cargo run -p zetta-cli -- repl
```

Local REPL commands:

- `:help`
- `:session`
- `:tools`
- `:history`
- `:search <text>`
- `:last`
- `:write <path>`
- `:show`
- `:new`
- `:reset`
- `:trim <turns>`
- `:retry`
- `:rerun <turns_back>`
- `:export <path>`
- `:provider`
- `:provider use <name>`
- `:provider clear`
- `:config`
- `:mode`
- `:mode <read-only|workspace-write|bypass-permissions>`
- `:overview`
- `:ui`
- `:ui <off|pretty|json>`
- `:events`
- `:events on|off`
- `:json`
- `:json on|off`
- `:load <session_id>`
- `:fork`
- `:exit`
- `:quit`

## Quick Start

```bash
cd Zetta-Agent
cargo run -p zetta-cli -- run --prompt "hello"
cargo run -p zetta-cli -- repl
cargo run -p zetta-cli -- tui
cargo run -p zetta-cli -- run --prompt "/tool echo staged rewrite"
cargo run -p zetta-cli -- --ui-mode pretty run --prompt "inspect the workspace"
cargo run -p zetta-cli -- provider set deepseek --api-base https://api.deepseek.com --api-key-env DEEPSEEK_API_KEY --model-name deepseek-chat
cargo run -p zetta-cli -- --provider deepseek run --prompt "hello from DeepSeek profile"
OPENAI_API_KEY=... cargo run -p zetta-cli -- --model-driver openai-compatible --model-name gpt-4o-mini run --prompt "hello from remote"
OPENAI_API_KEY=... cargo run -p zetta-cli -- --model-driver openai-compatible --model-name gpt-4o-mini --stream-output run --prompt "hello from remote"
OPENAI_API_KEY=... cargo run -p zetta-cli -- --model-driver openai-compatible --model-name gpt-4o-mini --request-timeout-seconds 60 --max-model-retries 3 --retry-backoff-millis 750 run --prompt "retryable provider demo"
cargo run -p zetta-cli -- tool list
cargo run -p zetta-cli -- tool call --name bash --raw "pwd"
cargo run -p zetta-cli -- tool call --name file_read_lines --input '{"path":"notes.txt","start_line":10,"end_line":20}'
cargo run -p zetta-cli -- tool call --name file_edit --input '{"path":"notes.txt","old_text":"before","new_text":"after"}'
cargo run -p zetta-cli -- tool call --name file_edit_lines --input '{"path":"notes.txt","start_line":10,"end_line":12,"new_text":"replacement"}'
cargo run -p zetta-cli -- --permission-mode read-only tool call --name file_write --input '{"path":"x.txt","content":"blocked"}'
cargo run -p zetta-cli -- --deny-tool bash tool list
cargo run -p zetta-cli -- --allow-tool file_read --allow-tool glob tool list
cargo run -p zetta-cli -- --readable-root ../claude-code-main tool call --name file_read --raw "../claude-code-main/README.md"
cargo run -p zetta-cli -- --hook-log /tmp/zetta-hooks.jsonl run --prompt '/tool echo hello'
cargo run -p zetta-cli -- --hook-deny-tool bash run --prompt '/tool bash pwd'
cargo run -p zetta-cli -- --hook-tag trusted --hook-metadata owner=codex run --prompt 'hello'
cargo run -p zetta-cli -- permission show
cargo run -p zetta-cli -- permission export /tmp/permissions.json
cargo run -p zetta-cli -- permission import /tmp/permissions.json --session-id <uuid>
cargo run -p zetta-cli -- permission allow-tool file_read
cargo run -p zetta-cli -- permission add-readable-root ../claude-code-main
cargo run -p zetta-cli -- permission set-mode read-only --session-id <uuid>
cargo run -p zetta-cli -- hook show
cargo run -p zetta-cli -- hook export /tmp/hooks.json
cargo run -p zetta-cli -- hook import /tmp/hooks.json --session-id <uuid>
cargo run -p zetta-cli -- hook deny-tool bash --reason "blocked by baseline hook policy"
cargo run -p zetta-cli -- hook allow-tool bash
cargo run -p zetta-cli -- hook add-tag trusted --session-id <uuid>
cargo run -p zetta-cli -- hook remove-tag trusted --session-id <uuid>
cargo run -p zetta-cli -- hook set-metadata owner codex --session-id <uuid>
cargo run -p zetta-cli -- hook unset-metadata owner --session-id <uuid>
```

To inspect a saved session:

```bash
cargo run -p zetta-cli -- session show --session-id <uuid>
cargo run -p zetta-cli -- session overview --session-id <uuid>
```

## Config Precedence
Runtime config is merged in this order:

1. workspace baseline files under `.zetta/project-permissions.json` and `.zetta/project-hooks.json`
2. runtime config store under `--config-dir`
3. session-specific config under `--config-dir`
4. one-off CLI flags such as `--allow-tool` and `--hook-deny-tool`

## Model Drivers

- `rule-based`: default local placeholder used for deterministic development
- `openai-compatible`: minimal remote chat client using `--model-name`, optional `--api-base`, and an API key from `OPENAI_API_KEY` or `--api-key-env`
  If `--system-prompt` is omitted, the CLI builds a default tool-oriented prompt from the currently visible tools.
  When the provider supports OpenAI-style native tool-calling, Zetta will use it automatically.
  If native tool-calling is not used, the runtime still falls back to the text `/tool ...` protocol.
  `--stream-output` enables incremental assistant deltas on stderr for model calls.
  `--request-timeout-seconds`, `--max-model-retries`, and `--retry-backoff-millis` control request timeout and retry behavior for transient provider failures.
  Malformed `/tool ...` responses are treated as structured tool-feedback instead of a final answer, which lets the model self-correct on the next planning step.

## Terminal UX

- `--ui-mode off` disables live stderr presentation
- `--ui-mode pretty` prints compact live turn progress to stderr
- `--ui-mode json` streams raw `EngineEvent` JSON to stderr
- `run --json` still writes the final event list to stdout; `--ui-mode` only affects the live stderr presenter

## Full-Screen TUI

Start it from a real terminal:

```bash
cargo run -p zetta-cli -- tui
```

Controls:

- `Enter`: submit the current prompt
- `Esc` or `Ctrl+C`: exit the TUI
- `Ctrl+N`: switch to a new session
- `Ctrl+U`: clear the current input buffer
- `Ctrl+L`: force a redraw
- `F1`: print the control summary into the status pane

The TUI requires an interactive terminal (TTY). It is not meant to run through a non-interactive pipe.

### OpenAI-Compatible Providers

Any provider that accepts an OpenAI-style `POST {base_url}/chat/completions` request can be used with the current runtime.

If the provider also supports OpenAI-style native tool-calling, Zetta will send tool definitions automatically and parse native tool calls before falling back to text parsing.

Generic custom endpoint:

```bash
MY_API_KEY=... cargo run -p zetta-cli -- \
  --model-driver openai-compatible \
  --api-key-env MY_API_KEY \
  --api-base https://your-host.example.com/v1 \
  --model-name your-model \
  run --prompt "Inspect the repository layout"
```

DeepSeek example:

```bash
DEEPSEEK_API_KEY=... cargo run -p zetta-cli -- \
  --model-driver openai-compatible \
  --api-key-env DEEPSEEK_API_KEY \
  --api-base https://api.deepseek.com \
  --model-name deepseek-chat \
  run --prompt "Review the auth module"
```

GLM example:

```bash
ZHIPU_API_KEY=... cargo run -p zetta-cli -- \
  --model-driver openai-compatible \
  --api-key-env ZHIPU_API_KEY \
  --api-base https://open.bigmodel.cn/api/paas/v4 \
  --model-name glm-5 \
  run --prompt "Summarize the project structure"
```

Provider profiles:

```bash
cargo run -p zetta-cli -- provider set deepseek \
  --api-base https://api.deepseek.com \
  --api-key-env DEEPSEEK_API_KEY \
  --model-name deepseek-chat

cargo run -p zetta-cli -- provider list
cargo run -p zetta-cli -- provider show deepseek
cargo run -p zetta-cli -- --provider deepseek run --prompt "Inspect the repository layout"
cargo run -p zetta-cli -- --provider deepseek repl
```

Profiles are stored under `--config-dir/providers.json`. CLI flags still override profile values when both are present.

## Phase Status

1. `P0`: runtime core, tools, permissions, hooks, sessions
2. `P1`: interactive CLI and REPL ergonomics
3. `P2`: model/provider depth and native tool-calling
4. `P3`: full terminal UX, live turn presentation, session overviews, and a full-screen TUI
5. `P4`: next stage, focused on integrations and external surfaces
