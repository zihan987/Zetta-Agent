# Zetta 使用指南

这是当前 `v0.4.0` 版本 Zetta CLI、REPL 和 TUI 的中文使用指南。

## 它现在是什么

Zetta 目前是一个以 CLI、REPL 和全屏 TUI 为主的 agent runtime。

它可以：

- 把 prompt 交给模型处理
- 在多轮规划中调用工具
- 读取、搜索和编辑文件
- 执行受限的 `bash` 命令
- 持久化 session
- 应用 permission 和 hook 策略
- 通过 `--ui-mode` 在 stderr 上显示实时 turn 进度
- 通过 `zetta tui` 进入全屏 terminal UI

## 构建

在仓库根目录执行：

```bash
cargo check
cargo test
```

## 基础运行

使用默认的本地占位模型：

```bash
cargo run -p zetta-cli -- run --prompt "hello"
cargo run -p zetta-cli -- --ui-mode pretty run --prompt "检查当前工作区"
```

启动交互式 REPL：

```bash
cargo run -p zetta-cli -- repl
```

启动全屏 TUI：

```bash
cargo run -p zetta-cli -- tui
cargo run -p zetta-cli -- --provider deepseek tui
```

TUI 快捷键：

- `Enter` 发送当前输入
- `Ctrl+J` 插入换行
- `Up` / `Down` 滚动左侧会话区
- `Shift+Up` / `Shift+Down` 滚动右侧活动区
- `Ctrl+N` 新建 session
- `Ctrl+U` 清空输入框
- `Ctrl+L` 强制重绘
- `Esc` 或 `Ctrl+C` 退出
- `F1` 把快捷键说明追加到右侧活动区

REPL 内置本地命令：

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

快速查看某个 session 的概览：

```bash
cargo run -p zetta-cli -- session overview --session-id <uuid>
```

创建并使用 provider profile：

```bash
cargo run -p zetta-cli -- provider set deepseek \
  --api-base https://api.deepseek.com \
  --api-key-env DEEPSEEK_API_KEY \
  --model-name deepseek-chat

cargo run -p zetta-cli -- --provider deepseek run --prompt "检查认证流程"
cargo run -p zetta-cli -- --provider deepseek repl
```

让它自己决定是否使用工具：

```bash
cargo run -p zetta-cli -- run --prompt "找到认证相关代码，并解释登录流程"
```

如果模型判断需要调用工具，Zetta 会运行一个有上限的多轮 loop，最后输出 assistant 的最终回复。

## 真实模型

使用 OpenAI-compatible driver：

```bash
OPENAI_API_KEY=YOUR_KEY \
cargo run -p zetta-cli -- \
  --model-driver openai-compatible \
  --model-name gpt-4o-mini \
  run --prompt "检查 src/main.rs，并总结入口逻辑"
```

如果 provider 支持 OpenAI 风格的原生 tool-calling，Zetta 会自动优先使用；如果不支持，仍然会回退到文本 `/tool ...` 协议。

常用运行时参数：

```bash
OPENAI_API_KEY=YOUR_KEY \
cargo run -p zetta-cli -- \
  --model-driver openai-compatible \
  --model-name gpt-4o-mini \
  --stream-output \
  --request-timeout-seconds 60 \
  --max-model-retries 3 \
  --retry-backoff-millis 750 \
  run --prompt "搜索 TODO，并告诉我先处理哪些"
```

使用自定义 OpenAI-compatible 接口：

```bash
MY_API_KEY=YOUR_KEY \
cargo run -p zetta-cli -- \
  --model-driver openai-compatible \
  --api-key-env MY_API_KEY \
  --api-base https://your-host.example.com/v1 \
  --model-name your-model \
  run --prompt "检查 src/main.rs，并总结入口逻辑"
```

DeepSeek 示例：

```bash
DEEPSEEK_API_KEY=YOUR_KEY \
cargo run -p zetta-cli -- \
  --model-driver openai-compatible \
  --api-key-env DEEPSEEK_API_KEY \
  --api-base https://api.deepseek.com \
  --model-name deepseek-chat \
  run --prompt "检查认证流程"
```

GLM 示例：

```bash
ZHIPU_API_KEY=YOUR_KEY \
cargo run -p zetta-cli -- \
  --model-driver openai-compatible \
  --api-key-env ZHIPU_API_KEY \
  --api-base https://open.bigmodel.cn/api/paas/v4 \
  --model-name glm-5 \
  run --prompt "总结一下仓库结构"
```

