#!/usr/bin/env bash
# Container start: verify egress lockdown, migrate the DB, then run the bot in the foreground.
#
# Required env: GYMCLAW_TELEGRAM_USER_ID, TELEGRAM_BOT_TOKEN, OPENROUTER_API_KEY.
# Optional env: GYMCLAW_MODEL (default z-ai/glm-5.3-flash), GYMCLAW_PROVIDER (OpenRouter provider
# tried first, default inference-net/fp4; `auto` = OpenRouter's routing), GYMCLAW_MODEL_FAST,
# GYMCLAW_MODEL_VISION, GYMCLAW_ALLOW_DIRECT_EGRESS=1 (local testing only).
set -euo pipefail
OWNER="${GYMCLAW_TELEGRAM_USER_ID:?GYMCLAW_TELEGRAM_USER_ID required}"
: "${TELEGRAM_BOT_TOKEN:?TELEGRAM_BOT_TOKEN required}" "${OPENROUTER_API_KEY:?OPENROUTER_API_KEY required}"
[[ "$OWNER" =~ ^[0-9]+$ ]] || { echo "GYMCLAW_TELEGRAM_USER_ID must be numeric" >&2; exit 1; }
export GYMCLAW_PROVIDER="${GYMCLAW_PROVIDER:-inference-net/fp4}"

# Fail closed: the only way out must be the egress proxy. If the container can open
# a direct connection, the internal network was replaced by one with internet access.
if [[ "${GYMCLAW_ALLOW_DIRECT_EGRESS:-}" != 1 ]] && timeout 3 bash -c '</dev/tcp/1.1.1.1/443' 2>/dev/null; then
  echo "Direct internet egress is reachable; refusing to start (expected internal network + proxy)" >&2
  exit 1
fi

gymclaw migrate >/dev/null
exec gymclaw serve
