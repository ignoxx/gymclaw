#!/usr/bin/env bash
# Host side: export GymClaw state from a NemoClaw sandbox into an archive that
# deploy/restore.sh understands. Read-only for the sandbox (SQLite online backup).
#
#   deploy/export-nemoclaw.sh [out.tar.gz]
#
# For a real move, run `scripts/gymclaw-tool runtime pause` in the sandbox first so
# the export carries a paused runtime and nothing fires twice.
set -euo pipefail
SANDBOX="${GYMCLAW_SANDBOX:-gymclaw}"
REPO=/sandbox/.openclaw/workspace/gymclaw
OUT="${1:-gymclaw-export-$(date -u +%Y%m%dT%H%M%SZ).tar.gz}"
TMP=/sandbox/.openclaw/gymclaw-export  # downloads must come from under /sandbox

trap 'nemoclaw "$SANDBOX" exec --no-tty -- rm -rf "$TMP" "$TMP.tar.gz" >/dev/null 2>&1' EXIT
nemoclaw "$SANDBOX" exec --no-tty -- sh -c "set -e; umask 077; rm -rf $TMP $TMP.tar.gz; mkdir -p $TMP
  $REPO/.venv/bin/python -c 'import sqlite3; s=sqlite3.connect(\"file:$REPO/data/gymclaw.db?mode=ro\", uri=True); d=sqlite3.connect(\"$TMP/gymclaw.db\"); s.backup(d); d.close()'
  tar -czf $TMP.tar.gz -C $TMP gymclaw.db -C $REPO --exclude='data/gymclaw.db*' data openclaw/USER.md openclaw/memory openclaw/media"
nemoclaw "$SANDBOX" download "$TMP.tar.gz" "$OUT" >/dev/null
chmod 600 "$OUT"
echo "$OUT"
