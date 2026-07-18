import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { afterEach, test } from 'node:test';

import {
  generateToken,
  initAgent,
  initServer,
  optionValues,
  runInit,
  writeConfig,
} from '../src/setup.js';

const tempDirs = [];
const cliPath = path.resolve('src/cli.js');

function tempDir() {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'stackhour-setup-test-'));
  tempDirs.push(dir);
  return dir;
}

function configPath() {
  return path.join(tempDir(), 'nested', 'config.json');
}

function readConfig(file) {
  return JSON.parse(fs.readFileSync(file, 'utf8'));
}

function mode(file) {
  return fs.statSync(file).mode & 0o777;
}

function runCli(args, { env = {}, cwd = tempDir() } = {}) {
  const childEnv = { ...process.env, ...env };
  // A nested Node process inherits this under `node --test`; removing it makes
  // the subprocess behave like an actual command-line invocation.
  delete childEnv.NODE_TEST_CONTEXT;
  return spawnSync(process.execPath, ['--experimental-sqlite', '--no-warnings', cliPath, ...args], {
    cwd,
    encoding: 'utf8',
    env: childEnv,
  });
}

afterEach(() => {
  for (const dir of tempDirs.splice(0)) fs.rmSync(dir, { recursive: true, force: true });
});

test('generated setup tokens contain 256 bits of URL-safe randomness', () => {
  const tokens = new Set(Array.from({ length: 64 }, () => generateToken()));
  assert.equal(tokens.size, 64);
  for (const token of tokens) {
    assert.match(token, /^[A-Za-z0-9_-]{43}$/);
    assert.equal(Buffer.from(token, 'base64url').length, 32);
  }
});

test('config writes create parents, use mode 0600, and leave no temporary file', () => {
  const file = configPath();
  writeConfig(file, { server: { tokens: { local: 'secret' } } });

  assert.deepEqual(readConfig(file), { server: { tokens: { local: 'secret' } } });
  assert.equal(mode(file), 0o600);
  assert.deepEqual(fs.readdirSync(path.dirname(file)), ['config.json']);

  fs.chmodSync(file, 0o644);
  writeConfig(file, { agent: { token: 'replacement' } });
  assert.deepEqual(readConfig(file), { agent: { token: 'replacement' } });
  assert.equal(mode(file), 0o600);
  assert.deepEqual(fs.readdirSync(path.dirname(file)), ['config.json']);
});

test('a failed atomic write preserves the old config and cleans up its temporary file', () => {
  const file = configPath();
  writeConfig(file, { keep: 'original-secret' });
  const circular = {};
  circular.self = circular;

  assert.throws(() => writeConfig(file, circular), /circular/i);
  assert.deepEqual(readConfig(file), { keep: 'original-secret' });
  assert.equal(mode(file), 0o600);
  assert.deepEqual(fs.readdirSync(path.dirname(file)), ['config.json']);
});

test('server initialization creates secure defaults and a matching local agent', () => {
  const file = configPath();
  const result = initServer({ configPath: file });
  const saved = readConfig(file);

  assert.equal(result.configPath, file);
  assert.equal(saved.server.host, '0.0.0.0');
  assert.equal(saved.server.port, 4040);
  assert.equal(saved.server.tokens[os.hostname()], result.token);
  assert.equal(Buffer.from(result.token, 'base64url').length, 32);
  assert.equal(path.basename(saved.server.db), 'stackhour.db');
  assert.deepEqual(saved.agent, {
    serverUrl: 'http://127.0.0.1:4040',
    token: result.token,
    machine: os.hostname(),
    projectRoots: [],
  });
  assert.equal(mode(file), 0o600);
});

test('server initialization validates port, host, machine, and explicit token before writing', () => {
  const invalid = [
    [{ port: 0 }, /port/i],
    [{ port: 65536 }, /port/i],
    [{ port: 1.5 }, /port/i],
    [{ port: 'not-a-port' }, /port/i],
    [{ host: '' }, /host/i],
    [{ host: '   ' }, /host/i],
    [{ machine: '' }, /machine/i],
    [{ machine: '   ' }, /machine/i],
    [{ token: '' }, /token/i],
  ];

  for (const [options, error] of invalid) {
    const file = configPath();
    assert.throws(() => initServer({ configPath: file, ...options }), error);
    assert.equal(fs.existsSync(file), false);
  }
});

test('agent initialization normalizes its URL and project roots', () => {
  const dir = tempDir();
  const file = path.join(dir, 'config.json');
  const firstRoot = path.join(dir, 'projects');
  const secondRoot = path.join(dir, 'other-projects');
  fs.mkdirSync(firstRoot);
  fs.mkdirSync(secondRoot);

  const result = initAgent({
    configPath: file,
    serverUrl: 'https://stackhour.example.test/base/',
    token: 'agent-secret',
    machine: '  laptop  ',
    projectRoots: [firstRoot, secondRoot],
  });

  assert.deepEqual(result.agent, {
    serverUrl: 'https://stackhour.example.test/base',
    token: 'agent-secret',
    machine: 'laptop',
    projectRoots: [fs.realpathSync(firstRoot), fs.realpathSync(secondRoot)],
  });
  assert.deepEqual(readConfig(file).agent, result.agent);
  assert.equal(mode(file), 0o600);
});

