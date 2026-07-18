import fs from 'node:fs';
import path from 'node:path';
import { pathToFileURL } from 'node:url';
import { DatabaseSync } from 'node:sqlite';
import { loadConfig } from './config.js';
import { optionValues } from './setup.js';

function timestamp(now = Date.now()) {
  return new Date(now).toISOString().replace(/[:.]/g, '-');
}

function fsyncFile(file) {
  const fd = fs.openSync(file, 'r');
  try { fs.fsyncSync(fd); } finally { fs.closeSync(fd); }
}

function fsyncDir(dir) {
  let fd;
  try { fd = fs.openSync(dir, 'r'); fs.fsyncSync(fd); }
  catch { /* directory fsync is unavailable on some platforms */ }
  finally { if (fd !== undefined) fs.closeSync(fd); }
}

function requireFile(file, label) {
  try {
    if (!fs.statSync(file).isFile()) throw new Error('not a regular file');
  } catch (err) { throw new Error(`${label} does not exist or is not a file: ${file}`); }
}

function finalizeSnapshot(file) {
  const db = new DatabaseSync(file);
  try {
    db.exec('PRAGMA busy_timeout = 5000');
    const checkpoint = db.prepare('PRAGMA wal_checkpoint(TRUNCATE)').get();
    if (checkpoint?.busy) throw new Error('backup snapshot is busy');
    db.exec('PRAGMA journal_mode = DELETE');
  } finally { db.close(); }
  for (const sidecar of [`${file}-wal`, `${file}-shm`]) fs.rmSync(sidecar, { force: true });
}

export function verifyBackup(file) {
  const backupPath = path.resolve(file);
  requireFile(backupPath, 'backup');
  let db;
  try {
    // immutable=1 prevents SQLite from creating -wal/-shm files during what
    // must remain a genuinely zero-write verification or restore preview.
    const location = pathToFileURL(backupPath);
    location.searchParams.set('immutable', '1');
    db = new DatabaseSync(location, { readOnly: true });
  }
  catch (err) { throw new Error(`cannot open backup: ${err.message}`); }
  try {
    const checks = db.prepare('PRAGMA quick_check').all();
    if (checks.length !== 1 || checks[0].quick_check !== 'ok') throw new Error('SQLite quick_check failed');
    const tables = db.prepare("SELECT name FROM sqlite_master WHERE type='table' ORDER BY name").all().map((row) => row.name);
    if (!tables.includes('heartbeats')) throw new Error('not a Stackhour database (heartbeats table missing)');
    const heartbeats = db.prepare('SELECT count(*) count FROM heartbeats').get().count;
    return { ok: true, backupPath, bytes: fs.statSync(backupPath).size, heartbeats, tables };
  } finally { db.close(); }
}

export async function createBackup(dbPath, outputPath = null, { force = false, now = Date.now() } = {}) {
  const sourcePath = path.resolve(dbPath);
  requireFile(sourcePath, 'database');
  const destination = path.resolve(outputPath || path.join(path.dirname(sourcePath), 'backups', `stackhour-${timestamp(now)}.db`));
  if (destination === sourcePath) throw new Error('backup output must differ from the database');
  if (fs.existsSync(destination) && !force) throw new Error(`backup exists: ${destination}; pass --force to replace it`);
  fs.mkdirSync(path.dirname(destination), { recursive: true });
  const tmp = `${destination}.${process.pid}.tmp`;
  let source;
  try {
    for (const file of [tmp, `${tmp}-wal`, `${tmp}-shm`]) fs.rmSync(file, { force: true });
    source = new DatabaseSync(sourcePath, { readOnly: true });
    const check = source.prepare('PRAGMA quick_check').get()?.quick_check;
    if (check !== 'ok') throw new Error('source database failed SQLite quick_check');
    const { backup } = await import('node:sqlite');
    if (typeof backup === 'function') await backup(source, tmp);
    else source.prepare('VACUUM INTO ?').run(tmp);
    source.close();
    source = null;
    finalizeSnapshot(tmp);
    verifyBackup(tmp);
    fs.chmodSync(tmp, 0o600);
    fsyncFile(tmp);
    for (const sidecar of [`${destination}-wal`, `${destination}-shm`]) fs.rmSync(sidecar, { force: true });
    fs.renameSync(tmp, destination);
    fs.chmodSync(destination, 0o600);
    fsyncDir(path.dirname(destination));
    const verified = verifyBackup(destination);
    return { ...verified, outputPath: destination };
  } finally {
    try { source?.close(); } catch { /* already closed */ }
    for (const file of [tmp, `${tmp}-wal`, `${tmp}-shm`]) fs.rmSync(file, { force: true });
  }
}

