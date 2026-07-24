#!/usr/bin/env bash
set -euo pipefail
base="$(cd "$(dirname "$0")/.." && pwd)"
app="$base/gpui/target/release/stackhour-bench-gpui"
x11_runner="$base/scripts/with-gpui-x11.sh"
display="${BENCH_DISPLAY:-${DISPLAY:-:0}}"
mkdir -p "$base/screenshots"

BENCH_DISPLAY="$display" DISPLAY="$display" node "$base/scripts/gpu-preflight.mjs"
env -u WAYLAND_DISPLAY -u XDG_SESSION_TYPE \
DISPLAY="$display" BENCH_DISPLAY="$display" \
BENCH_FIXTURE="$base/.fixture" \
BENCH_LSP="$base/node_modules/.bin/typescript-language-server" \
bash -c \
  '"$1" "$2" & pid=$!; sleep 8; import -window root "$3"; kill "$pid"; wait "$pid" || true' \
  _ "$x11_runner" "$app" "$base/screenshots/gpui.png"
identify "$base/screenshots/gpui.png"
