// Zed agent panel: Zed persists agent threads to a local SQLite DB
// (macOS: ~/Library/Application Support/Zed/threads/threads.db,
//  Linux: ~/.local/share/zed/threads/threads.db). We watch its DB/WAL signature and diff
// thread updated_at values to emit agent heartbeats — this catches AI work in
// the panel that never touches files (planning, chat, ACP agents).
// The schema is undocumented, so everything here is defensive: we discover
// the table/columns at runtime and fail soft. A consistent SQLite backup is
// read when supported, with a read-only direct fallback on early Node 22.
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { DATA_DIR } from '../config.js';

const CANDIDATE_PATHS = [
  path.join(os.homedir(), 'Library', 'Application Support', 'Zed', 'threads', 'threads.db'),
  path.join(os.homedir(), '.local', 'share', 'zed', 'threads', 'threads.db'),
];

function dbSignature(src) {
  // The WAL carries committed changes before checkpointing. Ignore -shm:
  // read-only connections can mutate its lock metadata without changing data.
  return ['', '-wal'].map((suffix) => {
    try {
      const st = fs.statSync(src + suffix, { bigint: true });
      return `${suffix}:${st.size}:${st.mtimeNs}:${st.ctimeNs}`;
    } catch { return `${suffix}:missing`; }
  }).join('|');
}

async function snapshotDb(src, dataDir, DatabaseSync, backup) {
  const dst = path.join(dataDir, 'zed-threads-copy.db');
  const tmp = `${dst}.${process.pid}.tmp`;
  fs.mkdirSync(dataDir, { recursive: true });
  for (const file of [tmp, `${tmp}-wal`, `${tmp}-shm`]) fs.rmSync(file, { force: true });
  const source = new DatabaseSync(src, { readOnly: true });
  try {
    await backup(source, tmp);
    fs.chmodSync(tmp, 0o600);
    for (const file of [dst, `${dst}-wal`, `${dst}-shm`]) fs.rmSync(file, { force: true });
    fs.renameSync(tmp, dst);
    return dst;
  } finally {
    try { source.close(); } catch { /* ignore */ }
    for (const file of [tmp, `${tmp}-wal`, `${tmp}-shm`]) fs.rmSync(file, { force: true });
  }
}

function quoteIdent(name) {
  return `"${String(name).replaceAll('"', '""')}"`;
}

export async function watchZed(cfg, state, options = {}) {
  const candidatePaths = options.candidatePaths || CANDIDATE_PATHS;
  const dataDir = options.dataDir || DATA_DIR;
  const dbPath = candidatePaths.find((p) => fs.existsSync(p));
  if (!dbPath) return [];

  const signature = dbSignature(dbPath);
  if (state.zedDbSignature === signature) return [];
  state.zedThreads ||= {};

  let db;
  let copiedPath = null;
  try {
    const { DatabaseSync, backup } = await import('node:sqlite');
    // sqlite.backup was added after the initial Node 22 SQLite release. Keep a
    // read-only fallback so the documented Node >= 22 floor still fails soft.
    if (typeof backup === 'function') {
      copiedPath = await snapshotDb(dbPath, dataDir, DatabaseSync, backup);
      db = new DatabaseSync(copiedPath, { readOnly: true });
    } else {
      db = new DatabaseSync(dbPath, { readOnly: true });
    }
  } catch (err) {
    console.error('[tempo] zed watcher: cannot open threads.db:', err.message);
    return [];
  }

  const rows = [];
  const now = Date.now() / 1000;
  try {
    const tables = db.prepare("SELECT name FROM sqlite_master WHERE type='table'").all().map((t) => t.name);
    const ordered = tables.includes('threads')
      ? ['threads', ...tables.filter((t) => t !== 'threads')]
      : tables;
    let table = null;
    let cols = [];
    for (const candidate of ordered) {
      const candidateCols = db.prepare(`PRAGMA table_info(${quoteIdent(candidate)})`).all().map((c) => c.name);
      if (candidateCols.includes('id') && candidateCols.includes('updated_at')) {
        table = candidate;
        cols = candidateCols;
        break;
      }
    }
    if (!table) {
      state.zedDbSignature = signature;
      delete state.zedDbMtime;
      return [];
    }
    const summaryCol = cols.includes('summary') ? 'summary' : null;
    const previousThreads = state.zedThreads;
    const nextThreads = {};

    const fields = ['id', 'updated_at', summaryCol].filter(Boolean).map(quoteIdent).join(', ');
    for (const t of db.prepare(`SELECT ${fields} FROM ${quoteIdent(table)}`).all()) {
      const key = String(t.id);
      const updated = String(t.updated_at);
      if (previousThreads[key] === updated) {
        nextThreads[key] = updated;
        continue;
      }
      const firstSight = !(key in previousThreads);
      nextThreads[key] = updated;
      if (firstSight && !state.zedInitDone) continue; // no historical flood on first run
      rows.push({
        time: now,
        source: 'zed-agent',
        project: 'zed-agent', // thread rows don't carry a project path
        entity: (summaryCol && t[summaryCol]) ? String(t[summaryCol]).slice(0, 120) : `thread ${key}`,
        entity_type: 'app',
        category: 'ai coding',
        actor: 'agent',
        is_write: 0,
      });
    }
    state.zedThreads = nextThreads;
    state.zedInitDone = true;
    state.zedDbSignature = signature;
    delete state.zedDbMtime;
  } catch (err) {
    console.error('[tempo] zed watcher:', err.message);
  } finally {
    try { db.close(); } catch { /* ignore */ }
    if (copiedPath) {
      for (const file of [copiedPath, `${copiedPath}-wal`, `${copiedPath}-shm`]) {
        try { fs.chmodSync(file, 0o600); } catch { /* sidecar may not exist */ }
      }
    }
  }
  return rows;
}
