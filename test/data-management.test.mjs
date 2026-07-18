import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { afterEach, test } from 'node:test';
import { DatabaseSync } from 'node:sqlite';

import { dataStats, exportData, parseTime, pruneData, runData } from '../src/data.js';
import { insertHeartbeats, openDb, upsertAgentStatus, upsertWakatimeDay } from '../src/db.js';

const tempDirs = [];
const repoRoot = path.resolve(import.meta.dirname, '..');

function tempDir() {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'stackhour-data-test-'));
  tempDirs.push(dir);
  return dir;
}

function fixture() {
  const dir = tempDir();
  const dbPath = path.join(dir, 'stackhour.db');
  const db = openDb(dbPath);
  return { dir, dbPath, db };
}

function outputCapture() {
  let text = '';
  return { stdout: { write(value) { text += value; } }, read: () => text };
}

function readJsonl(file) {
  return fs.readFileSync(file, 'utf8').trimEnd().split('\n').map(JSON.parse);
}

function heartbeat(time, overrides = {}) {
  return {
    time,
    machine: 'workstation',
    source: 'editor-files',
    project: 'stackhour',
    entity: `/work/file-${time}.js`,
    actor: 'human',
    ...overrides,
  };
}

function cli(configPath, args) {
  return spawnSync(process.execPath, ['--experimental-sqlite', '--no-warnings', 'src/cli.js', 'data', ...args], {
    cwd: repoRoot,
    encoding: 'utf8',
    env: { ...process.env, STACKHOUR_CONFIG: configPath },
  });
}

afterEach(() => {
  for (const dir of tempDirs.splice(0)) fs.rmSync(dir, { recursive: true, force: true });
});

test('dataStats reports an initialized empty database without inventing coverage', () => {
  const { dbPath, db } = fixture();
  db.close();

  const stats = dataStats(dbPath);
  assert.equal(stats.dbPath, dbPath);
  assert.ok(stats.databaseBytes > 0);
  assert.deepEqual({
    heartbeats: stats.heartbeats,
    firstHeartbeat: stats.firstHeartbeat,
    lastHeartbeat: stats.lastHeartbeat,
    machines: stats.machines,
    projects: stats.projects,
    agentStatuses: stats.agentStatuses,
    wakatimeDays: stats.wakatimeDays,
  }, {
    heartbeats: 0,
    firstHeartbeat: null,
    lastHeartbeat: null,
    machines: 0,
    projects: 0,
    agentStatuses: 0,
    wakatimeDays: 0,
  });
});

test('dataStats counts populated data, distinct dimensions, and exact time coverage', () => {
  const { dbPath, db } = fixture();
  insertHeartbeats(db, [
    heartbeat(30, { machine: 'laptop', project: 'api' }),
    heartbeat(10, { machine: 'desktop', project: 'web' }),
    heartbeat(20, { machine: 'desktop', project: 'api' }),
  ]);
  upsertWakatimeDay(db, '2026-07-17', 'legacy', 123);
  upsertWakatimeDay(db, '2026-07-18', 'legacy', 456);
  upsertAgentStatus(db, { machine: 'desktop', time: 40, watchers: {} }, 41);
  db.close();

  const stats = dataStats(dbPath);
  assert.equal(stats.heartbeats, 3);
  assert.equal(stats.firstHeartbeat, 10);
  assert.equal(stats.lastHeartbeat, 30);
  assert.equal(stats.machines, 2);
  assert.equal(stats.projects, 2);
  assert.equal(stats.agentStatuses, 1);
  assert.equal(stats.wakatimeDays, 2);
});

