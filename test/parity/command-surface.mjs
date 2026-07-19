#!/usr/bin/env node
// COMMAND-SURFACE PARITY: boot the reference Node coordinator and the Rust
// bridge against the same local mock Bot API, feed both the same updates, and
// diff the requests they make.
//
// What is compared:
//   * the setMyCommands payload (every command + description, in order)
//   * the /help body
//   * the inline keyboard layout carried by /help and /menu
//
// SAFETY: no test here may touch api.telegram.org. Both implementations are
// pinned to a 127.0.0.1 mock with a fake token; the Node side additionally
// runs behind a fetch guard that throws on any non-mock URL. See
// mock-bot-api.mjs and run-node-coordinator.mjs.
//
// Usage: node command-surface.mjs [--bin <stackhour binary>]
// Exit code 0 = the surfaces match.

import { spawn } from 'node:child_process';
import { mkdtempSync, mkdirSync, writeFileSync, readFileSync, existsSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join, dirname } from 'node:path';
import { fileURLToPath } from 'node:url';

const HERE = dirname(fileURLToPath(import.meta.url));
const REPO = join(HERE, '..', '..');
const REFERENCE = process.env.PARITY_REFERENCE ||
  join(process.env.HOME, '.claude-remote', 'coordinator.mjs');

const argv = process.argv.slice(2);
const BIN = (() => {
  const i = argv.indexOf('--bin');
  if (i >= 0) return argv[i + 1];
  return join(REPO, 'target', 'release', 'stackhour');
})();

const CHAT = 4242;
// Obviously fake. The real token lives only in the owner's config and is
// never read, copied, or logged by this harness.
const TOKEN = '123456:FAKE-TOKEN-FOR-PARITY-TESTS';

const UPDATES = [
  { update_id: 1, message: { message_id: 11, chat: { id: CHAT }, from: { id: 7, is_bot: false }, text: '/help' } },
  { update_id: 2, message: { message_id: 12, chat: { id: CHAT }, from: { id: 7, is_bot: false }, text: '/menu' } },
];

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

function startMock(outPath, updatesPath) {
  return new Promise((resolve, reject) => {
    const p = spawn(process.execPath, [join(HERE, 'mock-bot-api.mjs'), outPath, updatesPath], {
      stdio: ['ignore', 'pipe', 'inherit'],
    });
    p.stdout.on('data', (d) => {
      const m = /PORT (\d+)/.exec(d.toString());
      if (m) resolve({ proc: p, base: `http://127.0.0.1:${m[1]}` });
    });
    p.on('exit', (c) => reject(new Error(`mock exited early (${c})`)));
  });
}

async function stop(proc) {
  proc.kill('SIGTERM');
  await new Promise((r) => proc.on('exit', r));
}

/** Run one implementation against a fresh mock; return its recorded requests. */
async function record(name, launch) {
  const dir = mkdtempSync(join(tmpdir(), `parity-${name}-`));
  const out = join(dir, 'requests.json');
  const updatesPath = join(dir, 'updates.json');
  writeFileSync(updatesPath, JSON.stringify(UPDATES));

  const mock = await startMock(out, updatesPath);
  const child = launch(dir, mock.base);
  // Long enough for: setMyCommands, the banner, one getUpdates carrying both
  // updates, and the replies.
  await sleep(3000);
  child.kill('SIGKILL');
  await new Promise((r) => child.on('exit', r));
  await stop(mock.proc);
  return JSON.parse(readFileSync(out, 'utf8'));
}

const nodeConfig = (base) => ({
  token: TOKEN,
  chatId: CHAT,
  defaultTarget: 'gcp',
  apiRoot: base, // ignored by the Node coordinator; the fetch guard redirects.
  targets: {
    gcp: { label: '☁️ GCP', type: 'local', cwd: '/tmp', claudeBin: '/bin/true', codexBin: '/bin/true' },
    mac: { label: '🖥️ Mac', type: 'remote' },
    blort: { label: '🚀 Blort', type: 'local', cwd: '/tmp', claudeBin: '/bin/true' },
  },
});

async function recordNode() {
  return record('node', (dir, base) => {
    const scratch = join(dir, 'ref');
    writeFileSync(join(dir, 'config.json'), JSON.stringify(nodeConfig(base)));
    // The copy reads config.json from its own directory, so put it there too.
    const child = spawn(
      process.execPath,
      [join(HERE, 'run-node-coordinator.mjs'), REFERENCE, scratch, base],
      { stdio: ['ignore', 'ignore', 'inherit'] },
    );
    // config.json must exist next to the copy before the import runs; the
    // runner creates the dir first, so write it on the next tick.
    const cfg = join(scratch, 'config.json');
    const write = () => { try { writeFileSync(cfg, JSON.stringify(nodeConfig(base))); } catch { setTimeout(write, 5); } };
    write();
    return child;
  });
}

