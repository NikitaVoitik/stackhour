#!/usr/bin/env bash
set -euo pipefail
base="$(cd "$(dirname "$0")/.." && pwd)"
app="$base/gpui/target/release/stackhour-bench-gpui"
mkdir -p "$base/screenshots"
capture="$(mktemp -d /tmp/stackhour-gpui-capture.XXXXXX)"
cleanup() {
  rm -rf "$capture"
}
trap cleanup EXIT

xvfb-run -a -s '-screen 0 1280x800x24 -dpi 96' bash -c '
  set -euo pipefail
  base="$1"
  app="$2"
  capture="$3"
  runtime="$capture/runtime"
  mkdir -p "$runtime"
  chmod 700 "$runtime"
  XDG_RUNTIME_DIR="$runtime" weston \
    --backend=x11-backend.so \
    --width=1280 \
    --height=800 \
    --use-pixman \
    --shell=kiosk-shell.so \
    --socket=wayland-gpui \
    --debug \
    --no-config \
    --log="$capture/weston.log" &
  weston_pid=$!
  trap "kill $weston_pid 2>/dev/null || true" EXIT
  for _ in $(seq 1 100); do
    [[ -S "$runtime/wayland-gpui" ]] && break
    sleep 0.05
  done
  XDG_RUNTIME_DIR="$runtime" WAYLAND_DISPLAY=wayland-gpui \
    BENCH_FIXTURE="$base/.fixture" \
    BENCH_LSP="$base/node_modules/.bin/typescript-language-server" \
    VK_ICD_FILENAMES=/usr/share/vulkan/icd.d/lvp_icd.x86_64.json \
    LIBGL_ALWAYS_SOFTWARE=1 \
    "$app" &
  app_pid=$!
  sleep 8
  cd "$capture"
  XDG_RUNTIME_DIR="$runtime" WAYLAND_DISPLAY=wayland-gpui weston-screenshooter
  shot="$(find "$capture" -maxdepth 1 -name "wayland-screenshot-*.png" -print -quit)"
  [[ -n "$shot" ]]
  mv "$shot" "$base/screenshots/gpui.png"
  kill "$app_pid" 2>/dev/null || true
  wait "$app_pid" 2>/dev/null || true
' _ "$base" "$app" "$capture"
identify "$base/screenshots/gpui.png"