test('dataStats fails clearly for missing, non-SQLite, and incomplete databases', () => {
  const dir = tempDir();
  const missing = path.join(dir, 'missing.db');
  assert.throws(() => dataStats(missing), /database does not exist/);

  const malformed = path.join(dir, 'malformed.db');
  fs.writeFileSync(malformed, 'PRIVATE-CONTENTS-THAT-MUST-NOT-LEAK');
  assert.throws(() => dataStats(malformed), (err) => {
    assert.doesNotMatch(err.message, /PRIVATE-CONTENTS/);
    return true;
  });

  const incomplete = path.join(dir, 'incomplete.db');
  const db = new DatabaseSync(incomplete);
  db.exec('CREATE TABLE unrelated (id INTEGER)');
  db.close();
  assert.throws(() => dataStats(incomplete), /heartbeats|no such table/i);
});

test('dataStats tolerates legacy databases without optional tables', () => {
  const dbPath = path.join(tempDir(), 'legacy.db');
  const db = new DatabaseSync(dbPath);
  db.exec('CREATE TABLE heartbeats (time REAL, machine TEXT, project TEXT)');
  db.prepare('INSERT INTO heartbeats VALUES (?, ?, ?)').run(12, 'old', 'legacy');
  db.close();

  const stats = dataStats(dbPath);
  assert.equal(stats.heartbeats, 1);
  assert.equal(stats.machines, 1);
  assert.equal(stats.projects, 1);
  assert.equal(stats.agentStatuses, 0);
  assert.equal(stats.wakatimeDays, 0);
});

test('parseTime accepts epoch boundaries, fractions, numeric strings, and ISO dates', () => {
  assert.equal(parseTime(undefined), null);
  assert.equal(parseTime(null), null);
  assert.equal(parseTime(''), null);
  assert.equal(parseTime(0), 0);
  assert.equal(parseTime('0'), 0);
  assert.equal(parseTime(0.125), 0.125);
  assert.equal(parseTime(8.64e12), 8.64e12);
  assert.equal(parseTime('2026-07-18T12:34:56.789Z'), 1784378096.789);
  assert.equal(parseTime('2026-07-18'), Date.parse('2026-07-18') / 1000);
  assert.equal(parseTime('1969-12-31T23:59:59Z'), -1);
});

test('parseTime rejects non-finite, out-of-range, and nonsensical input with the option name', () => {
  for (const value of [Infinity, -Infinity, NaN, 8.64e12 + 1, 'not-a-date']) {
    assert.throws(() => parseTime(value, 'before'), /before must be a Unix timestamp or ISO date/);
  }
});

test('exportData writes a versioned JSONL header and inclusive, deterministic filtered rows', () => {
  const { dir, dbPath, db } = fixture();
  const start = Date.parse('2026-07-18T00:00:00Z') / 1000;
  insertHeartbeats(db, [
    heartbeat(start + 20, { entity: '/z.js', project: 'zeta' }),
    heartbeat(start - 1, { entity: '/before.js', project: 'before' }),
    heartbeat(start + 10, { entity: '/b.js', project: 'beta' }),
    heartbeat(start + 10, { entity: '/a.js', project: 'alpha', source: 'codex-cli', actor: 'agent' }),
    heartbeat(start + 30, { entity: '/after.js', project: 'after' }),
  ]);
  upsertWakatimeDay(db, '2026-07-17', 'old', 1);
  upsertWakatimeDay(db, '2026-07-18', 'zeta', 2);
  upsertWakatimeDay(db, '2026-07-18', 'alpha', 3);
  upsertWakatimeDay(db, '2026-07-19', 'new', 4);
  db.close();

  const output = path.join(dir, 'nested', 'export.jsonl');
  const result = exportData(dbPath, output, {
    from: start + 10,
    to: start + 20,
    now: 1234.5,
  });
  assert.deepEqual(result, { outputPath: output, heartbeats: 3, wakatimeDays: 2 });

  const lines = readJsonl(output);
  assert.deepEqual(lines[0], {
    type: 'stackhour-export', version: 1, createdAt: 1234.5,
    from: start + 10, to: start + 20,
  });
  assert.deepEqual(lines.slice(1, 4).map((line) => [line.type, line.data.time, line.data.entity]), [
    ['heartbeat', start + 10, '/b.js'],
    ['heartbeat', start + 10, '/a.js'],
    ['heartbeat', start + 20, '/z.js'],
  ]);
  assert.deepEqual(lines.slice(4).map((line) => [line.type, line.data.date, line.data.project]), [
    ['wakatime-day', '2026-07-18', 'alpha'],
    ['wakatime-day', '2026-07-18', 'zeta'],
  ]);
  assert.ok(lines.slice(1, 4).every((line) => line.data.id > 0 && line.data.created_at > 0));
});

