// macOS only: frontmost-app + idle tracking for apps with no other signal
// (Claude Desktop chat, Codex Desktop UI). Also catches WebStorm/Zed focus as
// a coarse fallback. Needs Automation permission for System Events on first run.
import { execFile } from 'node:child_process';
import { promisify } from 'node:util';

const exec = promisify(execFile);

async function frontmostApp() {
  const { stdout } = await exec('osascript', ['-e',
    'tell application "System Events" to get name of first application process whose frontmost is true']);
  return stdout.trim();
}

async function idleSeconds() {
  const { stdout } = await exec('/bin/sh', ['-c',
    "ioreg -c IOHIDSystem | awk '/HIDIdleTime/ {print int($NF/1000000000); exit}'"]);
  return Number(stdout.trim() || 0);
}

export async function watchMacApps(cfg, state) {
  const idle = await idleSeconds();
  if (idle >= cfg.agent.idleSeconds) return [];
  const app = await frontmostApp();
  const mapped = cfg.agent.apps[app];
  if (!mapped) return [];
  return [{
    time: Date.now() / 1000,
    source: mapped.source,
    project: mapped.source, // chat apps have no project; attribute to the app itself
    entity: app,
    entity_type: 'app',
    category: mapped.category || 'coding',
    is_write: 0,
  }];
}
