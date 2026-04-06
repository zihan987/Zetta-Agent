# Zetta Minimal Usage

This is the shortest practical guide for using the current `v0.4.0` Zetta CLI, REPL, and TUI.

## What It Is

Zetta is currently a terminal-first agent runtime with a CLI, an interactive REPL, and a full-screen TUI.

It can:

- run a prompt through a model
- call tools across multiple planning steps
- read, search, and edit files
- execute a limited `bash` subset
- persist sessions
- enforce permission and hook policy
- show live turn progress on stderr with `--ui-mode`
- run in a full-screen terminal UI with `zetta tui`

## Build

From the repo root:

```bash
cargo check
cargo test
```

## Basic Run

Use the default local placeholder model:

```bash
cargo run -p zetta-cli -- run --prompt "hello"
cargo run -p zetta-cli -- --ui-mode pretty run --prompt "inspect the workspace"
```

Start an interactive REPL:

```bash
cargo run -p zetta-cli -- repl
```

Start the full-screen TUI:

```bash
cargo run -p zetta-cli -- tui
cargo run -p zetta-cli -- --provider deepseek tui
```

TUI controls:

- `Enter` sends the prompt
- `Ctrl+J` inserts a newline
- `Up` / `Down` scroll the conversation pane
- `Shift+Up` / `Shift+Down` scroll the activity pane
- `Ctrl+N` starts a new session
- `Ctrl+U` clears the composer
- `Ctrl+L` redraws the screen
- `Esc` or `Ctrl+C` exits
- `F1` appends the key summary into the activity pane

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
- `:overview`
- `:ui`
- `:ui <off|pretty|json>`
- `:mode`
- `:mode <read-only|workspace-write|bypass-permissions>`
- `:events`
- `:events on|off`
- `:json`
- `:json on|off`
- `:load <session_id>`
- `:fork`
- `:exit`
- `:quit`

Inspect a saved session quickly:

```bash
cargo run -p zetta-cli -- session overview --session-id <uuid>
```

Create and use a provider profile:

```bash
cargo run -p zetta-cli -- provider set deepseek \
  --api-base https://api.deepseek.com \
  --api-key-env DEEPSEEK_API_KEY \
  --model-name deepseek-chat

cargo run -p zetta-cli -- --provider deepseek run --prompt "Review the auth flow"
cargo run -p zetta-cli -- --provider deepseek repl
```

Ask it to use tools:

```bash
cargo run -p zetta-cli -- run --prompt "Find the auth code and explain the login flow"
```

If the model decides to call tools, Zetta will run a bounded multi-step loop and then print the final assistant response.

## Real Model

Use the OpenAI-compatible driver:

```bash
OPENAI_API_KEY=YOUR_KEY \
cargo run -p zetta-cli -- \
  --model-driver openai-compatible \
  --model-name gpt-4o-mini \
  run --prompt "Inspect src/main.rs and summarize the entrypoint"
```

When the provider supports native OpenAI-style tool-calling, Zetta will use that automatically. If it does not, Zetta falls back to the text `/tool ...` protocol.

Useful runtime knobs:

```bash
OPENAI_API_KEY=YOUR_KEY \
cargo run -p zetta-cli -- \
  --model-driver openai-compatible \
  --model-name gpt-4o-mini \
  --stream-output \
  --request-timeout-seconds 60 \
  --max-model-retries 3 \
  --retry-backoff-millis 750 \
  run --prompt "Search for TODOs and tell me which ones matter first"
```

Use a custom OpenAI-compatible endpoint:

```bash
MY_API_KEY=YOUR_KEY \
cargo run -p zetta-cli -- \
  --model-driver openai-compatible \
  --api-key-env MY_API_KEY \
  --api-base https://your-host.example.com/v1 \
  --model-name your-model \
  run --prompt "Inspect src/main.rs and summarize the entrypoint"
```

DeepSeek example:

```bash
DEEPSEEK_API_KEY=YOUR_KEY \
cargo run -p zetta-cli -- \
  --model-driver openai-compatible \
  --api-key-env DEEPSEEK_API_KEY \
  --api-base https://api.deepseek.com \
  --model-name deepseek-chat \
  run --prompt "Review the auth flow"
```

GLM example:

