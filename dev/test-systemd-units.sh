#!/bin/sh
set -eu

root=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)

command -v systemd-analyze >/dev/null 2>&1 || {
  echo "systemd-analyze is required to verify generated service contracts." >&2
  exit 1
}

systemd-analyze verify \
  "$root/deploy/systemd/stackhour-bridge.service" \
  "$root/deploy/systemd/stackhour-bridge-worker@blort.service"

echo "systemd service contracts passed"
