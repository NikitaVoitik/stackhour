#!/usr/bin/env bash
set -euo pipefail

if [[ $# -lt 1 ]]; then
  echo "usage: $0 command [args...]" >&2
  exit 2
fi

"$@" &
app_pid=$!

cleanup() {
  kill "$app_pid" 2>/dev/null || true
  wait "$app_pid" 2>/dev/null || true
}
trap cleanup EXIT INT TERM

window_id=
for _ in $(seq 1 200); do
  window_id=$(xdotool search --onlyvisible --pid "$app_pid" --name '^Stackhour UI Benchmark$' 2>/dev/null | head -1 || true)
  [[ -n "$window_id" ]] && break
  if ! kill -0 "$app_pid" 2>/dev/null; then
    wait "$app_pid"
  fi
  sleep 0.05
done
if [[ -z "$window_id" ]]; then
  echo "GPUI X11 window did not become visible" >&2
  exit 3
fi

window_hex=$(printf '0x%x' "$window_id")
wmctrl -i -r "$window_hex" -b add,fullscreen

geometry_ok=0
for _ in $(seq 1 100); do
  geometry=$(xwininfo -id "$window_id" 2>/dev/null || true)
  width=$(sed -n 's/^[[:space:]]*Width: //p' <<<"$geometry")
  height=$(sed -n 's/^[[:space:]]*Height: //p' <<<"$geometry")
  if [[ "$width" == 1280 && "$height" == 800 ]]; then
    geometry_ok=1
    break
  fi
  sleep 0.05
done
if ((geometry_ok == 0)); then
  echo "GPUI X11 window failed to reach 1280x800" >&2
  xwininfo -id "$window_id" >&2 || true
  exit 4
fi

set +e
wait "$app_pid"
status=$?
set -e
trap - EXIT INT TERM
exit "$status"
