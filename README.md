# ccwatch

> [中文文档](README.zh-CN.md)

A universal **AI agent session monitor**. Watches agent CLIs running in tmux (Claude Code / Codex / Gemini / more to come), classifies each session's state (`working` / `waiting` for input / `idle`), and reports a message **only when the state transitions**.

Why: when you run several cc/codex sessions at once, you don't want to keep manually asking "progress?", and you're tired of agents going silent when they finish or get stuck.

- Pure Rust, single binary, zero runtime dependencies.
- Config-driven: add a new agent = add one `[[profiles]]` block, no code change.
- Three modes: `once` (for cron) / `daemon` (resident, self-delivering) / `check` (debug).

## Build

```bash
cargo build --release
# Output: target/release/ccwatch (single binary)
```

## Configure

Copy the example config to the default location (or point anywhere with `--config`):

```bash
mkdir -p ~/.config/ccwatch
cp config.example.toml ~/.config/ccwatch/config.toml
```

Config lookup order: `--config <PATH>` > `~/.config/ccwatch/config.toml` > `./config.example.toml`.

See `config.example.toml` comments for details. Summary:

- `[general]` — which sessions to watch (`session_prefixes`), how many lines to capture (`capture_lines`), poll interval (`poll_interval_secs`), state file path (`state_file`, supports `~`).
- `[delivery]` — `none` (print to stdout only) or `telegram` (set `bot_token` / `chat_id`, optional `proxy`).
- `[transitions]` — switches for which transitions to report.
- `[[profiles]]` — per-agent pane-feature regexes.

## Three modes

### `ccwatch once` — run once, print to stdout (for cron)

Scans once, prints **state-transition events** to stdout; **stays completely silent** (empty output) if there's no transition. The first run only builds a baseline and doesn't report. Ideal for scheduled jobs:

```bash
ccwatch once   # empty stdout when nothing changed
```

### `ccwatch daemon` — resident loop, self-delivering

Polls every `poll_interval_secs`, delivers on its own (`delivery.mode` decides print-to-stdout vs send-to-Telegram):

```bash
ccwatch daemon                 # use the interval from config
ccwatch daemon --interval 15   # override to 15s
```

On delivery failure it skips writing the state file for that round and retries next round (no missed reports).

### `ccwatch check` — list current sessions + states (debug)

```bash
ccwatch check
# SESSION          PROFILE  STATE    CONTEXT
# ccA              claude   idle     C-2 monitoring API
# ccD              claude   working
```

## State classification logic

For each matching tmux session:

1. `tmux capture-pane -t <session> -p -S -<capture_lines>` grabs the tail of the pane text.
2. Pick a profile: first match the session name against the `session_match` regex; if unrecognized, sniff pane content with the `detect` regex. Skip the session if neither matches.
3. Match by priority: **working > waiting > idle**. Each state is a set of regexes; any hit means that state; none means `unknown`.

> Both codex and claude `working` contain `esc to interrupt`, while codex idle has an empty `›` input box — disambiguated by the priority of the working rules plus idle's `(?m)^›\s*$`.

## Transition reporting rules

| Transition | Report | Switch |
|------|------|------|
| working/waiting/unknown → idle | `✅ <session> done, idle` | `notify_done` |
| any → waiting | `⏸ <session> stuck, waiting on you` | `notify_waiting` |
| idle/waiting → working | `▶ <session> started` (off by default) | `notify_working` |
| session gone | `⚫ <session> session ended` | `notify_gone` |
| new session, first seen already waiting | `⏸ <session> waiting on you from the start` | `notify_new_waiting` |

Reports carry one line of context: for waiting, grabs the last menu/question line; for idle, grabs the most recent `●`/`✻`/`✔` summary line; truncated to ~120 chars.

## Adding a new agent profile

No code change — add a block to the config:

```toml
[[profiles]]
name = "myagent"
session_match = "^my"               # session-name regex (optional)
detect = "MyAgent banner|keyword"   # pane-content sniff (when name unrecognized)
working = ["esc to interrupt"]      # any hit = working
waiting = ["Press Enter", "\\(y/n\\)"]
idle = ["(?m)^>\\s*$"]
```

Use `ccwatch check` to verify classification and tune the regexes if needed.

## Hooking into Hermes / cron

`once` mode's empty-output-means-silent behavior is naturally suited to cron / Hermes no_agent triggers:

```cron
* * * * * /path/to/ccwatch once --config ~/.config/ccwatch/config.toml
```

stdout has content only when there's a transition, forwarded to you by the upper layer (cron mail / Hermes). For a direct Telegram connection, use `daemon` + `delivery.mode = "telegram"`.
