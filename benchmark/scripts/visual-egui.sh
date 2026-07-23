#!/usr/bin/env bash
set -euo pipefail
base="$(cd "$(dirname "$0")/.." && pwd)"
app="$base/egui/target/release/stackhour-bench-egui"
display="${BENCH_DISPLAY:-${DISPLAY:-:0}}"
mkdir -p "$base/screenshots"

BENCH_DISPLAY="$display" DISPLAY="$display" node "$base/scripts/gpu-preflight.mjs"
DISPLAY="$display" BENCH_DISPLAY="$display" \
BENCH_FIXTURE="$base/.fixture" \
BENCH_LSP="$base/node_modules/.bin/typescript-language-server" \
bash -c \
  '"$1" & pid=$!; sleep 8; import -window root "$2"; kill "$pid"; wait "$pid" || true' \
  _ "$app" "$base/screenshots/egui.png"
identify "$base/screenshots/egui.png"
