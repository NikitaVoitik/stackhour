import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { once } from 'node:events';
import { afterEach, test } from 'node:test';

import { runAgent, saveQueue } from '../src/agent/index.js';
import { startServer } from '../src/server.js';
import { initServer, writeConfig } from '../src/setup.js';
import {
  createMachineToken,
  listMachineTokens,
  revokeMachineToken,
  runToken,
} from '../src/tokens.js';

const tempDirs = [];
const servers = [];

function tempDir() {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'stackhour-token-test-'));
  tempDirs.push(dir);
  return dir;
}

function configPath() {
  return path.join(tempDir(), 'nested', 'config.json');
}

function readConfig(file) {
  return JSON.parse(fs.readFileSync(file, 'utf8'));
}

function captureOutput() {
  let value = '';
  return {
    stdout: { write(chunk) { value += chunk; } },
    read() { return value; },
    clear() { value = ''; },
  };
}

function serverConfig(dir, server = {}) {
  return {
    server: {
      db: path.join(dir, 'stackhour.db'),
      host: '127.0.0.1',
      port: 0,
      tokens: {},
      ...server,
    },
    summary: {
      capSeconds: 120,
      lastEventCreditSeconds: 60,
      reattributeWindowSeconds: 120,
      joinGapSeconds: 300,
    },
  };
}

async function serve(server = {}) {
  const cfg = serverConfig(tempDir(), server);
  const instance = startServer(cfg);
  servers.push(instance);
  await once(instance, 'listening');
  return {
    base: `http://127.0.0.1:${instance.address().port}`,
    cfg,
  };
}

function bearer(token) {
  return { authorization: `Bearer ${token}` };
}

function jsonPost(body, token, headers = {}) {
  return {
    method: 'POST',
    headers: {
      'content-type': 'application/json',
      ...(token === undefined ? {} : bearer(token)),
      ...headers,
    },
    body: JSON.stringify(body),
  };
}

function heartbeat(machine, time = 100) {
  return {
    time,
    machine,
    source: 'editor-files',
    project: 'stackhour',
    entity: `/work/${machine}/server.js`,
    entity_type: 'file',
    category: 'coding',
    actor: 'human',
    is_write: 1,
  };
}

afterEach(async () => {
  await Promise.all(servers.splice(0).map((server) => new Promise((resolve) => server.close(resolve))));
  for (const dir of tempDirs.splice(0)) fs.rmSync(dir, { recursive: true, force: true });
});

test('machine-token lifecycle preserves config, rotates explicitly, uses mode 0600, and never lists secrets', () => {
  const file = configPath();
  writeConfig(file, {
    server: {
      host: '127.0.0.1',
      port: 4242,
      db: '/preserve/stackhour.db',
      tokens: { zed: 'zed-secret' },
      customServerSetting: true,
    },
    agent: { machine: 'local', token: 'agent-secret' },
    pricing: { privateModel: { input: 12 } },
  });
  fs.chmodSync(file, 0o644);

  assert.deepEqual(createMachineToken('macbook', { configPath: file, token: 'mac-secret' }), {
    machine: 'macbook', token: 'mac-secret',
  });
  let saved = readConfig(file);
  assert.deepEqual(saved.server.tokens, { zed: 'zed-secret', macbook: 'mac-secret' });
  assert.equal(saved.server.customServerSetting, true);
  assert.deepEqual(saved.agent, { machine: 'local', token: 'agent-secret' });
  assert.deepEqual(saved.pricing, { privateModel: { input: 12 } });
  assert.equal(fs.statSync(file).mode & 0o777, 0o600);
  assert.deepEqual(listMachineTokens({ configPath: file }), ['macbook', 'zed']);

  assert.throws(
    () => createMachineToken('macbook', { configPath: file, token: 'must-not-write' }),
    /--force/,
  );
  assert.equal(readConfig(file).server.tokens.macbook, 'mac-secret');

  assert.deepEqual(createMachineToken('macbook', {
    configPath: file, force: true, token: 'rotated-secret',
  }), { machine: 'macbook', token: 'rotated-secret' });
  saved = readConfig(file);
  assert.equal(saved.server.tokens.macbook, 'rotated-secret');
  assert.equal(saved.server.tokens.zed, 'zed-secret');

  const output = captureOutput();
  assert.deepEqual(runToken(['list'], { configPath: file, stdout: output.stdout }), ['macbook', 'zed']);
  assert.equal(output.read(), 'macbook\nzed\n');
  assert.doesNotMatch(output.read(), /secret|agent-secret/);

  output.clear();
  assert.deepEqual(runToken(['revoke', 'macbook'], { configPath: file, stdout: output.stdout }), {
    machine: 'macbook',
  });
  assert.equal(output.read(), 'Revoked token for macbook\n');
  assert.doesNotMatch(output.read(), /rotated-secret|zed-secret|agent-secret/);
  assert.deepEqual(readConfig(file).server.tokens, { zed: 'zed-secret' });
  assert.equal(fs.statSync(file).mode & 0o777, 0o600);
  assert.throws(() => revokeMachineToken('macbook', { configPath: file }), /no token exists/);
});

