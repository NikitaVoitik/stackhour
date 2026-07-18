import fs from 'node:fs';
import path from 'node:path';
import { DATA_DIR } from '../config.js';
import { watchFiles } from './watch-files.js';
import { watchClaude } from './watch-claude.js';
import { watchCodex } from './watch-codex.js';
import { watchMacApps } from './watch-mac.js';
import { watchSsh } from './watch-ssh.js';
import { watchZed } from './watch-zed.js';

const STATE_PATH = path.join(DATA_DIR, 'agent-state.json');
const QUEUE_PATH = path.join(DATA_DIR, 'queue.jsonl');

function loadState() {
  try { return JSON.parse(fs.readFileSync(STATE_PATH, 'utf8')); } catch { return {}; }
}
function saveState(state) {
  fs.mkdirSync(DATA_DIR, { recursive: true });
  fs.writeFileSync(STATE_PATH, JSON.stringify(state));
}

async function send(cfg, rows) {
  const res = await fetch(`${cfg.agent.serverUrl}/api/ingest`, {
    method: 'POST',
    headers: {
      'content-type': 'application/json',
      ...(cfg.agent.token ? { authorization: `Bearer ${cfg.agent.token}` } : {}),
    },
    body: JSON.stringify(rows),
    signal: AbortSignal.timeout(10_000),
  });
  if (!res.ok) throw new Error(`ingest failed: HTTP ${res.status}`);
  return res.json();
}

function enqueue(rows) {
  fs.mkdirSync(DATA_DIR, { recursive: true });
  fs.appendFileSync(QUEUE_PATH, rows.map((r) => JSON.stringify(r)).join('\n') + '\n');
}

function drainQueue() {
  try {
    const lines = fs.readFileSync(QUEUE_PATH, 'utf8').split('\n').filter(Boolean);
    return lines.map((l) => { try { return JSON.parse(l); } catch { return null; } }).filter(Boolean);
  } catch { return []; }
}

export async function runAgent(cfg, { once = false } = {}) {
  const machine = cfg.agent.machine;
  console.log(`[tempo] agent starting on ${machine} -> ${cfg.agent.serverUrl} (every ${cfg.agent.intervalSeconds}s)`);

  const tick = async () => {
    const state = loadState();
    const batches = [];
    const w = cfg.agent.watch;
    const run = async (enabled, name, fn) => {
      if (!enabled) return;
      try { batches.push(await fn(cfg, state)); }
      catch (err) { console.error(`[tempo] watcher ${name} failed:`, err.message); }
    };
    await run(w.files && cfg.agent.projectRoots.length, 'files', watchFiles);
    await run(w.claude, 'claude', watchClaude);
    await run(w.codex, 'codex', watchCodex);
    await run(w.macApps && process.platform === 'darwin', 'macApps', watchMacApps);
    await run(w.ssh && process.platform === 'linux', 'ssh', watchSsh);
    await run(w.zed, 'zed', watchZed);
    saveState(state);

    let rows = batches.flat().map((r) => ({ machine, ...r }));
    const queued = drainQueue();
    if (queued.length) rows = [...queued, ...rows];
    if (!rows.length) return;

    try {
      const result = await send(cfg, rows);
      if (queued.length) fs.rmSync(QUEUE_PATH, { force: true });
      console.log(`[tempo] sent ${rows.length} heartbeats (${result.inserted} new${queued.length ? `, ${queued.length} from queue` : ''})`);
    } catch (err) {
      // keep only this tick's fresh rows; queued ones are already on disk
      const fresh = rows.slice(queued.length);
      if (fresh.length) enqueue(fresh);
      console.error(`[tempo] server unreachable (${err.message}); queued ${fresh.length}`);
    }
  };

  await tick();
  if (once) return;
  setInterval(() => tick().catch((e) => console.error('[tempo] tick error:', e)), cfg.agent.intervalSeconds * 1000);
}
