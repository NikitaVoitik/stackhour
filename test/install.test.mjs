import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { afterEach, test } from 'node:test';

import { installService, launchdPlist, runInstall, systemdUnit } from '../src/install.js';

const dirs = [];
function fixture() {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'stackhour-install-test-'));
  dirs.push(dir);
  const repoRoot = path.join(dir, 'repo with space');
  const executable = path.join(repoRoot, 'bin', 'stackhour');
  fs.mkdirSync(path.dirname(executable), { recursive: true });
  fs.writeFileSync(executable, '#!/bin/sh\n', { mode: 0o755 });
  return { dir, home: path.join(dir, 'home'), repoRoot, executable };
}
afterEach(() => {
  for (const dir of dirs.splice(0)) fs.rmSync(dir, { recursive: true, force: true });
});

test('systemd units quote the actual executable and choose role-specific behavior', () => {
  const server = systemdUnit('server', '/home/me/repo with space/bin/stackhour');
  assert.match(server, /ExecStart="\/home\/me\/repo with space\/bin\/stackhour" serve/);
  assert.match(server, /Environment="PATH=/);
  assert.match(server, /RestartSec=5/);
  assert.match(systemdUnit('agent', '/opt/stackhour'), /ExecStart="\/opt\/stackhour" agent[\s\S]*RestartSec=10/);
  assert.throws(() => systemdUnit('other', '/x'), /server or agent/);
  assert.throws(() => systemdUnit('agent', '/bad\npath'), /newline/);
});

test('launchd plist escapes paths and provides a Homebrew-aware PATH', () => {
  const plist = launchdPlist('/Users/me/a&b/<stackhour>', '/Users/me/node&bin');
  assert.match(plist, /a&amp;b\/&lt;stackhour&gt;/);
  assert.match(plist, /com\.stackhour\.agent/);
  assert.match(plist, /\/opt\/homebrew\/bin:\/usr\/local\/bin/);
  assert.match(plist, /node&amp;bin/);
});

test('Linux installation writes user units and invokes systemctl deterministically', () => {
  const f = fixture();
  const calls = [];
  const result = installService('server', {
    platform: 'linux', home: f.home, repoRoot: f.repoRoot,
    run: (command, args) => calls.push([command, args]),
  });
  assert.equal(result.service, 'stackhour-server.service');
  assert.equal(fs.statSync(result.path).mode & 0o777, 0o644);
  assert.match(fs.readFileSync(result.path, 'utf8'), new RegExp(f.executable.replace(/[.*+?^${}()|[\]\\]/g, '\\$&')));
  assert.deepEqual(calls, [
    ['systemctl', ['--user', 'daemon-reload']],
    ['systemctl', ['--user', 'enable', '--now', 'stackhour-server.service']],
  ]);
});

test('macOS installation writes and loads a launch agent, tolerating an unloaded prior service', () => {
  const f = fixture();
  const calls = [];
  const result = installService('agent', {
    platform: 'darwin', home: f.home, uid: 501, repoRoot: f.repoRoot,
    run: (command, args) => {
      calls.push([command, args]);
      if (args[0] === 'bootout') throw new Error('not loaded');
    },
  });
  assert.equal(result.service, 'com.stackhour.agent');
  assert.equal(calls.length, 4);
  assert.deepEqual(calls.map((call) => call[1][0]), ['bootout', 'bootstrap', 'enable', 'kickstart']);
  assert.match(fs.readFileSync(result.path, 'utf8'), /repo with space/);
  assert.throws(() => installService('server', { platform: 'darwin', home: f.home, uid: 501, repoRoot: f.repoRoot }), /agent role only/);
});

test('installer rejects unsupported environments and missing executables', () => {
  const f = fixture();
  assert.throws(() => installService('agent', { platform: 'win32', home: f.home, repoRoot: f.repoRoot }), /not supported/);
  assert.throws(() => installService('agent', { platform: 'linux', home: f.home, repoRoot: path.join(f.dir, 'missing') }), /not found/);
});

test('runInstall installs both server roles or only an agent and validates usage', () => {
  const roles = [];
  let output = '';
  const options = {
    installer: (role) => { roles.push(role); return { role }; },
    stdout: { write: (value) => { output += value; } },
  };
  assert.deepEqual(runInstall(['server'], options), [{ role: 'server' }, { role: 'agent' }]);
  assert.deepEqual(roles, ['server', 'agent']);
  assert.match(output, /server and stackhour-agent/);
  roles.length = 0;
  assert.deepEqual(runInstall(['agent'], options), [{ role: 'agent' }]);
  assert.deepEqual(roles, ['agent']);
  assert.throws(() => runInstall([], options), /usage/);
});
