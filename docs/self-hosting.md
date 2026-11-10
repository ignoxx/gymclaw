# Self-hosting GymClaw

GymClaw is a single-owner app: one deployment, one Telegram user, one gym. Run your own copy.

## What you need

- A host with Docker (any small VPS works; it has to stay on, since GymClaw runs your week on a schedule).
- A Telegram bot token from [@BotFather](https://t.me/BotFather) and your numeric Telegram user ID
  (for example from [@userinfobot](https://t.me/userinfobot)).
- An [OpenRouter](https://openrouter.ai) API key. Set a spending limit on it.
- A Google account with a **dedicated** calendar for workouts, plus a Google Cloud OAuth client
  ([Google Calendar](#google-calendar)).
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
| `GYMCLAW_DASHBOARD_HOST` | no | Hostname for the dashboard behind Traefik. The bot's Telegram menu button opens it as a Mini App. |

The stack is three containers: GymClaw (one Rust binary: bot, agent, coach, scheduler), an egress
proxy with a host allowlist, and the dashboard (read-only except the agent's notes and plan
targets). Coolify steps, backups and moving hosts: [deploy/README.md](../deploy/README.md).

## First run

1. **Connect Google Calendar** ([below](#google-calendar)).
2. **Say hi to your bot.** It runs a short setup interview: goal, experience, days, time window, gym
   and travel time, equipment, injuries. Then send a photo of your plan or let it build one. Every
   exercise is mapped to an illustrated catalog entry; unknown weights start at 0 and are learned
   from the first session.
3. **Turn on the runtime** ([below](#runtime-authority)).

## Google Calendar

Use one dedicated Google calendar for GymClaw's workouts. Blockers you add there count, and so does
an optional read-only [personal calendar](#personal-calendar-read-only). Moving, resizing or
deleting a workout in any calendar app counts as your intent; GymClaw adapts.

1. In Google Cloud, enable the Google Calendar API and configure the OAuth consent screen (add your
   account as a test user while the app is in testing).
2. Create a **Desktop app** OAuth client and download its JSON. Keep it out of Git.
3. Copy it into the data volume and authorize against the dedicated calendar's ID (never `primary`):

   ```bash
   docker compose exec -T gymclaw sh -c 'cat > data/google-client.json' < google-client.json
   docker compose exec -it gymclaw gymclaw calendar-auth --calendar-id <dedicated calendar ID>
   docker compose exec gymclaw gymclaw call calendar_sync
   ```

`calendar-auth` prints a URL. Open it in any browser, approve, and paste back the URL the browser
ends up on (a `127.0.0.1` page that won't load, which is expected). Google expires refresh tokens of
testing-mode apps after 7 days: re-run `calendar-auth` whenever GymClaw reports
`CALENDAR_AUTH_REQUIRED`.

The `calendar.events.owned` scope covers every calendar you own, so GymClaw binds its DB to the one
ID you authorized and refuses any other. Tokens live in the SQLite DB in plain text: keep the DB
and backups private.

### Personal calendar (read-only)

Your everyday calendar can block workout slots without GymClaw getting write access. It is read as
an ICS feed; GymClaw only GETs it and keeps busy start and end times, no titles or notes.

1. Apple Calendar: right-click the calendar → **Share Calendar…** → tick **Public Calendar** → copy
   the `webcal://…` link. (Google's **Secret address in iCal format** works the same way.)
2. On the Docker host, run `scripts/connect-personal-calendar` and paste the link at the hidden
   prompt. It checks the feed host is in `EGRESS_EXTRA_HOSTS` and connects it through stdin, so the
   link never lands in argv or shell history. `personal_calendar_status` shows its health.

The feed refreshes at most every 5 minutes. Events marked **Free** and cancelled events don't block.
A failed fetch keeps the last known blockers. Anyone holding the link can read the calendar; to
stop, call `personal_calendar_disconnect`, then turn off **Public Calendar** to revoke the link.

## Runtime authority

Reminders, rest timers, the Sunday plan and calendar publishing run inside `gymclaw serve`, and
nothing is sent until you turn it on. Ask the bot ("turn on reminders, and poll the gym's crowd"),
or:

```bash
docker compose exec gymclaw gymclaw call runtime_resume '{"owner_asked": true, "crowd_polling": true}'
```

**Calendar writes** are a separate grant. Check what would be written with
`calendar_pending_writes`, then add `"calendar_writes": true` to `runtime_resume` (or ask the bot).
`calendar_writes_revoke` takes that grant back. `runtime_pause` stops everything and revokes
calendar writes; resuming never restores them unless asked. `runtime_status` shows what's on.

## When a message goes missing

Proactive messages go through an outbox with delivery receipts; `deliveries` lists them. A delivery
stuck in SENDING or UNKNOWN is never retried automatically, since Telegram has no exactly-once
send. The bot asks you whether it arrived; you can also check the chat and resolve it:

```bash
docker compose exec gymclaw gymclaw call delivery_resolve '{"delivery_id": "ID", "outcome": "not-sent"}'
```

`not-sent` queues it again; `sent` closes it.

## Running it without Docker

Rust stable. The default `heic` feature (iPhone photos) needs system libheif ≥ 1.17 (Debian:
`libheif-dev` from bookworm-backports; macOS: `brew install libheif`); build with
`--no-default-features` without it.

```bash
cargo build --release
set -a; source .env; set +a   # .env is not loaded automatically
gymclaw migrate               # create or upgrade data/gymclaw.db
gymclaw serve                 # the bot
```

The DB is `GYMCLAW_DB`, default `data/gymclaw.db` under `GYMCLAW_DATA_DIR` (default `data`). The
container entrypoint runs `migrate` on every start; `serve` does not.

## Things to know

- `gymclaw call <tool> '<json>'` runs any agent tool by hand, exactly as the agent would (`-` reads
  the JSON from stdin); `gymclaw tools` lists them with their schemas.
- The DB and backups hold your Google OAuth token and chat history. Keep them private.
- Only one instance per bot token. Two would both poll Telegram and send every reminder twice.
