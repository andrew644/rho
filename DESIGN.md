# rho — a Rust TUI coding agent

A Rust re-implementation of the core of [pi](https://github.com/earendil-works/pi):
a terminal coding-agent harness that talks to **local models only** via the
`openai-completions` API (OpenAI-compatible `/v1/chat/completions` with SSE
streaming and tool calling). No extension system; all configuration lives in a
single YAML file.

## Architecture

```
rho
├── config      ~/.config/rho/config.yaml → Provider, Model, Loop, Compaction, Discord settings
├── client      OpenAI-compatible chat-completions client (SSE streaming, tool calls, reasoning_content)
├── tools       read, bash, edit, write (same schemas/semantics as pi)
├── prompt      system prompt builder (preamble/tools/rules/docs/pwd sections + AGENTS.md project context)
├── agent       the agentic loop: user msg → stream → tool calls → results → repeat
├── loops       /loop command: forever | N times | duration | until-condition prompt
├── session     JSONL session persistence per working directory (/new /resume /session /name, --continue)
├── compaction  token estimation + cut-point logic for /compact and auto-compaction
├── templates   prompt templates: markdown files → slash commands with $1/$@ substitutions
├── discord     remote control via Discord (gateway websocket + REST send)
└── tui         ratatui chat UI: wrapped transcript (reasoning blocks, code fences,
                capped tool output), spinner/percent footer, multiline input,
                slash-command autocomplete, theming
```

### Data flow

- `agent::Agent` owns the conversation (`Vec<Message>`) and a `mpsc` event channel.
- The TUI (and the Discord bridge) feed user prompts into the agent and render
  `AgentEvent`s: `AssistantTextDelta`, `ReasoningDelta`, `ToolStarted`,
  `ToolFinished`, `TurnDone`, `LoopProgress`, `Info`, `ContextUsage`, `Error`.
- Only one in-flight request to the model at a time (local server cannot
  handle concurrency), so the agent processes prompts from a queue serially.
- After every command the agent persists the conversation to its session file
  and emits `ContextUsage` (estimated tokens, message count) for the footer.

### Model API (openai-completions)

- `POST {base_url}/chat/completions` with `Authorization: Bearer {api_key}`.
- Request: `model`, `messages` (system/user/assistant/tool), `tools`,
  `tool_choice: "auto"`, `stream: true`, `max_tokens`.
- SSE deltas carry `content`, `reasoning_content` (shown as a dim reasoning
  line), and `tool_calls` accumulated by index across deltas.
- Finish reasons: `stop` → done, `tool_calls` → execute tools, append tool
  results, continue; `length` → recovery (see Context management).

### Tools

Same four tools, names, JSON schemas and descriptions as pi:

- `read(path, offset?, limit?)` — 2000-line / 50KB truncation; images returned as a note (text-only client).
- `bash(command, timeout?)` — spawns `bash -c`, combined stdout/stderr, last 2000 lines / 50KB kept, exit code reported.
- `edit(path, edits[{oldText,newText}])` — exact unique matches, applied against original content, all-or-nothing on failure.
- `write(path, content)` — creates parent dirs, overwrites.

### System prompt

Built in pi's sectioned form: preamble ("You are a coding assistant running
inside rho…"), `<tools>`, `<rules>`, `<project_context>` (contents of
`AGENTS.md` in the working directory, if present), `<pwd>`.

### Sessions

Every conversation is saved automatically as JSONL (`header` record +
`message` records) under the OS state directory, grouped by working
directory. `/new`, `/resume [n]`, `/session`, `/name`, and `rho --continue`
manage sessions; resumed sessions rebuild the history under a freshly built
system prompt for the current working directory.

### /pwd

`/pwd [path]` prints or changes the working directory. Relative paths resolve
against the current pwd. While typing the argument, Tab/Shift-Tab complete
directory names (unique matches complete and continue into the directory,
ambiguous matches cycle; `~` expands to home). Changing it rebuilds the tools (so `read`/`bash`/
`edit`/`write` resolve relative paths against the new pwd), rebuilds the
system prompt for the new directory (including its `AGENTS.md`), notifies the
model, and redirects subsequent `!` / `!!` shell commands.

### Compaction

Token usage is estimated at ~4 chars/token. When
`est_tokens + reserve_tokens > context_window`, the agent compacts before
the next model request: it walks back from the end to find a user- or
assistant-message cut point that keeps `keep_recent_tokens` of recent
history (tool results are never split from their tool call), asks the model
for a structured summary of everything before the cut, and rebuilds the
history as system + summary + kept tail. `/compact [instructions]` runs the
same flow manually; `!cmd` / `!!cmd` run shell commands with or without
feeding the output to the model.

