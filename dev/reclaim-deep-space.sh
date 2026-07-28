#!/bin/sh
set -eu

ROOT=$(CDPATH='' cd -- "$(dirname "$0")/.." && pwd)
DEBUG_DIR="$ROOT/target/debug"
MIN_FREE_KB=${STACKHOUR_DEEP_MIN_FREE_KB:-12582912}

available_kb=$(df -Pk "$ROOT" | awk 'NR == 2 { print $4 }')
case "$available_kb:$MIN_FREE_KB" in
    *[!0-9:]* | :* | *:) echo "could not read available disk space" >&2; exit 1 ;;
esac

if [ "$available_kb" -ge "$MIN_FREE_KB" ]; then
    printf 'deep-space available_kb=%s action=keep\n' "$available_kb"
    exit 0
fi

case "$DEBUG_DIR" in
    "$ROOT"/target/debug) ;;
    *) echo "refusing to clean an unexpected build directory" >&2; exit 1 ;;
esac

if [ -d "$DEBUG_DIR" ]; then
    # target/debug contains only Cargo output. Keep verification tools and
    # reports in their separate target subdirectories.
    find "$DEBUG_DIR" -mindepth 1 -delete
fi

available_kb=$(df -Pk "$ROOT" | awk 'NR == 2 { print $4 }')
printf 'deep-space available_kb=%s action=reclaimed-cargo-debug\n' "$available_kb"
if [ "$available_kb" -lt "$MIN_FREE_KB" ]; then
    echo "Deep verification needs at least 12 GiB of free disk space" >&2
    exit 1
fi
