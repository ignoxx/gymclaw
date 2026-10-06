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

Every action goes through `scripts/gymclaw-tool`, which validates it and writes to SQLite. Scheduled
jobs (calendar watcher, crowd poll, reminders, rest timers, Sunday plan) are OpenClaw cron jobs that
`runtime sync` creates and owns; quiet ticks never wake the model.

Setup: [docs/self-hosting.md](../docs/self-hosting.md). Inference timeouts:
[docs/inference-runtime.md](../docs/inference-runtime.md).
