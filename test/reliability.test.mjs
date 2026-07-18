import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { once } from 'node:events';
import { afterEach, test } from 'node:test';
import { DatabaseSync } from 'node:sqlite';

import { pruneOffsets, readFirstJsonLine, readNewLines } from '../src/agent/tail.js';
import { acquireAgentLock, appendQueue, loadState, readQueue, runAgent, saveQueue, saveState, takeSendBatch } from '../src/agent/index.js';
import { gitBranch, watchFiles } from '../src/agent/watch-files.js';
import { projectFromTitle } from '../src/agent/watch-mac.js';
import { watchClaude } from '../src/agent/watch-claude.js';
import { watchCodex } from '../src/agent/watch-codex.js';
import { watchZed } from '../src/agent/watch-zed.js';
import { reattributeFileSaves, startServer } from '../src/server.js';
import { buildSegments, computeCredits, dayBuckets, totalsBy } from '../src/summarize.js';
import { insertHeartbeats, openDb } from '../src/db.js';

const tempDirs = [];

function tempDir() {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'tempo-test-'));
  tempDirs.push(dir);
  return dir;
}

afterEach(() => {
  for (const dir of tempDirs.splice(0)) fs.rmSync(dir, { recursive: true, force: true });
});

test('JSONL tail keeps an incomplete final record for the next tick', () => {
  const file = path.join(tempDir(), 'events.jsonl');
  const offsets = {};
  fs.writeFileSync(file, '{"old":true}\n');

  assert.deepEqual(readNewLines(file, offsets), []); // first sight starts at EOF
  fs.appendFileSync(file, '{"complete":1}\n{"split":');
  assert.deepEqual(readNewLines(file, offsets), [{ complete: 1 }]);
  const committed = offsets[file];
  assert.ok(committed < fs.statSync(file).size);

  fs.appendFileSync(file, '2}\n');
  assert.deepEqual(readNewLines(file, offsets), [{ split: 2 }]);
  assert.equal(offsets[file], fs.statSync(file).size);
});

test('JSONL tail skips a malformed complete line without blocking later records', () => {
  const file = path.join(tempDir(), 'events.jsonl');
  const offsets = {};
  fs.writeFileSync(file, 'seed\n');
  readNewLines(file, offsets);
  fs.appendFileSync(file, 'not-json\n{"ok":true}\n');
  assert.deepEqual(readNewLines(file, offsets), [{ ok: true }]);
});

test('head reader handles a session metadata line larger than 64 KiB without loading the rollout', () => {
  const file = path.join(tempDir(), 'rollout.jsonl');
  const first = { type: 'session_meta', payload: { cwd: '/work/large', padding: 'x'.repeat(70 * 1024) } };
  fs.writeFileSync(file, `${JSON.stringify(first)}\n${JSON.stringify({ type: 'later' })}\n`);
  assert.deepEqual(readFirstJsonLine(file), first);
  assert.equal(readFirstJsonLine(file, 1024), null);
});

test('tail offset pruning removes only stale files after the size threshold', () => {
  const offsets = { live: 1, stale: 2, alsoStale: 3 };
  pruneOffsets(offsets, ['live'], 4);
  assert.deepEqual(offsets, { live: 1, stale: 2, alsoStale: 3 });
  pruneOffsets(offsets, ['live'], 3);
  assert.deepEqual(offsets, { live: 1 });
});

test('branch detection handles normal repositories, worktrees, and detached HEADs', () => {
  const dir = tempDir();
  const normal = path.join(dir, 'normal');
  fs.mkdirSync(path.join(normal, '.git'), { recursive: true });
  fs.writeFileSync(path.join(normal, '.git', 'HEAD'), 'ref: refs/heads/main\n');
  assert.equal(gitBranch(normal), 'main');

  const worktree = path.join(dir, 'worktree');
  const gitDir = path.join(dir, 'metadata', 'worktrees', 'topic');
  fs.mkdirSync(worktree);
  fs.mkdirSync(gitDir, { recursive: true });
  fs.writeFileSync(path.join(worktree, '.git'), `gitdir: ${path.relative(worktree, gitDir)}\n`);
  fs.writeFileSync(path.join(gitDir, 'HEAD'), 'ref: refs/heads/codex/reliable\n');
  assert.equal(gitBranch(worktree), 'codex/reliable');

  fs.writeFileSync(path.join(gitDir, 'HEAD'), '0123456789abcdef\n');
  assert.equal(gitBranch(worktree, {}), '0123456789ab');
});

