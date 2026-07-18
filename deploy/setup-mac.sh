#!/bin/bash
# One-shot tempo setup for macOS. Run from anywhere inside the cloned repo:
#   ./deploy/setup-mac.sh http://your-server:4040 <token> [projectRoot ...]
set -euo pipefail
umask 077

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
NODE_BIN=$(command -v node)
NODE_MAJOR=$("$NODE_BIN" -e 'console.log(process.versions.node.split(".")[0])')
if [ "$NODE_MAJOR" -lt 22 ]; then
  echo "node >= 22 required (found $("$NODE_BIN" --version))"; exit 1
fi

# --- config -----------------------------------------------------------------
if [ -f "$CONFIG" ]; then
  echo "config exists at $CONFIG — leaving it alone"
else
  mkdir -p "$CONFIG_DIR"
  CONFIG_TMP="$CONFIG.tmp"
  rm -f "$CONFIG_TMP"
  "$NODE_BIN" -e 'const [serverUrl, token, ...projectRoots] = process.argv.slice(1);
    process.stdout.write(JSON.stringify({ agent: { serverUrl, token, projectRoots } }, null, 2) + "\n")' \
    "$SERVER_URL" "$TOKEN" "${ROOTS[@]}" > "$CONFIG_TMP"
  chmod 600 "$CONFIG_TMP"
  mv "$CONFIG_TMP" "$CONFIG"
  echo "wrote $CONFIG"
fi

# --- launchd ----------------------------------------------------------------
mkdir -p "$HOME/Library/LaunchAgents"
PLIST_TMP="$PLIST.tmp"
rm -f "$PLIST_TMP"
NODE_BIN_XML=$("$NODE_BIN" -e 'const v = process.argv[1]; process.stdout.write(v.replace(/[&<>]/g, c => ({"&":"&amp;","<":"&lt;",">":"&gt;"})[c]))' "$NODE_BIN")
CLI_XML=$("$NODE_BIN" -e 'const v = process.argv[1]; process.stdout.write(v.replace(/[&<>]/g, c => ({"&":"&amp;","<":"&lt;",">":"&gt;"})[c]))' "$REPO/src/cli.js")
cat > "$PLIST_TMP" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>com.nikita.tempo-agent</string>
  <key>ProgramArguments</key>
  <array>
    <string>$NODE_BIN_XML</string>
    <string>--experimental-sqlite</string>
    <string>--no-warnings</string>
    <string>$CLI_XML</string>
    <string>agent</string>
  </array>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><true/>
  <key>StandardOutPath</key><string>/tmp/tempo-agent.log</string>
  <key>StandardErrorPath</key><string>/tmp/tempo-agent.log</string>
</dict>
</plist>
EOF
mv "$PLIST_TMP" "$PLIST"
launchctl unload "$PLIST" 2>/dev/null || true

# --- first tick: triggers the macOS Automation permission prompt ------------
echo "running one tick to trigger the Automation permission prompt (allow it)..."
"$NODE_BIN" --experimental-sqlite --no-warnings "$REPO/src/cli.js" agent --once || true

launchctl load "$PLIST"
echo "agent loaded (logs: /tmp/tempo-agent.log)"

echo
echo "done. check: $REPO/bin/tempo status"
echo "for window-title project detection, also grant Accessibility to node/terminal in System Settings > Privacy & Security."
