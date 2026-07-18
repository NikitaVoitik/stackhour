import { accessSync, constants, statSync } from 'node:fs';
import { delimiter, dirname, isAbsolute, join } from 'node:path';

export const SERVICE_NAME = 'stackhour-bridge.service';
export const LAUNCHD_LABEL = 'com.stackhour.bridge-worker';

export function validateCoordinatorConfig(config) {
  const errors = [];
  if (!config || typeof config !== 'object') return ['config must be an object'];
  if (!config.token || typeof config.token !== 'string') errors.push('token is required');
  if (!Number.isSafeInteger(config.chatId)) errors.push('chatId must be an integer');
  if (!['gcp', 'mac'].includes(config.defaultTarget)) errors.push('defaultTarget must be gcp or mac');
  for (const name of ['gcp', 'mac']) if (!config.targets?.[name]) errors.push(`targets.${name} is required`);
  const local = config.targets?.gcp;
  if (local) {
    for (const key of ['cwd', 'claudeBin', 'codexBin']) if (!local[key]) errors.push(`targets.gcp.${key} is required`);
    if (!['default', 'bypassPermissions'].includes(local.permissionMode || 'default')) errors.push('targets.gcp.permissionMode is invalid');
  }
  return errors;
}

export function validateWorkerConfig(config) {
  const errors = [];
  if (!config || typeof config !== 'object') return ['config must be an object'];
  for (const key of ['gcpSsh', 'gcpKey', 'remoteDir', 'remoteNode', 'claudeBin', 'codexBin', 'cwd']) {
    if (!config[key] || typeof config[key] !== 'string') errors.push(`${key} is required`);
  }
  if (!['default', 'bypassPermissions'].includes(config.permissionMode || 'default')) errors.push('permissionMode is invalid');
  return errors;
}

export function findExecutable(command, pathValue = process.env.PATH || '') {
  if (!command) return null;
  const candidates = isAbsolute(command) ? [command] : pathValue.split(delimiter).filter(Boolean).map((dir) => join(dir, command));
  for (const candidate of candidates) {
    try { accessSync(candidate, constants.X_OK); if (statSync(candidate).isFile()) return candidate; } catch {}
  }
  return null;
}

export function mergedPath(...values) {
  const seen = new Set();
  const parts = [];
  for (const value of values) for (const part of String(value || '').split(delimiter)) {
    if (part && !seen.has(part)) { seen.add(part); parts.push(part); }
  }
  return parts.join(delimiter);
}

export function binaryPath(binary) {
  return isAbsolute(binary) ? dirname(binary) : '';
}

function systemdQuote(value) {
  return `"${String(value).replace(/\\/g, '\\\\').replace(/"/g, '\\"')}"`;
}

export function renderSystemdUnit({ nodePath, runtimeDir, home, pathValue }) {
  return `[Unit]
Description=Stackhour Telegram bridge coordinator for Claude Code and Codex
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
WorkingDirectory=${systemdQuote(runtimeDir)}
ExecStart=${systemdQuote(nodePath)} ${systemdQuote(join(runtimeDir, 'coordinator.mjs'))}
Restart=always
RestartSec=5
UMask=0077
Environment=${systemdQuote(`HOME=${home}`)}
Environment=${systemdQuote(`PATH=${pathValue}`)}

[Install]
WantedBy=default.target
`;
}

export function xmlEscape(value) {
  return String(value).replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;').replace(/"/g, '&quot;').replace(/'/g, '&apos;');
}

export function renderLaunchAgent({ nodePath, runtimeDir, home, pathValue }) {
  const x = xmlEscape;
  return `<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>${LAUNCHD_LABEL}</string>
  <key>ProgramArguments</key>
  <array>
    <string>${x(nodePath)}</string>
    <string>${x(join(runtimeDir, 'worker.mjs'))}</string>
  </array>
  <key>WorkingDirectory</key>
  <string>${x(runtimeDir)}</string>
  <key>EnvironmentVariables</key>
  <dict>
    <key>HOME</key>
    <string>${x(home)}</string>
    <key>PATH</key>
    <string>${x(pathValue)}</string>
  </dict>
  <key>RunAtLoad</key>
  <true/>
  <key>KeepAlive</key>
  <true/>
  <key>ThrottleInterval</key>
  <integer>10</integer>
  <key>StandardOutPath</key>
  <string>${x(join(runtimeDir, 'worker.launchd.out.log'))}</string>
  <key>StandardErrorPath</key>
  <string>${x(join(runtimeDir, 'worker.launchd.err.log'))}</string>
</dict>
</plist>
`;
}

export function shellQuote(value) {
  return `'${String(value).replace(/'/g, `'"'"'`)}'`;
}