async function recordRust(configDir) {
  return record('rust', (dir, base) => {
    writeFileSync(join(dir, 'config.json'), JSON.stringify(nodeConfig(base)));
    // With no config dir, the registry falls back to shipped defaults only —
    // which is the state the Node bridge is being compared against.
    const cfgDir = configDir || join(dir, 'no-such-config-dir');
    return spawn(BIN, ['bridge', 'coordinator', '--runtime-dir', dir], {
      stdio: ['ignore', 'ignore', 'inherit'],
      env: {
        ...process.env,
        STACKHOUR_CONFIG_DIR: cfgDir,
        STACKHOUR_CONFIG: join(cfgDir, 'config.json'),
        STACKHOUR_DATA: join(dir, 'data'),
      },
    });
  });
}

// --- extraction -------------------------------------------------------------

const first = (reqs, method, pred = () => true) =>
  reqs.find((r) => r.method === method && pred(r))?.body;

function surfaceOf(reqs) {
  const setMy = first(reqs, 'setMyCommands');
  const help = first(reqs, 'sendMessage', (r) => /Claude \+ Codex bridge<\/b>/.test(r.body.text || ''));
  const menu = first(reqs, 'sendMessage', (r) => /^🎛 Controls/.test(r.body.text || ''));
  return {
    setMyCommands: setMy,
    helpText: help?.text,
    helpParseMode: help?.parse_mode,
    helpKeyboard: help?.reply_markup,
    menuText: menu?.text,
    menuKeyboard: menu?.reply_markup,
  };
}

// --- run --------------------------------------------------------------------

if (!existsSync(BIN)) {
  console.error(`missing binary: ${BIN}\nbuild it with: cargo build --release`);
  process.exit(2);
}
if (!existsSync(REFERENCE)) {
  console.error(`missing reference coordinator: ${REFERENCE}`);
  process.exit(2);
}

const nodeReqs = await recordNode();
const rustReqs = await recordRust();
const a = surfaceOf(nodeReqs);
const b = surfaceOf(rustReqs);

const failures = [];
function cmp(label, x, y) {
  const sx = JSON.stringify(x, null, 2);
  const sy = JSON.stringify(y, null, 2);
  if (sx === sy) { console.log(`ok   ${label}`); return; }
  failures.push(label);
  console.log(`FAIL ${label}\n  node: ${sx}\n  rust: ${sy}`);
}

// Guard against a vacuous pass: if neither side sent anything, every cmp()
// below would compare undefined to undefined and report OK.
for (const [label, value] of [
  ['setMyCommands', a.setMyCommands],
  ['/help text', a.helpText],
  ['/help keyboard', a.helpKeyboard],
  ['/menu text', a.menuText],
]) {
  if (value === undefined) failures.push(`the reference coordinator produced no ${label}`);
}
if (!Array.isArray(a.setMyCommands?.commands) || !Array.isArray(b.setMyCommands?.commands)) {
  failures.push('setMyCommands.commands must be an array on both sides');
}
if (process.env.PARITY_DUMP) {
  console.log('--- node surface ---\n' + JSON.stringify(a, null, 2));
  console.log('--- rust surface ---\n' + JSON.stringify(b, null, 2));
}

cmp('setMyCommands payload', a.setMyCommands, b.setMyCommands);
cmp('/help text', a.helpText, b.helpText);
cmp('/help parse_mode', a.helpParseMode, b.helpParseMode);
cmp('/help inline keyboard', a.helpKeyboard, b.helpKeyboard);
cmp('/menu text', a.menuText, b.menuText);
cmp('/menu inline keyboard', a.menuKeyboard, b.menuKeyboard);

// Every command the Node bridge registers must be present in the Rust one.
const names = (p) => (p?.commands || []).map((c) => c.command);
const missing = names(a.setMyCommands).filter((n) => !names(b.setMyCommands).includes(n));
if (missing.length) { failures.push(`missing commands: ${missing.join(', ')}`); }
else console.log(`ok   all ${names(a.setMyCommands).length} Node commands registered by Rust`);

// The three surfaces must be GENERATED from one table, not written twice: a
// command added to config has to show up in registration AND in /help without
// touching either.
{
  const cfgDir = mkdtempSync(join(tmpdir(), 'parity-cfgdir-'));
  mkdirSync(join(cfgDir, 'commands'), { recursive: true });
  writeFileSync(
    join(cfgDir, 'commands', 'parityprobe.toml'),
    'description = "Parity probe"\nkind = "shell"\nargv = ["true"]\n',
  );
  const probe = surfaceOf(await recordRust(cfgDir));
  const registered = (probe.setMyCommands?.commands || []).some((c) => c.command === 'parityprobe');
  const helped = /\/parityprobe — Parity probe/.test(probe.helpText || '');
  // ...and it must not have displaced any Node command.
  const stillThere = names(a.setMyCommands).every((n) =>
    (probe.setMyCommands?.commands || []).some((c) => c.command === n));
  if (registered && helped && stillThere) {
    console.log('ok   a config-only command reaches setMyCommands and /help');
  } else {
    failures.push(
      `config-driven command (registered=${registered} helped=${helped} builtinsIntact=${stillThere})`);
    console.log(`FAIL config-driven command\n  ${JSON.stringify(probe.setMyCommands)}\n  ${probe.helpText}`);
  }
}

if (failures.length) {
  console.error(`\n${failures.length} parity failure(s): ${failures.join('; ')}`);
  process.exit(1);
}
console.log('\ncommand surface parity: OK');