test('file watcher emits worktree branch metadata and recovers from a backward clock jump', async () => {
  const dir = tempDir();
  const root = path.join(dir, 'projects');
  const project = path.join(root, 'app');
  const gitDir = path.join(dir, 'git-meta');
  fs.mkdirSync(path.join(project, 'src'), { recursive: true });
  fs.mkdirSync(gitDir);
  fs.writeFileSync(path.join(project, '.git'), `gitdir: ${path.relative(project, gitDir)}\n`);
  fs.writeFileSync(path.join(gitDir, 'HEAD'), 'ref: refs/heads/topic\n');
  const file = path.join(project, 'src', 'main.js');
  fs.writeFileSync(file, 'export default 1;\n');
  const now = Date.now() / 1000;
  fs.utimesSync(file, now, now);
  const cfg = { agent: { projectRoots: [root], ignoreDirs: [], maxScanDepth: 8, intervalSeconds: 20 } };
  const state = { filesLastScan: now - 1 };
  const rows = await watchFiles(cfg, state);
  assert.equal(rows.length, 1);
  assert.equal(rows[0].project, 'app');
  assert.equal(rows[0].branch, 'topic');
  assert.equal(rows[0].language, 'JavaScript');

  state.filesLastScan = Date.now() / 1000 + 3600;
  const second = path.join(project, 'src', 'clock-reset.js');
  fs.writeFileSync(second, 'export default 2;\n');
  assert.ok((await watchFiles(cfg, state)).some((r) => r.entity === second));
});

test('macOS title parsing keeps configured and default project extraction stable', () => {
  assert.equal(projectFromTitle('WebStorm', 'tempo – src/server.js', {}), 'tempo');
  assert.equal(projectFromTitle('Zed', 'server.js — tempo', {}), 'tempo');
  assert.equal(projectFromTitle('Custom', '[client] editing', { projectFromTitle: '^\\[([^\\]]+)\\]' }), 'client');
  assert.equal(projectFromTitle('Zed', '', {}), null);
});

