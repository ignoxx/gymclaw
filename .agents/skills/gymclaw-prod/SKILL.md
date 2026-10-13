---
name: gymclaw-prod
description: GymClaw production on Coolify. Use to read the owner's Telegram sessions with the agent ("check today's session"), prod logs and errors, prod DB state, or to confirm a merge to main deployed.
---

GymClaw prod runs as a Coolify compose app. Host and app ID come from
`~/.config/gymclaw/prod.env` (outside the repo; ask the owner if it is missing). Containers are renamed on every
deploy, so reach them only through `prod` (next to this file), which resolves the current names:

```
prod ps | exec [--dashboard] <cmd…> | logs [2h] | turns [YYYY-MM-DD] [--all] | wait-deploy [sha] | db-snapshot [out]
```

- **Session review**: `prod turns` is the owner's Telegram timeline for one UTC day: inbound and
  outbound messages, tool calls with args and timing, model latency. Start here before opening raw
  logs. Times print in Europe/Berlin.
- **Data questions**: `prod db-snapshot`, then query the local copy with `sqlite3`.
- **After a merge**: `prod wait-deploy` blocks until both services run `origin/main` and are healthy
  (Coolify auto-deploys `main` on push; it takes a few minutes).
- **Ops CLI**: `prod exec scripts/gymclaw-tool <verb> …` (Python/OpenClaw build) or
  `prod exec gymclaw call <tool> '<json>'` (Rust build). `prod ps` shows which build runs.

Every command except `exec` is read-only. Treat `exec` as live: a write verb moves the owner's real
calendar or messages them on Telegram, so run one only when the owner asked for that change. One
Telegram poller may run at a time; never start a second bot instance against the prod token.

Deployment details (IDs, hostnames, network) stay out of this public repo;
`.githooks/pre-commit` enforces that.
