import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { execFileSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';

const REPO_ROOT = path.dirname(path.dirname(fileURLToPath(import.meta.url)));

function atomicWrite(file, content, mode = 0o644) {
  fs.mkdirSync(path.dirname(file), { recursive: true });
  const tmp = `${file}.${process.pid}.tmp`;
  try {
    fs.rmSync(tmp, { force: true });
    const fd = fs.openSync(tmp, 'wx', mode);
    try { fs.writeFileSync(fd, content); fs.fsyncSync(fd); }
    finally { fs.closeSync(fd); }
    fs.renameSync(tmp, file);
    fs.chmodSync(file, mode);
  } finally { fs.rmSync(tmp, { force: true }); }
}

function systemdQuote(value) {
  if (/\r|\n/.test(value)) throw new Error('service executable path contains a newline');
  return `"${String(value).replaceAll('\\', '\\\\').replaceAll('"', '\\"')}"`;
}

function xml(value) {
  return String(value).replace(/[&<>"']/g, (char) => ({
    '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&apos;',
  })[char]);
}

export function systemdUnit(role, executable, nodeDir = path.dirname(process.execPath)) {
  if (!['server', 'agent'].includes(role)) throw new Error('service role must be server or agent');
  const description = role === 'server' ? 'Stackhour coding time-tracking server' : 'Stackhour coding time-tracking agent';
  const command = role === 'server' ? 'serve' : 'agent';
  return `[Unit]\nDescription=${description}\nAfter=network-online.target\nWants=network-online.target\n\n`
    + `[Service]\nEnvironment=${systemdQuote(`PATH=${nodeDir}:/usr/local/bin:/usr/bin:/bin`)}\n`
    + `ExecStart=${systemdQuote(executable)} ${command}\nRestart=always\nRestartSec=${role === 'server' ? 5 : 10}\n\n`
    + '[Install]\nWantedBy=default.target\n';
}

export function launchdPlist(executable, nodeDir = path.dirname(process.execPath)) {
  return `<?xml version="1.0" encoding="UTF-8"?>\n`
    + '<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "https://www.apple.com/DTDs/PropertyList-1.0.dtd">\n'
    + '<plist version="1.0">\n<dict>\n'
    + '  <key>Label</key><string>com.stackhour.agent</string>\n'
    + '  <key>ProgramArguments</key><array>\n'
    + `    <string>${xml(executable)}</string><string>agent</string>\n`
    + '  </array>\n'
    + '  <key>EnvironmentVariables</key><dict>\n'
    + `    <key>PATH</key><string>${xml(`${nodeDir}:/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin`)}</string>\n`
    + '  </dict>\n'
    + '  <key>RunAtLoad</key><true/>\n  <key>KeepAlive</key><true/>\n'
    + '  <key>StandardOutPath</key><string>/tmp/stackhour-agent.log</string>\n'
    + '  <key>StandardErrorPath</key><string>/tmp/stackhour-agent.log</string>\n'
    + '</dict>\n</plist>\n';
}

export function installService(role, {
  platform = process.platform,
  home = os.homedir(),
  uid = process.getuid?.(),
  repoRoot = REPO_ROOT,
  nodePath = process.execPath,
  run = execFileSync,
} = {}) {
  const executable = path.join(repoRoot, 'bin', 'stackhour');
  if (!fs.existsSync(executable)) throw new Error(`Stackhour executable not found: ${executable}`);

  if (platform === 'linux') {
    const unitName = `stackhour-${role}.service`;
    const unitPath = path.join(home, '.config', 'systemd', 'user', unitName);
    atomicWrite(unitPath, systemdUnit(role, executable, path.dirname(nodePath)));
    run('systemctl', ['--user', 'daemon-reload'], { stdio: 'inherit' });
    run('systemctl', ['--user', 'enable', '--now', unitName], { stdio: 'inherit' });
    return { platform, role, service: unitName, path: unitPath };
  }

  if (platform === 'darwin') {
    if (role !== 'agent') throw new Error('automatic macOS installation supports the agent role only');
    if (!Number.isInteger(uid)) throw new Error('cannot determine the macOS user id');
    const label = 'com.stackhour.agent';
    const plistPath = path.join(home, 'Library', 'LaunchAgents', `${label}.plist`);
    atomicWrite(plistPath, launchdPlist(executable, path.dirname(nodePath)));
    const domain = `gui/${uid}`;
    try { run('launchctl', ['bootout', domain, plistPath], { stdio: 'ignore' }); } catch { /* not loaded */ }
    run('launchctl', ['bootstrap', domain, plistPath], { stdio: 'inherit' });
    run('launchctl', ['enable', `${domain}/${label}`], { stdio: 'inherit' });
    run('launchctl', ['kickstart', '-k', `${domain}/${label}`], { stdio: 'inherit' });
    return { platform, role, service: label, path: plistPath };
  }

  throw new Error(`automatic service installation is not supported on ${platform}`);
}

export function runInstall(args, { installer = installService, stdout = process.stdout } = {}) {
  const role = args[0];
  if (role === 'server') {
    const server = installer('server');
    const agent = installer('agent');
    stdout.write('Installed and started stackhour-server and stackhour-agent\n');
    return [server, agent];
  }
  if (role === 'agent') {
    const agent = installer('agent');
    stdout.write('Installed and started stackhour-agent\n');
    return [agent];
  }
  throw new Error('usage: stackhour install <server|agent>');
}