test('Claude watcher tails new records and preserves human, relay, edit, token, and stale semantics', async () => {
  const projectsDir = tempDir();
  const file = path.join(projectsDir, 'session.jsonl');
  const now = Date.parse('2026-07-18T12:00:00Z') / 1000;
  const line = (value) => `${JSON.stringify(value)}\n`;
  fs.writeFileSync(file, line({ type: 'user', timestamp: '2026-07-18T11:00:00Z', cwd: '/p/old', message: { content: 'history' } }));
  const state = {};
  const cfg = {};
  assert.deepEqual(await watchClaude(cfg, state, { projectsDir, now }), []);

  fs.appendFileSync(file,
    line({ type: 'user', timestamp: '2026-07-18T11:59:50Z', cwd: '/work/app', entrypoint: 'claude-desktop', message: { content: 'prompt' } })
    + line({ type: 'user', timestamp: '2026-07-18T11:59:51Z', cwd: '/work/app', entrypoint: 'claude-desktop', message: { content: [{ type: 'tool_result', content: 'ok' }] } })
    + line({
      type: 'assistant', timestamp: '2026-07-18T11:59:52Z', cwd: '/work/app', entrypoint: 'claude-desktop', gitBranch: 'main',
      message: {
        id: 'msg-one',
        model: 'claude-sonnet-4',
        usage: { input_tokens: 100, cache_creation_input_tokens: 20, cache_read_input_tokens: 30, output_tokens: 10 },
        content: [{ type: 'tool_use', name: 'Edit', input: { file_path: '/work/app/a.js' } }],
      },
    })
    + line({
      type: 'assistant', timestamp: '2026-07-18T11:59:53Z', cwd: '/work/app', entrypoint: 'claude-desktop',
      message: {
        id: 'msg-two', model: 'claude-sonnet-4',
        usage: { input_tokens: 10, cache_creation_input_tokens: 0, cache_read_input_tokens: 0, output_tokens: 2 },
        content: 'second response',
      },
    })
    + line({
      type: 'assistant', timestamp: '2026-07-18T11:59:54Z', cwd: '/work/app', entrypoint: 'claude-desktop',
      message: {
        id: 'msg-one', model: 'claude-sonnet-4',
        usage: { input_tokens: 100, cache_creation_input_tokens: 20, cache_read_input_tokens: 30, output_tokens: 10 },
        content: 'duplicate streamed record',
      },
    })
    + 'malformed\n'
    + line({ type: 'assistant', timestamp: '2026-07-18T10:00:00Z', cwd: '/work/app', message: { content: 'stale' } }));

  const rows = await watchClaude(cfg, state, { projectsDir, now });
  assert.equal(rows.length, 5);
  assert.deepEqual(rows.map((r) => r.actor), ['human', 'agent', 'agent', 'agent', 'agent']);
  assert.equal(rows[0].source, 'claude-desktop');
  assert.equal(rows[0].project, 'app');
  assert.equal(rows[2].entity, '/work/app/a.js');
  assert.equal(rows[2].is_write, 1);
  assert.equal(rows[2].tokens_in, 150);
  assert.equal(rows[2].tokens_out, 10);
  assert.ok(rows[2].cost > 0);
  assert.equal(rows[3].tokens_in, 10);
  assert.equal(rows[4].tokens_in || 0, 0);
  assert.equal(rows.reduce((n, r) => n + (r.tokens_in || 0), 0), 160);
  assert.deepEqual(state.claudeUsageById['msg-one'], { input: 100, cacheWrite: 20, cacheRead: 30, output: 10 });
  assert.deepEqual(state.claudeUsageById['msg-two'], { input: 10, cacheWrite: 0, cacheRead: 0, output: 2 });
  fs.appendFileSync(file, line({
    type: 'assistant', timestamp: '2026-07-18T11:59:55Z', cwd: '/work/app', entrypoint: 'claude-desktop',
    message: {
      id: 'msg-two', model: 'claude-sonnet-4',
      usage: { input_tokens: 10, cache_creation_input_tokens: 0, cache_read_input_tokens: 0, output_tokens: 12 },
      content: 'completed output on later tick',
    },
  }));
  const later = await watchClaude(cfg, state, { projectsDir, now });
  assert.equal(later.length, 1);
  assert.equal(later[0].tokens_in || 0, 0);
  assert.equal(later[0].tokens_out, 10);
  assert.deepEqual(state.claudeUsageById['msg-two'], { input: 10, cacheWrite: 0, cacheRead: 0, output: 12 });
});

