#!/bin/bash
# Compatibility wrapper for the built-in macOS setup flow:
#   ./deploy/setup-mac.sh <enrollment-code> [projectRoot ...]
set -euo pipefail
umask 077

ENROLLMENT="${1:?usage: setup-mac.sh <enrollment-code> [projectRoot ...]}"
shift
ROOTS=("$@")

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

# --- node check -------------------------------------------------------------
if ! command -v node >/dev/null; then
  echo "node not found — install it first (brew install node)"; exit 1
fi
NODE_BIN=$(command -v node)
NODE_MAJOR=$("$NODE_BIN" -e 'console.log(process.versions.node.split(".")[0])')
if [ "$NODE_MAJOR" -lt 22 ]; then
  echo "node >= 22 required (found $("$NODE_BIN" --version))"; exit 1
fi

INIT_ARGS=(init agent "--enrollment=$ENROLLMENT" --install)
for project_root in "${ROOTS[@]}"; do INIT_ARGS+=("--project-root=$project_root"); done
"$REPO/bin/stackhour" "${INIT_ARGS[@]}"

echo
echo "done. check: $REPO/bin/stackhour doctor"
echo "for window-title project detection, also grant Accessibility to node/terminal in System Settings > Privacy & Security."