Summary requests serialize the conversation to plain text (`[User]:`,
`[Assistant]:`, `[Assistant tool calls]:`, `[Tool result]:`) with tool
results capped at 2000 characters, so the summarization request is always
smaller than the context it replaces.

Every request is built from a sanitized copy of the history: assistant
messages always carry content or tool calls, every tool call is answered
exactly once (missing answers get a synthetic tool result), and orphaned
tool results are dropped. This keeps strict servers such as vLLM from
rejecting the follow-up request with HTTP 400 after a failed turn.

Like pi, a response truncated at `max_tokens` triggers a single compact-and-
retry recovery. A truncated attempt is never replayed as-is: text-only
truncations are omitted from model context, while truncated tool calls stay
in history with synthetic "not executed" results so the model can re-issue
them. If the retry truncates again the turn ends with "Response was
truncated before completion." plus a recovery-failed note, and the session
stays usable for the next prompt. A context-overflow API error (matched with
pi's overflow patterns) takes the same single compact-and-retry path.

### Prompt templates

`~/.config/rho/prompts/*.md` and `<cwd>/.rho/prompts/*.md` load as slash
commands (filename = name). Arguments follow pi's shell-style quoting and
substitute via `$1..$n`, `$@`/`$ARGUMENTS`, `${1:-default}`, `${@:-default}`,
`${@:N}`, `${@:N:L}`.

### CLI modes

Interactive TUI by default; `--print` (or redirected stdin/stdout with a
prompt) runs one prompt and prints the final answer. `--continue` /
`--resume <path>` resume sessions, `--model <id>` overrides the model,
`@file` arguments and piped stdin are folded into the prompt.

### Configuration — `~/.config/rho/config.yaml`

```yaml
provider:
  base_url: http://192.168.88.12:8000/v1
  api_key: "1234"
model:
  id: qwen4exp
  max_tokens: 4096
  models: []            # extra ids selectable with /model
context_window: 200000
theme: dark             # dark | light | ansi
compaction:
  enabled: true
  reserve_tokens: 16384
  keep_recent_tokens: 20000
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
11. [x] End-to-end smoke test against the local server
12. [x] Compare features in Pi and Rho and document what still needs to be implemented (see PARITY.md)
13. [x] Continue implementing features from Pi until parity is met (sessions, compaction, prompt templates, shell escapes, print mode, /model — see PARITY.md §2)

## UI TODO

Improvements identified from a hands-on review of the running TUI.

### Transcript readability

1. [x] **Word wrap + reasoning block** — wrapped lines re-prefix `∴ ` and break
   mid-word ("acknow / ledge"). Wrap on word boundaries with a hanging indent;
   render reasoning as one dim block; add a Ctrl+R toggle to hide/collapse it.
2. [x] **Turn separation** — insert a blank line (or thin separator) between
   turns so user input, reasoning, tool output, and answers don't run together.
3. [x] **Markdown / code rendering** — assistant output shows raw backticks and
   fences; detect code fences (indent + dim background, no reflow) at minimum,
   ideally syntax highlighting.
4. [x] **Tool output presentation** — replace per-line `# ` prefixes with a
   single header line (`read Cargo.toml:3 ✓`) followed by 2-column-indented
   output, capped at N lines with an "… +N lines (expand)" note.

### Status bar / footer

5. [x] **Real spinner + elapsed time** — replace literal `…….` dots with
   animated frames (`⠋⠙⠹⠸…`) and show elapsed seconds for the current turn.
6. [x] **Context usage as percent** — show `~478/128k (0.4%)` instead of a bare
   token count; shift color yellow→red as auto-compaction approaches.
7. [x] **Footer identity info** — include session name (after `/name`,
   `--continue`) and cwd basename so side-by-side agents are distinguishable.

### Input UX

8. [x] **Busy-state input affordance** — dim the input border or show
   "queued" while the agent is running so it's clear what Enter will do.
9. [x] **Slash-command autocomplete** — typing `/` + Tab (or arrows) filters
   and completes built-in commands and template names.
10. [x] **Multiline input** — Shift+Enter / Ctrl+J insert a newline instead of
    submitting, so pasted multi-line prompts don't fire early.

### Robustness / theming

11. [x] **Configurable theme** — yellow tool lines and dark-gray reasoning are
    unreadable on light terminals; add a palette/theme section to config
    (at least dark/light/ansi variants).
12. [x] **Rich welcome screen** — first frame should show version, model,
    context window, cwd, and session file so config can be verified before the
    first prompt.
