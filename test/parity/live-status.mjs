#!/usr/bin/env node
// LIVE STATUS-STREAMING PARITY: boot the reference Node coordinator and the
// Rust bridge against the same local mock Bot API, send both the same prompt,
// run both against the same fake `claude`, and diff the ORDERED sequence of
// Bot API calls each one makes.
//
// The behaviour under test is the thing the owner sees on his phone:
//   * the inbound message gets a 👀 reaction;
//   * ONE status message is posted and then EDITED as activity arrives —
//     never a new message per update;
//   * a typing indicator accompanies the first status and every edit;
//   * the answer is delivered and only then is the status message deleted.
//
// SAFETY: no test here may touch api.telegram.org. Both implementations are
// pinned to a 127.0.0.1 mock with a fake token; the Node side additionally
// runs behind a fetch guard that throws on any non-mock URL. See
// mock-bot-api.mjs and run-node-coordinator.mjs.
//
// Usage: node live-status.mjs [--bin <stackhour binary>]
// Exit code 0 = the sequences match.

import { spawn } from 'node:child_process';
import { mkdtempSync, writeFileSync, readFileSync, existsSync, chmodSync } from 'node:fs';
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
const INBOUND_MSG_ID = 77;

const UPDATES = [{
  update_id: 1,
  message: {
    message_id: INBOUND_MSG_ID,
    chat: { id: CHAT },
    from: { id: 7, is_bot: false },
    text: 'summarise the parity slice',
  },
}];

// A fake `claude` speaking --output-format stream-json. The sleeps straddle
// the 800ms activity throttle so each event yields exactly one status render;
// the five-event burst and the repeated Grep prove the throttle and the
// last-shown dedupe.
const FAKE_ENGINE = `#!/usr/bin/env bash
cat > /dev/null
echo '{"type":"system","subtype":"init","session_id":"sess-parity-1"}'
for i in 1 2 3; do
  echo '{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Bash","input":{"command":"burst '"$i"'"}}]}}'
done
sleep 1.2
echo '{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Read","input":{"file_path":"/tmp/notes.md"}}]}}'
sleep 1.2
echo '{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Read","input":{"file_path":"/tmp/notes.md"}}]}}'
sleep 1.2
echo '{"type":"result","subtype":"success","session_id":"sess-parity-1","result":"the parity answer"}'
`;

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

function writeEngine(dir) {
  const bin = join(dir, 'fake-claude');
  writeFileSync(bin, FAKE_ENGINE);
  chmodSync(bin, 0o755);
  return bin;
}

const config = (base, bin) => ({
  token: TOKEN,
  chatId: CHAT,
  defaultTarget: 'gcp',
  apiRoot: base, // the Rust bridge honours this; the Node side is redirected
  targets: {     // by the fetch guard in run-node-coordinator.mjs.
    gcp: { label: '☁️ GCP', type: 'local', cwd: '/tmp', claudeBin: bin, permissionMode: 'bypassPermissions' },
    mac: { label: '🖥️ Mac', type: 'remote' },
  },
});

/** Run one implementation against a fresh mock; return its recorded requests. */
async function record(name, launch) {
  const dir = mkdtempSync(join(tmpdir(), `parity-status-${name}-`));
  const out = join(dir, 'requests.json');
  const updatesPath = join(dir, 'updates.json');
  writeFileSync(updatesPath, JSON.stringify(UPDATES));

  const mock = await startMock(out, updatesPath);
  const child = launch(dir, mock.base);
  // Long enough for the boot calls, the update, ~4s of fake engine, and the
  // delivery.
  await sleep(9000);
  child.kill('SIGKILL');
  await new Promise((r) => child.on('exit', r));
  await stop(mock.proc);
  return JSON.parse(readFileSync(out, 'utf8'));
}

async function recordNode() {
  return record('node', (dir, base) => {
    const scratch = join(dir, 'ref');
    const bin = writeEngine(dir);
    const child = spawn(
      process.execPath,
      [join(HERE, 'run-node-coordinator.mjs'), REFERENCE, scratch, base],
      { stdio: ['ignore', 'ignore', 'inherit'] },
    );
    // config.json must exist next to the copy before the import runs; the
    // runner creates the dir first, so retry until it lands.
    const cfg = join(scratch, 'config.json');
    const write = () => {
      try { writeFileSync(cfg, JSON.stringify(config(base, bin))); }
      catch { setTimeout(write, 5); }
    };
    write();
    return child;
  });
}

async function recordRust() {
  return record('rust', (dir, base) => {
    const bin = writeEngine(dir);
    writeFileSync(join(dir, 'config.json'), JSON.stringify(config(base, bin)));
    const cfgDir = join(dir, 'no-such-config-dir');
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

// Durations are wall-clock and differ by a second between runs; nothing else
// in the payloads is allowed to drift.
const normalise = (v) => JSON.parse(
  JSON.stringify(v, Object.keys(v || {}).sort()).replace(/· \d+m?\d*s/g, '· <dur>'),
);

/**
 * The ordered (method, body) sequence for the JOB, with the boot calls dropped
 * and polling removed.
 *
 * `getUpdates` is excluded on purpose: against the mock it returns instantly
 * instead of long-polling for 50s, so both implementations spin on it at
 * whatever rate their runtime allows. That rate says nothing about behaviour.
 */
function sequenceOf(reqs) {
  const start = reqs.findIndex((r) => r.method === 'setMessageReaction');
  return reqs
    .slice(start < 0 ? 0 : start)
    .filter((r) => r.method !== 'getUpdates')
    .map((r) => ({ method: r.method, body: normalise(r.body) }));
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
const a = sequenceOf(nodeReqs);
const b = sequenceOf(rustReqs);

const failures = [];

if (process.env.PARITY_DUMP) {
  console.log('--- node ---\n' + JSON.stringify(a, null, 2));
  console.log('--- rust ---\n' + JSON.stringify(b, null, 2));
}

// Guard against a vacuous pass: the reference must actually have streamed.
const expected = ['setMessageReaction', 'sendMessage', 'sendChatAction', 'editMessageText', 'deleteMessage'];
for (const method of expected) {
  if (!a.some((r) => r.method === method)) {
    failures.push(`the reference coordinator never sent ${method} — the harness did not exercise a job`);
  }
}

for (let i = 0; i < Math.max(a.length, b.length); i++) {
  const x = JSON.stringify(a[i] ?? null, null, 2);
  const y = JSON.stringify(b[i] ?? null, null, 2);
  if (x === y) { console.log(`ok   ${i} ${a[i].method}`); continue; }
  failures.push(`call #${i}`);
  console.log(`FAIL ${i}\n  node: ${x}\n  rust: ${y}`);
}

// The headline property, asserted on its own so a regression names itself.
const statuses = (s) => s.filter((r) => r.method === 'sendMessage').length;
if (statuses(a) === statuses(b)) {
  console.log(`ok   ${statuses(b)} sendMessage calls — the job edits, it does not spam`);
} else {
  failures.push(`sendMessage count: node ${statuses(a)} vs rust ${statuses(b)}`);
}

if (failures.length) {
  console.error(`\n${failures.length} parity failure(s): ${failures.join('; ')}`);
  process.exit(1);
}
console.log(`\nlive status-streaming parity: OK (${a.length} calls matched)`);
