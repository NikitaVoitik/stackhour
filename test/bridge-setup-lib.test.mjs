import assert from 'node:assert/strict';
import test from 'node:test';
import {
  LAUNCHD_LABEL, mergedPath, renderLaunchAgent, renderSystemdUnit,
  shellQuote, validateCoordinatorConfig, validateWorkerConfig,
} from '../src/bridge/setup-lib.mjs';

const coordinator = {
  token: 'test-token',
  chatId: -100123,
  defaultTarget: 'gcp',
  targets: {
    gcp: {
      cwd: '/tmp/work',
      claudeBin: '/bin/claude',
      codexBin: '/bin/codex',
      permissionMode: 'default',
    },
    mac: { permissionMode: 'default' },
  },
};

const worker = {
  gcpSsh: 'user@example.com',
  gcpKey: '/tmp/key',
  remoteDir: '/home/user/.local/share/stackhour/bridge',
  remoteNode: '/usr/bin/node',
  claudeBin: '/bin/claude',
  codexBin: '/bin/codex',
  cwd: '/tmp/work',
  permissionMode: 'default',
};

test('validates coordinator and worker configs', () => {
  assert.deepEqual(validateCoordinatorConfig(coordinator), []);
  assert.deepEqual(validateWorkerConfig(worker), []);
  assert.ok(validateCoordinatorConfig({}).length >= 4);
  assert.ok(validateWorkerConfig({}).length >= 7);
});

test('renders a concrete user systemd service', () => {
  const unit = renderSystemdUnit({
    nodePath: '/usr/bin/node',
    runtimeDir: '/home/me/.local/share/stackhour/bridge',
    home: '/home/me',
    pathValue: '/usr/bin:/bin',
  });
  assert.match(unit, /ExecStart="\/usr\/bin\/node" "\/home\/me\/\.local\/share\/stackhour\/bridge\/coordinator\.mjs"/);
  assert.match(unit, /WantedBy=default\.target/);
  assert.doesNotMatch(unit, /CHANGE_ME|__HOME__|User=/);
});

test('renders a concrete and escaped LaunchAgent', () => {
  const plist = renderLaunchAgent({
    nodePath: '/opt/node&tools/node',
    runtimeDir: '/Users/me/A & B',
    home: '/Users/me',
    pathValue: '/usr/bin:/bin',
  });
  assert.match(plist, new RegExp(LAUNCHD_LABEL.replaceAll('.', '\\.')));
  assert.match(plist, /node&amp;tools/);
  assert.match(plist, /A &amp; B/);
  assert.doesNotMatch(plist, /__HOME__|__NODE__/);
});

test('merges paths without duplicates', () => {
  assert.equal(mergedPath('/a:/b', '/b:/c'), '/a:/b:/c');
});

test('quotes remote shell values', () => {
  assert.equal(shellQuote("a'b"), "'a'\"'\"'b'");
});
