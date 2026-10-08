#!/usr/bin/env bash
# Consistent snapshot of all GymClaw state into /backups/gymclaw-<UTC timestamp>.tar.gz.
# Runs inside the gymclaw container (Coolify scheduled task: `backup.sh`).
# Keeps the newest ${GYMCLAW_BACKUP_KEEP:-14} archives. Archives contain the
# Google OAuth token and chat history; copy them off-host only to private storage.
set -euo pipefail
ROOT=/opt/gymclaw
STATE=/home/node/.openclaw-gymclaw
DEST=/backups
STAMP="$(date -u +%Y%m%dT%H%M%SZ)"
TMP="$(mktemp -d)"; trap 'rm -rf "$TMP"' EXIT
umask 077

# SQLite online backup API: consistent even while the gateway is writing.
"$ROOT/.venv/bin/python" - "$TMP" <<'PY'
import sqlite3, sys
from pathlib import Path
out = Path(sys.argv[1])
for name, src in {"gymclaw.db": "/opt/gymclaw/data/gymclaw.db",
                  "openclaw.sqlite": "/home/node/.openclaw-gymclaw/state/openclaw.sqlite"}.items():
    if Path(src).exists():
        with sqlite3.connect(f"file:{src}?mode=ro", uri=True) as s, sqlite3.connect(out / name) as d:
            s.backup(d)
PY

# data/turns (turn logs, openclaw/plugins/turn-log) is included on purpose: it is the eval/latency
# dataset and small (a few KB per turn). Same privacy class as the chat history above.
tar -czf "$DEST/gymclaw-$STAMP.tar.gz" \
  -C "$TMP" . \
  -C "$ROOT" --exclude='data/gymclaw.db*' --exclude='data/backups' data openclaw \
  -C /home/node --exclude='.openclaw-gymclaw/state/openclaw.sqlite*' --exclude='.openclaw-gymclaw/logs' .openclaw-gymclaw
ls -1t "$DEST"/gymclaw-*.tar.gz | tail -n +"$(( ${GYMCLAW_BACKUP_KEEP:-14} + 1 ))" | xargs -r rm -f
echo "$DEST/gymclaw-$STAMP.tar.gz"
