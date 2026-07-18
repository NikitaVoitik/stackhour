import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import { mkdtempSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import test from 'node:test';
import { fileURLToPath } from 'node:url';

const cli = fileURLToPath(new URL('../src/cli.js', import.meta.url));

function runCli(args, env = {}) {
  return spawnSync(process.execPath, ['--experimental-sqlite', '--no-warnings', cli, ...args], {
    encoding: 'utf8',
    env: { HOME: process.env.HOME, PATH: process.env.PATH, ...env },
  });
}

test('CLI exposes first-class bridge install and doctor commands', () => {
  const result = runCli(['bridge', '--help']);
  assert.equal(result.status, 0);
  assert.match(result.stdout, /stackhour bridge install <coordinator\|worker>/);
  assert.match(result.stdout, /stackhour bridge doctor <coordinator\|worker>/);
});

test('main help lists the bridge subcommand', () => {
  const result = runCli([]);
  assert.equal(result.status, 0);
  assert.match(result.stdout, /bridge install <coordinator\|worker>/);
});

test('non-interactive install fails before writing when required secrets are absent', () => {
  const runtime = mkdtempSync(join(tmpdir(), 'stackhour-bridge-test-'));
  const result = runCli(['bridge', 'install', 'coordinator', '--non-interactive', '--runtime-dir', runtime]);
  assert.equal(result.status, 1);
  assert.match(result.stderr, /TELEGRAM_BOT_TOKEN is required/);
});