只要某个 provider 兼容 OpenAI 风格接口，当前 Zetta 只需要这几个参数：

- `--model-driver openai-compatible`
- `--api-base`
- `--model-name`
- `--api-key-env`

如果先用 `provider set` 保存好了 profile，后面可以直接用 `--provider <name>` 自动补这些值。

## 直接调用工具

查看当前权限上下文下可见的工具：

```bash
cargo run -p zetta-cli -- tool list
```

直接调用某个工具：

```bash
cargo run -p zetta-cli -- tool call --name file_read_lines --input '{"path":"src/main.rs","start_line":1,"end_line":80}'
```

搜索内容：

```bash
cargo run -p zetta-cli -- tool call --name grep --input '{"pattern":"TODO"}'
```

读取文件：

```bash
cargo run -p zetta-cli -- tool call --name file_read --raw "README.md"
```

按精确文本编辑：

```bash
cargo run -p zetta-cli -- tool call --name file_edit --input '{"path":"src/main.rs","old_text":"before","new_text":"after"}'
```

按行范围编辑：

```bash
cargo run -p zetta-cli -- tool call --name file_edit_lines --input '{"path":"src/main.rs","start_line":10,"end_line":14,"new_text":"replacement lines"}'
```

执行安全的验证命令：

```bash
cargo run -p zetta-cli -- tool call --name bash --raw "pwd"
```

`bash` 是刻意受限的。命令链、重定向和高风险可执行程序默认都会被拦截，除非切到 `bypass-permissions`。

## Session 工作流

使用一个固定的 session id 继续工作：

```bash
cargo run -p zetta-cli -- run --session-id 11111111-1111-1111-1111-111111111111 --prompt "继续上一次的工作"
```

查看保存的 session：

```bash
cargo run -p zetta-cli -- session show --session-id 11111111-1111-1111-1111-111111111111
```

## Permission 基础

默认配置目录在：

- `.zetta/`
- `.zetta/sessions/`

查看当前全局 permission 配置：

```bash
cargo run -p zetta-cli -- permission show
```

全局禁用 `bash`：

```bash
cargo run -p zetta-cli -- permission deny-tool bash
```

只允许一小组只读工具：

```bash
cargo run -p zetta-cli -- --allow-tool file_read --allow-tool file_read_lines --allow-tool grep --allow-tool glob tool list
```

以只读模式运行：

```bash
cargo run -p zetta-cli -- --permission-mode read-only run --prompt "检查项目，并总结主要模块"
```

临时扩展可读根目录：

```bash
cargo run -p zetta-cli -- --readable-root ../claude-code-main tool call --name file_read --raw "../claude-code-main/README.md"
```

## Hook 基础

查看当前 hook 配置：

```bash
cargo run -p zetta-cli -- hook show
```

把 hook 事件写入日志：

```bash
cargo run -p zetta-cli -- --hook-log /tmp/zetta-hooks.jsonl run --prompt '/tool echo hello'
```

通过 hook policy 拦截工具：

```bash
cargo run -p zetta-cli -- hook deny-tool bash --reason "review 阶段禁止执行"
```

## JSON / Event 模式

把 engine 事件输出成 JSON：

```bash
cargo run -p zetta-cli -- run --json --prompt "/tool echo hello"
```

如果你想围绕当前 runtime 做包装、脚本化或自动化，这是最适合的模式。

## 现在适合用来做什么

当前适合：

- 可脚本化的 agent CLI
- 有边界的工具辅助代码检查
- 受控文件编辑
- 可复现的 session 和 policy 行为

当前还不要期待：

- 类似 Codex CLI 的全屏 TUI
- 完整 MCP 产品能力
- IDE bridge 工作流
- 打磨完的交互体验

## 建议先跑的几条命令

如果你只是想先确认 runtime 正常，可以按顺序跑：

```bash
cargo run -p zetta-cli -- tool list
cargo run -p zetta-cli -- run --prompt "hello"
cargo run -p zetta-cli -- tool call --name file_read_lines --input '{"path":"README.md","start_line":1,"end_line":30}'
cargo run -p zetta-cli -- --permission-mode read-only run --prompt "总结一下仓库结构"
```
