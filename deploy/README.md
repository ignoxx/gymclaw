# Deploying GymClaw

One Docker Compose stack. `gymclaw` and `dashboard` run the same image, which GitHub Actions
([`image.yml`](../.github/workflows/image.yml)) builds on every push to `main` and pushes to
`ghcr.io/ignoxx/gymclaw:main` (plus a `:<commit sha>` tag). The host only pulls it.


- **gymclaw**: one Rust binary (`gymclaw serve`): Telegram long polling, the agent
  (OpenRouter), the coach, the scheduler and the outbox. No ports are published.
- **egress**: a Squid proxy that allows HTTPS `CONNECT` to an allowlist only
  (Telegram, OpenRouter, Google Calendar/OAuth, MySports, plus `EGRESS_EXTRA_HOSTS`).

- **dashboard**: the web dashboard. It reads the DB through a read-only connection; its
  only writes are edits to the agent's notes (Memory tab). No internet access and no
  published port.

`gymclaw` sits only on an internal network, so the proxy is its single way out. It
refuses to start if a direct connection to the internet works. The container runs
as a non-root user with a read-only root filesystem, no capabilities and
`no-new-privileges`.

## State

| Volume | Mounted at | Contents |
| --- | --- | --- |
| `gymclaw-data` | `/opt/gymclaw/data` | SQLite DB (plans, sets, crowd, outbox, chat history, agent notes, Google OAuth token), crowd config, inbound `media/`, `turns/` turn logs (JSONL, format in [`src/agent/turnlog.rs`](../src/agent/turnlog.rs)), `heartbeat` |
| `gymclaw-workspace` | `/opt/gymclaw/openclaw` (read-only) | Old OpenClaw workspace. Its `USER.md` "Owner notes" are imported into the DB once on first start |
| `gymclaw-backups` | `/backups` | `backup.sh` archives |

The old `openclaw-state` volume is no longer mounted; delete it once the Rust runtime has run fine
for a while.

## Coolify

1. Create a **Docker Compose** application from this repo (compose file
   `/docker-compose.yaml`). Don't deploy yet.
2. On the server, pre-create the application's network as **internal**. Coolify
   attaches every compose service to a network named after the application UUID,
   and only creates it if it's missing:

   ```bash
   docker network create --internal --attachable <application-uuid>
   ```

   Without this, `gymclaw` would have direct internet access, and it refuses to
   start.
3. Set the environment variables: `GYMCLAW_TELEGRAM_USER_ID`, `TELEGRAM_BOT_TOKEN`,
   `OPENROUTER_API_KEY`, `GYMCLAW_DASHBOARD_HOST`, optionally `GYMCLAW_MODEL`, `TZ`,
   `GYMCLAW_PROVIDER` (default `inference-net/fp4`) and `EGRESS_EXTRA_HOSTS` (for example the personal calendar feed host).
4. Turn off **Escape special characters in labels** (Advanced settings) so the
   dashboard's Traefik labels get `GYMCLAW_DASHBOARD_HOST` filled in.
5. Add a scheduled task on the `gymclaw` service: `backup.sh`, daily (for example
   `30 3 * * *`).
6. Set a spending limit on the OpenRouter key.
7. Deploy from CI instead of on push, so Coolify never pulls an image that isn't built yet:
   - Turn off **Auto Deploy** (Advanced settings) on the application.
   - Create an API token (Keys & Tokens, deploy permission) and copy the app's **Deploy
     Webhook** URL (Webhooks).
   - Add them as GitHub Actions secrets `COOLIFY_TOKEN` and `COOLIFY_DEPLOY_WEBHOOK`.
   - After the first image push, make the `gymclaw` GHCR package public (package settings),
     or add a GHCR registry login on the server.

   Roll back by setting `GYMCLAW_IMAGE_TAG` to an older commit sha and redeploying.

Plain Docker works the same way: `docker compose up -d` with an `.env` file. To run a
local build instead, `docker build -f deploy/Dockerfile -t ghcr.io/ignoxx/gymclaw:local .` and
set `GYMCLAW_IMAGE_TAG=local` plus `--pull never`. Compose creates the internal network itself.

## Dashboard on your tailnet

Traefik serves the dashboard at `GYMCLAW_DASHBOARD_HOST` with a normal Let's Encrypt
certificate, but only to Tailscale source addresses (`100.64.0.0/10`); everyone else
gets a 403. The name resolves publicly to the server's public IP, which keeps the
certificate's HTTP challenge working. Tailnet devices reach that IP through
Tailscale, by having the server advertise its own public IP as a subnet route:

```bash
tailscale set --advertise-routes=<public-ip>/32 --advertise-exit-node   # keep existing flags
```

Approve the route in the Tailscale admin console. Linux clients also need
`--accept-routes`; macOS, iOS and Android accept routes by default.

## Moving hosts

Only one instance may run at a time. Two pollers on one bot token conflict, and
two runtimes would send every reminder twice.

1. Old host: `gymclaw call runtime_pause` (or ask the bot to pause), run `backup.sh`, copy the newest `/backups` archive
   off-host, then stop the old deployment.

2. New host: deploy once so the volumes exist, then stop `gymclaw` and restore:

   ```bash
   docker stop <gymclaw container>
   docker run --rm -i --network none --volumes-from <gymclaw container> \
     --entrypoint restore.sh <gymclaw image> - --replace < gymclaw-backup.tar.gz
   docker start <gymclaw container>
   ```

3. Resume (the scheduler runs in-process, there are no jobs to recreate):

   ```bash
   docker exec <gymclaw container> gymclaw call runtime_resume '{"owner_asked": true, "crowd_polling": true}'
   ```

   `pause` revokes calendar writes; add `"calendar_writes": true` to grant them again if wanted.

4. Check: message the bot, send `/status`, and `gymclaw call deliveries`.

Google auth travels with the DB. If it's gone (`CALENDAR_AUTH_REQUIRED`), re-run
`docker compose exec -it gymclaw gymclaw calendar-auth --calendar-id <id>`
([setup](../docs/self-hosting.md#first-run)).

## Backups

`backup.sh` writes a consistent `/backups/gymclaw-<UTC>.tar.gz` (`gymclaw backup` of the DB,
`data/`, the old workspace) and keeps the newest 14. Archives contain the
Google OAuth token and chat history, so copy them off-host only to private storage:

```bash
docker cp <gymclaw container>:/backups/<archive> .
```
