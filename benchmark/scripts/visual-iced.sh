#!/usr/bin/env bash
set -euo pipefail
base="$(cd "$(dirname "$0")/.." && pwd)"
app="$base/iced/target/release/stackhour-bench-iced"
display="${BENCH_DISPLAY:-${DISPLAY:-:0}}"
mkdir -p "$base/screenshots"

BENCH_DISPLAY="$display" DISPLAY="$display" node "$base/scripts/gpu-preflight.mjs"
DISPLAY="$display" BENCH_DISPLAY="$display" \
ICED_BACKEND=wgpu \
BENCH_FIXTURE="$base/.fixture" \
BENCH_LSP="$base/node_modules/.bin/typescript-language-server" \
bash -c \
  '"$1" & pid=$!; sleep 8; import -window root "$2"; kill "$pid"; wait "$pid" || true' \
  _ "$app" "$base/screenshots/iced.png"
identify "$base/screenshots/iced.png"