test('agent initialization rejects invalid URLs, credentials, machines, and roots', () => {
  const dir = tempDir();
  const ordinaryFile = path.join(dir, 'not-a-directory');
  fs.writeFileSync(ordinaryFile, 'x');
  const invalid = [
    [{ serverUrl: undefined, token: 'x' }, /server-url/i],
    [{ serverUrl: 'not a url', token: 'x' }, /valid http/i],
    [{ serverUrl: 'ftp://example.test', token: 'x' }, /http or https/i],
    [{ serverUrl: 'https://example.test', token: '' }, /token/i],
    [{ serverUrl: 'https://example.test', token: 'x', machine: '' }, /machine/i],
    [{ serverUrl: 'https://example.test', token: 'x', machine: '   ' }, /machine/i],
    [{ serverUrl: 'https://example.test', token: 'x', projectRoots: [path.join(dir, 'missing')] }, /project root/i],
    [{ serverUrl: 'https://example.test', token: 'x', projectRoots: [ordinaryFile] }, /project root/i],
  ];

  for (const [options, error] of invalid) {
    const file = path.join(dir, `config-${Math.random()}.json`);
    assert.throws(() => initAgent({ configPath: file, ...options }), error);
    assert.equal(fs.existsSync(file), false);
  }
});

test('initializing one role preserves the other role and unrelated settings', () => {
  const serverFile = configPath();
  writeConfig(serverFile, {
    agent: { serverUrl: 'https://remote.test', token: 'existing-agent-secret', machine: 'mac', projectRoots: [] },
    pricing: { privateModel: { input: 12 } },
  });
  initServer({ configPath: serverFile, token: 'new-server-secret' });
  const withServer = readConfig(serverFile);
  assert.equal(withServer.agent.token, 'existing-agent-secret');
  assert.deepEqual(withServer.pricing, { privateModel: { input: 12 } });
  assert.equal(withServer.server.tokens[os.hostname()], 'new-server-secret');

  const agentFile = configPath();
  writeConfig(agentFile, {
    server: { host: '127.0.0.1', port: 4040, tokens: { server: 'existing-server-secret' }, db: '/tmp/db' },
    wakatime: { apiKey: 'unrelated-secret' },
  });
  initAgent({ configPath: agentFile, serverUrl: 'https://remote.test', token: 'new-agent-secret' });
  const withAgent = readConfig(agentFile);
  assert.equal(withAgent.server.tokens.server, 'existing-server-secret');
  assert.deepEqual(withAgent.wakatime, { apiKey: 'unrelated-secret' });
  assert.equal(withAgent.agent.token, 'new-agent-secret');
});

test('existing role configs are not overwritten without force', () => {
  const serverFile = configPath();
  writeConfig(serverFile, { server: { tokens: { server: 'keep-server-secret' } } });
  assert.throws(() => initServer({ configPath: serverFile, token: 'replacement' }), /--force/);
  assert.equal(readConfig(serverFile).server.tokens.server, 'keep-server-secret');

  const agentFile = configPath();
  writeConfig(agentFile, { agent: { token: 'keep-agent-secret' } });
  assert.throws(() => initAgent({
    configPath: agentFile,
    serverUrl: 'https://new.test',
    token: 'replacement',
  }), /--force/);
  assert.equal(readConfig(agentFile).agent.token, 'keep-agent-secret');
});

test('force replaces only the requested role', () => {
  const file = configPath();
  writeConfig(file, {
    server: { tokens: { server: 'old-server' } },
    agent: { serverUrl: 'https://old.test', token: 'old-agent', machine: 'old', projectRoots: [] },
  });

  initAgent({
    configPath: file,
    force: true,
    serverUrl: 'https://new.test/',
    token: 'new-agent',
    machine: 'new',
  });
  assert.equal(readConfig(file).server.tokens.server, 'old-server');
  assert.equal(readConfig(file).agent.token, 'new-agent');

  initServer({ configPath: file, force: true, token: 'new-server' });
  assert.equal(readConfig(file).server.tokens[os.hostname()], 'new-server');
  assert.equal(readConfig(file).agent.token, 'new-agent');
});

test('malformed existing config is rejected without replacement or secret disclosure', () => {
  for (const init of [
    (file) => initServer({ configPath: file }),
    (file) => initAgent({ configPath: file, serverUrl: 'https://example.test', token: 'x' }),
  ]) {
    const file = configPath();
    const malformed = '{"private":"do-not-leak"';
    fs.mkdirSync(path.dirname(file), { recursive: true });
    fs.writeFileSync(file, malformed, { mode: 0o600 });
    assert.throws(init.bind(null, file), (error) => {
      assert.match(error.message, /cannot read existing config/i);
      assert.doesNotMatch(error.message, /do-not-leak/);
      return true;
    });
    assert.equal(fs.readFileSync(file, 'utf8'), malformed);
  }
});