test('exportData validates arguments before creating output', () => {
  const { dir, dbPath, db } = fixture();
  db.close();
  assert.throws(() => exportData(dbPath), /--output is required/);
  assert.throws(() => exportData(dbPath, path.join(dir, 'bad.jsonl'), { from: 20, to: 10 }), /from must not be after to/);
  assert.equal(fs.existsSync(path.join(dir, 'bad.jsonl')), false);
});

test('exportData creates private files, refuses overwrite, and force-replaces atomically', () => {
  const { dir, dbPath, db } = fixture();
  insertHeartbeats(db, [heartbeat(10)]);
  db.close();
  const output = path.join(dir, 'export.jsonl');

  exportData(dbPath, output, { now: 1 });
  assert.equal(fs.statSync(output).mode & 0o777, 0o600);
  const original = fs.readFileSync(output, 'utf8');
  assert.throws(() => exportData(dbPath, output, { now: 2 }), /output exists.*--force/);
  assert.equal(fs.readFileSync(output, 'utf8'), original);
  assert.deepEqual(fs.readdirSync(dir).filter((name) => name.endsWith('.tmp')), []);

  exportData(dbPath, output, { force: true, now: 2 });
  assert.equal(readJsonl(output)[0].createdAt, 2);
  assert.equal(fs.statSync(output).mode & 0o777, 0o600);
  assert.deepEqual(fs.readdirSync(dir).filter((name) => name.endsWith('.tmp')), []);
});

test('failed forced export cleans its temporary file and preserves the existing destination', () => {
  const { dir, dbPath, db } = fixture();
  db.close();
  const destination = path.join(dir, 'destination');
  fs.mkdirSync(destination);

  assert.throws(() => exportData(dbPath, destination, { force: true }), /directory|EISDIR|ENOTEMPTY/i);
  assert.equal(fs.statSync(destination).isDirectory(), true);
  assert.deepEqual(fs.readdirSync(dir).filter((name) => name.endsWith('.tmp')), []);
});

test('exports do not follow a destination symlink when replacing it', () => {
  const { dir, dbPath, db } = fixture();
  db.close();
  const target = path.join(dir, 'sensitive.txt');
  const output = path.join(dir, 'export.jsonl');
  fs.writeFileSync(target, 'DO NOT REPLACE');
  fs.symlinkSync(target, output);

  assert.throws(() => exportData(dbPath, output), /output exists/);
  assert.equal(fs.readFileSync(target, 'utf8'), 'DO NOT REPLACE');
  exportData(dbPath, output, { force: true });
  assert.equal(fs.lstatSync(output).isSymbolicLink(), false);
  assert.equal(fs.readFileSync(target, 'utf8'), 'DO NOT REPLACE');
});

test('pruneData dry-run reports strict boundaries and makes no database writes', () => {
  const { dbPath, db } = fixture();
  const cutoff = Date.parse('2026-07-18T12:00:00Z') / 1000;
  insertHeartbeats(db, [heartbeat(cutoff - 1), heartbeat(cutoff), heartbeat(cutoff + 1)]);
  upsertWakatimeDay(db, '2026-07-17', 'old', 1);
  upsertWakatimeDay(db, '2026-07-18', 'boundary', 2);
  db.close();

  const before = fs.statSync(dbPath).mtimeMs;
  const result = pruneData(dbPath, '2026-07-18T12:00:00Z');
  assert.deepEqual(result, {
    dryRun: true, cutoff, cutoffDate: '2026-07-18', heartbeats: 1, wakatimeDays: 1,
  });
  const verify = new DatabaseSync(dbPath, { readOnly: true });
  assert.equal(verify.prepare('SELECT count(*) count FROM heartbeats').get().count, 3);
  assert.equal(verify.prepare('SELECT count(*) count FROM wakatime_days').get().count, 2);
  verify.close();
  assert.equal(fs.statSync(dbPath).mtimeMs, before);
});

