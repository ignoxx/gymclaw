# Deploying GymClaw

One Docker Compose stack, built from this repo:

- **gymclaw**: the official OpenClaw image (pinned `2026.7.1`) plus GymClaw's Python
  domain. The gateway listens on loopback only, inside the container. Telegram uses
  long polling, so no ports are published.
- **egress**: a Squid proxy that allows HTTPS `CONNECT` to an allowlist only
  (Telegram, OpenRouter, Google Calendar/OAuth, MySports, plus `EGRESS_EXTRA_HOSTS`).

- **dashboard**: the read-only web dashboard. It reads the DB through a read-only
  mount and has no internet access and no published port.

`gymclaw` sits only on an internal network, so the proxy is its single way out. It
refuses to start if a direct connection to the internet works. The container runs
as a non-root user with a read-only root filesystem, no capabilities and
`no-new-privileges`.

## State

| Volume | Mounted at | Contents |
| --- | --- | --- |
| `gymclaw-data` | `/opt/gymclaw/data` | SQLite DB (plans, sets, crowd, outbox, Google OAuth token), crowd config, previews |
| `gymclaw-workspace` | `/opt/gymclaw/openclaw` | Agent workspace: `USER.md` notes, `memory/`, inbound `media/` |
| `openclaw-state` | `/home/node/.openclaw-gymclaw` | OpenClaw config, gateway token, chat sessions, scheduled jobs |
| `gymclaw-backups` | `/backups` | `backup.sh` archives |

Repo-owned workspace files (`AGENTS.md`, `SOUL.md`, `HEARTBEAT.md`, skills) are
synced from the image on every start. `USER.md` is seeded once, then belongs to
the agent.

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
   `OPENROUTER_API_KEY`, `GYMCLAW_DASHBOARD_HOST`, optionally `GYMCLAW_MODEL` and
   `EGRESS_EXTRA_HOSTS` (for example the personal calendar feed host).
4. Add a scheduled task on the `gymclaw` service: `backup.sh`, daily (for example
   `30 3 * * *`).
5. Set a spending limit on the OpenRouter key.

Plain Docker works the same way: `docker compose up -d --build` with an `.env`
file. Compose creates the internal network itself.

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

## Moving from a NemoClaw sandbox (or another host)

Only one instance may run at a time. Two pollers on one bot token conflict, and
two runtimes would send every reminder twice.

1. Old host: pause the runtime, then stop the sandbox after the export.

   ```bash
   nemoclaw gymclaw exec -- sh -c 'cd /sandbox/.openclaw/workspace/gymclaw && scripts/gymclaw-tool runtime pause'
   deploy/export-nemoclaw.sh gymclaw-export.tar.gz
   nemoclaw gymclaw snapshot create --name before-vps   # rollback point
   nemoclaw gymclaw stop
   ```

   From another container host, use the newest `/backups` archive instead (after
   `runtime pause`).

2. New host: deploy once so the volumes exist, then stop `gymclaw` and restore:

   ```bash
   docker stop <gymclaw container>
   docker run --rm -i --network none --volumes-from <gymclaw container> \
     --entrypoint restore.sh <gymclaw image> - --replace < gymclaw-export.tar.gz
   docker start <gymclaw container>
   ```

3. Rebind the DB to this deployment, resume, and recreate the scheduled jobs:

   ```bash
   docker exec -w /opt/gymclaw <gymclaw container> sh -c '
     scripts/gymclaw-tool runtime relocate &&
     scripts/gymclaw-tool runtime resume --allow-runtime-changes --allow-messages &&
     scripts/gymclaw-tool runtime plan'
   docker exec -w /opt/gymclaw <gymclaw container> scripts/gymclaw-tool runtime sync \
     --allow-runtime-changes --allow-messages --with-crowd-poll [--allow-calendar-writes]
   ```

   `pause` revokes calendar writes, so grant them again explicitly if wanted. Chat
   history doesn't move from NemoClaw; everything GymClaw needs is in the DB and
   workspace.

4. Check: message the bot, check `runtime deliveries`, and confirm the calendar
   watcher runs (`openclaw cron list --all`).

## Backups

`backup.sh` writes a consistent `/backups/gymclaw-<UTC>.tar.gz` (SQLite online
backup, workspace, OpenClaw state) and keeps the newest 14. Archives contain the
Google OAuth token and chat history, so copy them off-host only to private storage:

```bash
docker cp <gymclaw container>:/backups/<archive> .
```