test('option parsing is exact, repeatable, and keeps values containing equals signs', () => {
  const args = [
    '--project-root=/one',
    '--project-root=/two=part',
    '--project-roots=/wrong',
    '--machine=first',
    '--machine=last',
    '--machine',
    'split-value',
  ];
  assert.deepEqual(optionValues(args, 'project-root'), ['/one', '/two=part']);
  assert.deepEqual(optionValues(args, 'machine'), ['first', 'last']);
  assert.deepEqual(optionValues(args, 'missing'), []);
});

test('runInit applies repeated CLI options while output excludes preserved secrets', () => {
  const dir = tempDir();
  const file = path.join(dir, 'config.json');
  const one = path.join(dir, 'one');
  const two = path.join(dir, 'two');
  fs.mkdirSync(one);
  fs.mkdirSync(two);
  writeConfig(file, {
    server: { tokens: { server: 'preserved-server-secret' } },
    wakatime: { apiKey: 'unrelated-api-secret' },
  });
  let output = '';

  const result = runInit([
    'agent',
    '--server-url=https://stackhour.example.test/',
    '--token=new-agent-secret',
    '--machine=first',
    '--machine=macbook',
    `--project-root=${one}`,
    `--project-root=${two}`,
  ], { configPath: file, stdout: { write: (chunk) => { output += chunk; } } });

  assert.equal(result.agent.machine, 'macbook');
  assert.deepEqual(result.agent.projectRoots, [one, two]);
  assert.match(output, /Created agent config/);
  assert.doesNotMatch(output, /new-agent-secret|preserved-server-secret|unrelated-api-secret/);
});

test('server init output reveals only the newly generated enrollment token', () => {
  const file = configPath();
  writeConfig(file, {
    agent: { serverUrl: 'https://old.test', token: 'preserved-agent-secret', machine: 'mac', projectRoots: [] },
    wakatime: { apiKey: 'unrelated-api-secret' },
  });
  let output = '';
  const result = runInit(['server', '--host=127.0.0.1', '--port=4141'], {
    configPath: file,
    stdout: { write: (chunk) => { output += chunk; } },
  });

  assert.match(output, /Created server config/);
  assert.match(output, new RegExp(result.token));
  assert.doesNotMatch(output, /preserved-agent-secret|unrelated-api-secret/);
});

test('CLI init agent supports environment token fallback and reports failures safely', (t) => {
  const dir = tempDir();
  const file = path.join(dir, 'config.json');
  const root = path.join(dir, 'projects');
  fs.mkdirSync(root);
  const env = {
    STACKHOUR_CONFIG: file,
    STACKHOUR_DATA: path.join(dir, 'data'),
    STACKHOUR_TOKEN: 'environment-agent-secret',
  };

  const success = runCli([
    'init', 'agent',
    '--server-url=https://stackhour.example.test/',
    '--machine=macbook',
    `--project-root=${root}`,
  ], { env, cwd: dir });
  if (success.error?.code === 'EPERM') return t.skip('sandbox does not permit nested process execution');
  assert.equal(success.status, 0, success.stderr);
  assert.match(success.stdout, /Created agent config/);
  assert.doesNotMatch(success.stdout + success.stderr, /environment-agent-secret/);
  assert.equal(readConfig(file).agent.token, 'environment-agent-secret');
  assert.equal(mode(file), 0o600);

  const refused = runCli([
    'init', 'agent',
    '--server-url=https://other.example.test',
    '--token=command-line-secret',
  ], { env, cwd: dir });
  assert.equal(refused.status, 1);
  assert.match(refused.stderr, /--force/);
  assert.doesNotMatch(refused.stdout + refused.stderr, /environment-agent-secret|command-line-secret/);
  assert.equal(readConfig(file).agent.token, 'environment-agent-secret');
});

test('CLI init rejects malformed config without printing its contents', (t) => {
  const dir = tempDir();
  const file = path.join(dir, 'config.json');
  const malformed = '{"secret":"never-print-this"';
  fs.writeFileSync(file, malformed, { mode: 0o600 });
  const result = runCli(['init', 'server'], {
    env: { STACKHOUR_CONFIG: file, STACKHOUR_DATA: path.join(dir, 'data') },
    cwd: dir,
  });
  if (result.error?.code === 'EPERM') return t.skip('sandbox does not permit nested process execution');

  assert.equal(result.status, 1);
  assert.match(result.stderr, /cannot read existing config/i);
  assert.doesNotMatch(result.stdout + result.stderr, /never-print-this/);
  assert.equal(fs.readFileSync(file, 'utf8'), malformed);
});
