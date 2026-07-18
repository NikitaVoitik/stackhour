import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { afterEach, test } from 'node:test';
import { DatabaseSync } from 'node:sqlite';

import { createBackup, restoreBackup, runBackup, verifyBackup } from '../src/backup.js';
import { startServer } from '../src/server.js';

const tempDirs = [];
const openDatabases = [];
const openServers = [];

function tempDir() {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'stackhour-backup-test-'));
  tempDirs.push(dir);
  return dir;
}

function makeDb(file, entities = ['one'], { keepOpen = false } = {}) {
  fs.mkdirSync(path.dirname(file), { recursive: true });
  const db = new DatabaseSync(file);
  db.exec(`
    PRAGMA journal_mode = WAL;
    CREATE TABLE heartbeats (
      id INTEGER PRIMARY KEY,
      time REAL NOT NULL,
      machine TEXT NOT NULL,
      source TEXT NOT NULL,
      project TEXT NOT NULL,
      entity TEXT NOT NULL,
      actor TEXT NOT NULL DEFAULT 'human'
    );
  `);
  const insert = db.prepare(`
    INSERT INTO heartbeats (time, machine, source, project, entity, actor)
    VALUES (?, 'machine', 'test', 'project', ?, 'human')
  `);
  entities.forEach((entity, index) => insert.run(index + 1, entity));
  if (keepOpen) {
    openDatabases.push(db);
    return db;
  }
  db.close();
  return null;
}

function entities(file) {
  const db = new DatabaseSync(file, { readOnly: true });
  try { return db.prepare('SELECT entity FROM heartbeats ORDER BY id').all().map((row) => row.entity); }
  finally { db.close(); }
}

function mode(file) {
  return fs.statSync(file).mode & 0o777;
}

function outputSink() {
  let value = '';
  return {
    stdout: { write(chunk) { value += String(chunk); } },
    read() { return value; },
  };
}

afterEach(async () => {
  for (const server of openServers.splice(0)) {
    if (server.listening) await new Promise((resolve) => server.close(resolve));
  }
  for (const db of openDatabases.splice(0)) {
    try { db.close(); } catch { /* already closed */ }
  }
  for (const dir of tempDirs.splice(0)) fs.rmSync(dir, { recursive: true, force: true });
});

test('createBackup snapshots committed WAL rows and produces a standalone mode-0600 database', async () => {
  const dir = tempDir();
  const source = path.join(dir, 'live.db');
  const db = makeDb(source, ['wal-one'], { keepOpen: true });
  db.prepare(`INSERT INTO heartbeats
    (time, machine, source, project, entity, actor)
    VALUES (2, 'machine', 'test', 'project', 'wal-two', 'human')`).run();
  assert.equal(fs.existsSync(`${source}-wal`), true);

  const output = path.join(dir, 'snapshots', 'live.db');
  const result = await createBackup(source, output);

  assert.equal(result.outputPath, output);
  assert.equal(result.heartbeats, 2);
  assert.deepEqual(entities(output), ['wal-one', 'wal-two']);
  assert.equal(mode(output), 0o600);
  assert.equal(fs.existsSync(`${output}-wal`), false);
  assert.equal(fs.existsSync(`${output}-shm`), false);
});

test('createBackup uses the timestamped default path and replaces an existing output only with force', async () => {
  const dir = tempDir();
  const source = path.join(dir, 'stackhour.db');
  makeDb(source, ['source']);
  const now = Date.parse('2026-07-18T12:34:56.789Z');

  const automatic = await createBackup(source, null, { now });
  assert.equal(automatic.outputPath, path.join(dir, 'backups', 'stackhour-2026-07-18T12-34-56-789Z.db'));
  assert.deepEqual(entities(automatic.outputPath), ['source']);

  const custom = path.join(dir, 'custom.db');
  makeDb(custom, ['old-output']);
  await assert.rejects(createBackup(source, custom), /backup exists.*--force/);
  assert.deepEqual(entities(custom), ['old-output']);
  await createBackup(source, custom, { force: true });
  assert.deepEqual(entities(custom), ['source']);
  assert.equal(mode(custom), 0o600);
});

test('createBackup refuses the source path and safely replaces a destination symlink without changing its referent', async () => {
  const dir = tempDir();
  const source = path.join(dir, 'source.db');
  const referent = path.join(dir, 'referent.db');
  const link = path.join(dir, 'backup.db');
  makeDb(source, ['source']);
  makeDb(referent, ['referent']);

  await assert.rejects(createBackup(source, source, { force: true }), /output must differ/);
  fs.symlinkSync(referent, link);
  await assert.rejects(createBackup(source, link), /backup exists/);
  await createBackup(source, link, { force: true });

  assert.equal(fs.lstatSync(link).isSymbolicLink(), false);
  assert.deepEqual(entities(link), ['source']);
  assert.deepEqual(entities(referent), ['referent']);
});

