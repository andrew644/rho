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
  # models: [other-model]     # extra ids selectable with :model
# theme: light              # dark | light | ansi
# clipboard: auto           # auto | terminal | system (how copies reach the clipboard)
# compaction:
#   enabled: true
#   reserve_tokens: 16384
#   keep_recent_tokens: 20000
# discord:
#   enabled: true
#   bot_token: "..."
#   channel_id: "..."        # optional; where prompts with no Discord origin go
#   allowed_user_id: "..."   # your Discord user id; restricts who may drive the agent
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
  prompt history. Ctrl+C or `:quit` exits.
- Drag with the mouse to select transcript text; releasing copies it and the
  footer says where it went (`copied 132 characters to terminal`). Double-click
  selects a word, triple-click selects the row, Ctrl+Y re-copies the current
  selection, and Esc clears the highlight. Selection is limited to the visible
  rows and copies what is on screen (wrapped lines and indented tool output
  included). Scrolling or streaming keeps the highlight on the same text.
  Paste with the terminal's usual paste (bracketed paste is enabled, so
  multi-line pastes land in the input without submitting).
- Ctrl+R toggles the reasoning block. Tab / Shift-Tab complete colon commands
  and template names; after `:pwd ` they also complete directory paths
  (bash-style: unique matches complete and descend, ambiguous matches cycle;
  `~` expands to home; dotfiles appear once you type `.`). Shift+Enter or Ctrl+J insert newlines in the input
  (pasted multiline text is never submitted early); Enter submits. Long input wraps on the last whitespace
  before the edge of the box (the box grows up to 8 rows) instead of scrolling sideways.
- Tools: `read`, `bash`, `edit`, `write` (same semantics as pi).
- `!cmd` runs a shell command and shares its output with the model;
  `!!cmd` runs it without sharing. `:pwd <path>` changes the working
  directory for tools, agent requests, and `!`/`!!` commands.
- Anything that is broken but survivable — a session file that cannot be
  created, so the conversation stops being saved — is drawn as an error line in
  the transcript (`! session persistence disabled: …`) rather than written to
  the stderr the TUI repaints. `rho -p` prints the same thing to stderr as a
  warning and still answers.

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

Commands are typed in the input with a `:` prefix.

```
:loop [forever|N|10m|until "cond"] <task>   agentic loop (rho-only feature)
:compact [instructions]                     summarize old history to free context
:new                                        start a new session
:resume [n]                                 list recent sessions / open one
:session                                    session file, id, message/token counts
:discord                                    bridge status: connection, intents, servers, counters, errors
:name <text>                                name the current session
:model [id]                                 list models / switch
:pwd [path]                                 show / change the working directory
:stop  :help  :quit
```

Everything in that list, and the `!`/`!!` shell escapes too, works when typed
into Discord instead of the input: rho runs it exactly as if it had been typed
at the keyboard and posts back to the channel the same text it puts in the
transcript. `:quit` is the one exception — it only closes the window from the
keyboard.

Conversations are saved automatically as JSONL sessions under the state
directory, grouped by working directory. Long conversations are compacted
automatically when the context estimate nears `context_window`, or manually
with `:compact`.

If a response is cut off at `max_tokens`, rho compacts and retries the
request once (like pi). A context-overflow error from the server gets the
same compact-and-retry treatment, and a turn that still ends in failure
leaves the session history valid, so you can keep prompting.

### Prompt templates

Markdown files in `~/.config/rho/prompts/*.md` or `<cwd>/.rho/prompts/*.md`
become commands invoked as `:name` (filename = command name). Supported
substitutions:
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

### :loop

```
:loop forever <task>
:loop 5 <task>
:loop 30m <task>
:loop until "<yes/no condition>" <task>
```

Each iteration re-sends the task prompt to the agent. `until` asks the model
to evaluate the condition (YES/NO) after every iteration.

### Discord remote control

With `discord.enabled`, rho connects to the Discord gateway as a bot and
pipes every message it can see — DMs and any server channel it is a member
of — into the same serial prompt queue as the TUI; final answers are posted
back, split at 2000 chars. No channel has to be configured for the bot to
hear you.

Replies go back to the channel the question came from: DM the bot and the
answer appears in the DM, post in a server channel and it appears in that
channel. `channel_id` is only the fallback destination, used for prompts that
have no Discord origin (typed in the TUI or `rho -p`); leave it empty and
those are not mirrored to Discord, while messages sent to the bot still get
answered where they were sent.

Because a server channel needs no configuration, set `allowed_user_id`: while
it is empty anyone who can DM the bot or who shares a server with it can drive
the agent, and rho warns about that at startup. Messages from bots are always
ignored.

Messages that read as commands — `:compact`, `:session`, `:model gpt-4`,
`!cargo test`, `!!git status` — are not sent to the model. They go to the TUI,
which runs them exactly as if they had been typed at the keyboard, and posts
back to the channel the same lines it puts in the transcript: the shell output
for `!`/`!!`, the model list for `:model`, the compaction note for `:compact`,
the error line for a bad path or an unknown command. A command that arrives
while the agent is busy waits its turn, so its reply cannot be confused with
the turn already running, and a `:loop` posts each iteration as it finishes
rather than saving everything up for an end that may never come. `:quit` is
refused: the keyboard is the only place that closes the window. In headless
`rho --print` there is no window to run commands in, and the bot says so in the
channel it was asked in.

