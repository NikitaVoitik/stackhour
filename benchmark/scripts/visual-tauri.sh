#!/usr/bin/env bash
set -euo pipefail
base="$(cd "$(dirname "$0")/.." && pwd)"
app="$base/src-tauri/target/release/stackhour-bench-vue-tauri"
mkdir -p "$base/screenshots"
BENCH_FIXTURE="$base/.fixture" BENCH_LSP="$base/node_modules/.bin/typescript-language-server" WEBKIT_DISABLE_COMPOSITING_MODE=1 GDK_BACKEND=x11 xvfb-run -a -s '-screen 0 1280x800x24 -dpi 96' dbus-run-session -- bash -c '"$1" & pid=$!; sleep 8; import -window root "$2"; kill "$pid"; wait "$pid" || true' _ "$app" "$base/screenshots/vue-tauri.png"
identify "$base/screenshots/vue-tauri.png"
