#!/bin/bash
# One-shot tempo setup for macOS. Run from anywhere inside the cloned repo:
#   ./deploy/setup-mac.sh http://your-server:4040 <token> [projectRoot ...]
set -euo pipefail

SERVER_URL="${1:?usage: setup-mac.sh <serverUrl> <token> [projectRoot ...]}"
TOKEN="${2:?usage: setup-mac.sh <serverUrl> <token> [projectRoot ...]}"
shift 2
ROOTS=("$@")

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
CONFIG_DIR="$HOME/.config/tempo"
CONFIG="$CONFIG_DIR/config.json"
PLIST="$HOME/Library/LaunchAgents/com.nikita.tempo-agent.plist"

# --- node check -------------------------------------------------------------
if ! command -v node >/dev/null; then
  echo "node not found — install it first (brew install node)"; exit 1
fi
NODE_MAJOR=$(node -e 'console.log(process.versions.node.split(".")[0])')
if [ "$NODE_MAJOR" -lt 22 ]; then
  echo "node >= 22 required (found $(node --version))"; exit 1
fi

# --- config -----------------------------------------------------------------
if [ -f "$CONFIG" ]; then
  echo "config exists at $CONFIG — leaving it alone"
else
  mkdir -p "$CONFIG_DIR"
  ROOTS_JSON=$(printf '"%s",' "${ROOTS[@]:-}" | sed 's/,$//; s/""//')
  cat > "$CONFIG" <<EOF
{
  "agent": {
    "serverUrl": "$SERVER_URL",
    "token": "$TOKEN",
    "projectRoots": [$ROOTS_JSON]
  }
}
EOF
  chmod 600 "$CONFIG"
  echo "wrote $CONFIG"
fi

# --- launchd ----------------------------------------------------------------
mkdir -p "$HOME/Library/LaunchAgents"
cat > "$PLIST" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>com.nikita.tempo-agent</string>
  <key>ProgramArguments</key>
  <array><string>/bin/sh</string><string>-c</string><string>exec "$REPO/bin/tempo" agent</string></array>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><true/>
  <key>StandardOutPath</key><string>/tmp/tempo-agent.log</string>
  <key>StandardErrorPath</key><string>/tmp/tempo-agent.log</string>
</dict>
</plist>
EOF
launchctl unload "$PLIST" 2>/dev/null || true
launchctl load "$PLIST"
echo "agent loaded (logs: /tmp/tempo-agent.log)"

# --- first tick: triggers the macOS Automation permission prompt ------------
echo "running one tick to trigger the Automation permission prompt (allow it)..."
"$REPO/bin/tempo" agent --once || true

echo
echo "done. check: $REPO/bin/tempo status"
echo "for window-title project detection, also grant Accessibility to node/terminal in System Settings > Privacy & Security."