test('Codex watcher learns metadata then classifies prompts, token events, and patch files', async () => {
  const sessionsDir = tempDir();
  const file = path.join(sessionsDir, 'rollout-fixture.jsonl');
  const now = Date.parse('2026-07-18T12:00:00Z') / 1000;
  const line = (value) => `${JSON.stringify(value)}\n`;
  fs.writeFileSync(file, line({
    type: 'session_meta', timestamp: '2026-07-18T11:00:00Z',
    payload: { cwd: '/work/tempo', originator: 'Codex Desktop', padding: 'x'.repeat(70 * 1024) },
  }));
  const state = {};
  const cfg = {};
  assert.deepEqual(await watchCodex(cfg, state, { sessionsDir, now }), []);
  assert.equal(state.codexMeta[file].source, 'codex-desktop');

  fs.appendFileSync(file,
    line({ type: 'turn_context', timestamp: '2026-07-18T11:59:48Z', payload: { cwd: '/work/tempo', model: 'gpt-5.6-sol' } })
    + line({ type: 'event_msg', timestamp: '2026-07-18T11:59:49Z', payload: { type: 'user_message', message: 'prompt' } })
    + line({
      type: 'event_msg', timestamp: '2026-07-18T11:59:50Z',
      payload: { type: 'token_count', info: { last_token_usage: { input_tokens: 1000, cached_input_tokens: 800, output_tokens: 50, reasoning_output_tokens: 10 } } },
    })
    + line({
      type: 'event_msg', timestamp: '2026-07-18T11:59:51Z',
      payload: { type: 'patch_apply_end', changes: { '/work/tempo/a.js': { kind: 'update' }, '/work/tempo/b.js': { kind: 'add' } } },
    }));

  const rows = await watchCodex(cfg, state, { sessionsDir, now });
  assert.equal(rows.length, 4);
  assert.equal(rows[0].actor, 'human');
  assert.equal(rows[0].source, 'codex-desktop');
  assert.equal(rows[1].actor, 'agent');
  assert.equal(rows[1].tokens_in, 1000);
  assert.equal(rows[1].tokens_out, 60);
  assert.ok(rows[1].cost > 0);
  assert.deepEqual(rows.slice(2).map((r) => r.entity).sort(), ['/work/tempo/a.js', '/work/tempo/b.js']);
  assert.ok(rows.slice(2).every((r) => r.is_write === 1 && r.actor === 'agent'));
  assert.deepEqual(await watchCodex(cfg, state, { sessionsDir, now }), []);
});

test('Codex watcher retries session metadata when first sight sees a partial head line', async () => {
  const sessionsDir = tempDir();
  const file = path.join(sessionsDir, 'rollout-partial.jsonl');
  const now = Date.parse('2026-07-18T12:00:00Z') / 1000;
  const meta = JSON.stringify({
    type: 'session_meta', timestamp: '2026-07-18T11:59:00Z',
    payload: { cwd: '/work/recovered', originator: 'Codex Desktop' },
  });
  const split = Math.floor(meta.length / 2);
  fs.writeFileSync(file, meta.slice(0, split));
  const state = {};
  assert.deepEqual(await watchCodex({}, state, { sessionsDir, now }), []);
  assert.equal(state.codexMeta[file].cwd, undefined);

  fs.appendFileSync(file, `${meta.slice(split)}\n${JSON.stringify({
    type: 'event_msg', timestamp: '2026-07-18T11:59:30Z', payload: { type: 'user_message', message: 'hello' },
  })}\n`);
  const rows = await watchCodex({}, state, { sessionsDir, now });
  assert.equal(state.codexMeta[file].cwd, '/work/recovered');
  assert.equal(rows.length, 1);
  assert.equal(rows[0].project, 'recovered');
  assert.equal(rows[0].source, 'codex-desktop');
  assert.equal(rows[0].actor, 'human');
});

test('reattribution requires a same-machine agent write and chooses the nearest edit', () => {
  const save = { time: 100, machine: 'mac', source: 'editor-files', entity: '/p/a.js', entity_type: 'file', actor: 'human' };
  const rows = [
    save,
    { ...save, time: 99, machine: 'linux', source: 'codex-cli', actor: 'agent', is_write: 1 },
    { ...save, time: 98, source: 'claude-code', actor: 'agent', is_write: 0 },
    { ...save, time: 80, source: 'claude-code', actor: 'agent', is_write: 1 },
    { ...save, time: 95, source: 'codex-desktop', actor: 'agent', is_write: 1 },
  ];
  const out = reattributeFileSaves(rows, 120);
  assert.equal(out[0].actor, 'agent');
  assert.equal(out[0].source, 'codex-desktop');
  assert.equal(rows[0].actor, 'human'); // query transformation does not mutate raw rows
});

test('credit streams preserve human switching and parallel agent projects', () => {
  const human = computeCredits([
    { time: 0, machine: 'm', source: 'ssh', actor: 'human', project: 'a' },
    { time: 10, machine: 'm', source: 'ssh', actor: 'human', project: 'b' },
  ]);
  assert.equal(human.reduce((n, r) => n + r.credit, 0), 70);

  const agents = computeCredits([
    { time: 0, machine: 'm', source: 'codex', actor: 'agent', project: 'a' },
    { time: 10, machine: 'm', source: 'codex', actor: 'agent', project: 'b' },
  ]);
  assert.equal(agents.reduce((n, r) => n + r.credit, 0), 120);
});

