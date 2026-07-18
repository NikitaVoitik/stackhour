import fs from 'node:fs';
import path from 'node:path';
import { DATA_DIR } from '../config.js';
import { watchFiles } from './watch-files.js';
import { watchClaude } from './watch-claude.js';
import { watchCodex } from './watch-codex.js';
import { watchMacApps } from './watch-mac.js';
import { watchSsh } from './watch-ssh.js';
import { watchZed } from './watch-zed.js';
import { VERSION } from '../version.js';

const STATE_PATH = path.join(DATA_DIR, 'agent-state.json');
const QUEUE_PATH = path.join(DATA_DIR, 'queue.jsonl');
const LOCK_PATH = path.join(DATA_DIR, 'agent.lock');
const MAX_SEND_BYTES = 4 * 1024 * 1024;
const DEFAULT_WATCHERS = {
  files: watchFiles,
  claude: watchClaude,
  codex: watchCodex,
  macApps: watchMacApps,
  ssh: watchSsh,
  zed: watchZed,
};

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

async function post(cfg, endpoint, body, timeoutMs = 10_000) {
  const res = await fetch(`${cfg.agent.serverUrl}${endpoint}`, {
    method: 'POST',
    headers: {
      'content-type': 'application/json',
      ...(cfg.agent.token ? { authorization: `Bearer ${cfg.agent.token}` } : {}),
    },
    body: JSON.stringify(body),
    signal: AbortSignal.timeout(timeoutMs),
  });
  if (!res.ok) throw new Error(`${endpoint} failed: HTTP ${res.status}`);
  return res.json();
}

function watcherInputMarker(name, state) {
  if (name === 'claude') return JSON.stringify(state.claudeOffsets || {});
  if (name === 'codex') return JSON.stringify(state.codexOffsets || {});
  if (name === 'zed') return state.zedDbSignature || state.zedDbMtime || '';
  return null;
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
  watcherFns = DEFAULT_WATCHERS,
} = {}) {
  const releaseLock = acquireAgentLock(lockPath);
  const machine = cfg.agent.machine;
  console.log(`[stackhour] agent starting on ${machine} -> ${cfg.agent.serverUrl} (every ${cfg.agent.intervalSeconds}s)`);

  const tick = async () => {
    const state = loadState(statePath);
    state.watcherHealth ||= {};
    const batches = [];
    const w = cfg.agent.watch;
    const run = async (name, configured, available, reason) => {
      const health = state.watcherHealth[name] ||= {};
      health.enabled = Boolean(configured);
      health.available = Boolean(available);
      if (!configured || !available) {
        health.lastCount = 0;
        health.reason = configured ? reason : 'disabled in config';
        return;
      }
      delete health.reason;
      const before = watcherInputMarker(name, state);
      const started = performance.now();
      try {
        const rows = await watcherFns[name](cfg, state);
        if (!Array.isArray(rows)) throw new Error('watcher returned a non-array result');
        const finished = Date.now() / 1000;
        const inputChanged = before !== watcherInputMarker(name, state) || rows.length > 0;
        Object.assign(health, {
          lastOk: finished,
          lastDurationMs: Math.round((performance.now() - started) * 10) / 10,
          lastCount: rows.length,
          consecutiveErrors: 0,
          error: null,
        });
        if (inputChanged) health.lastInput = finished;
        if (rows.length) {
          health.lastEvent = Math.max(...rows.map((row) => Number(row.time) || finished));
          health.unmatchedInputRuns = 0;
        } else if (inputChanged) {
          health.unmatchedInputRuns = (health.unmatchedInputRuns || 0) + 1;
        }
        batches.push(rows);
      } catch (err) {
        const finished = Date.now() / 1000;
        Object.assign(health, {
          lastDurationMs: Math.round((performance.now() - started) * 10) / 10,
          lastError: finished,
          consecutiveErrors: (health.consecutiveErrors || 0) + 1,
          error: String(err.message || err).slice(0, 500),
        });
        console.error(`[stackhour] watcher ${name} failed:`, err.message);
      }
    };
    await run('files', w.files, w.files && cfg.agent.projectRoots.length > 0, 'no project roots configured');
    await run('claude', w.claude, w.claude);
    await run('codex', w.codex, w.codex);
    await run('macApps', w.macApps, w.macApps && process.platform === 'darwin', 'requires macOS');
    await run('ssh', w.ssh, w.ssh && process.platform === 'linux', 'requires Linux');
    await run('zed', w.zed, w.zed);
    const fresh = batches.flat().map((r) => ({ machine, ...r }));
    const queued = readQueue(queuePath);
    // Persist heartbeats before their watcher offsets. A crash between these
    // writes can cause a harmless replay, but cannot silently lose activity.
    if (fresh.length) appendQueue(fresh, queuePath);
    saveState(state, statePath);

    const pending = fresh.length ? [...queued, ...fresh] : queued;
    let serverFailed = false;
    if (pending.length) {
      const rows = takeSendBatch(pending);
      try {
        const result = await post(cfg, '/api/ingest', rows);
        const remaining = pending.slice(rows.length);
        saveQueue(remaining, queuePath);
        console.log(`[stackhour] sent ${rows.length} heartbeats (${result.inserted} new${remaining.length ? `, ${remaining.length} queued` : ''})`);
      } catch (err) {
        serverFailed = true;
        console.error(`[stackhour] server unreachable (${err.message}); queued ${pending.length}`);
      }
    }

    const queuedAfterSend = readQueue(queuePath);
    let queueBytes = 0;
    try { queueBytes = fs.statSync(queuePath).size; } catch { /* no queue */ }
    const report = {
      time: Date.now() / 1000,
      machine,
      version: VERSION,
      nodeVersion: process.version,
      intervalSeconds: cfg.agent.intervalSeconds,
      queueDepth: queuedAfterSend.length,
      queueBytes,
      watchers: state.watcherHealth,
    };
    if (!serverFailed) {
      try { await post(cfg, '/api/agent-status', report, 3_000); }
      catch (err) { console.error(`[stackhour] health report failed: ${err.message}`); }
    }
    return report;
  };

  let firstReport;
  try { firstReport = await tick(); }
  catch (err) { releaseLock(); throw err; }
  if (once) { releaseLock(); return firstReport; }
  process.once('exit', releaseLock);
  const schedule = () => setTimeout(async () => {
    try { await tick(); }
    catch (e) { console.error('[stackhour] tick error:', e); }
    schedule();
  }, cfg.agent.intervalSeconds * 1000);
  schedule();
}
