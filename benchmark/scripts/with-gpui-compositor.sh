#!/usr/bin/env bash
set -euo pipefail

if [[ $# -lt 1 ]]; then
  echo "usage: $0 command [args...]" >&2
  exit 2
fi

runtime_root="$(mktemp -d /tmp/stackhour-gpui-wayland.XXXXXX)"
runtime="$runtime_root/runtime"
mkdir -p "$runtime"
chmod 700 "$runtime"

cleanup_outer() {
  rm -rf "$runtime_root"
}
trap cleanup_outer EXIT

xvfb-run -a -s '-screen 0 1280x800x24 -dpi 96' bash -c '
  set -euo pipefail
  runtime="$1"
  shift
  XDG_RUNTIME_DIR="$runtime" weston \
    --backend=x11-backend.so \
    --width=1280 \
    --height=800 \
    --use-pixman \
    --shell=kiosk-shell.so \
    --socket=wayland-gpui \
    --no-config \
    --log="$runtime/weston.log" &
  weston_pid=$!
  cleanup_inner() {
    kill "$weston_pid" 2>/dev/null || true
    wait "$weston_pid" 2>/dev/null || true
  }
  trap cleanup_inner EXIT
  for _ in $(seq 1 100); do
    [[ -S "$runtime/wayland-gpui" ]] && break
    sleep 0.05
  done
  [[ -S "$runtime/wayland-gpui" ]]
  XDG_RUNTIME_DIR="$runtime" WAYLAND_DISPLAY=wayland-gpui \
    VK_ICD_FILENAMES=/usr/share/vulkan/icd.d/lvp_icd.x86_64.json \
    LIBGL_ALWAYS_SOFTWARE=1 \
    "$@"
' _ "$runtime" "$@"
