#!/usr/bin/env bash
# Pull a consistent event-seq batch from loopback. Does not prune. Does not copy live sqlite.
# Required: AEGIS_ARCHIVE_ROOT. Default URL is stick loopback, not the public site.
set -euo pipefail
ROOT="${AEGIS_ARCHIVE_ROOT:-}"
if [ -z "$ROOT" ]; then
  echo "set AEGIS_ARCHIVE_ROOT to the originals directory (not GitHub)"
  exit 2
fi
URL="${AEGIS_EXPORT_URL:-http://127.0.0.1:8787/ops/export}"
FROM="${AEGIS_EXPORT_FROM:-1}"
LIMIT="${AEGIS_EXPORT_LIMIT:-50}"
mkdir -p "$ROOT"
stamp=$(date -u +%Y%m%dT%H%M%SZ)
dest="$ROOT/batch-$stamp"
mkdir -p "$dest"
curl -sS --max-time 20 "$URL?from=$FROM&limit=$LIMIT" -o "$dest/batch.json"
here="$(cd "$(dirname "$0")" && pwd)"
python3 "$here/archive_verify.py" "$dest"
echo PULL_OK
echo "Prune is not performed."