Every prompt rho accepts puts a 👀 reaction on the message that asked for it,
and the bot shows as typing in that channel until the answer is on its way
back — refreshed every 8 s, because Discord lets its own typing banner expire
after about 10 s. In a server, reactions need the *Add Reactions* **and**
*Read Message History* permissions (Discord wants the second one to create a
reaction, not only the first); *View Channel* and *Send Messages* are still all
it takes to see a channel and answer in it, and direct messages need none of
them. A refusal is reported once, names the permissions it wanted, and says
what it does not affect, because reactions are cosmetic: prompts and
replies work without them. Messages rho does not accept — from a bot, from the
wrong user, or with no text to read — get neither reaction nor typing.

Reading text in a **server** channel needs the privileged *Message Content
Intent*: without it Discord delivers `MESSAGE_CREATE` for server channels with
an empty `content` field, so rho sees the message but has no text to enqueue
(DM text is never stripped, which is why DMs work without it). Enable it for
the bot under *Bot → Privileged Gateway Intents → Message Content Intent* in
the developer portal. When the intent is not granted the gateway closes the
`IDENTIFY` with `4013 Disallowed intents`; rho drops the intent, prints a
warning and reconnects, so DMs keep working but server channels stay unread.

When server messages still seem to vanish, the bridge says why **in the
transcript** — a TUI owns the terminal, so its notes are drawn as lines in the
rho window rather than written to stderr (headless `rho -p` runs have no TUI
and keep printing them to stderr). Every message the gateway delivers gets one
line, so if nothing appears at all, Discord never sent it:

- `@alice in server "Work" #9: "build it" — queued as a prompt` — the message
  arrived and reached the agent's queue; `DM #7` is the same for direct
  messages.
- `@alice in server "Work" #9: ":session" — queued as a command` — the message
  read as a `:` command or a `!`/`!!` shell escape, so it went to the window
  rather than to the model, and its output comes back to that channel.
- `@alice in server "Work" #9: arrived with no text content, so nothing was
  queued` — the message reached rho with an empty `content`, so the *Message
  Content Intent* is not being applied.
- `@bob in DM #7: ignored, user id 123 is not allowed_user_id` — right bot,
  wrong author; rho remembers the id it saw.
- `discord gateway identified (intents 0x1c01, message content granted)` —
  Discord confirms the intent on the live session; `WARNING: Discord did not
  grant the Message Content Intent` means the portal switch is off (or was
  saved after this session identified, so restart rho).
- `discord: server "Name" (id) available` — one line per server the bot is in
  and can read; a missing server means the bot was never invited there or
  lacks *View Channel*.
- `@alice in server "Work" #9: not queued — the agent is not accepting prompts
  (channel closed)` — the message reached the gateway but never reached the
  agent, so no answer was ever coming.
- `discord: no answer came for #9, so its typing indicator was dropped` — the
  turn was interrupted before it produced an answer, so nothing is still being
  written in that channel.

Anything the bridge *fails* to do is drawn as an error line (`! `, in red)
rather than as a note (`· `): a gateway that closes, stops or loses the Message
Content Intent, the first server message that arrives with no text to read, a
frame it cannot read (JSON that does not parse, or a binary frame from a session
it cannot decode), a reply Discord refuses (`401` bad token, `403` missing
permissions, a `429` that outlasts the retries), reply chunks dropped because
the outbox is full, an answer that had no text to post, a reaction Discord
refused (the first one, which names *Add Reactions* and *Read Message
History*), a typing ping Discord
refused, and a message that never reached the agent. So a bridge that is
connected but not delivering is loud, not silent.

`:discord` prints the same picture on demand: connection state, the intents
Discord granted, the servers the bot can see, how many messages arrived, how
many were queued as prompts and how many as commands, how many reply chunks
went out, the last inbound line, how many reactions and typing indicators went
out and how many Discord refused, how many errors were reported and the last of
them (its own lines turn red when they are reporting one). Messages from other
bots are counted but only mentioned the first time, so a busy channel cannot
push your transcript along.

Outgoing replies are throttled to 4 messages per 5 s window and a `429` is
retried after the `Retry-After` Discord reports (up to 3 attempts); a full
outbox drops the newest chunks instead of blocking the agent, and names the
ones it dropped. Reactions and typing pings ride their own queue with their own
4 per 5 s window, so a slow reaction cannot hold up a reply; a typing ping is
tried once and made up on the next refresh, because a late one is worthless. Dropped gateway connections resume with `op 6` using the saved
`session_id`/`seq` when possible, otherwise they re-identify, and reconnect
waits back off from 1 s to 60 s. Authentication failures (`4004`, `4014`) stop
the bridge instead of retrying; progress and failures are reported the same way
as inbound traffic.

Replies are posted with `Authorization: Bot <token>`, so `discord.bot_token`
must hold the bot token from the developer portal (not an OAuth2 access
token); a `401` means the token is missing or wrong, a `403` means the bot
lacks permission in that channel. Both arrive in the transcript as an error
line naming the channel that refused the reply, and `:discord` keeps the last
one.

## Tests

```sh
cargo test                    # unit tests (offline)
cargo test -- --ignored       # live tests, requires a running local model
```