test('confirmed prune transaction deletes only rows strictly before each boundary', () => {
  const { dbPath, db } = fixture();
  const cutoff = Date.parse('2026-07-18T00:00:00Z') / 1000;
  insertHeartbeats(db, [heartbeat(cutoff - 0.001), heartbeat(cutoff), heartbeat(cutoff + 0.001)]);
  upsertWakatimeDay(db, '2026-07-17', 'old', 1);
  upsertWakatimeDay(db, '2026-07-18', 'boundary', 2);
  upsertWakatimeDay(db, '2026-07-19', 'new', 3);
  db.close();

  assert.deepEqual(pruneData(dbPath, cutoff, { confirm: true }), {
    dryRun: false, cutoff, cutoffDate: '2026-07-18', heartbeats: 1, wakatimeDays: 1,
  });
  const verify = new DatabaseSync(dbPath, { readOnly: true });
  assert.deepEqual(verify.prepare('SELECT time FROM heartbeats ORDER BY time').all().map((row) => row.time), [cutoff, cutoff + 0.001]);
  assert.deepEqual(verify.prepare('SELECT date FROM wakatime_days ORDER BY date').all().map((row) => row.date), ['2026-07-18', '2026-07-19']);
  verify.close();
});

test('confirmed prune rolls back heartbeat deletion when the second deletion fails', () => {
  const { dbPath, db } = fixture();
  insertHeartbeats(db, [heartbeat(1), heartbeat(20)]);
  upsertWakatimeDay(db, '1970-01-01', 'legacy', 1);
  db.exec(`
    CREATE TRIGGER reject_wakatime_prune BEFORE DELETE ON wakatime_days
    BEGIN SELECT RAISE(ABORT, 'simulated prune failure'); END;
  `);
  db.close();

  assert.throws(() => pruneData(dbPath, '1970-01-02', { confirm: true }), /simulated prune failure/);
  const verify = new DatabaseSync(dbPath, { readOnly: true });
  assert.equal(verify.prepare('SELECT count(*) count FROM heartbeats').get().count, 2);
  assert.equal(verify.prepare('SELECT count(*) count FROM wakatime_days').get().count, 1);
  verify.close();
});

test('pruneData requires a cutoff and never creates a missing database', () => {
  const dir = tempDir();
  const missing = path.join(dir, 'missing.db');
  assert.throws(() => pruneData(missing), /--before is required/);
  assert.equal(fs.existsSync(missing), false);
  assert.throws(() => pruneData(missing, 10), /database does not exist/);
  assert.equal(fs.existsSync(missing), false);
});

test('runData renders stable human and JSON stats without leaking config tokens', () => {
  const { dbPath, db } = fixture();
  insertHeartbeats(db, [heartbeat(10, { machine: 'safe-machine', project: 'safe-project' })]);
  db.close();
  const cfg = { server: { db: dbPath, token: 'TOP-SECRET', tokens: { machine: 'ALSO-SECRET' } } };

  const human = outputCapture();
  const humanResult = runData(['stats'], { cfg, stdout: human.stdout });
  assert.equal(humanResult.heartbeats, 1);
  assert.match(human.read(), /^1 heartbeats · 1 machines · 1 projects\n0 imported days · \d+ bytes\n$/);
  assert.doesNotMatch(human.read(), /TOP-SECRET|ALSO-SECRET/);

  const json = outputCapture();
  runData(['stats', '--json'], { cfg, stdout: json.stdout });
  assert.equal(JSON.parse(json.read()).heartbeats, 1);
  assert.doesNotMatch(json.read(), /TOP-SECRET|ALSO-SECRET/);
});

