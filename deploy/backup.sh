#!/usr/bin/env bash
# Consistent snapshot of all GymClaw state into /backups/gymclaw-<UTC timestamp>.tar.gz.
# Runs inside the gymclaw container (Coolify scheduled task: `backup.sh`).
# Keeps the newest ${GYMCLAW_BACKUP_KEEP:-14} archives. Archives contain the
# Google OAuth token and chat history; copy them off-host only to private storage.
set -euo pipefail
ROOT=/opt/gymclaw
DEST=/backups
STAMP="$(date -u +%Y%m%dT%H%M%SZ)"
TMP="$(mktemp -d)"; trap 'rm -rf "$TMP"' EXIT
umask 077

# VACUUM INTO: consistent even while the bot is writing.
gymclaw backup "$TMP/gymclaw.db" >/dev/null

# data/turns (turn logs) and data/media (photos the owner sent) are included on purpose; same
# privacy class as the chat history in the DB.
tar -czf "$DEST/gymclaw-$STAMP.tar.gz" -C "$TMP" gymclaw.db -C "$ROOT" --exclude='data/gymclaw.db*' --exclude='data/backups' data
ls -1t "$DEST"/gymclaw-*.tar.gz | tail -n +"$(( ${GYMCLAW_BACKUP_KEEP:-14} + 1 ))" | xargs -r rm -f
echo "$DEST/gymclaw-$STAMP.tar.gz"
