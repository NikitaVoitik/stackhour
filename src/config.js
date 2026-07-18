import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';

export const CONFIG_PATH = process.env.TEMPO_CONFIG
  || path.join(os.homedir(), '.config', 'tempo', 'config.json');

export const DATA_DIR = process.env.TEMPO_DATA
  || path.join(os.homedir(), '.local', 'share', 'tempo');

export function expandHome(p) {
  if (!p) return p;
  return p.startsWith('~') ? path.join(os.homedir(), p.slice(1)) : p;
}

const DEFAULTS = {
  server: {
    port: 4040,
    host: '0.0.0.0',
    db: path.join(DATA_DIR, 'tempo.db'),
    token: '',
  },
  agent: {
    serverUrl: 'http://127.0.0.1:4040',
    token: '',
    machine: os.hostname(),
    intervalSeconds: 20,
    projectRoots: [],
    watch: { files: true, claude: true, codex: true, macApps: true },
    // frontmost-app tracking (macOS only): process name -> source, or {source, category}
    apps: {
      Claude: { source: 'claude-desktop', category: 'ai coding' },
      Codex: { source: 'codex-desktop', category: 'ai coding' },
      ChatGPT: { source: 'codex-desktop', category: 'ai coding' },
      WebStorm: { source: 'webstorm', category: 'coding' },
      Zed: { source: 'zed', category: 'coding' },
    },
    idleSeconds: 120,
    ignoreDirs: ['node_modules', '.git', 'dist', 'build', 'out', 'target',
      '.next', '.venv', 'venv', 'vendor', 'Library', '.cache', 'Pods',
      'DerivedData', 'Temp', 'Logs', 'obj'],
    maxScanDepth: 8,
  },
  // credit model: each heartbeat earns time until the next one, capped.
  summary: { capSeconds: 120, lastEventCreditSeconds: 60 },
  wakatime: { apiKey: '' },
};

function deepMerge(base, extra) {
  const out = { ...base };
  for (const [k, v] of Object.entries(extra || {})) {
    out[k] = v && typeof v === 'object' && !Array.isArray(v) && base[k] && typeof base[k] === 'object'
      ? deepMerge(base[k], v)
      : v;
  }
  return out;
}

export function loadConfig() {
  let user = {};
  if (fs.existsSync(CONFIG_PATH)) {
    user = JSON.parse(fs.readFileSync(CONFIG_PATH, 'utf8'));
  }
  const cfg = deepMerge(DEFAULTS, user);
  cfg.server.db = expandHome(cfg.server.db);
  cfg.agent.projectRoots = (cfg.agent.projectRoots || []).map(expandHome);
  // normalize app entries: allow plain-string shorthand
  for (const [name, v] of Object.entries(cfg.agent.apps)) {
    if (typeof v === 'string') cfg.agent.apps[name] = { source: v, category: 'coding' };
  }
  return cfg;
}