test('token creation rejects duplicate secrets and unsafe names without changing the config or leaking secrets', () => {
  const file = configPath();
  writeConfig(file, {
    server: { tokens: { linux: 'same-private-secret' }, keep: 'server-setting' },
    unrelated: { token: 'unrelated-private-secret' },
  });
  const before = fs.readFileSync(file, 'utf8');

  for (const invalidCase of [
    ['macbook', { token: 'same-private-secret' }, /another machine/],
    ['', { token: 'new' }, /machine/],
    ['   ', { token: 'new' }, /machine/],
    [`bad\nmachine`, { token: 'new' }, /printable/],
    ['x'.repeat(201), { token: 'new' }, /1-200/],
    ['macbook', { token: '' }, /empty/],
  ]) {
    const [machine, tokenOptions, expected] = invalidCase;
    assert.throws(
      () => createMachineToken(machine, { configPath: file, ...tokenOptions }),
      (error) => {
        assert.match(error.message, expected);
        assert.doesNotMatch(error.message, /same-private-secret|unrelated-private-secret/);
        return true;
      },
    );
    assert.equal(fs.readFileSync(file, 'utf8'), before);
  }

  assert.throws(() => listMachineTokens({ configPath: path.join(tempDir(), 'missing.json') }), /cannot read config/);
  const noServer = configPath();
  writeConfig(noServer, { private: 'do-not-print' });
  assert.throws(() => listMachineTokens({ configPath: noServer }), (error) => {
    assert.match(error.message, /server is not initialized/);
    assert.doesNotMatch(error.message, /do-not-print/);
    return true;
  });
});

test('token management rejects array and string token maps without rewriting malformed config', () => {
  for (const tokens of [['array-secret'], 'string-secret']) {
    const file = configPath();
    writeConfig(file, {
      server: { tokens, keep: 'server-setting' },
      unrelated: { token: 'never-print-unrelated' },
    });
    const before = fs.readFileSync(file, 'utf8');

    for (const operation of [
      () => listMachineTokens({ configPath: file }),
      () => createMachineToken('macbook', { configPath: file, token: 'new-secret' }),
      () => revokeMachineToken('macbook', { configPath: file }),
    ]) {
      assert.throws(operation, (error) => {
        assert.match(error.message, /server\.tokens must be an object/);
        assert.doesNotMatch(error.message, /array-secret|string-secret|never-print-unrelated/);
        return true;
      });
      assert.equal(fs.readFileSync(file, 'utf8'), before);
    }
  }
});

test('token command creates and rotates only the named credential and has secret-safe list and usage output', () => {
  const file = configPath();
  writeConfig(file, {
    server: { tokens: { existing: 'never-print-existing' } },
    agent: { token: 'never-print-agent' },
  });
  const output = captureOutput();

  const created = runToken(['create', 'macbook', '--token=created-secret'], {
    configPath: file,
    stdout: output.stdout,
  });
  assert.deepEqual(created, { machine: 'macbook', token: 'created-secret' });
  assert.equal(output.read(), 'Token for macbook: created-secret\n');
  assert.doesNotMatch(output.read(), /never-print-existing|never-print-agent/);

  output.clear();
  const rotated = runToken(['create', 'macbook', '--force', '--token=rotated-secret'], {
    configPath: file,
    stdout: output.stdout,
  });
  assert.deepEqual(rotated, { machine: 'macbook', token: 'rotated-secret' });
  assert.equal(output.read(), 'Token for macbook: rotated-secret\n');
  assert.doesNotMatch(output.read(), /created-secret|never-print-existing|never-print-agent/);
  assert.deepEqual(readConfig(file).server.tokens, {
    existing: 'never-print-existing', macbook: 'rotated-secret',
  });

  output.clear();
  assert.throws(() => runToken(['unknown'], { configPath: file, stdout: output.stdout }), /usage:/);
  assert.equal(output.read(), '');
});

