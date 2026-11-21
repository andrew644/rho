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
  and tool activity (#).
- Esc interrupts the current generation/loop. PgUp/PgDn scroll, Up/Down walk
  prompt history. Ctrl+C or `/quit` exits.
- Tools: `read`, `bash`, `edit`, `write` (same semantics as pi).

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

## Tests

```sh
cargo test                    # unit tests (offline)
cargo test -- --ignored       # live tests, requires a running local model
```
