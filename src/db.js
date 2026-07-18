import fs from 'node:fs';
import path from 'node:path';
import { DatabaseSync } from 'node:sqlite';

export function openDb(dbPath) {
  fs.mkdirSync(path.dirname(dbPath), { recursive: true });
  const db = new DatabaseSync(dbPath);
  db.exec(`
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
      created_at REAL NOT NULL
    );
  `);
  // migration for DBs created before the actor column existed
  const cols = db.prepare('PRAGMA table_info(heartbeats)').all().map((c) => c.name);
  if (!cols.includes('actor')) {
    db.exec(`
      ALTER TABLE heartbeats ADD COLUMN actor TEXT NOT NULL DEFAULT 'human';
      UPDATE heartbeats SET actor = 'agent'
        WHERE source LIKE 'claude-%' OR source LIKE 'codex-%';
    `);
  }
  db.exec(`
    CREATE UNIQUE INDEX IF NOT EXISTS hb_dedupe
      ON heartbeats (time, machine, source, project, entity);
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
      (time, machine, source, project, entity, entity_type, category, language, branch, is_write, actor, created_at)
    VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)`);
  let inserted = 0;
  const now = Date.now() / 1000;
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
      now,
    );
    inserted += r.changes;
  }
  return inserted;
}

export function upsertWakatimeDay(db, date, project, seconds) {
  db.prepare(`
    INSERT INTO wakatime_days (date, project, seconds) VALUES (?, ?, ?)
    ON CONFLICT (date, project) DO UPDATE SET seconds = excluded.seconds
  `).run(date, project, seconds);
}