```bash
ZHIPU_API_KEY=YOUR_KEY \
cargo run -p zetta-cli -- \
  --model-driver openai-compatible \
  --api-key-env ZHIPU_API_KEY \
  --api-base https://open.bigmodel.cn/api/paas/v4 \
  --model-name glm-5 \
  run --prompt "Summarize the repository structure"
```

If a provider is OpenAI-compatible, Zetta currently only needs:

- `--model-driver openai-compatible`
- `--api-base`
- `--model-name`
- `--api-key-env`

If you save a provider profile with `provider set`, `--provider <name>` can fill these values for you.

## Direct Tool Use

List visible tools under the current permission context:

```bash
cargo run -p zetta-cli -- tool list
```

Call a tool directly:

```bash
cargo run -p zetta-cli -- tool call --name file_read_lines --input '{"path":"src/main.rs","start_line":1,"end_line":80}'
```

Search content:

```bash
cargo run -p zetta-cli -- tool call --name grep --input '{"pattern":"TODO"}'
```

Read a file:

```bash
cargo run -p zetta-cli -- tool call --name file_read --raw "README.md"
```

Edit exact text:

```bash
cargo run -p zetta-cli -- tool call --name file_edit --input '{"path":"src/main.rs","old_text":"before","new_text":"after"}'
```

Edit a line range:

```bash
cargo run -p zetta-cli -- tool call --name file_edit_lines --input '{"path":"src/main.rs","start_line":10,"end_line":14,"new_text":"replacement lines"}'
```

Run a safe verification command:

```bash
cargo run -p zetta-cli -- tool call --name bash --raw "pwd"
```

`bash` is intentionally restricted. Chaining, redirection, and high-risk executables are blocked unless you switch to `bypass-permissions`.

## Session Workflow

Run with a known session id:

```bash
cargo run -p zetta-cli -- run --session-id 11111111-1111-1111-1111-111111111111 --prompt "continue from the previous work"
```

Inspect a saved session:

```bash
cargo run -p zetta-cli -- session show --session-id 11111111-1111-1111-1111-111111111111
```

## Permission Basics

Default config lives under:

- `.zetta/`
- `.zetta/sessions/`

Show current global permission config:

```bash
cargo run -p zetta-cli -- permission show
```

Deny `bash` globally:

```bash
cargo run -p zetta-cli -- permission deny-tool bash
```

Allow only a small read-only subset:

```bash
cargo run -p zetta-cli -- --allow-tool file_read --allow-tool file_read_lines --allow-tool grep --allow-tool glob tool list
```

Run in read-only mode:

```bash
cargo run -p zetta-cli -- --permission-mode read-only run --prompt "Inspect the project and summarize the main modules"
```

Temporarily expand readable roots:

```bash
cargo run -p zetta-cli -- --readable-root ../claude-code-main tool call --name file_read --raw "../claude-code-main/README.md"
```

## Hook Basics

Show current hook config:

```bash
cargo run -p zetta-cli -- hook show
```

Log hook events:

```bash
cargo run -p zetta-cli -- --hook-log /tmp/zetta-hooks.jsonl run --prompt '/tool echo hello'
```

Block a tool through hook policy:

```bash
cargo run -p zetta-cli -- hook deny-tool bash --reason "blocked during review"
```

## JSON/Event Mode

Emit engine events as JSON:

```bash
cargo run -p zetta-cli -- run --json --prompt "/tool echo hello"
```

This is the mode to use if you want to build wrappers or automation around the current runtime.

## What To Expect Today

Use Zetta today when you want:

- a scriptable agent CLI
- bounded tool-assisted code inspection
- controlled file editing
- reproducible session and policy behavior

Do not expect yet:

- a full-screen TUI like Codex CLI
- MCP product parity
- IDE bridge workflows
- polished interactive UX

## Good First Commands

If you just want to sanity-check the runtime, run these in order:

```bash
cargo run -p zetta-cli -- tool list
cargo run -p zetta-cli -- run --prompt "hello"
cargo run -p zetta-cli -- tool call --name file_read_lines --input '{"path":"README.md","start_line":1,"end_line":30}'
cargo run -p zetta-cli -- --permission-mode read-only run --prompt "Summarize the repository layout"
```
