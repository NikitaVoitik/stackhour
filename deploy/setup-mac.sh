#!/bin/bash
# Compatibility wrapper for the built-in macOS setup flow:
#   ./deploy/setup-mac.sh <enrollment-code> [projectRoot ...]
set -euo pipefail
umask 077

ENROLLMENT="${1:?usage: setup-mac.sh <enrollment-code> [projectRoot ...]}"
shift
ROOTS=("$@")

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BIN="$REPO/target/release/stackhour"

# --- build ------------------------------------------------------------------
# Stackhour is one Rust binary; there is no Node runtime to check for.
if [ ! -x "$BIN" ]; then
  if ! command -v cargo >/dev/null; then
    echo "$BIN not found and cargo is not installed — install Rust from https://rustup.rs"; exit 1
  fi
  echo "building $BIN ..."
  (cd "$REPO" && cargo build --release)
fi

INIT_ARGS=(init agent "--enrollment=$ENROLLMENT" --install)
for project_root in "${ROOTS[@]}"; do INIT_ARGS+=("--project-root=$project_root"); done
"$BIN" "${INIT_ARGS[@]}"

echo
echo "done. check: $BIN doctor"
echo "for window-title project detection, also grant Accessibility to your terminal in System Settings > Privacy & Security."