test('failed backup validation removes temporary files and preserves an existing destination', async () => {
  const dir = tempDir();
  const corrupt = path.join(dir, 'corrupt.db');
  const destination = path.join(dir, 'destination.db');
  fs.writeFileSync(corrupt, 'not sqlite');
  makeDb(destination, ['keep']);

  await assert.rejects(createBackup(corrupt, destination, { force: true }), /database|SQLite|file/i);
  assert.deepEqual(entities(destination), ['keep']);
  assert.equal(fs.existsSync(`${destination}.${process.pid}.tmp`), false);
  assert.equal(fs.existsSync(`${destination}.${process.pid}.tmp-wal`), false);
  assert.equal(fs.existsSync(`${destination}.${process.pid}.tmp-shm`), false);
});

test('verifyBackup reports valid metadata and refuses missing, corrupt, and non-Stackhour files', () => {
  const dir = tempDir();
  const valid = path.join(dir, 'valid.db');
  makeDb(valid, ['one', 'two']);
  const verified = verifyBackup(valid);
  assert.equal(verified.ok, true);
  assert.equal(verified.backupPath, valid);
  assert.equal(verified.heartbeats, 2);
  assert.ok(verified.bytes > 0);
  assert.ok(verified.tables.includes('heartbeats'));

  assert.throws(() => verifyBackup(path.join(dir, 'missing.db')), /does not exist or is not a file/);
  const corrupt = path.join(dir, 'corrupt.db');
  fs.writeFileSync(corrupt, Buffer.from('definitely not sqlite'));
  assert.throws(() => verifyBackup(corrupt), /database|SQLite|file/i);
  const unrelated = path.join(dir, 'unrelated.db');
  const db = new DatabaseSync(unrelated);
  db.exec('CREATE TABLE unrelated (id INTEGER)');
  db.close();
  assert.throws(() => verifyBackup(unrelated), /not a Stackhour database/);
  assert.throws(() => verifyBackup(dir), /does not exist or is not a file/);
});

test('restore dry run verifies the source but performs zero target-side writes', () => {
  const dir = tempDir();
  const source = path.join(dir, 'backup.db');
  const target = path.join(dir, 'missing', 'stackhour.db');
  makeDb(source, ['backup']);
  const before = fs.readdirSync(dir).sort();

  const result = restoreBackup(target, source);

  assert.deepEqual(result, { dryRun: true, target, source, heartbeats: 1 });
  assert.deepEqual(fs.readdirSync(dir).sort(), before);
  assert.equal(fs.existsSync(path.dirname(target)), false);
  assert.equal(fs.existsSync(`${target}.maintenance.lock`), false);
});

test('confirmed restore creates a missing target with secure permissions and no rollback', () => {
  const dir = tempDir();
  const source = path.join(dir, 'backup.db');
  const target = path.join(dir, 'data', 'stackhour.db');
  makeDb(source, ['restored']);

  const result = restoreBackup(target, source, { confirm: true });

  assert.equal(result.dryRun, false);
  assert.equal(result.rollbackPath, null);
  assert.deepEqual(entities(target), ['restored']);
  assert.equal(mode(target), 0o600);
  assert.equal(fs.existsSync(`${target}.maintenance.lock`), false);
  assert.equal(fs.existsSync(`${target}.${process.pid}.restore.tmp`), false);
});

test('confirmed restore preserves the previous database as rollback and removes stale sidecars', () => {
  const dir = tempDir();
  const source = path.join(dir, 'backup.db');
  const target = path.join(dir, 'stackhour.db');
  makeDb(source, ['new']);
  makeDb(target, ['old']);
  fs.writeFileSync(`${target}-wal`, '');
  fs.writeFileSync(`${target}-shm`, '');
  const now = Date.parse('2026-07-18T13:14:15.016Z');

  const result = restoreBackup(target, source, { confirm: true, now });

  assert.equal(result.rollbackPath, `${target}.pre-restore-2026-07-18T13-14-15-016Z`);
  // Check before opening the WAL-mode databases again: a normal SQLite read
  // may legitimately recreate these files.
  assert.equal(fs.existsSync(`${target}-wal`), false);
  assert.equal(fs.existsSync(`${target}-shm`), false);
  assert.deepEqual(entities(target), ['new']);
  assert.deepEqual(entities(result.rollbackPath), ['old']);
  assert.equal(fs.existsSync(`${target}.maintenance.lock`), false);
});

