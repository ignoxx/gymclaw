# Self-hosting GymClaw

GymClaw is a single-owner app: one deployment, one Telegram user, one gym. Run your own copy.

## What you need

- A host with Docker (any small VPS works; it has to stay on, since GymClaw runs your week on a schedule).
- A Telegram bot token from [@BotFather](https://t.me/BotFather) and your numeric Telegram user ID
  (for example from [@userinfobot](https://t.me/userinfobot)).
- An [OpenRouter](https://openrouter.ai) API key. Set a spending limit on it.
- A Google account with a **dedicated** calendar for workouts, plus a Google Cloud OAuth client
  ([Google Calendar setup](technical-reference.md#google-calendar-setup)).
- Optional: a live check-in count for your gym ([crowd data](crowd.md)). Without it, GymClaw
  still plans and coaches; it just can't pick quiet hours from data.

## Deploy

```bash
git clone https://github.com/ignoxx/gymclaw && cd gymclaw
cat > .env <<'EOF'
GYMCLAW_TELEGRAM_USER_ID=123456789
TELEGRAM_BOT_TOKEN=...
OPENROUTER_API_KEY=...
TZ=Europe/Berlin
EOF
docker compose up -d --build
```

| Variable | Required | Notes |
| --- | --- | --- |
| `GYMCLAW_TELEGRAM_USER_ID` | yes | The only Telegram user the bot answers. |
| `TELEGRAM_BOT_TOKEN` | yes | |
| `OPENROUTER_API_KEY` | yes | |
| `TZ` | no | Your IANA timezone. Also seeds the profile timezone on first start. Default `Europe/Berlin`. |
| `GYMCLAW_MODEL` | no | OpenRouter model ID. Default `z-ai/glm-5.3-flash`. |
| `GYMCLAW_PROVIDER` | no | Pinned OpenRouter provider, or `auto`. |
| `EGRESS_EXTRA_HOSTS` | no | Extra hosts the egress proxy allows, e.g. your gym's crowd API or personal calendar feed. |
| `GYMCLAW_DASHBOARD_HOST` | no | Hostname for the read-only dashboard behind Traefik. |

The stack is three containers: GymClaw (OpenClaw gateway plus the Python domain), an egress proxy
with a host allowlist, and the read-only dashboard. Details, Coolify steps, backups and moving hosts:
[deploy/README.md](../deploy/README.md).

## First run

1. **Connect Google Calendar.** OAuth needs a browser on loopback, which the container can't
   open, so authorize on your own machine and ship the DB to the server. Locally, follow
   [Google Calendar setup](technical-reference.md#google-calendar-setup) (it ends with
   `calendar auth` and `calendar sync`), then:

   ```bash
   tar -czf gymclaw-auth.tar.gz -C data gymclaw.db
   docker compose stop gymclaw
   docker compose run --rm -T --no-deps --entrypoint restore.sh gymclaw - --replace < gymclaw-auth.tar.gz
   docker compose start gymclaw
   docker compose exec -w /opt/gymclaw gymclaw scripts/gymclaw-tool runtime relocate
   ```

   The token now lives in the server's DB; delete the local copy.
2. **Say hi to your bot.** It runs a short setup interview: goal, experience, days, time window, gym
   and travel time, equipment, injuries. Send a photo of your plan or let it build one.
   See [onboarding](technical-reference.md#onboarding).
3. **Turn on the runtime.** Reminders, rest timers and the Sunday plan are scheduled jobs. They stay
   off until you enable them:

   ```bash
   docker compose exec -w /opt/gymclaw gymclaw scripts/gymclaw-tool runtime plan     # preview
   docker compose exec -w /opt/gymclaw gymclaw scripts/gymclaw-tool runtime sync \
     --allow-runtime-changes --allow-messages --template-id <first template> [--with-crowd-poll]
   ```

4. **Calendar writes** are a separate opt-in: preview with `calendar publish`, then grant
   `runtime sync --allow-calendar-writes`. Revoke any time with `runtime revoke-calendar-writes`.

`runtime pause` stops all callbacks and messages and revokes calendar writes. Resuming needs
`runtime resume --allow-runtime-changes --allow-messages`.

## Your gym's crowd data

Every gym exposes this differently, if at all. See [crowd data](crowd.md) for what GymClaw
supports and how to connect yours.

## Installing into an existing OpenClaw

Not supported yet. GymClaw expects its own OpenClaw profile: its workspace files (`AGENTS.md`,
`SOUL.md`, `HEARTBEAT.md`) define the agent's whole persona and would replace your assistant's, and the
coach plugin calls the Python tool and SQLite DB from inside the gateway's environment. To try it
anyway, mirror what [deploy/entrypoint.sh](../deploy/entrypoint.sh) does on a separate
`--profile gymclaw`: the workspace, the two plugins in [openclaw/plugins](../openclaw/plugins), the
Telegram settings, and a Python 3.12+ venv with GymClaw installed. Then use `runtime sync` as above.

## When a message goes missing

Proactive messages go through an outbox with delivery receipts. `runtime deliveries` lists them.
A delivery stuck in SENDING or UNKNOWN is never retried automatically, since Telegram has no
exactly-once send. Check the chat, then resolve it:

```bash
scripts/gymclaw-tool runtime resolve --delivery-id ID --outcome sent|not-sent --confirm-sender-stopped
```

`not-sent` queues it again.

## Things to know

- The OpenClaw image is pinned (`deploy/Dockerfile`). The coach plugin uses OpenClaw's internal
  Telegram runtime, so check it after upgrading OpenClaw.
- The DB and backups hold your Google OAuth token and chat history. Keep them private.
- Only one instance per bot token. Two would both poll Telegram and send every reminder twice.