test('totals, local-day buckets, and timeline segments preserve credit metadata', () => {
  const midnight = Date.parse('2026-07-18T00:30:00Z') / 1000;
  const rows = [
    { time: midnight, machine: 'm', source: 'ssh', actor: 'human', project: 'p', credit: 10, tokens_in: 3, tokens_out: 2, cost: 0.125 },
    { time: midnight + 100, machine: 'm', source: 'editor', actor: 'human', project: 'p', credit: 20, tokens_in: 1, tokens_out: 0, cost: 0.125 },
    { time: midnight + 500, machine: 'm', source: 'ssh', actor: 'human', project: 'p', credit: 30, tokens_in: 0, tokens_out: 0, cost: 0 },
  ];
  assert.deepEqual(totalsBy(rows, ['project']), [{ project: 'p', seconds: 60, tokens: 6, cost: 0.25 }]);
  assert.equal(dayBuckets(rows, ['project'], 480)[0].date, '2026-07-17');
  const segments = buildSegments(rows, { joinGapSeconds: 300 });
  assert.equal(segments.length, 2);
  assert.equal(segments[0].seconds, 30);
  assert.deepEqual(segments[0].sources.sort(), ['editor', 'ssh']);
});

test('agent state and queue writes are atomic and queue batches stay bounded', () => {
  const dir = tempDir();
  const statePath = path.join(dir, 'state.json');
  const queuePath = path.join(dir, 'queue.jsonl');
  saveState({ offset: 42 }, statePath);
  assert.deepEqual(loadState(statePath), { offset: 42 });
  assert.equal(fs.statSync(statePath).mode & 0o777, 0o600);

  const queued = Array.from({ length: 10 }, (_, i) => ({ i, payload: 'x'.repeat(80) }));
  saveQueue(queued, queuePath);
  assert.deepEqual(readQueue(queuePath), queued);
  appendQueue([{ i: 10, payload: 'tail' }], queuePath);
  queued.push({ i: 10, payload: 'tail' });
  assert.deepEqual(readQueue(queuePath), queued);
  assert.equal(fs.statSync(queuePath).mode & 0o777, 0o600);
  const batch = takeSendBatch(queued, 350);
  assert.ok(batch.length > 0 && batch.length < queued.length);
  assert.ok(Buffer.byteLength(JSON.stringify(batch)) <= 350);

  saveQueue(queued.slice(batch.length), queuePath);
  assert.deepEqual(readQueue(queuePath), queued.slice(batch.length));
  saveQueue([], queuePath);
  assert.equal(fs.existsSync(queuePath), false);
  assert.deepEqual(fs.readdirSync(dir).sort(), ['state.json']);
});

test('agent lock excludes a second process and recovers stale lock files', () => {
  const lockPath = path.join(tempDir(), 'agent.lock');
  const release = acquireAgentLock(lockPath);
  assert.throws(() => acquireAgentLock(lockPath), /already running/);
  release();
  assert.equal(fs.existsSync(lockPath), false);

  fs.writeFileSync(lockPath, 'not-a-pid');
  const releaseRecovered = acquireAgentLock(lockPath);
  releaseRecovered();
  assert.equal(fs.existsSync(lockPath), false);
});