test('restore replaces a target symlink itself while preserving its referent as rollback state', () => {
  const dir = tempDir();
  const source = path.join(dir, 'backup.db');
  const referent = path.join(dir, 'original.db');
  const target = path.join(dir, 'stackhour.db');
  makeDb(source, ['new']);
  makeDb(referent, ['referent']);
  fs.symlinkSync(referent, target);

  const result = restoreBackup(target, source, { confirm: true, now: Date.parse('2026-07-18T13:30:00Z') });

  assert.equal(fs.lstatSync(target).isSymbolicLink(), false);
  assert.deepEqual(entities(target), ['new']);
  assert.equal(fs.lstatSync(result.rollbackPath).isSymbolicLink(), true);
  assert.equal(fs.realpathSync(result.rollbackPath), referent);
  assert.deepEqual(entities(referent), ['referent']);
});

test('restore refuses same-file aliases, an existing maintenance lock, and rollback collisions without mutation', () => {
  const dir = tempDir();
  const source = path.join(dir, 'backup.db');
  const target = path.join(dir, 'target.db');
  makeDb(source, ['backup']);
  makeDb(target, ['target']);
  const sourceAlias = path.join(dir, 'backup-link.db');
  fs.symlinkSync(source, sourceAlias);
  assert.throws(() => restoreBackup(source, sourceAlias, { confirm: true }), /must differ/);

  fs.writeFileSync(`${target}.maintenance.lock`, 'someone-else', { mode: 0o600 });
  assert.throws(() => restoreBackup(target, source, { confirm: true }), /maintenance already in progress/);
  assert.deepEqual(entities(target), ['target']);
  assert.equal(fs.readFileSync(`${target}.maintenance.lock`, 'utf8'), 'someone-else');
  fs.rmSync(`${target}.maintenance.lock`);

  const now = Date.parse('2026-07-18T14:00:00Z');
  const rollback = `${target}.pre-restore-2026-07-18T14-00-00-000Z`;
  fs.writeFileSync(rollback, 'reserved');
  assert.throws(() => restoreBackup(target, source, { confirm: true, now }), /rollback file already exists/);
  assert.deepEqual(entities(target), ['target']);
  assert.equal(fs.readFileSync(rollback, 'utf8'), 'reserved');
  assert.equal(fs.existsSync(`${target}.maintenance.lock`), false);
});

test('restore refuses a database with an active write transaction and cleans its lock and temporary copy', () => {
  const dir = tempDir();
  const source = path.join(dir, 'backup.db');
  const target = path.join(dir, 'target.db');
  makeDb(source, ['backup']);
  const busy = makeDb(target, ['target'], { keepOpen: true });
  busy.exec('BEGIN IMMEDIATE');

  assert.throws(() => restoreBackup(target, source, { confirm: true }), /database is busy.*stop stackhour-server/);
  assert.deepEqual(entities(target), ['target']);
  assert.equal(fs.existsSync(`${target}.maintenance.lock`), false);
  assert.equal(fs.existsSync(`${target}.${process.pid}.restore.tmp`), false);
  busy.exec('ROLLBACK');
});

test('a failed replacement restores the original database and cleans maintenance artifacts', () => {
  const dir = tempDir();
  const source = path.join(dir, 'backup.db');
  const target = path.join(dir, 'target.db');
  makeDb(source, ['new']);
  makeDb(target, ['original']);
  const originalRename = fs.renameSync;
  let injected = false;
  fs.renameSync = (from, to) => {
    if (!injected && from === `${target}.${process.pid}.restore.tmp` && to === target) {
      injected = true;
      const err = new Error('injected replacement failure');
      err.code = 'EIO';
      throw err;
    }
    return originalRename(from, to);
  };
  try {
    assert.throws(
      () => restoreBackup(target, source, { confirm: true, now: Date.parse('2026-07-18T15:00:00Z') }),
      /injected replacement failure/,
    );
  } finally {
    fs.renameSync = originalRename;
  }

  assert.equal(injected, true);
  assert.deepEqual(entities(target), ['original']);
  assert.equal(fs.existsSync(`${target}.pre-restore-2026-07-18T15-00-00-000Z`), false);
  assert.equal(fs.existsSync(`${target}.${process.pid}.restore.tmp`), false);
  assert.equal(fs.existsSync(`${target}.maintenance.lock`), false);
});

test('maintenance lock setup failure does not leave a stale lock behind', () => {
  const dir = tempDir();
  const source = path.join(dir, 'backup.db');
  const target = path.join(dir, 'target.db');
  makeDb(source, ['new']);
  makeDb(target, ['original']);
  const originalWrite = fs.writeFileSync;
  let injected = false;
  fs.writeFileSync = (file, ...args) => {
    if (!injected && typeof file === 'number') {
      injected = true;
      const err = new Error('injected lock write failure');
      err.code = 'EIO';
      throw err;
    }
    return originalWrite(file, ...args);
  };
  try {
    assert.throws(() => restoreBackup(target, source, { confirm: true }), /injected lock write failure/);
  } finally {
    fs.writeFileSync = originalWrite;
  }

  assert.equal(injected, true);
  assert.equal(fs.existsSync(`${target}.maintenance.lock`), false);
  assert.deepEqual(entities(target), ['original']);
});

