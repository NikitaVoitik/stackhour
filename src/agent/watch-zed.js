// Zed agent panel: Zed persists agent threads to a local SQLite DB
// (macOS: ~/Library/Application Support/Zed/threads/threads.db,
//  Linux: ~/.local/share/zed/threads/threads.db). We watch its mtime and diff
// thread updated_at values to emit agent heartbeats — this catches AI work in
// the panel that never touches files (planning, chat, ACP agents).
// The schema is undocumented, so everything here is defensive: we discover
// the table/columns at runtime and fail soft. The DB is copied before opening
// so we never contend with Zed's own connection.
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { DATA_DIR } from '../config.js';

const CANDIDATE_PATHS = [
  path.join(os.homedir(), 'Library', 'Application Support', 'Zed', 'threads', 'threads.db'),
  path.join(os.homedir(), '.local', 'share', 'zed', 'threads', 'threads.db'),
];

function copyDb(src) {
  const dst = path.join(DATA_DIR, 'zed-threads-copy.db');
  fs.mkdirSync(DATA_DIR, { recursive: true });
  fs.copyFileSync(src, dst);
  for (const suffix of ['-wal', '-shm']) {
    try { fs.copyFileSync(src + suffix, dst + suffix); }
    catch { fs.rmSync(dst + suffix, { force: true }); }
  }
  return dst;
}

export async function watchZed(cfg, state) {
  const dbPath = CANDIDATE_PATHS.find((p) => fs.existsSync(p));
  if (!dbPath) return [];

  const st = fs.statSync(dbPath);
  if (state.zedDbMtime && st.mtimeMs <= state.zedDbMtime) return [];
  state.zedDbMtime = st.mtimeMs;
  state.zedThreads ||= {};

  let db;
  try {
    const { DatabaseSync } = await import('node:sqlite');
    db = new DatabaseSync(copyDb(dbPath), { readOnly: true });
  } catch (err) {
    console.error('[tempo] zed watcher: cannot open threads.db:', err.message);
    return [];
  }

  const rows = [];
  const now = Date.now() / 1000;
  try {
    const tables = db.prepare("SELECT name FROM sqlite_master WHERE type='table'").all().map((t) => t.name);
    const table = tables.includes('threads') ? 'threads' : tables[0];
    if (!table) return [];
    const cols = db.prepare(`PRAGMA table_info(${table})`).all().map((c) => c.name);
    if (!cols.includes('id') || !cols.includes('updated_at')) return [];
    const summaryCol = cols.includes('summary') ? 'summary' : null;

    for (const t of db.prepare(`SELECT ${['id', 'updated_at', summaryCol].filter(Boolean).join(', ')} FROM ${table}`).all()) {
      const key = String(t.id);
      const updated = String(t.updated_at);
      if (state.zedThreads[key] === updated) continue;
      const firstSight = !(key in state.zedThreads);
      state.zedThreads[key] = updated;
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
    state.zedInitDone = true;
  } catch (err) {
    console.error('[tempo] zed watcher:', err.message);
  } finally {
    try { db.close(); } catch { /* ignore */ }
  }
  return rows;
}
