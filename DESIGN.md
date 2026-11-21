# rho — a Rust TUI coding agent

A Rust re-implementation of the core of [pi](https://github.com/earendil-works/pi):
a terminal coding-agent harness that talks to **local models only** via the
`openai-completions` API (OpenAI-compatible `/v1/chat/completions` with SSE
streaming and tool calling). No extension system; all configuration lives in a
single YAML file.

## Architecture

```
rho
├── config      ~/.config/rho/config.yaml → Provider, Model, Loop, Discord settings
├── client      OpenAI-compatible chat-completions client (SSE streaming, tool calls, reasoning_content)
├── tools       read, bash, edit, write (same schemas/semantics as pi)
├── prompt      system prompt builder (preamble/tools/rules/docs/cwd sections + AGENTS.md project context)
├── agent       the agentic loop: user msg → stream → tool calls → results → repeat
├── loop        /loop command: forever | N times | duration | until-condition prompt
├── discord     remote control via Discord (gateway websocket + REST send)
└── tui         ratatui chat UI: transcript viewport, streaming status, input line, slash commands
```

### Data flow

- `agent::Agent` owns the conversation (`Vec<Message>`) and a `mpsc` event channel.
- The TUI (and the Discord bridge) feed user prompts into the agent and render
  `AgentEvent`s: `AssistantTextDelta`, `ReasoningDelta`, `ToolStarted`,
  `ToolFinished`, `TurnDone`, `LoopProgress`, `Error`.
- Only one in-flight request to the model at a time (local server cannot
  handle concurrency), so the agent processes prompts from a queue serially.

### Model API (openai-completions)

- `POST {base_url}/chat/completions` with `Authorization: Bearer {api_key}`.
- Request: `model`, `messages` (system/user/assistant/tool), `tools`,
  `tool_choice: "auto"`, `stream: true`, `max_tokens`.
- SSE deltas carry `content`, `reasoning_content` (shown as a dim reasoning
  line), and `tool_calls` accumulated by index across deltas.
- Finish reasons: `stop` → done, `tool_calls` → execute tools, append tool
  results, continue; `length` → surface truncation note.

### Tools

Same four tools, names, JSON schemas and descriptions as pi:

- `read(path, offset?, limit?)` — 2000-line / 50KB truncation; images returned as a note (text-only client).
- `bash(command, timeout?)` — spawns `bash -c`, combined stdout/stderr, last 2000 lines / 50KB kept, exit code reported.
- `edit(path, edits[{oldText,newText}])` — exact unique matches, applied against original content, all-or-nothing on failure.
- `write(path, content)` — creates parent dirs, overwrites.

### System prompt

Built in pi's sectioned form: preamble ("You are a coding assistant running
inside rho…"), `<tools>`, `<rules>`, `<project_context>` (contents of
`AGENTS.md` in cwd, if present), `<cwd>`.

### Configuration — `~/.config/rho/config.yaml`

```yaml
provider:
  base_url: http://192.168.88.12:8000/v1
  api_key: "1234"
model:
  id: qwen4exp
  max_tokens: 4096
context_window: 200000
discord:
  enabled: false
  bot_token: ""
  channel_id: ""
  allowed_user_id: ""
loop:
  max_turns_per_run: 64
```

### /loop

`/loop [forever | <n> | <duration e.g. 10m, 90s> | until: <prompt>] <task prompt>`

- forever / n times / duration: repeatedly send the task prompt to the agent;
  stop on user interrupt (Esc / `/stop`).
- `until: <condition prompt>`: after each run, ask the model a yes/no
  condition (non-streaming, `tool_choice: none`) and stop when it answers YES.
- The loop runs as a background task producing `LoopProgress` events; a
  status line in the TUI shows iteration / elapsed / remaining.

### Discord remote control

- Plain websocket + JSON against the Discord **gateway** (v10, intents
  `GUILDS|GUILD_MESSAGES|DIRECT_MESSAGES` via `IDENTIFY`, heartbeat loop,
  `HELLO`/`RESUME` handled) — no heavy crate dependency.
- Incoming `MESSAGE_CREATE` from the configured channel (or DM) from the
  allowed user: text is enqueued as an agent prompt; agent replies
  (final assistant text, truncated to 2000 chars, split if longer) are POSTed
  to `/channels/{id}/messages`.
- Shares the same serial prompt queue as the TUI, so the two never issue
  concurrent model requests.

## Implementation steps

1. [x] `DESIGN.md`
2. [x] Scaffold Cargo project; dependencies compile
3. [x] `config` module + tests
4. [x] `client` module: SSE parsing, request building, delta/tool-call accumulation + tests
5. [x] `tools` module: read/write/edit/bash + tests
6. [x] `prompt` module: system prompt + AGENTS.md context + tests
7. [x] `agent` module: serial event loop, tool execution, events + tests
8. [x] `loop` module: loop specs/parsing/condition check + tests
9. [x] `tui` module: ratatui app, slash commands incl. /loop, integrate agent
10. [x] `discord` module: gateway client + bridge + tests for pure parts
11. [ ] End-to-end smoke test against the local server
12. [ ] Compare features in Pi and Rho and document what still needs to be implemented.
13. [ ] Continue implementing features from Pi until parity is met.
