// macOS only: frontmost-app + idle tracking. This is the *human presence*
// signal — it only fires when input is active, and it reads the focused
// window title to detect which project Nikita is actually looking at
// (WebStorm/Zed titles contain the project). Needs Automation permission for
// System Events; window titles additionally need Accessibility. Falls back to
// app-level attribution when the title is unavailable or unparseable.
import { execFile } from 'node:child_process';
import { promisify } from 'node:util';

const exec = promisify(execFile);

// default title -> project extractors, first match wins
const DEFAULT_TITLE_PATTERNS = {
  // JetBrains: "project – path/to/file" (en dash)
  WebStorm: ['^([^–—]+?)\\s+[–—]'],
  // Zed: "filename — project" (em dash)
  Zed: ['\\s+[—]\\s+([^—]+)$'],
};

async function frontmost() {
  const script = `
    tell application "System Events"
      set p to first application process whose frontmost is true
      set appName to name of p
      set winTitle to ""
      try
        set winTitle to name of front window of p
      end try
      return appName & linefeed & winTitle
    end tell`;
  const { stdout } = await exec('osascript', ['-e', script]);
  const [app, ...rest] = stdout.split('\n');
  return { app: (app || '').trim(), title: rest.join('\n').trim() };
}

async function idleSeconds() {
  const { stdout } = await exec('/bin/sh', ['-c',
    "ioreg -c IOHIDSystem | awk '/HIDIdleTime/ {print int($NF/1000000000); exit}'"]);
  return Number(stdout.trim() || 0);
}

export function projectFromTitle(app, title, appCfg) {
  if (!title) return null;
  const patterns = appCfg.projectFromTitle
    ? [appCfg.projectFromTitle]
    : DEFAULT_TITLE_PATTERNS[app] || [];
  for (const p of patterns) {
    const m = title.match(new RegExp(p));
    if (m?.[1]) return m[1].trim();
  }
  return null;
}

export async function watchMacApps(cfg, state) {
  const idle = await idleSeconds();
  if (idle >= cfg.agent.idleSeconds) return [];
  const { app, title } = await frontmost();
  const mapped = cfg.agent.apps[app];
  if (!mapped) return [];
  const project = projectFromTitle(app, title, mapped);
  return [{
    time: Date.now() / 1000,
    source: mapped.source,
    project: project || mapped.source, // no parseable project -> attribute to the app
    entity: title || app,
    entity_type: 'app',
    category: mapped.category || 'coding',
    actor: 'human',
    is_write: 0,
  }];
}
