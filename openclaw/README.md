# OpenClaw workspace

The agent's workspace. [deploy/entrypoint.sh](../deploy/entrypoint.sh) syncs these files into the
running workspace on every start, except `USER.md`.

| File | Purpose |
| --- | --- |
| `AGENTS.md` | Agent instructions and the GymClaw tool reference. Always in the prompt. |
| `SOUL.md` | Voice and tone. |
| `HEARTBEAT.md` | Checklist for the isolated heartbeat run (pending events, stuck deliveries). |
| `USER.md` | Owner preferences. Seeded once on first start, then owned by the agent. |
| `plugins/coach` | Model-free workout flow in Telegram: set cards, typed sets, rest timer, swaps. [README](plugins/coach/README.md) |
| `plugins/failure-details` | Replaces OpenClaw's generic failure reply with a sanitized cause. [README](plugins/failure-details/README.md) |
| `plugins/one-reply` | At most one owner message per agent run; drops leftover narration after the reply went out. [README](plugins/one-reply/README.md) |
| `plugins/turn-log` | Appends every agent turn and coach action to `data/turns/YYYY-MM-DD.jsonl` for latency numbers and replay. [README](plugins/turn-log/README.md) |

Every action goes through `scripts/gymclaw-tool`, which validates it and writes to SQLite. Scheduled
jobs (calendar watcher, crowd poll, reminders, rest timers, Sunday plan) are OpenClaw cron jobs that
`runtime sync` creates and owns; quiet ticks never wake the model.

## Model

GLM-5.3-flash on OpenRouter reasons by default and can't turn it off (`reasoning.enabled: false`
returns 400), so the entrypoint sends `reasoning: {"effort": "minimal"}` and pins the provider to
`inference-net/fp4` (override with `GYMCLAW_PROVIDER`, `auto` for default routing). Default routing
mostly hit Together, where one call still spent ~1.7k reasoning tokens (31 s).
`reasoningDefault: "off"` keeps reasoning out of Telegram.

Setup: [docs/self-hosting.md](../docs/self-hosting.md).