test('server initialization enrolls the local agent in server.tokens with a matching credential', () => {
  const file = configPath();
  writeConfig(file, { pricing: { keep: true }, wakatime: { apiKey: 'private-waka-key' } });
  const result = initServer({
    configPath: file,
    machine: 'build-host',
    token: 'enrollment-secret',
    host: '127.0.0.1',
    port: 4141,
  });
  const saved = readConfig(file);

  assert.deepEqual(saved.server.tokens, { 'build-host': 'enrollment-secret' });
  assert.equal(Object.hasOwn(saved.server, 'token'), false);
  assert.equal(saved.agent.machine, 'build-host');
  assert.equal(saved.agent.token, 'enrollment-secret');
  assert.equal(result.token, 'enrollment-secret');
  assert.deepEqual(saved.pricing, { keep: true });
  assert.deepEqual(saved.wakatime, { apiKey: 'private-waka-key' });
  assert.equal(fs.statSync(file).mode & 0o777, 0o600);
});

test('Bearer, Basic, and api_key auth resolve machine principals exactly and reject near misses', async () => {
  const { base } = await serve({ tokens: { macbook: 'mac-secret', linux: 'linux-secret' } });

  const transports = [
    { headers: bearer('mac-secret') },
    { headers: { authorization: `Basic ${Buffer.from('mac-secret').toString('base64')}` } },
    { headers: { authorization: `Basic ${Buffer.from('mac-secret:').toString('base64')}` } },
    { path: '?api_key=mac-secret' },
  ];
  for (const transport of transports) {
    const response = await fetch(`${base}/api/auth-check${transport.path || ''}`, {
      headers: transport.headers,
    });
    assert.equal(response.status, 200);
    assert.deepEqual(await response.json(), {
      ok: true, version: '0.1.0', machine: 'macbook',
    });
  }

  // Varying lengths and long equal prefixes indirectly exercise both branches
  // of the constant-time equality wrapper without relying on noisy timing tests.
  for (const wrong of ['', 'm', 'mac-secre', 'mac-secret-x', 'mac-secreu', 'x'.repeat(500)]) {
    const response = await fetch(`${base}/api/auth-check`, { headers: bearer(wrong) });
    assert.equal(response.status, 401, `unexpectedly accepted ${JSON.stringify(wrong)}`);
  }
  assert.equal((await fetch(`${base}/api/auth-check`, {
    headers: bearer('wrong'),
  })).status, 401);
  assert.equal((await fetch(`${base}/api/auth-check?api_key=mac-secret`, {
    headers: bearer('wrong'),
  })).status, 401, 'an explicit Authorization header must not fall back to the query token');
});

test('open, legacy, and global authentication remain compatible and unrestricted', async () => {
  const open = await serve();
  assert.deepEqual(await (await fetch(`${open.base}/api/auth-check`)).json(), {
    ok: true, version: '0.1.0', machine: null,
  });
  assert.equal((await fetch(`${open.base}/api/ingest`, jsonPost([
    heartbeat('any-open-machine', 10),
  ]))).status, 200);

  const legacy = await serve({ token: 'legacy-secret', tokens: {} });
  assert.equal((await fetch(`${legacy.base}/api/auth-check`)).status, 401);
  assert.deepEqual(await (await fetch(`${legacy.base}/api/auth-check`, {
    headers: bearer('legacy-secret'),
  })).json(), { ok: true, version: '0.1.0', machine: null });
  assert.equal((await fetch(`${legacy.base}/api/ingest`, jsonPost([
    heartbeat('arbitrary-legacy-machine', 20),
  ], 'legacy-secret'))).status, 200);

  const mixed = await serve({
    token: 'global-secret',
    tokens: { macbook: 'mac-secret' },
  });
  assert.equal((await fetch(`${mixed.base}/api/ingest`, jsonPost([
    heartbeat('linux', 30), heartbeat('macbook', 31),
  ], 'global-secret'))).status, 200);
  assert.equal((await fetch(`${mixed.base}/api/ingest`, jsonPost([
    heartbeat('linux', 32),
  ], 'mac-secret'))).status, 403);
});

test('HTTP authentication ignores malformed array and string token maps without treating their values as credentials', async () => {
  for (const tokens of [['must-not-be-a-machine-token'], 'must-not-be-a-machine-token']) {
    const open = await serve({ tokens });
    const anonymous = await fetch(`${open.base}/api/auth-check`);
    assert.equal(anonymous.status, 200);
    assert.deepEqual(await anonymous.json(), { ok: true, version: '0.1.0', machine: null });

    const legacy = await serve({ tokens, token: 'legacy-secret' });
    assert.equal((await fetch(`${legacy.base}/api/auth-check`, {
      headers: bearer('must-not-be-a-machine-token'),
    })).status, 401);
    assert.deepEqual(await (await fetch(`${legacy.base}/api/auth-check`, {
      headers: bearer('legacy-secret'),
    })).json(), { ok: true, version: '0.1.0', machine: null });
  }
});

