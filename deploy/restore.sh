#!/usr/bin/env bash
# Restore an archive from backup.sh (new or OpenClaw-era) into this deployment's
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
# The OpenClaw workspace only matters for the one-time USER.md notes import; skip it if read-only.
if [[ -d "$TMP/openclaw" && -w "$ROOT/openclaw" ]]; then
  cp -R "$TMP/openclaw/." "$ROOT/openclaw/"
fi
cd "$ROOT" && gymclaw migrate >/dev/null
echo "Restored $ARCHIVE"
