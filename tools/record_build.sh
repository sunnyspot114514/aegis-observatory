#!/usr/bin/env bash
# Print build evidence. Does not deploy. Does not claim the stick is running this tree.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"
echo "git_head=$(git rev-parse HEAD 2>/dev/null || echo none)"
echo "git_status_short=$(git status --porcelain | wc -l | tr -d ' ') dirty_paths"
if [ -f Cargo.lock ]; then
  if command -v sha256sum >/dev/null; then
    echo "cargo_lock_sha256=$(sha256sum Cargo.lock | awk '{print $1}')"
  else
    echo "cargo_lock_sha256=$(shasum -a 256 Cargo.lock | awk '{print $1}')"
  fi
fi
echo "package_version=$(awk -F'\"' '/^version =/{print $2; exit}' Cargo.toml)"
BIN="${1:-}"
if [ -n "$BIN" ] && [ -f "$BIN" ]; then
  if command -v sha256sum >/dev/null; then
    echo "binary_sha256=$(sha256sum "$BIN" | awk '{print $1}')"
  else
    echo "binary_sha256=$(shasum -a 256 "$BIN" | awk '{print $1}')"
  fi
  echo "binary_path=$BIN"
else
  echo "binary_sha256=not_provided"
fi
echo "deployed_on_device=unknown"
echo RECORD_BUILD_OK