test('machine tokens strictly and atomically enforce ingest, agent status, and WakaTime machine identity', async () => {
  const { base } = await serve({ tokens: { macbook: 'mac-secret', linux: 'linux-secret' } });

  for (const endpoint of ['/api/ingest', '/api/agent-status', '/api/v1/users/current/heartbeats']) {
    const body = endpoint === '/api/agent-status'
      ? { time: 100, machine: 'macbook', watchers: {} }
      : endpoint === '/api/ingest' ? [heartbeat('macbook')] : { time: 100, entity: '/work/a.js' };
    assert.equal((await fetch(`${base}${endpoint}`, jsonPost(body, 'wrong-secret', {
      'x-machine-name': 'macbook',
    }))).status, 401);
  }

  const mixedBatch = await fetch(`${base}/api/ingest`, jsonPost([
    heartbeat('macbook', 100), heartbeat('linux', 101),
  ], 'mac-secret'));
  assert.equal(mixedBatch.status, 403);
  assert.match((await mixedBatch.json()).error, /macbook/);
  assert.deepEqual(await (await fetch(`${base}/api/recent`)).json(), [], 'a rejected batch must not partially insert');

  const wrongStatus = await fetch(`${base}/api/agent-status`, jsonPost({
    time: Date.now() / 1000,
    machine: 'linux',
    version: 'test',
    nodeVersion: process.version,
    intervalSeconds: 20,
    queueDepth: 0,
    queueBytes: 0,
    watchers: {},
  }, 'mac-secret'));
  assert.equal(wrongStatus.status, 403);
  assert.deepEqual(await (await fetch(`${base}/api/agent-status`)).json(), []);

  for (const wakaPath of [
    '/api/v1/users/current/heartbeats',
    '/api/v1/users/current/heartbeats.bulk',
    '/users/current/heartbeats',
    '/users/current/heartbeats.bulk',
  ]) {
    const wrongWaka = await fetch(`${base}${wakaPath}`, jsonPost({
      time: 200,
      entity: '/work/linux/a.js',
      project: 'stackhour',
    }, 'mac-secret', { 'x-machine-name': 'linux' }));
    assert.equal(wrongWaka.status, 403, wakaPath);
  }
  assert.deepEqual(await (await fetch(`${base}/api/recent`)).json(), []);

  assert.equal((await fetch(`${base}/api/ingest`, jsonPost([
    heartbeat('macbook', 300),
  ], 'mac-secret'))).status, 200);
  assert.equal((await fetch(`${base}/api/agent-status`, jsonPost({
    time: Date.now() / 1000,
    machine: 'macbook',
    version: 'test',
    nodeVersion: process.version,
    intervalSeconds: 20,
    queueDepth: 0,
    queueBytes: 0,
    watchers: {},
  }, 'mac-secret'))).status, 200);
  const basic = Buffer.from('mac-secret:').toString('base64');
  assert.equal((await fetch(`${base}/api/v1/users/current/heartbeats.bulk`, {
    method: 'POST',
    headers: {
      'content-type': 'application/json',
      authorization: `Basic ${basic}`,
      'x-machine-name': 'macbook',
    },
    body: JSON.stringify([{ time: 301, entity: '/work/macbook/waka.js', project: 'stackhour' }]),
  })).status, 202);

  const recent = await (await fetch(`${base}/api/recent?limit=10`)).json();
  assert.deepEqual(recent.map((row) => row.machine), ['macbook', 'macbook']);
  assert.deepEqual((await (await fetch(`${base}/api/agent-status`)).json()).map((row) => row.machine), ['macbook']);
});

test('a matching one-shot agent drains its queue and records health with its machine token', async () => {
  const { base, cfg } = await serve({ tokens: { buildhost: 'build-secret' } });
  const dir = tempDir();
  const queuePath = path.join(dir, 'queue.jsonl');
  const statePath = path.join(dir, 'state.json');
  const lockPath = path.join(dir, 'agent.lock');
  saveQueue([heartbeat('buildhost', 500)], queuePath);
  const agentCfg = {
    server: cfg.server,
    agent: {
      serverUrl: base,
      token: 'build-secret',
      machine: 'buildhost',
      intervalSeconds: 20,
      projectRoots: [],
      watch: { files: false, claude: false, codex: false, macApps: false, ssh: false, zed: false },
    },
  };

  const report = await runAgent(agentCfg, { once: true, queuePath, statePath, lockPath });
  assert.equal(report.machine, 'buildhost');
  assert.equal(report.queueDepth, 0);
  assert.equal(fs.existsSync(queuePath), false);
  assert.equal(fs.existsSync(lockPath), false);

  const recent = await (await fetch(`${base}/api/recent`)).json();
  assert.equal(recent.length, 1);
  assert.equal(recent[0].machine, 'buildhost');
  const status = await (await fetch(`${base}/api/agent-status`)).json();
  assert.equal(status.length, 1);
  assert.equal(status[0].machine, 'buildhost');
});