test('runData parses export options, uses the last repeated value, and reports completion', () => {
  const { dir, dbPath, db } = fixture();
  insertHeartbeats(db, [heartbeat(5), heartbeat(10), heartbeat(15)]);
  db.close();
  const first = path.join(dir, 'unused.jsonl');
  const output = path.join(dir, 'chosen.jsonl');
  const capture = outputCapture();

  const result = runData([
    'export', `--output=${first}`, `--output=${output}`, '--from=10', '--to=15', '--force',
  ], { cfg: { server: { db: dbPath } }, stdout: capture.stdout });
  assert.deepEqual({ heartbeats: result.heartbeats, wakatimeDays: result.wakatimeDays }, { heartbeats: 2, wakatimeDays: 0 });
  assert.equal(fs.existsSync(first), false);
  assert.match(capture.read(), new RegExp(`^Exported 2 heartbeats and 0 imported days to ${output.replace(/[.*+?^${}()|[\]\\]/g, '\\$&')}\\n$`));
});

test('runData previews and confirms prune, and rejects unknown subcommands', () => {
  const { dbPath, db } = fixture();
  insertHeartbeats(db, [heartbeat(1), heartbeat(20)]);
  db.close();
  const cfg = { server: { db: dbPath } };

  const preview = outputCapture();
  assert.equal(runData(['prune', '--before=10'], { cfg, stdout: preview.stdout }).dryRun, true);
  assert.match(preview.read(), /Would delete 1 heartbeats.*rerun with --confirm/);
  assert.equal(dataStats(dbPath).heartbeats, 2);

  const confirmed = outputCapture();
  assert.equal(runData(['prune', '--before=10', '--confirm'], { cfg, stdout: confirmed.stdout }).dryRun, false);
  assert.equal(confirmed.read(), 'Deleted 1 heartbeats and 0 imported days\n');
  assert.equal(dataStats(dbPath).heartbeats, 1);

  assert.throws(() => runData([], { cfg, stdout: outputCapture().stdout }), /usage: stackhour data/);
  assert.throws(() => runData(['unknown'], { cfg, stdout: outputCapture().stdout }), /usage: stackhour data/);
});

test('the real CLI honors STACKHOUR_CONFIG for JSON stats and wraps safe errors', (t) => {
  const { dir, dbPath, db } = fixture();
  insertHeartbeats(db, [heartbeat(10)]);
  db.close();
  const configPath = path.join(dir, 'config.json');
  fs.writeFileSync(configPath, `${JSON.stringify({ server: { db: dbPath, token: 'CLI-PRIVATE-TOKEN' } })}\n`);

  const stats = cli(configPath, ['stats', '--json']);
  // Some restricted sandboxes prohibit child_process even though the same
  // command works from their outer shell. Keep the end-to-end assertion live
  // everywhere else while allowing the module-level suite to run there.
  if (stats.error?.code === 'EPERM') {
    t.skip('sandbox prohibits child_process');
    return;
  }
  assert.ifError(stats.error);
  assert.equal(stats.status, 0, stats.stderr);
  assert.equal(JSON.parse(stats.stdout).heartbeats, 1);
  assert.equal(stats.stderr, '');
  assert.doesNotMatch(stats.stdout, /CLI-PRIVATE-TOKEN/);

  const missingOutput = cli(configPath, ['export']);
  assert.equal(missingOutput.status, 1);
  assert.equal(missingOutput.stdout, '');
  assert.match(missingOutput.stderr, /^stackhour data: --output is required\n$/);
  assert.doesNotMatch(missingOutput.stderr, /CLI-PRIVATE-TOKEN/);

  const unknown = cli(configPath, ['wat']);
  assert.equal(unknown.status, 1);
  assert.match(unknown.stderr, /^stackhour data: usage: stackhour data/);
});
