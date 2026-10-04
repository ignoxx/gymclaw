#!/usr/bin/env bash
# Restore an archive from backup.sh or export-nemoclaw.sh into this deployment's
# volumes. Run in a one-off container while the gymclaw service is stopped:
#
#   docker run --rm -i --network none --volumes-from <stopped gymclaw container> \
#     --entrypoint restore.sh <gymclaw image> - [--replace] < archive.tar.gz
#
# <archive> is a path inside the container (e.g. /backups/...) or - for stdin.
# --replace is required when a DB already exists; the current state is backed up first.
set -euo pipefail
ARCHIVE="${1:?usage: restore.sh <archive|-> [--replace]}"
ROOT=/opt/gymclaw
STATE=/home/node/.openclaw-gymclaw
if [[ -e "$ROOT/data/gymclaw.db" ]]; then
  [[ "${2:-}" == --replace ]] || { echo "GymClaw DB already exists; pass --replace" >&2; exit 1; }
  echo "Backed up current state: $(backup.sh)"
fi

TMP="$(mktemp -d)"; trap 'rm -rf "$TMP"' EXIT
tar -xzf "$ARCHIVE" -C "$TMP"
[[ -f "$TMP/gymclaw.db" ]] || { echo "Archive has no gymclaw.db" >&2; exit 1; }
umask 077
rm -f "$ROOT"/data/gymclaw.db-wal "$ROOT"/data/gymclaw.db-shm
cp "$TMP/gymclaw.db" "$ROOT/data/gymclaw.db"
[[ -d "$TMP/data" ]] && cp -R "$TMP/data/." "$ROOT/data/"
[[ -d "$TMP/openclaw" ]] && cp -R "$TMP/openclaw/." "$ROOT/openclaw/"
# OpenClaw state only exists in container backups; a NemoClaw export starts fresh.
if [[ -d "$TMP/.openclaw-gymclaw" ]]; then
  cp -R "$TMP/.openclaw-gymclaw/." "$STATE/"
  if [[ -f "$TMP/openclaw.sqlite" ]]; then
    mkdir -p "$STATE/state"
    rm -f "$STATE"/state/openclaw.sqlite-wal "$STATE"/state/openclaw.sqlite-shm
    cp "$TMP/openclaw.sqlite" "$STATE/state/openclaw.sqlite"
  fi
fi
cd "$ROOT" && .venv/bin/python -m gymclaw.cli db init >/dev/null
echo "Restored $ARCHIVE"