test('restore rejects missing and invalid inputs before creating maintenance artifacts', () => {
  const dir = tempDir();
  const target = path.join(dir, 'data', 'target.db');
  assert.throws(() => restoreBackup(target), /backup file is required/);
  assert.throws(() => restoreBackup(target, path.join(dir, 'missing.db'), { confirm: true }), /backup.*does not exist/);
  const invalid = path.join(dir, 'invalid.db');
  const db = new DatabaseSync(invalid);
  db.exec('CREATE TABLE other (id INTEGER)');
  db.close();
  assert.throws(() => restoreBackup(target, invalid, { confirm: true }), /not a Stackhour database/);
  assert.equal(fs.existsSync(path.dirname(target)), false);
});

test('startServer refuses to open a database covered by the maintenance lock', () => {
  const dir = tempDir();
  const dbPath = path.join(dir, 'stackhour.db');
  fs.writeFileSync(`${dbPath}.maintenance.lock`, 'restore-in-progress');
  const cfg = {
    server: { db: dbPath, host: '127.0.0.1', port: 0, token: '', tokens: {} },
    summary: { reattributeWindowSeconds: 120, capSeconds: 120, lastEventCreditSeconds: 60, joinGapSeconds: 300 },
  };
  assert.throws(() => startServer(cfg), /database maintenance is in progress/);
  assert.equal(fs.existsSync(dbPath), false);
});

test('runBackup drives create, verify, dry-run restore, and confirmed restore with concise output', async () => {
  const dir = tempDir();
  const live = path.join(dir, 'live.db');
  const output = path.join(dir, 'explicit backup.db');
  makeDb(live, ['live']);
  const cfg = {
    server: {
      db: live,
      token: 'legacy-secret-must-never-print',
      tokens: { laptop: 'machine-secret-must-never-print' },
    },
  };

  let sink = outputSink();
  const created = await runBackup(['create', `--output=${output}`], { cfg, stdout: sink.stdout });
  assert.equal(created.outputPath, output);
  assert.equal(sink.read(), `Created backup ${output} (1 heartbeats)\n`);

  sink = outputSink();
  await runBackup(['verify', output], { cfg, stdout: sink.stdout });
  assert.equal(sink.read(), `Backup OK: ${output} (1 heartbeats)\n`);

  sink = outputSink();
  const preview = await runBackup(['restore', output], { cfg, stdout: sink.stdout });
  assert.equal(preview.dryRun, true);
  assert.match(sink.read(), /^Would restore .*; rerun with --confirm after stopping stackhour-server\n$/);
  assert.deepEqual(entities(live), ['live']);

  makeDb(path.join(dir, 'replacement-source.db'), ['replacement']);
  const replacement = path.join(dir, 'replacement-source.db');
  sink = outputSink();
  const restored = await runBackup(['restore', replacement, '--confirm'], { cfg, stdout: sink.stdout });
  assert.equal(restored.dryRun, false);
  assert.deepEqual(entities(live), ['replacement']);
  assert.match(sink.read(), /^Restored 1 heartbeats to .*; previous database: .*\n$/);
  assert.doesNotMatch(sink.read(), /legacy-secret|machine-secret/);
});

test('runBackup parses force/output options and rejects unsupported or incomplete commands', async () => {
  const dir = tempDir();
  const live = path.join(dir, 'live.db');
  const output = path.join(dir, 'output.db');
  makeDb(live, ['new']);
  makeDb(output, ['old']);
  const cfg = { server: { db: live } };
  const sink = outputSink();

  await assert.rejects(runBackup(['create', `--output=${output}`], { cfg, stdout: sink.stdout }), /--force/);
  await runBackup(['create', `--output=${output}`, '--force'], { cfg, stdout: sink.stdout });
  assert.deepEqual(entities(output), ['new']);
  await assert.rejects(runBackup([], { cfg, stdout: sink.stdout }), /usage: stackhour backup/);
  await assert.rejects(runBackup(['unknown'], { cfg, stdout: sink.stdout }), /usage: stackhour backup/);
  await assert.rejects(runBackup(['verify'], { cfg, stdout: sink.stdout }), /path|file|string/i);
  await assert.rejects(runBackup(['restore'], { cfg, stdout: sink.stdout }), /backup file is required/);
});
