#!/usr/bin/env bash
set -euo pipefail
base="$(cd "$(dirname "$0")/.." && pwd)"
app="$base/qt/build/stackhour-bench-qt"
mkdir -p "$base/screenshots"
BENCH_FIXTURE="$base/.fixture" \
BENCH_LSP="$base/node_modules/.bin/typescript-language-server" \
QT_QUICK_BACKEND=software \
xvfb-run -a -s '-screen 0 1280x800x24 -dpi 96' bash -c \
  '"$1" & pid=$!; sleep 8; import -window root "$2"; kill "$pid"; wait "$pid" || true' \
  _ "$app" "$base/screenshots/qt-qml.png"
identify "$base/screenshots/qt-qml.png"