test('database migration resumes a partially-added token schema', () => {
  const dbPath = path.join(tempDir(), 'legacy.db');
  const legacy = new DatabaseSync(dbPath);
  legacy.exec(`
    CREATE TABLE heartbeats (
      id INTEGER PRIMARY KEY, time REAL NOT NULL, machine TEXT NOT NULL,
      source TEXT NOT NULL, project TEXT NOT NULL, entity TEXT NOT NULL,
      entity_type TEXT NOT NULL DEFAULT 'file', category TEXT NOT NULL DEFAULT 'coding',
      language TEXT, branch TEXT, is_write INTEGER NOT NULL DEFAULT 0,
      created_at REAL NOT NULL, tokens_in INTEGER NOT NULL DEFAULT 0
    );
    INSERT INTO heartbeats
      (time, machine, source, project, entity, created_at, tokens_in)
      VALUES (1, 'm', 'claude-code', 'p', '/p/a', 1, 7);
    INSERT INTO heartbeats
      (time, machine, source, project, entity, category, created_at, tokens_in)
      VALUES (2, 'm', 'wakatime', 'p', '/p/b', 'maintenance', 1, 0);
    INSERT INTO heartbeats
      (time, machine, source, project, entity, category, created_at, tokens_in)
      VALUES (3, 'm', 'wakatime', 'p', '/p/c', 'AI coding', 1, 0);
  `);
  legacy.close();

  const db = openDb(dbPath);
  try {
    const columns = new Set(db.prepare('PRAGMA table_info(heartbeats)').all().map((c) => c.name));
    assert.ok(columns.has('actor') && columns.has('tokens_in') && columns.has('tokens_out') && columns.has('cost'));
    const rows = db.prepare('SELECT actor, tokens_in, tokens_out, cost FROM heartbeats ORDER BY time').all();
    assert.deepEqual(rows.map((row) => ({ ...row })), [
      { actor: 'agent', tokens_in: 7, tokens_out: 0, cost: 0 },
      { actor: 'human', tokens_in: 0, tokens_out: 0, cost: 0 },
      { actor: 'agent', tokens_in: 0, tokens_out: 0, cost: 0 },
    ]);
  } finally {
    db.close();
  }
});

test('dedupe identity preserves actor separation and keeps token data on the first row', () => {
  const db = openDb(path.join(tempDir(), 'dedupe.db'));
  const base = {
    time: 10, machine: 'm', source: 'codex-cli', project: 'p', entity: '/p/a',
    entity_type: 'file', actor: 'agent', tokens_in: 100, tokens_out: 10, cost: 0.1,
  };
  try {
    assert.equal(insertHeartbeats(db, [base]), 1);
    assert.equal(insertHeartbeats(db, [{ ...base, tokens_in: 999, cost: 9 }]), 0);
    assert.equal(insertHeartbeats(db, [{ ...base, actor: 'human', tokens_in: 0, cost: 0 }]), 1);
    const rows = db.prepare('SELECT actor, tokens_in, cost FROM heartbeats ORDER BY actor').all();
    assert.deepEqual(rows.map((r) => ({ ...r })), [
      { actor: 'agent', tokens_in: 100, cost: 0.1 },
      { actor: 'human', tokens_in: 0, cost: 0 },
    ]);
    assert.equal(insertHeartbeats(db, [{
      ...base, time: 13, tokens_in: 'not-a-number', tokens_out: -4, cost: Infinity,
    }]), 1);
    assert.deepEqual({ ...db.prepare('SELECT tokens_in, tokens_out, cost FROM heartbeats WHERE time = 13').get() },
      { tokens_in: 0, tokens_out: 0, cost: 0 });
    db.exec(`CREATE TRIGGER reject_time_12 BEFORE INSERT ON heartbeats
      WHEN NEW.time = 12 BEGIN SELECT RAISE(ABORT, 'forced failure'); END`);
    assert.throws(() => insertHeartbeats(db, [
      { ...base, time: 11 },
      { ...base, time: 12 },
    ]));
    assert.equal(db.prepare('SELECT count(*) n FROM heartbeats WHERE time IN (11, 12)').get().n, 0);
  } finally {
    db.close();
  }
});

