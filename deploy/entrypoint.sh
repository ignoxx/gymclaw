#!/usr/bin/env bash
# Container start: verify egress lockdown, sync workspace, apply declarative
# OpenClaw config, migrate the DB, then run the gateway in the foreground.
#
# Required env: GYMCLAW_TELEGRAM_USER_ID, TELEGRAM_BOT_TOKEN, OPENROUTER_API_KEY.
# Optional env: GYMCLAW_MODEL (default z-ai/glm-5.3-flash), GYMCLAW_PROVIDER (OpenRouter
# provider tried first, default inference-net/fp4; `auto` = OpenRouter's default routing),
# GYMCLAW_ALLOW_DIRECT_EGRESS=1 (local testing only).
set -euo pipefail
ROOT=/opt/gymclaw
OWNER="${GYMCLAW_TELEGRAM_USER_ID:?GYMCLAW_TELEGRAM_USER_ID required}"
: "${TELEGRAM_BOT_TOKEN:?TELEGRAM_BOT_TOKEN required}" "${OPENROUTER_API_KEY:?OPENROUTER_API_KEY required}"
[[ "$OWNER" =~ ^[0-9]+$ ]] || { echo "GYMCLAW_TELEGRAM_USER_ID must be numeric" >&2; exit 1; }

# Fail closed: the only way out must be the egress proxy. If the container can open
# a direct connection, the internal network was replaced by one with internet access.
if [[ "${GYMCLAW_ALLOW_DIRECT_EGRESS:-}" != 1 ]] && "$ROOT/.venv/bin/python" - <<'PY'
import socket, sys
try:
    socket.create_connection(("1.1.1.1", 443), timeout=3).close()
except OSError:
    sys.exit(1)
PY
then
  echo "Direct internet egress is reachable; refusing to start (expected internal network + proxy)" >&2
  exit 1
fi

# Repo-owned workspace files always follow the image. USER.md and memory/media
# are written by the agent at runtime, so they are only seeded once.
WS="$ROOT/openclaw"
cp -f "$ROOT"/openclaw.dist/{AGENTS.md,SOUL.md,HEARTBEAT.md,README.md} "$WS/"
rm -rf "$WS/skills" && cp -R "$ROOT/openclaw.dist/skills" "$WS/skills"
[[ -e "$WS/USER.md" ]] || cp "$ROOT/openclaw.dist/USER.md" "$WS/USER.md"
mkdir -p "$WS/memory" "$WS/media"

MODEL="openrouter/${GYMCLAW_MODEL:-z-ai/glm-5.3-flash}"
PLUGINS="$ROOT/openclaw.dist/plugins"
# Values are JSON-encoded by Python; nothing here is interpolated into JSON by hand.
PROVIDER="${GYMCLAW_PROVIDER:-inference-net/fp4}"
CONFIG="$(OWNER="$OWNER" MODEL="$MODEL" PROVIDER="$PROVIDER" PLUGINS="$PLUGINS" WS="$WS" ROOT="$ROOT" "$ROOT/.venv/bin/python" - <<'PY'
import json, os
e = os.environ
owner, model = e["OWNER"], e["MODEL"]
# Sent verbatim in each OpenRouter request. Falls back to other providers if the pinned one is down.
extra_body = {"reasoning": {"effort": "minimal"}}
if e["PROVIDER"] != "auto":
    extra_body["provider"] = {"order": [e["PROVIDER"]], "allow_fallbacks": True}
settings = {
    "gateway.mode": "local",
    "gateway.bind": "loopback",
    "gateway.port": 18789,
    "gateway.reload.mode": "hot",
    "update.checkOnStart": False,
    "agents.defaults.workspace": e["WS"],
    "agents.defaults.skipBootstrap": True,
    "agents.defaults.model.primary": model,
    "agents.defaults.models": {model: {"params": {"maxTokens": 16384, "extra_body": extra_body}}},
    "agents.defaults.timeoutSeconds": 600,
    "agents.defaults.thinkingDefault": "off",
    "agents.defaults.reasoningDefault": "off",
    "agents.defaults.compaction": {"mode": "safeguard", "timeoutSeconds": 120, "maxHistoryShare": 0.35,
        "recentTurnsPreserve": 1, "qualityGuard": {"enabled": True, "maxRetries": 0},
        "notifyUser": True, "truncateAfterCompaction": True},
    "channels.telegram.enabled": True,
    "channels.telegram.configWrites": False,
    "channels.telegram.accounts.default.enabled": True,
    "channels.telegram.accounts.default.allowFrom": [owner],
    "channels.telegram.accounts.default.dmPolicy": "allowlist",
    "channels.telegram.accounts.default.groupPolicy": "disabled",
    "channels.telegram.capabilities.inlineButtons": "dm",
    "channels.telegram.actions.reactions": True,
    "channels.telegram.reactionLevel": "extensive",
    "channels.telegram.streaming.mode": "off",
    "commands.ownerAllowFrom": [f"telegram:{owner}"],
    "tools.web.search.enabled": False,
    "messages.suppressToolErrors": True,
    "plugins.allow": ["telegram", "openrouter", "memory-core", "gymclaw-failure-details", "gymclaw-coach"],
    "plugins.load.paths": [f"{e['PLUGINS']}/failure-details", f"{e['PLUGINS']}/coach"],
    "plugins.entries.bonjour.enabled": False,
    "plugins.entries.telegram.enabled": True,
    "plugins.entries.gymclaw-failure-details.enabled": True,
    "plugins.entries.gymclaw-failure-details.hooks.allowConversationAccess": True,
    "plugins.entries.gymclaw-coach.enabled": True,
    "plugins.entries.gymclaw-coach.config": {"ownerId": owner, "tool": f"{e['ROOT']}/scripts/gymclaw-tool"},
}
print(json.dumps([{"path": k, "value": v} for k, v in settings.items()]))
PY
)"
openclaw --profile gymclaw config set --batch-json "$CONFIG" >/dev/null
# Loopback gateway token shared by the gateway and in-container CLI calls (GymClaw
# callbacks, coach plugin). Generated once and kept in the state volume.
if ! openclaw --profile gymclaw config get gateway.auth.token >/dev/null 2>&1; then
  openclaw --profile gymclaw config set gateway.auth.mode token >/dev/null
  openclaw --profile gymclaw config set gateway.auth.token "$(head -c 32 /dev/urandom | od -An -tx1 | tr -d ' \n')" >/dev/null
fi

cd "$ROOT"
.venv/bin/python -m gymclaw.cli db init >/dev/null
exec openclaw --profile gymclaw gateway
