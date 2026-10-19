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
| `GYMCLAW_PROVIDER` | no | Pinned OpenRouter provider. Default `inference-net/fp4`. |
| `GYMCLAW_MODEL_FAST`, `GYMCLAW_MODEL_VISION` | no | Models for background triage and photos. Default: `GYMCLAW_MODEL`. |
| `EGRESS_EXTRA_HOSTS` | no | Extra hosts the egress proxy allows, e.g. your gym's crowd API or personal calendar feed. |
| `GYMCLAW_DASHBOARD_HOST` | no | Hostname for the dashboard behind Traefik. |

The stack is three containers: GymClaw (one Rust binary: bot, agent, coach, scheduler), an egress proxy
with a host allowlist, and the dashboard (read-only except the agent's notes). Details, Coolify steps, backups and moving hosts:
[deploy/README.md](../deploy/README.md).

## First run

1. **Connect Google Calendar.** Create the OAuth client
   ([Google Calendar setup](technical-reference.md#google-calendar-setup)), copy its JSON into the
   data volume and authorize from inside the container:

   ```bash
   docker compose exec -T gymclaw sh -c 'cat > data/google-client.json' < google-client.json
   docker compose exec -it gymclaw gymclaw calendar-auth --calendar-id <dedicated calendar ID>
   docker compose exec gymclaw gymclaw call calendar_sync
   ```

   `calendar-auth` prints a URL. Open it in any browser, approve, and paste back the URL the browser
   ends up on (a `127.0.0.1` page that won't load). If the Google app is in testing mode, its tokens
   expire after 7 days: re-run `calendar-auth` whenever GymClaw reports `CALENDAR_AUTH_REQUIRED`.
2. **Say hi to your bot.** It runs a short setup interview: goal, experience, days, time window, gym
   and travel time, equipment, injuries. Send a photo of your plan or let it build one.
   See [onboarding](technical-reference.md#onboarding).
3. **Turn on the runtime.** Reminders, rest timers and the Sunday plan stay off until you enable
   them. Ask the bot ("turn on reminders", "and poll the gym's crowd"), or:

   ```bash
   docker compose exec gymclaw gymclaw call runtime_resume '{"owner_asked": true, "crowd_polling": true}'
   ```

   Weekly planning uses the first template of the plan you confirmed during setup.
4. **Calendar writes** are a separate opt-in: check `calendar_pending_writes`, then grant with
   `runtime_resume` and `"calendar_writes": true` (or ask the bot). Revoke any time with
   `calendar_writes_revoke`.

`runtime_pause` stops all reminders and messages and revokes calendar writes. `runtime_resume`
turns them back on; calendar writes need to be granted again. `runtime_status` shows what's on.

## Your gym's crowd data

Every gym exposes this differently, if at all. See [crowd data](crowd.md) for what GymClaw
supports and how to connect yours.

## When a message goes missing

Proactive messages go through an outbox with delivery receipts. `deliveries` lists them.
A delivery stuck in SENDING or UNKNOWN is never retried automatically, since Telegram has no
exactly-once send. The bot asks you whether it arrived; you can also check the chat and resolve it:

```bash
docker compose exec gymclaw gymclaw call delivery_resolve '{"delivery_id": "ID", "outcome": "not-sent"}'
```

`not-sent` queues it again; `sent` closes it.

## Things to know

- `gymclaw call <tool> '<json>'` runs any agent tool by hand; `gymclaw tools` lists them.
- The DB and backups hold your Google OAuth token and chat history. Keep them private.
- Only one instance per bot token. Two would both poll Telegram and send every reminder twice.
