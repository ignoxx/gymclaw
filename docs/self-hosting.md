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

Every setting is an environment variable; this table is the full list, and `.env.example` mirrors it.

| Variable | Required | Notes |
| --- | --- | --- |
| `GYMCLAW_TELEGRAM_USER_ID` | yes | The only Telegram user the bot answers. |
| `TELEGRAM_BOT_TOKEN` | yes | |
| `OPENROUTER_API_KEY` | yes | |
| `TZ` | no | Docker only: your IANA timezone; compose passes it on as `GYMCLAW_TIMEZONE`. Default `Europe/Berlin`. |
| `GYMCLAW_TIMEZONE` | no | Seeds the profile timezone on first start; later changes go through the profile. Default `Europe/Berlin`. |
| `GYMCLAW_MODEL` | no | OpenRouter chat model. Default `anthropic/claude-haiku-5.5`. |
| `GYMCLAW_PROVIDER` | no | OpenRouter provider tried first (e.g. `inference-net/fp4`). Unset or `auto`: OpenRouter picks. |
| `GYMCLAW_MODEL_VISION` | no | Reads photos (scale, plan screenshots). Default: `GYMCLAW_MODEL`. |
| `GYMCLAW_MODEL_AUDIO` | no | Transcribes voice notes; needs audio input. Default `google/gemini-3.5-flash-lite`. Adds ~1–3 s before a spoken set is logged. |
| `GOOGLE_CALENDAR_ID` | no | Read only by `calendar-auth` when `--calendar-id` isn't passed. |
| `GYMCLAW_DASHBOARD_HOST` | no | Docker: the dashboard's hostname behind Traefik. The bot's Telegram menu button opens it as a Mini App. |
| `EGRESS_EXTRA_HOSTS` | no | Docker: extra hosts the egress proxy allows, e.g. your gym's crowd API or personal calendar feed. |
| `GYMCLAW_IMAGE_TAG` | no | Docker: image tag to run. Default `main`; a commit sha rolls back. |
| `GYMCLAW_DATA_DIR`, `GYMCLAW_DB` | no | Data directory (default `data`) and DB (default `<data dir>/gymclaw.db`). |
| `GYMCLAW_ASSETS_DIR`, `GYMCLAW_CROWD_CONFIG` | no | Exercise pictures (default `assets/workout-guide`) and [crowd source](crowd.md) (default `data/crowd-config.json`). |
| `GYMCLAW_TURN_LOG_DIR` | no | Where full agent turns are logged ([turns.md](turns.md)); unset logs none. The image sets `data/turns`. |
| `GYMCLAW_BACKUP_KEEP` | no | How many archives `backup.sh` keeps; set it on the command (`GYMCLAW_BACKUP_KEEP=30 backup.sh`). Default 14. |
| `GYMCLAW_ALLOW_DIRECT_EGRESS` | no | `1` lets the container start with direct internet access (`docker run` only; compose never passes it). Local testing only. |

The stack is three containers: GymClaw (one Rust binary: bot, agent, coach, scheduler), an egress
proxy with a host allowlist, and the dashboard. Coolify steps, backups and moving hosts:
[deploy/README.md](../deploy/README.md).

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
ID you authorized and refuses any other. Tokens live in the SQLite DB in plain text.

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
set -a; source .env; set +a   # .env is not loaded automatically; set GYMCLAW_TIMEZONE there, not TZ
target/release/gymclaw migrate   # create or upgrade data/gymclaw.db
target/release/gymclaw serve     # the bot
```

The container entrypoint runs `migrate` on every start; `serve` does not.

## Things to know

- `gymclaw call <tool> '<json>'` runs any agent tool by hand, exactly as the agent would (`-` reads
  the JSON from stdin); `gymclaw tools` lists them with their schemas.
- The DB and backups hold your Google OAuth token and chat history ([SECURITY.md](../SECURITY.md)).
- Only one instance per bot token ([moving hosts](../deploy/README.md#moving-hosts)).
