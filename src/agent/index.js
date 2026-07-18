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
const LOCK_PATH = path.join(DATA_DIR, 'agent.lock');
const MAX_SEND_BYTES = 4 * 1024 * 1024;

function atomicWrite(file, content) {
  fs.mkdirSync(path.dirname(file), { recursive: true });
  const tmp = `${file}.tmp`;
  try {
    fs.rmSync(tmp, { force: true });
    const fd = fs.openSync(tmp, 'w', 0o600);
    try {
      fs.writeFileSync(fd, content);
      fs.fsyncSync(fd);
    } finally {
      fs.closeSync(fd);
    }
    fs.renameSync(tmp, file);
    let dirFd;
    try {
      dirFd = fs.openSync(path.dirname(file), 'r');
      fs.fsyncSync(dirFd);
    } catch { /* some platforms do not fsync directories */ }
    finally { if (dirFd !== undefined) fs.closeSync(dirFd); }
  } finally {
    fs.rmSync(tmp, { force: true });
  }
}

export function loadState(statePath = STATE_PATH) {
  try { return JSON.parse(fs.readFileSync(statePath, 'utf8')); } catch { return {}; }
}
export function saveState(state, statePath = STATE_PATH) {
  atomicWrite(statePath, JSON.stringify(state));
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

export function readQueue(queuePath = QUEUE_PATH) {
  try {
    const lines = fs.readFileSync(queuePath, 'utf8').split('\n').filter(Boolean);
    return lines.map((l) => { try { return JSON.parse(l); } catch { return null; } }).filter(Boolean);
  } catch { return []; }
}

export function saveQueue(rows, queuePath = QUEUE_PATH) {
  if (!rows.length) {
    fs.rmSync(queuePath, { force: true });
    return;
  }
  atomicWrite(queuePath, rows.map((r) => JSON.stringify(r)).join('\n') + '\n');
}

export function appendQueue(rows, queuePath = QUEUE_PATH) {
  if (!rows.length) return;
  fs.mkdirSync(path.dirname(queuePath), { recursive: true });
  const fd = fs.openSync(queuePath, 'a', 0o600);
  try {
    fs.writeFileSync(fd, rows.map((r) => JSON.stringify(r)).join('\n') + '\n');
    fs.fsyncSync(fd);
  } finally {
    fs.closeSync(fd);
  }
  fs.chmodSync(queuePath, 0o600);
}

export function takeSendBatch(rows, maxBytes = MAX_SEND_BYTES) {
  const batch = [];
  let bytes = 2; // JSON array brackets
  for (const row of rows) {
    const rowBytes = Buffer.byteLength(JSON.stringify(row)) + (batch.length ? 1 : 0);
    if (batch.length && bytes + rowBytes > maxBytes) break;
    batch.push(row);
    bytes += rowBytes;
  }
  return batch;
}

export function acquireAgentLock(lockPath = LOCK_PATH) {
  fs.mkdirSync(path.dirname(lockPath), { recursive: true });
  const tryOpen = () => {
    try {
      const fd = fs.openSync(lockPath, 'wx', 0o600);
      try {
        fs.writeFileSync(fd, String(process.pid));
        fs.fsyncSync(fd);
      } catch (err) {
        fs.closeSync(fd);
        fs.rmSync(lockPath, { force: true });
        throw err;
      }
      return fd;
    } catch (err) {
      if (err.code !== 'EEXIST') throw err;
      const owner = Number.parseInt(fs.readFileSync(lockPath, 'utf8'), 10);
      let alive = Number.isFinite(owner) && owner > 0;
      if (alive) {
        try { process.kill(owner, 0); }
        catch (killErr) { if (killErr.code === 'ESRCH') alive = false; }
      }
      if (alive) throw new Error(`stackhour agent already running (pid ${owner})`);
      fs.rmSync(lockPath, { force: true });
      return tryOpen();
    }
  };
  const fd = tryOpen();
  let released = false;
  return () => {
    if (released) return;
    released = true;
    try { fs.closeSync(fd); } catch { /* already closed */ }
    try {
      const owner = Number.parseInt(fs.readFileSync(lockPath, 'utf8'), 10);
      if (!Number.isFinite(owner) || owner === process.pid) fs.rmSync(lockPath, { force: true });
    } catch { /* already removed */ }
  };
}

export async function runAgent(cfg, {
  once = false,
  statePath = STATE_PATH,
  queuePath = QUEUE_PATH,
  lockPath = LOCK_PATH,
} = {}) {
  const releaseLock = acquireAgentLock(lockPath);
  const machine = cfg.agent.machine;
  console.log(`[stackhour] agent starting on ${machine} -> ${cfg.agent.serverUrl} (every ${cfg.agent.intervalSeconds}s)`);

  const tick = async () => {
    const state = loadState(statePath);
    const batches = [];
    const w = cfg.agent.watch;
    const run = async (enabled, name, fn) => {
      if (!enabled) return;
      try { batches.push(await fn(cfg, state)); }
      catch (err) { console.error(`[stackhour] watcher ${name} failed:`, err.message); }
    };
    await run(w.files && cfg.agent.projectRoots.length, 'files', watchFiles);
    await run(w.claude, 'claude', watchClaude);
    await run(w.codex, 'codex', watchCodex);
    await run(w.macApps && process.platform === 'darwin', 'macApps', watchMacApps);
    await run(w.ssh && process.platform === 'linux', 'ssh', watchSsh);
    await run(w.zed, 'zed', watchZed);
    const fresh = batches.flat().map((r) => ({ machine, ...r }));
    const queued = readQueue(queuePath);
    // Persist heartbeats before their watcher offsets. A crash between these
    // writes can cause a harmless replay, but cannot silently lose activity.
    if (fresh.length) appendQueue(fresh, queuePath);
    saveState(state, statePath);

    const pending = fresh.length ? [...queued, ...fresh] : queued;
    if (!pending.length) return;
    const rows = takeSendBatch(pending);

    try {
      const result = await send(cfg, rows);
      const remaining = pending.slice(rows.length);
      saveQueue(remaining, queuePath);
      console.log(`[stackhour] sent ${rows.length} heartbeats (${result.inserted} new${remaining.length ? `, ${remaining.length} queued` : ''})`);
    } catch (err) {
      console.error(`[stackhour] server unreachable (${err.message}); queued ${pending.length}`);
    }
  };

  try { await tick(); }
  catch (err) { releaseLock(); throw err; }
  if (once) { releaseLock(); return; }
  process.once('exit', releaseLock);
  const schedule = () => setTimeout(async () => {
    try { await tick(); }
    catch (e) { console.error('[stackhour] tick error:', e); }
    schedule();
  }, cfg.agent.intervalSeconds * 1000);
  schedule();
}