test('Zed watcher sees WAL updates, skips history, and avoids duplicate emissions', async () => {
  const dir = tempDir();
  const dbPath = path.join(dir, 'threads.db');
  const dataDir = path.join(dir, 'tempo-data');
  const writer = new DatabaseSync(dbPath);
  writer.exec(`
    PRAGMA journal_mode = WAL;
    PRAGMA wal_autocheckpoint = 0;
    CREATE TABLE unrelated (value TEXT);
    CREATE TABLE conversations (id TEXT PRIMARY KEY, updated_at TEXT, summary TEXT);
    INSERT INTO conversations VALUES ('one', '1', 'old thread');
  `);

  try {
    const state = {};
    const options = { candidatePaths: [dbPath], dataDir };
    assert.deepEqual(await watchZed({}, state, options), []);
    assert.equal(state.zedInitDone, true);
    const copiedWal = path.join(dataDir, 'zed-threads-copy.db-wal');
    fs.writeFileSync(copiedWal, 'deliberately-stale');

    writer.prepare('UPDATE conversations SET updated_at = ?, summary = ? WHERE id = ?')
      .run('2', 'new summary', 'one');
    const rows = await watchZed({}, state, options);
    assert.equal(rows.length, 1);
    assert.equal(rows[0].actor, 'agent');
    assert.equal(rows[0].entity, 'new summary');
    assert.notEqual(fs.existsSync(copiedWal) ? fs.readFileSync(copiedWal, 'utf8') : '', 'deliberately-stale');
    assert.equal(fs.statSync(path.join(dataDir, 'zed-threads-copy.db')).mode & 0o777, 0o600);
    if (fs.existsSync(copiedWal)) assert.equal(fs.statSync(copiedWal).mode & 0o777, 0o600);
    assert.deepEqual(await watchZed({}, state, options), []);

    writer.prepare('DELETE FROM conversations WHERE id = ?').run('one');
    assert.deepEqual(await watchZed({}, state, options), []);
    assert.equal('one' in state.zedThreads, false);
    writer.prepare('INSERT INTO conversations VALUES (?, ?, ?)').run('one', '3', 'recreated');
    const recreated = await watchZed({}, state, options);
    assert.equal(recreated.length, 1);
    assert.equal(recreated[0].entity, 'recreated');
  } finally {
    writer.close();
  }
});

