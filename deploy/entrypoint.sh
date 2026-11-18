#!/usr/bin/env bash
# Container start: verify egress lockdown, migrate the DB, then run the bot in the foreground.
# Environment variables: docs/self-hosting.md.
set -euo pipefail
OWNER="${GYMCLAW_TELEGRAM_USER_ID:?GYMCLAW_TELEGRAM_USER_ID required}"
: "${TELEGRAM_BOT_TOKEN:?TELEGRAM_BOT_TOKEN required}" "${OPENROUTER_API_KEY:?OPENROUTER_API_KEY required}"
[[ "$OWNER" =~ ^[0-9]+$ ]] || { echo "GYMCLAW_TELEGRAM_USER_ID must be numeric" >&2; exit 1; }

# Fail closed: the only way out must be the egress proxy. If the container can open
# a direct connection, the internal network was replaced by one with internet access.
if [[ "${GYMCLAW_ALLOW_DIRECT_EGRESS:-}" != 1 ]] && timeout 3 bash -c '</dev/tcp/1.1.1.1/443' 2>/dev/null; then
  echo "Direct internet egress is reachable; refusing to start (expected internal network + proxy)" >&2
  exit 1
fi

gymclaw migrate >/dev/null
exec gymclaw serve
