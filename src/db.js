import fs from 'node:fs';
import path from 'node:path';
import { DatabaseSync } from 'node:sqlite';

function nonnegativeNumber(value, { integer = false } = {}) {
  const number = Number(value || 0);
  if (!Number.isFinite(number) || number < 0) return 0;
  return integer ? Math.round(number) : number;
}

export function openDb(dbPath) {
  fs.mkdirSync(path.dirname(dbPath), { recursive: true });
  const db = new DatabaseSync(dbPath);
  db.exec(`
    PRAGMA busy_timeout = 5000;
    PRAGMA journal_mode = WAL;
    CREATE TABLE IF NOT EXISTS heartbeats (
      id INTEGER PRIMARY KEY,
      time REAL NOT NULL,            -- unix epoch seconds
      machine TEXT NOT NULL,
      source TEXT NOT NULL,          -- webstorm | zed | claude-code | claude-desktop | codex-cli | codex-desktop | editor-files | ...
      project TEXT NOT NULL,
      entity TEXT NOT NULL,          -- file path or app name
      entity_type TEXT NOT NULL DEFAULT 'file',  -- file | app
      category TEXT NOT NULL DEFAULT 'coding',
      language TEXT,
      branch TEXT,
      is_write INTEGER NOT NULL DEFAULT 0,
      actor TEXT NOT NULL DEFAULT 'human',   -- human | agent
      tokens_in INTEGER NOT NULL DEFAULT 0,
      tokens_out INTEGER NOT NULL DEFAULT 0,
      cost REAL NOT NULL DEFAULT 0,
      created_at REAL NOT NULL
    );
  `);
  // migrations for DBs created before newer columns existed
  const cols = new Set(db.prepare('PRAGMA table_info(heartbeats)').all().map((c) => c.name));
  db.exec('BEGIN IMMEDIATE');
  try {
    if (!cols.has('actor')) {
      db.exec(`
        ALTER TABLE heartbeats ADD COLUMN actor TEXT NOT NULL DEFAULT 'human';
        UPDATE heartbeats SET actor = 'agent'
          WHERE source LIKE 'claude-%' OR source LIKE 'codex-%'
            OR source = 'zed-agent'
            OR lower(category) = 'ai'
            OR lower(category) GLOB 'ai[^a-z0-9_]*'
            OR lower(category) GLOB '*[^a-z0-9_]ai'
            OR lower(category) GLOB '*[^a-z0-9_]ai[^a-z0-9_]*';
      `);
    }
    // Check every column independently so an upgrade interrupted between
    // ALTER statements is safely resumable on the next start.
    if (!cols.has('tokens_in')) {
      db.exec('ALTER TABLE heartbeats ADD COLUMN tokens_in INTEGER NOT NULL DEFAULT 0');
    }
    if (!cols.has('tokens_out')) {
      db.exec('ALTER TABLE heartbeats ADD COLUMN tokens_out INTEGER NOT NULL DEFAULT 0');
    }
    if (!cols.has('cost')) {
      db.exec('ALTER TABLE heartbeats ADD COLUMN cost REAL NOT NULL DEFAULT 0');
    }
    db.exec('COMMIT');
  } catch (err) {
    try { db.exec('ROLLBACK'); } catch { /* preserve original error */ }
    throw err;
  }
  db.exec(`
    DROP INDEX IF EXISTS hb_dedupe;
    CREATE UNIQUE INDEX IF NOT EXISTS hb_dedupe2
      ON heartbeats (time, machine, source, project, entity, actor);
    CREATE INDEX IF NOT EXISTS hb_time ON heartbeats (time);
    -- daily per-project totals imported from wakatime.com (historical backfill)
    CREATE TABLE IF NOT EXISTS wakatime_days (
      date TEXT NOT NULL,
      project TEXT NOT NULL,
      seconds REAL NOT NULL,
      UNIQUE (date, project)
    );
  `);
  return db;
}

export function insertHeartbeats(db, rows) {
  const stmt = db.prepare(`
    INSERT OR IGNORE INTO heartbeats
      (time, machine, source, project, entity, entity_type, category, language, branch, is_write, actor, tokens_in, tokens_out, cost, created_at)
    VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)`);
  let inserted = 0;
  const now = Date.now() / 1000;
  db.exec('BEGIN IMMEDIATE');
  try {
    for (const h of rows) {
      if (!h || !Number.isFinite(h.time)) continue;
      const r = stmt.run(
        h.time,
        String(h.machine || 'unknown'),
        String(h.source || 'unknown'),
        String(h.project || 'unknown'),
        String(h.entity || 'unknown'),
        h.entity_type === 'app' ? 'app' : 'file',
        String(h.category || 'coding'),
        h.language ? String(h.language) : null,
        h.branch ? String(h.branch) : null,
        h.is_write ? 1 : 0,
        h.actor === 'agent' ? 'agent' : 'human',
        nonnegativeNumber(h.tokens_in, { integer: true }),
        nonnegativeNumber(h.tokens_out, { integer: true }),
        nonnegativeNumber(h.cost),
        now,
      );
      inserted += r.changes;
    }
    db.exec('COMMIT');
  } catch (err) {
    try { db.exec('ROLLBACK'); } catch { /* preserve original error */ }
    throw err;
  }
  return inserted;
}

export function upsertWakatimeDay(db, date, project, seconds) {
  db.prepare(`
    INSERT INTO wakatime_days (date, project, seconds) VALUES (?, ?, ?)
    ON CONFLICT (date, project) DO UPDATE SET seconds = excluded.seconds
  `).run(date, project, seconds);
}
