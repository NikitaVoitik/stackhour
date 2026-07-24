#!/usr/bin/env bash
set -euo pipefail
base="$(cd "$(dirname "$0")/.." && pwd)"
app="$base/gpui/target/release/stackhour-bench-gpui"
display="${BENCH_DISPLAY:-${DISPLAY:-:0}}"
mkdir -p "$base/screenshots"

BENCH_DISPLAY="$display" DISPLAY="$display" node "$base/scripts/gpu-preflight.mjs"
env -u WAYLAND_DISPLAY -u XDG_SESSION_TYPE \
DISPLAY="$display" BENCH_DISPLAY="$display" \
BENCH_FIXTURE="$base/.fixture" \
BENCH_LSP="$base/node_modules/.bin/typescript-language-server" \
bash -c \
  '"$1" & pid=$!; sleep 8; import -window root "$2"; kill "$pid"; wait "$pid" || true' \
  _ "$app" "$base/screenshots/gpui.png"
identify "$base/screenshots/gpui.png"
