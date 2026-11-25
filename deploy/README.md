# Deploying gymclaw

One Docker Compose stack. `gymclaw` and `dashboard` run the same image, which GitHub Actions
([`image.yml`](../.github/workflows/image.yml)) builds once Tests pass on `main` and pushes to
`ghcr.io/ignoxx/gymclaw:main` (plus a `:<commit sha>` tag). The host only pulls it.

- **gymclaw**: one Rust binary (`gymclaw serve`): Telegram long polling, the agent
  (OpenRouter), the coach, the scheduler and the outbox. No ports are published.
- **egress**: a Squid proxy that allows HTTPS `CONNECT` to an allowlist only
  (Telegram, OpenRouter, MySports, plus `EGRESS_EXTRA_HOSTS`).
- **dashboard**: the web dashboard / Telegram Mini App. Reads the DB through a read-only
  connection; writes go through `DASHBOARD_TOOLS` and `POST /api/coach` in `src/web/mod.rs`.
  On non-loopback hosts every `/api/*` and `/export.csv` request is authenticated via Telegram
  Mini App `initData` HMAC-SHA256 (verified against `TELEGRAM_BOT_TOKEN` and
  `GYMCLAW_TELEGRAM_USER_ID`). `/calendar/<token>.ics` serves a read-only workout calendar feed
  gated by its secret token.

`gymclaw` sits only on an internal network, so the proxy is its single way out. It refuses to
start if a direct connection to the internet works. The container runs as a non-root user with a
read-only root filesystem, no capabilities and `no-new-privileges`.

## State

| Volume | Mounted at | Contents |
| --- | --- | --- |
| `gymclaw-data` | `/opt/gymclaw/data` | SQLite DB (plans, sets, crowd, outbox, chat history, agent notes, private calendar feed links), crowd config, inbound `media/`, `turns/` turn logs (JSONL, format in [`src/agent/turnlog.rs`](../src/agent/turnlog.rs)), `heartbeat` |
| `gymclaw-backups` | `/backups` | `backup.sh` archives |

## Plain Docker Compose

```bash
cp .env.example .env          # fill TELEGRAM_BOT_TOKEN, OPENROUTER_API_KEY, GYMCLAW_TELEGRAM_USER_ID
docker compose up -d
docker compose exec gymclaw gymclaw doctor
```

To expose the dashboard behind your own reverse proxy (Caddy, nginx, Traefik, etc.), copy
`docker-compose.override.example.yaml` to `docker-compose.override.yaml`, set
`GYMCLAW_DASHBOARD_HOST` to your HTTPS hostname, and proxy HTTPS traffic to `127.0.0.1:8766`.

To run a local build instead of pulling GHCR:

```bash
docker build -f deploy/Dockerfile -t ghcr.io/ignoxx/gymclaw:local .
GYMCLAW_IMAGE_TAG=local docker compose up -d --pull never
```

Roll back by setting `GYMCLAW_IMAGE_TAG` to an older commit sha and redeploying.

Note for PaaS / Compose orchestrators that attach every service to a shared default network:
make sure `gymclaw` stays on an `--internal` network so it cannot bypass `egress` (otherwise
`entrypoint.sh` refuses to start).

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
   docker exec <gymclaw container> gymclaw call runtime_resume '{"owner_asked": true}'
   ```

   `runtime_pause` keeps your existing crowd-polling grant; pass `"crowd_polling": true` if you
   want to enable it now.

4. Check: `gymclaw doctor`, message the bot, send `/status`, and `gymclaw call deliveries`.

## Backups

Schedule `docker compose exec -T gymclaw backup.sh` daily (e.g. via cron or systemd timer).
`backup.sh` writes a consistent `/backups/gymclaw-<UTC>.tar.gz` (`gymclaw backup` of the DB,
`data/`) and keeps the newest 14. Archives contain private calendar feed links and chat history, so copy
them off-host only to private storage:

```bash
docker cp <gymclaw container>:/backups/<archive> .
```