function acquireMaintenanceLock(dbPath) {
  const lockPath = `${dbPath}.maintenance.lock`;
  let fd;
  try {
    fd = fs.openSync(lockPath, 'wx', 0o600);
    fs.writeFileSync(fd, String(process.pid));
    fs.fsyncSync(fd);
  } catch (err) {
    if (fd !== undefined) fs.closeSync(fd);
    if (err.code === 'EEXIST') throw new Error(`maintenance already in progress (${lockPath})`);
    fs.rmSync(lockPath, { force: true });
    throw err;
  }
  return () => {
    try { fs.closeSync(fd); } catch { /* already closed */ }
    fs.rmSync(lockPath, { force: true });
  };
}

function prepareExistingDatabase(dbPath) {
  if (!fs.existsSync(dbPath)) return;
  const db = new DatabaseSync(dbPath);
  try {
    db.exec('PRAGMA busy_timeout = 1000');
    const checkpoint = db.prepare('PRAGMA wal_checkpoint(TRUNCATE)').get();
    if (checkpoint?.busy) throw new Error('database is busy; stop stackhour-server before restoring');
    db.exec('BEGIN EXCLUSIVE');
    db.exec('COMMIT');
  } catch (err) {
    try { db.exec('ROLLBACK'); } catch { /* no transaction */ }
    if (/busy|locked/i.test(err.message)) throw new Error('database is busy; stop stackhour-server before restoring');
    throw err;
  } finally { db.close(); }
}

export function restoreBackup(dbPath, backupPath, { confirm = false, now = Date.now() } = {}) {
  if (!backupPath) throw new Error('backup file is required');
  const target = path.resolve(dbPath);
  const source = path.resolve(backupPath);
  const verified = verifyBackup(source);
  if (fs.existsSync(target) && fs.realpathSync(target) === fs.realpathSync(source)) {
    throw new Error('backup file and target database must differ');
  }
  if (!confirm) return { dryRun: true, target, source, heartbeats: verified.heartbeats };

  fs.mkdirSync(path.dirname(target), { recursive: true });
  const release = acquireMaintenanceLock(target);
  const tmp = `${target}.${process.pid}.restore.tmp`;
  const rollbackPath = fs.existsSync(target) ? `${target}.pre-restore-${timestamp(now)}` : null;
  let oldMoved = false;
  try {
    if (rollbackPath && fs.existsSync(rollbackPath)) throw new Error(`rollback file already exists: ${rollbackPath}`);
    fs.rmSync(tmp, { force: true });
    fs.copyFileSync(source, tmp, fs.constants.COPYFILE_EXCL);
    fs.chmodSync(tmp, 0o600);
    fsyncFile(tmp);
    verifyBackup(tmp);
    prepareExistingDatabase(target);
    for (const sidecar of [`${target}-wal`, `${target}-shm`]) fs.rmSync(sidecar, { force: true });
    if (rollbackPath) {
      fs.renameSync(target, rollbackPath);
      oldMoved = true;
    }
    try {
      fs.renameSync(tmp, target);
      fsyncDir(path.dirname(target));
      verifyBackup(target);
    } catch (err) {
      fs.rmSync(target, { force: true });
      if (oldMoved) {
        fs.renameSync(rollbackPath, target);
        oldMoved = false;
      }
      throw err;
    }
    return { dryRun: false, target, source, rollbackPath, heartbeats: verified.heartbeats };
  } finally {
    fs.rmSync(tmp, { force: true });
    release();
  }
}

export async function runBackup(args, { cfg = loadConfig(), stdout = process.stdout } = {}) {
  const command = args[0];
  const value = (name) => optionValues(args, name).at(-1);
  if (command === 'create') {
    const result = await createBackup(cfg.server.db, value('output'), { force: args.includes('--force') });
    stdout.write(`Created backup ${result.outputPath} (${result.heartbeats} heartbeats)\n`);
    return result;
  }
  if (command === 'verify') {
    const result = verifyBackup(args[1]);
    stdout.write(`Backup OK: ${result.backupPath} (${result.heartbeats} heartbeats)\n`);
    return result;
  }
  if (command === 'restore') {
    const result = restoreBackup(cfg.server.db, args[1], { confirm: args.includes('--confirm') });
    if (result.dryRun) stdout.write(`Would restore ${result.source} to ${result.target}; rerun with --confirm after stopping stackhour-server\n`);
    else stdout.write(`Restored ${result.heartbeats} heartbeats to ${result.target}${result.rollbackPath ? `; previous database: ${result.rollbackPath}` : ''}\n`);
    return result;
  }
  throw new Error('usage: stackhour backup <create|verify FILE|restore FILE> [options]');
}