test('HTTP queries reattribute with lookaround context and recent agrees with summary', async () => {
  const dir = tempDir();
  const cfg = {
    server: { db: path.join(dir, 'tempo.db'), host: '127.0.0.1', port: 0, token: 'scratch-token' },
    summary: { capSeconds: 120, lastEventCreditSeconds: 60, reattributeWindowSeconds: 120, joinGapSeconds: 300 },
  };
  const server = startServer(cfg);
  await once(server, 'listening');
  const base = `http://127.0.0.1:${server.address().port}`;
  const common = { project: 'p', entity: '/p/a.js', entity_type: 'file', category: 'coding' };
  const rows = [
    { ...common, time: 100, machine: 'mac', source: 'editor-files', actor: 'human', is_write: 1 },
    { ...common, time: 101, machine: 'linux', source: 'editor-files', actor: 'human', is_write: 1 },
    { ...common, time: 219, machine: 'mac', source: 'codex-desktop', actor: 'agent', is_write: 1 },
  ];

  try {
    assert.equal((await fetch(`${base}/api/health`)).status, 200);
    const dashboard = await fetch(base);
    assert.equal(dashboard.status, 200);
    assert.match(await dashboard.text(), /const esc =/);
    assert.equal((await fetch(`${base}/missing`)).status, 404);

    const unauthorized = await fetch(`${base}/api/ingest`, {
      method: 'POST', headers: { 'content-type': 'application/json' }, body: '[]',
    });
    assert.equal(unauthorized.status, 401);
    const malformed = await fetch(`${base}/api/ingest`, {
      method: 'POST', headers: { 'content-type': 'application/json', authorization: 'Bearer scratch-token' }, body: '{',
    });
    assert.equal(malformed.status, 400);

    const ingest = await fetch(`${base}/api/ingest`, {
      method: 'POST',
      headers: { 'content-type': 'application/json', authorization: 'Bearer scratch-token' },
      body: JSON.stringify(rows),
    });
    assert.equal(ingest.status, 200);

    const summary = await (await fetch(`${base}/api/summary?from=90&to=110&groupBy=machine,actor,source`)).json();
    assert.ok(summary.totals.some((r) => r.machine === 'mac' && r.actor === 'agent' && r.source === 'codex-desktop'));
    assert.ok(summary.totals.some((r) => r.machine === 'linux' && r.actor === 'human' && r.source === 'editor-files'));

    const recent = await (await fetch(`${base}/api/recent?limit=3`)).json();
    const macSave = recent.find((r) => r.machine === 'mac' && r.time === 100);
    assert.equal(macSave.actor, 'agent');
    assert.equal(macSave.source, 'codex-desktop');
    const timeline = await (await fetch(`${base}/api/timeline?hours=1&to=300`)).json();
    assert.ok(timeline.segments.some((s) => s.actor === 'agent' && s.sources.includes('codex-desktop')));

    const basic = Buffer.from('scratch-token:').toString('base64');
    const waka = await fetch(`${base}/api/v1/users/current/heartbeats.bulk`, {
      method: 'POST',
      headers: {
        'content-type': 'application/json', authorization: `Basic ${basic}`,
        'x-machine-name': 'waka-machine', 'user-agent': 'webstorm/2026.1 plugin/1',
      },
      body: JSON.stringify([
        { time: 400, entity: '/p/waka.js', project: 'p', category: 'AI coding' },
        { time: 401, entity: '/p/maintenance.js', project: 'p', category: 'maintenance' },
      ]),
    });
    assert.equal(waka.status, 202);
    const wakaRows = await (await fetch(`${base}/api/recent?limit=10`)).json();
    const wakaRow = wakaRows.find((r) => r.time === 400);
    assert.equal(wakaRow.actor, 'agent');
    assert.equal(wakaRow.source, 'webstorm');
    assert.equal(wakaRow.machine, 'waka-machine');
    assert.equal(wakaRows.find((r) => r.time === 401).actor, 'human');

    const queuePath = path.join(dir, 'agent', 'queue.jsonl');
    const statePath = path.join(dir, 'agent', 'state.json');
    const lockPath = path.join(dir, 'agent', 'agent.lock');
    const queuedRow = { ...common, time: 300, machine: 'scratch-agent', source: 'codex-cli', actor: 'agent', is_write: 1 };
    saveQueue([queuedRow], queuePath);
    const agentCfg = {
      agent: {
        machine: 'scratch-agent', serverUrl: base, token: 'wrong-token', intervalSeconds: 20,
        projectRoots: [],
        watch: { files: false, claude: false, codex: false, macApps: false, ssh: false, zed: false },
      },
    };
    await runAgent(agentCfg, { once: true, statePath, queuePath, lockPath });
    assert.equal(readQueue(queuePath).length, 1);
    agentCfg.agent.token = 'scratch-token';
    await runAgent(agentCfg, { once: true, statePath, queuePath, lockPath });
    assert.equal(fs.existsSync(queuePath), false);
    const drained = await (await fetch(`${base}/api/recent?limit=10`)).json();
    assert.ok(drained.some((r) => r.machine === 'scratch-agent' && r.time === 300));

    const activeTime = Date.now() / 1000;
    const activeIngest = await fetch(`${base}/api/ingest`, {
      method: 'POST',
      headers: { 'content-type': 'application/json', authorization: 'Bearer scratch-token' },
      body: JSON.stringify([{ ...common, time: activeTime, machine: 'now-machine', source: 'ssh', actor: 'human' }]),
    });
    assert.equal(activeIngest.status, 200);
    const nowRows = await (await fetch(`${base}/api/now?window=invalid`)).json();
    assert.ok(nowRows.some((r) => r.machine === 'now-machine' && r.actor === 'human'));
    assert.deepEqual(await (await fetch(`${base}/api/wakatime-days`)).json(), []);

    assert.equal((await fetch(`${base}/api/summary?days=invalid&tz=invalid`)).status, 200);
    assert.equal((await fetch(`${base}/api/recent?limit=invalid`)).status, 200);
    const oversized = await fetch(`${base}/api/ingest`, {
      method: 'POST',
      headers: { 'content-type': 'application/json', authorization: 'Bearer scratch-token' },
      body: 'x'.repeat(5 * 1024 * 1024 + 1),
    });
    assert.equal(oversized.status, 413);
  } finally {
    server.close();
    await once(server, 'close');
  }
});
