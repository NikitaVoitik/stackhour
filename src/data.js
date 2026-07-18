import fs from 'node:fs';
import path from 'node:path';
import { DatabaseSync } from 'node:sqlite';
import { loadConfig } from './config.js';
import { optionValues } from './setup.js';

function existingDb(dbPath, readOnly = true) {
  if (!fs.existsSync(dbPath)) throw new Error(`database does not exist: ${dbPath}`);
  return new DatabaseSync(dbPath, { readOnly });
}

function scalar(db, sql, fallback = 0) {
  try { return db.prepare(sql).get()?.value ?? fallback; } catch { return fallback; }
}

export function dataStats(dbPath) {
  const db = existingDb(dbPath);
  try {
    const heartbeat = db.prepare('SELECT count(*) count, min(time) first, max(time) last FROM heartbeats').get();
    return {
      dbPath,
      databaseBytes: fs.statSync(dbPath).size,
      heartbeats: heartbeat.count,
      firstHeartbeat: heartbeat.first ?? null,
      lastHeartbeat: heartbeat.last ?? null,
      machines: scalar(db, 'SELECT count(DISTINCT machine) value FROM heartbeats'),
      projects: scalar(db, 'SELECT count(DISTINCT project) value FROM heartbeats'),
      agentStatuses: scalar(db, 'SELECT count(*) value FROM agent_status'),
      wakatimeDays: scalar(db, 'SELECT count(*) value FROM wakatime_days'),
    };
  } finally { db.close(); }
}

export function parseTime(value, name = 'time') {
  if (value === undefined || value === null || value === '') return null;
  const numeric = Number(value);
  if (Number.isFinite(numeric) && numeric >= 0 && numeric <= 8.64e12) return numeric;
  const millis = Date.parse(String(value));
  if (!Number.isFinite(millis)) throw new Error(`${name} must be a Unix timestamp or ISO date`);
  return millis / 1000;
}

function atomicExport(outputPath, write, force) {
  if (fs.existsSync(outputPath) && !force) throw new Error(`output exists: ${outputPath}; pass --force to replace it`);
  fs.mkdirSync(path.dirname(outputPath), { recursive: true });
  const tmp = `${outputPath}.${process.pid}.tmp`;
  try {
    fs.rmSync(tmp, { force: true });
    const fd = fs.openSync(tmp, 'wx', 0o600);
    try { write(fd); fs.fsyncSync(fd); } finally { fs.closeSync(fd); }
    fs.renameSync(tmp, outputPath);
    fs.chmodSync(outputPath, 0o600);
    let dirFd;
    try { dirFd = fs.openSync(path.dirname(outputPath), 'r'); fs.fsyncSync(dirFd); }
    catch { /* directory fsync is unavailable on some platforms */ }
    finally { if (dirFd !== undefined) fs.closeSync(dirFd); }
  } finally { fs.rmSync(tmp, { force: true }); }
}

export function exportData(dbPath, outputPath, { from = null, to = null, force = false, now = Date.now() / 1000 } = {}) {
  if (!outputPath) throw new Error('--output is required');
  const fromTime = parseTime(from, 'from') ?? 0;
  const toTime = parseTime(to, 'to') ?? Number.MAX_SAFE_INTEGER;
  if (fromTime > toTime) throw new Error('from must not be after to');
  const db = existingDb(dbPath);
  let heartbeats = 0;
  let wakatimeDays = 0;
  try {
    const rows = db.prepare('SELECT * FROM heartbeats WHERE time >= ? AND time <= ? ORDER BY time, id').iterate(fromTime, toTime);
    const fromDate = new Date(fromTime * 1000).toISOString().slice(0, 10);
    const toDate = toTime === Number.MAX_SAFE_INTEGER ? '9999-12-31' : new Date(toTime * 1000).toISOString().slice(0, 10);
    const days = db.prepare('SELECT * FROM wakatime_days WHERE date >= ? AND date <= ? ORDER BY date, project').iterate(fromDate, toDate);
    atomicExport(path.resolve(outputPath), (fd) => {
      fs.writeFileSync(fd, `${JSON.stringify({ type: 'stackhour-export', version: 1, createdAt: now, from: fromTime, to: toTime })}\n`);
      for (const row of rows) {
        fs.writeFileSync(fd, `${JSON.stringify({ type: 'heartbeat', data: row })}\n`);
        heartbeats++;
      }
      for (const row of days) {
        fs.writeFileSync(fd, `${JSON.stringify({ type: 'wakatime-day', data: row })}\n`);
        wakatimeDays++;
      }
    }, force);
  } finally { db.close(); }
  return { outputPath: path.resolve(outputPath), heartbeats, wakatimeDays };
}

export function pruneData(dbPath, before, { confirm = false } = {}) {
  const cutoff = parseTime(before, 'before');
  if (cutoff === null) throw new Error('--before is required');
  const cutoffDate = new Date(cutoff * 1000).toISOString().slice(0, 10);
  const db = existingDb(dbPath, !confirm);
  try {
    const heartbeats = db.prepare('SELECT count(*) count FROM heartbeats WHERE time < ?').get(cutoff).count;
    const wakatimeDays = db.prepare('SELECT count(*) count FROM wakatime_days WHERE date < ?').get(cutoffDate).count;
    if (!confirm) return { dryRun: true, cutoff, cutoffDate, heartbeats, wakatimeDays };
    db.exec('BEGIN IMMEDIATE');
    try {
      db.prepare('DELETE FROM heartbeats WHERE time < ?').run(cutoff);
      db.prepare('DELETE FROM wakatime_days WHERE date < ?').run(cutoffDate);
      db.exec('COMMIT');
    } catch (err) {
      try { db.exec('ROLLBACK'); } catch { /* preserve original error */ }
      throw err;
    }
    return { dryRun: false, cutoff, cutoffDate, heartbeats, wakatimeDays };
  } finally { db.close(); }
}

export function runData(args, { cfg = loadConfig(), stdout = process.stdout } = {}) {
  const command = args[0];
  const value = (name) => optionValues(args, name).at(-1);
  if (command === 'stats') {
    const result = dataStats(cfg.server.db);
    if (args.includes('--json')) stdout.write(`${JSON.stringify(result, null, 2)}\n`);
    else {
      stdout.write(`${result.heartbeats} heartbeats · ${result.machines} machines · ${result.projects} projects\n`);
      stdout.write(`${result.wakatimeDays} imported days · ${result.databaseBytes} bytes\n`);
    }
    return result;
  }
  if (command === 'export') {
    const result = exportData(cfg.server.db, value('output'), {
      from: value('from'), to: value('to'), force: args.includes('--force'),
    });
    stdout.write(`Exported ${result.heartbeats} heartbeats and ${result.wakatimeDays} imported days to ${result.outputPath}\n`);
    return result;
  }
  if (command === 'prune') {
    const result = pruneData(cfg.server.db, value('before'), { confirm: args.includes('--confirm') });
    if (result.dryRun) {
      stdout.write(`Would delete ${result.heartbeats} heartbeats and ${result.wakatimeDays} imported days; rerun with --confirm\n`);
    } else stdout.write(`Deleted ${result.heartbeats} heartbeats and ${result.wakatimeDays} imported days\n`);
    return result;
  }
  throw new Error('usage: stackhour data <stats|export|prune> [options]');
}
