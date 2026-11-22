# rho

A Rust TUI coding agent, modeled on [pi](https://github.com/earendil-works/pi).
Talks to local models over the OpenAI-compatible `chat/completions` API
(SSE streaming, tool calling, `reasoning_content`).

## Setup

Create `~/.config/rho/config.yaml`:

```yaml
provider:
  base_url: http://192.168.88.12:8000/v1
  api_key: "1234"
model:
  id: qwen4exp
  max_tokens: 4096
  # models: [other-model]     # extra ids selectable with /model
# theme: light              # dark | light | ansi
# clipboard: auto           # auto | terminal | system (how copies reach the clipboard)
# compaction:
#   enabled: true
#   reserve_tokens: 16384
#   keep_recent_tokens: 20000
# discord:
#   enabled: true
#   bot_token: "..."
#   channel_id: "..."        # optional; DMs/other channels also accepted
#   allowed_user_id: "..."   # optional; restrict who may drive the agent
# loop_config:
#   max_turns_per_run: 64
```

## Run

```sh
cargo run
```

## Usage

- Type a prompt and press Enter. The agent streams reasoning (∴), answers,
  and tool activity. Tool calls render as one header line (`bash ls -la ✓`)
  with indented output; long output is capped — Ctrl+O expands it. Code fences
  render as indented blocks. The footer shows session/pwd, model, an animated
  spinner with elapsed time, and context usage as a percentage of the window
  (yellow/red as auto-compaction approaches). While the agent runs, the input
  border dims and Enter queues the text.
- Esc interrupts the current generation/loop. The mouse wheel, PgUp/PgDn, and
  Home/End scroll the transcript (a scrollbar shows the viewport). Up/Down walk
  prompt history. Ctrl+C or `/quit` exits.
- Drag with the mouse to select transcript text; releasing copies it and the
  footer says where it went (`copied 132 characters to terminal`). Double-click
  selects a word, triple-click selects the row, Ctrl+Y re-copies the current
  selection, and Esc clears the highlight. Selection is limited to the visible
  rows and copies what is on screen (wrapped lines and indented tool output
  included). Scrolling or streaming keeps the highlight on the same text.
  Paste with the terminal's usual paste (bracketed paste is enabled, so
  multi-line pastes land in the input without submitting).
- Ctrl+R toggles the reasoning block. Tab / Shift-Tab complete slash commands
  and template names; after `/pwd ` they also complete directory paths
  (bash-style: unique matches complete and descend, ambiguous matches cycle;
  `~` expands to home; dotfiles appear once you type `.`). Shift+Enter or Ctrl+J insert newlines in the input
  (pasted multiline text is never submitted early); Enter submits. Long input wraps on the last whitespace
  before the edge of the box (the box grows up to 8 rows) instead of scrolling sideways.
- Tools: `read`, `bash`, `edit`, `write` (same semantics as pi).
- `!cmd` runs a shell command and shares its output with the model;
  `!!cmd` runs it without sharing. `/pwd <path>` changes the working
  directory for tools, agent requests, and `!`/`!!` commands.

### Clipboard

Copies are sent to the terminal emulator with the OSC 52 escape sequence, so
they work over ssh and inside tmux (tmux needs `set -g set-clipboard on`), and
no clipboard program has to be installed. If a desktop clipboard helper is
reachable (`wl-copy`, `xclip`, `xsel`, or `pbcopy`, with `WAYLAND_DISPLAY` /
`DISPLAY` set), rho pipes the text into it instead, since that also works in
terminals that ignore OSC 52. Force one path with `clipboard: terminal` or
`clipboard: system` in config.yaml. Terminal-side selection still works in most
emulators if you hold Shift while dragging (that path bypasses rho's mouse
handling).

### Commands

```
/loop [forever|N|10m|until "cond"] <task>   agentic loop (rho-only feature)
/compact [instructions]                     summarize old history to free context
/new                                        start a new session
/resume [n]                                 list recent sessions / open one
/session                                    session file, id, message/token counts
/name <text>                                name the current session
/model [id]                                 list models / switch
/pwd [path]                                 show / change the working directory
/stop  /help  /quit
```

Conversations are saved automatically as JSONL sessions under the state
directory, grouped by working directory. Long conversations are compacted
automatically when the context estimate nears `context_window`, or manually
with `/compact`.

If a response is cut off at `max_tokens`, rho compacts and retries the
request once (like pi). A context-overflow error from the server gets the
same compact-and-retry treatment, and a turn that still ends in failure
leaves the session history valid, so you can keep prompting.

### Prompt templates

Markdown files in `~/.config/rho/prompts/*.md` or `<cwd>/.rho/prompts/*.md`
become slash commands (filename = command name). Supported substitutions:
`$1..$n`, `$@`/`$ARGUMENTS`, `${1:-default}`, `${@:-default}`, `${@:N}`,
`${@:N:L}`; optional `description:` and `argument-hint:` frontmatter.

```markdown
---
description: Review staged git changes
argument-hint: "[focus]"
---
Review the staged changes. Focus on ${1:-correctness}.
```

### Print mode

```sh
rho --print "Summarize this repository"
git diff | rho --print "Review this change"
rho --print "@README.md what does this project do?"
rho --continue --print "pick up where we left off"
```

`--print` runs one prompt, writes the final answer to stdout, and exits.
`--continue` resumes the latest session for the directory, `--resume <path>`
a specific one, `--model <id>` overrides the configured model. Piped stdin
is prepended to the prompt and `@file` arguments are expanded into it. When
stdout/stdin are redirected, print mode is the default.

### /loop

```
/loop forever <task>
/loop 5 <task>
/loop 30m <task>
/loop until "<yes/no condition>" <task>
```

Each iteration re-sends the task prompt to the agent. `until` asks the model
to evaluate the condition (YES/NO) after every iteration.

### Discord remote control

With `discord.enabled`, rho connects to the Discord gateway as a bot and
pipes messages from the configured channel (or DMs) into the same serial
prompt queue as the TUI; final answers are posted back, split at 2000 chars.

`channel_id` is where replies go, so leave it empty and rho reads messages but
never answers. The simplest working setup is a DM with the bot: message the
bot once, right-click the DM channel to copy its ID, and put that ID in
`channel_id`. Discord strips message text for server channels unless the bot
has the privileged *Message Content Intent*, which rho does not request, so DM
the bot rather than adding it to a server.

Outgoing replies are throttled to 4 messages per 5 s window and a `429` is
retried after the `Retry-After` Discord reports (up to 3 attempts); a full
outbox drops the newest chunks instead of blocking the agent. Dropped
gateway connections resume with `op 6` using the saved `session_id`/`seq` when
possible, otherwise they re-identify, and reconnect waits back off from 1 s to
60 s. Authentication and intent failures (`4004`, `4013`, `4014`) stop the
bridge instead of retrying; progress and failures print to stderr.

Replies are posted with `Authorization: Bot <token>`, so `discord.bot_token`
must hold the bot token from the developer portal (not an OAuth2 access
token); a `401` means the token is missing or wrong, a `403` means the bot
lacks permission in that channel.

## Tests

```sh
cargo test                    # unit tests (offline)
cargo test -- --ignored       # live tests, requires a running local model
```
