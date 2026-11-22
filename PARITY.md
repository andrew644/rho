# Pi ↔ Rho feature comparison

Comparison of [pi](https://github.com/earendil-works/pi) (TypeScript,
`@earendil-works/pi-coding-agent`) and rho as of this writing, based on pi's
bundled documentation (`docs/` in the installed package). This document is
the deliverable for DESIGN.md step 12 and defines the parity target for
step 13.

## Scope definition

Rho's DESIGN.md states up front: *"a terminal coding-agent harness that talks
to local models only via the openai-completions API… No extension system;
all configuration lives in a single YAML file."* Full parity with pi is
therefore **not** the goal; parity of the **core coding-agent behavior**
under rho's design constraints is. This document splits pi's features into:

1. **Parity — implemented** (before step 13)
2. **Parity — implemented by step 13**
3. **Out of scope by design** (excluded by rho's design constraints)
4. **Rho-only additions** (not present in pi)

## 1. Parity — implemented before step 13

| Pi feature | Rho equivalent | Notes |
|---|---|---|
| Core tools: `read`, `bash`, `edit`, `write` | Same four tools | Same names, schemas, and semantics (2000-line/50KB truncation, unique-match edits, all-or-nothing) |
| Agentic loop (prompt → stream → tool calls → results → repeat) | `agent` module | Serial request queue, one in-flight request |
| SSE streaming, `reasoning_content`, tool-call accumulation | `client` module | Finish reasons `stop`/`tool_calls`/`length` handled |
| Sectioned system prompt + `AGENTS.md` project context | `prompt` module | preamble/tools/rules/project_context/cwd |
| Interactive TUI (transcript, streaming, input, slash commands) | `tui` module | ratatui; Esc interrupt, PgUp/PgDn scroll, prompt history |
| `/help`, `/quit`, `/stop` | TUI commands | |
| Follow-up messages while busy | Agent command queue | Enter queues; processed after the current task |
| Single local provider via openai-completions | `config` + `client` | Design constraint (pi: many providers via `/login`) |

## 2. Parity — implemented by step 13

| Pi feature | Rho implementation |
|---|---|
| Session persistence (auto-save JSONL per working directory) | `session` module; sessions stored under the state dir keyed by cwd; header + message records |
| `pi --continue` / `/new` / `/resume` / `/session` / `/name` | CLI `--continue`; TUI `/new`, `/resume [n]`, `/session`, `/name <text>` |
| Compaction: `/compact [instructions]` + auto-compaction near the context limit | `compaction` module; token estimate ≈ chars/4; reserve/keep-recent thresholds configurable; summary replaces old prefix, recent messages kept; conversation serialized for summarization with 2000-char tool results; cut points at user and assistant messages |
| Overflow / truncated-length recovery: one compact-and-retry attempt, session stays usable | `agent` + `client`: `ApiError::is_context_overflow` pattern matching, history sanitizer, synthetic results for truncated tool calls |
| Prompt templates (markdown files → `/name` commands with `$1`, `$@`, `${1:-def}` substitutions) | `templates` module; loaded from `~/.config/rho/prompts/*.md` and `<cwd>/.rho/prompts/*.md` |
| `!` / `!!` shell escapes (run a command, optionally feed output to the model) | TUI `!<cmd>` runs and appends output to conversation; `!!<cmd>` runs without sending to the model |
| Print mode (`pi --print "…"`, piped stdin, `@file` inclusion, initial prompts) | `rho --print [--continue] [--model id] [@file…] <prompt…>`; final answer to stdout, exit non-zero on error |
| `/model` model selection | `model.models` list in config; `/model` lists, `/model <id>` switches; CLI `--model` override |
| Multi-line editor (Shift+Enter), tool-output expand (Ctrl+O), thinking toggle (Ctrl+R in rho) | Shift+Enter/Ctrl+J insert newlines (bracketed paste inserts verbatim); Ctrl+O expands capped tool output; Ctrl+R shows/hides reasoning |
| Slash-command and template autocomplete | Tab / Shift-Tab cycle matching commands and template names |
| Footer showing model, context usage | Status bar shows session/pwd, model id, spinner + elapsed time, context usage as percent with yellow/red thresholds, session name |

## 3. Out of scope by design

Excluded by rho's stated design constraints (local-only provider, single YAML
config, no extension system), or by rho's minimal-harness philosophy. These
are **not** parity gaps for rho's purposes:

| Pi feature | Why excluded |
|---|---|
| Extension system, `context`/`tool`/command hooks | DESIGN.md: "No extension system" |
| Themes (pi theme system), custom keybindings, pi packages | pi's themes/extensions are tied to the extension system; rho ships a `theme: dark\|light\|ansi` config palette instead |
| Skills (`/skill:name`, skill discovery) | Superseded for rho by prompt templates; no discovery/trust machinery |
| MCP servers (`/mcp`) | External provider/plugin surface; rho is local-model-only by design |
| RPC mode, JSON event mode, TypeScript SDK | rho embeds its own harness (TUI + Discord bridge) instead |
| Multiple providers, `/login`/`/logout`, OAuth/subscriptions, auth store, `/llama` router management, `/thinking` levels, `/scoped-models` | Single local OpenAI-compatible provider by design |
| Session tree: `/tree`, `/fork`, `/clone`, `/import`, branch summaries, session picker UI | Rho keeps linear sessions only (still resumable/compactable) |
| `/export` (HTML), `/share`, `/bug` | Sharing/telemetry features; review-before-share caveats. Copying is covered by rho's own mouse selection (see §4) rather than a `/copy` command |
| `@`-file autocomplete in the TUI, image paste/drag, image tool results | Text-only client by design (see DESIGN.md tools section); `@file` works in CLI print mode |
| Project trust (`/trust`), settings hierarchy (`settings.json`, project settings) | Single YAML config by design |
| `/reload`, `/hotkeys`, `/changelog` | Tied to dynamic resource loading |
| External editor (Ctrl+G) | Not implemented; rho's input is an inline widget |

## 4. Rho-only additions (not in pi)

| Feature | Notes |
|---|---|
| `/loop` (forever / N times / duration / until-condition) | Not shipped by pi ("Pi… skips features like sub-agents and plan mode") |
| Discord remote-control bridge | Gateway websocket + REST relay sharing the serial prompt queue |
| In-transcript selection with clipboard copy | rho captures the mouse (wheel scroll), so it draws its own selection: drag / double-click word / triple-click row, Ctrl+Y to copy again, OSC 52 with `wl-copy`/`xclip`/`xsel`/`pbcopy` fallback, `clipboard:` config |

## Step 13 exit criteria

Step 13 is complete when everything in section 2 is implemented, unit-tested,
documented in README.md/DESIGN.md, and `cargo test` passes.
